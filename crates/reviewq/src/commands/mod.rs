mod actions;
mod doctor;
mod help;
mod history;
mod list;
mod review;
mod show;
mod sync;
mod tui;

use std::process::ExitCode;

use anyhow::Result;
use reviewq_app::config;

use crate::cli::{Cli, Command};
use crate::colour::Output;

/// The read commands exit with this when there is nothing to show, so shell
/// wrappers can branch on "nothing to do" without parsing output.
pub const EXIT_EMPTY: u8 = 2;

struct PagerEnv {
    reviewq_pager: Option<std::ffi::OsString>,
    pager: Option<std::ffi::OsString>,
    less: Option<std::ffi::OsString>,
    less_utf_char_def: Option<std::ffi::OsString>,
}

impl PagerEnv {
    fn current() -> Self {
        Self {
            reviewq_pager: std::env::var_os("REVIEWQ_PAGER"),
            pager: std::env::var_os("PAGER"),
            less: std::env::var_os("LESS"),
            less_utf_char_def: std::env::var_os("LESSUTFCHARDEF"),
        }
    }
}

fn page_out(output: &impl Output, text: &str) {
    page_out_with(output, text, PagerEnv::current());
}

fn page_out_with(output: &impl Output, text: &str, env: PagerEnv) {
    use std::io::Write as _;

    if !output.stdout_is_terminal() {
        output.write(crate::colour::plain(text));
        return;
    }

    let Some(argv) = pager_argv(env.reviewq_pager, env.pager) else {
        output.write(crate::colour::plain(text));
        return;
    };

    let mut command = std::process::Command::new(&argv[0]);
    command.args(&argv[1..]).stdin(std::process::Stdio::piped());
    for (name, value) in less_defaults(env.less, env.less_utf_char_def) {
        command.env(name, value);
    }

    let Ok(mut child) = command.spawn() else {
        output.write(crate::colour::plain(text));
        return;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(text.as_bytes());
    }
    drop(child.stdin.take());
    let _ = child.wait();
}

fn less_defaults(
    less: Option<std::ffi::OsString>,
    chardef: Option<std::ffi::OsString>,
) -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    if less.is_none() {
        out.push(("LESS", "FR"));
    }
    if chardef.is_none() {
        out.push((
            "LESSUTFCHARDEF",
            "E000-F8FF:p,F0000-FFFFD:p,100000-10FFFD:p",
        ));
    }
    out
}

fn pager_argv(
    reviewq: Option<std::ffi::OsString>,
    pager: Option<std::ffi::OsString>,
) -> Option<Vec<String>> {
    let chosen = reviewq
        .or(pager)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "less".to_string());
    let argv: Vec<String> = chosen.split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// Load the config, then run the command against it.
///
/// One load, before anything else, and a failure ends the run here. Every
/// command gets it whether or not it reads the forge: a config that doesn't
/// parse is a config whose repos, rules and identity are unknown, so a `list`
/// that carried on regardless would be showing a queue it can no longer explain.
/// It costs a file read and a parse, which is nothing next to opening the ledger.
pub async fn dispatch(cli: Cli, output: &impl Output) -> Result<ExitCode> {
    // Before the config, and deliberately: the documentation is most wanted by
    // somebody whose config does not work, and a help page that refused to
    // print until the config parsed would be missing exactly then. It reads the
    // config only for presentation; missing settings use their defaults.
    if let Command::Help(args) = &cli.command {
        let presentation = config::load(cli.config.as_deref())
            .map_or(Default::default(), |loaded| loaded.config.output);
        return help::run(&presentation, args, output);
    }

    let loaded = config::load(cli.config.as_deref())?;
    if loaded.created {
        output.println(format!(
            "wrote a default config to {} — edit it before syncing",
            loaded.path.display()
        ));
    }
    // Every command, not just `doctor`: a key nobody reads is usually a typo,
    // and a typo in a rule changes what you track. Said once, briefly, with the
    // command that can say more.
    if !loaded.unknown.is_empty() {
        output.eprintln(format!(
            "warning: {} in {} {} not read: {} — see `reviewq doctor`",
            match loaded.unknown.len() {
                1 => "one setting".to_string(),
                n => format!("{n} settings"),
            },
            loaded.path.display(),
            match loaded.unknown.len() {
                1 => "is",
                _ => "are",
            },
            loaded.unknown.join(", ")
        ));
    }

    let cfg = &loaded;
    // Logging shares stderr with sync's progress line; the in-place rewrite is
    // only tidy when nothing else is writing there.
    let logging = cli.verbose > 0;

    match cli.command {
        Command::Sync(args) => sync::run(cfg, &args, logging, output).await,
        Command::List(args) => list::run(cfg, &args, output),
        Command::Next(args) => list::next(cfg, &args, output),
        Command::Show(args) => show::run(cfg, &args, output),
        Command::Done(args) => actions::done(cfg, &args, output).await,
        Command::Snooze(args) => actions::snooze(&args, output),
        Command::Mute(args) => actions::mute(&args, output),
        Command::Unmute(args) => actions::unmute(&args, output),
        Command::Defer(args) => actions::defer(&args, output),
        Command::Undefer(args) => actions::undefer(&args, output),
        Command::Track(args) => actions::track(cfg, &args, output).await,
        Command::Untrack(args) => actions::untrack(&args, output),
        Command::Review(args) => review::run(cfg, &args, output).await,
        Command::History(args) => history::run(cfg, &args, output).await,
        Command::Tui => tui::run(cfg, output).await,
        Command::Doctor => doctor::run(cfg, output).await,
        // Taken above, before the config was loaded — matched here so that
        // adding a command and forgetting to dispatch it still fails to
        // compile.
        Command::Help(_) => unreachable!("help is served before the config"),
    }
}

#[cfg(test)]
mod pager_tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;

    use super::{PagerEnv, less_defaults, page_out_with, pager_argv};
    use crate::colour::testing::FakeOutput;

    #[test]
    fn terminal_paging_applies_shared_precedence_defaults_and_spawn_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pager = dir.path().join("capture-pager");
        let captured = dir.path().join("captured");
        std::fs::write(
            &pager,
            "#!/bin/sh\nprintf '%s\\n%s\\n' \"$LESS\" \"$LESSUTFCHARDEF\" > \"$1\"\ncat >> \"$1\"\n",
        )
        .expect("pager script");
        let mut permissions = std::fs::metadata(&pager).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&pager, permissions).unwrap();
        let output = FakeOutput::new(false).with_stdout_terminal();
        let reviewq_pager = format!("{} {}", pager.display(), captured.display());

        page_out_with(
            &output,
            "activity page\n",
            PagerEnv {
                reviewq_pager: Some(OsString::from(reviewq_pager)),
                pager: Some(OsString::from("/the/general/pager/must/not/run")),
                less: None,
                less_utf_char_def: None,
            },
        );

        assert_eq!(
            std::fs::read_to_string(&captured).unwrap(),
            "FR\nE000-F8FF:p,F0000-FFFFD:p,100000-10FFFD:p\nactivity page\n"
        );
        assert!(
            output.stdout.borrow().is_empty(),
            "the pager child owns successful paged output"
        );

        page_out_with(
            &output,
            "fallback page\n",
            PagerEnv {
                reviewq_pager: Some(OsString::from("/definitely/not/a/pager")),
                pager: None,
                less: None,
                less_utf_char_def: None,
            },
        );
        assert_eq!(&*output.stdout.borrow(), "fallback page\n");
    }

    #[test]
    fn pager_selection_honours_empty_values_and_reader_settings() {
        let os = |value: &str| Some(OsString::from(value));

        assert_eq!(pager_argv(None, None).unwrap(), ["less"]);
        assert_eq!(pager_argv(None, os("bat")).unwrap(), ["bat"]);
        assert_eq!(
            pager_argv(os("less -S"), os("bat")).unwrap(),
            ["less", "-S"]
        );
        assert_eq!(pager_argv(None, os("")), None);
        assert_eq!(pager_argv(os("   "), None), None);
        assert_eq!(
            less_defaults(os("R"), None),
            [(
                "LESSUTFCHARDEF",
                "E000-F8FF:p,F0000-FFFFD:p,100000-10FFFD:p"
            )]
        );
        assert!(less_defaults(os("R"), os("x")).is_empty());
    }
}
