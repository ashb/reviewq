mod actions;
mod doctor;
mod list;
mod review;
mod show;
mod sync;
mod tui;

use std::process::ExitCode;

use anyhow::Result;
use reviewq_app::config;

use crate::cli::{Cli, Command};

/// The read commands exit with this when there is nothing to show, so shell
/// wrappers can branch on "nothing to do" without parsing output.
pub const EXIT_EMPTY: u8 = 2;

/// Load the config, then run the command against it.
///
/// One load, before anything else, and a failure ends the run here. Every
/// command gets it whether or not it reads the forge: a config that doesn't
/// parse is a config whose repos, rules and identity are unknown, so a `list`
/// that carried on regardless would be showing a queue it can no longer explain.
/// It costs a file read and a parse, which is nothing next to opening the ledger.
pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let loaded = config::load(cli.config.as_deref())?;
    if loaded.created {
        println!(
            "wrote a default config to {} — edit it before syncing",
            loaded.path.display()
        );
    }
    let cfg = &loaded;
    // Logging shares stderr with sync's progress line; the in-place rewrite is
    // only tidy when nothing else is writing there.
    let logging = cli.verbose > 0;
    match cli.command {
        Command::Sync(args) => sync::run(cfg, &args, logging).await,
        Command::List(args) => list::run(&args),
        Command::Next(args) => list::next(&args),
        Command::Show(args) => show::run(cfg, &args),
        Command::Done(args) => actions::done(cfg, &args).await,
        Command::Snooze(args) => actions::snooze(&args),
        Command::Mute(args) => actions::mute(&args),
        Command::Unmute(args) => actions::unmute(&args),
        Command::Defer(args) => actions::defer(&args),
        Command::Undefer(args) => actions::undefer(&args),
        Command::Track(args) => actions::track(cfg, &args).await,
        Command::Review(args) => review::run(cfg, &args).await,
        Command::Tui => tui::run(cfg).await,
        Command::Doctor => doctor::run(cfg).await,
    }
}
