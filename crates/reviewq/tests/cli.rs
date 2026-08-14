//! The CLI's observable contract: exit codes and stderr wording. Scripts and
//! cron jobs branch on these, so they are tested rather than assumed.
//!
//! Nothing here touches the network. `doctor`'s successful path is exercised by
//! hand against real GitHub, since faking it would only test the fake.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use reviewq_core::model::PrSnapshot;
use reviewq_ledger::Ledger;

/// Cargo builds the binary before running integration tests and hands us its
/// path, so no dependency on `assert_cmd` is needed.
const BIN: &str = env!("CARGO_BIN_EXE_reviewq");

/// Run the binary against a config and ledger made for this call and thrown away
/// after it.
///
/// Every spawn in this file goes through here or [`run_in`], and both set
/// `REVIEWQ_CONFIG` and `REVIEWQ_DB`. Neither may be omitted: without them the
/// binary reads the developer's own config and `Ledger::open` *creates* their
/// ledger — a test suite has no business anywhere near either.
fn run(args: &[&str]) -> Output {
    let (_dir, config, db) = workspace();
    run_in(&config, &db, args)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A temp dir holding a minimal valid config and a path for a fresh ledger.
///
/// Every command loads and validates the config before doing anything, so a test
/// that reaches past argument parsing needs a real one. Held by the caller: the
/// directory (and everything in it) is removed when it drops.
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        r#"
        [[project]]
        repos = [{ owner = "apache", name = "airflow" }]
        [[project.interest]]
        labels = ["area:task-sdk"]
        "#,
    )
    .expect("write config");
    let db = dir.path().join("reviewq.db");
    (dir, config, db)
}

/// Run the binary against a specific config and ledger — both required, for the
/// reason in [`run`].
fn run_in(config: &Path, db: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("REVIEWQ_CONFIG", config)
        .env("REVIEWQ_DB", db)
        .env("NO_COLOR", "1")
        .output()
        .expect("binary runs")
}

/// Nothing here may reach the developer's own config or ledger.
///
/// Enforced rather than remembered: the only `Command::new(BIN)` in this file is
/// the one inside `run_in`, which sets both environment variables. A test that
/// spawned the binary itself could silently read — and write — real data, and
/// would look exactly like every other test while doing it.
#[test]
fn every_spawn_goes_through_the_helper_that_isolates_config_and_ledger() {
    let source = include_str!("cli.rs");
    // Call sites, which stand alone on their line — not the mentions of the
    // pattern in this test and its own doc comment.
    let spawns = source
        .lines()
        .filter(|line| line.trim() == "Command::new(BIN)")
        .count();
    assert_eq!(
        spawns, 1,
        "found {spawns} spawns; only `run_in` may construct one"
    );

    let helper = source.split_once("fn run_in(").expect("run_in exists").1;
    assert!(
        helper.contains("REVIEWQ_CONFIG") && helper.contains("REVIEWQ_DB"),
        "run_in must set both isolation variables"
    );
}

#[test]
fn help_and_version_succeed() {
    for args in [["--help"], ["--version"]] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            stderr(&output)
        );
    }
}

#[test]
fn every_subcommand_is_reachable() {
    for name in [
        "sync", "list", "next", "show", "done", "snooze", "mute", "unmute", "defer", "undefer",
        "track", "untrack", "review", "doctor", "help",
    ] {
        let output = run(&[name, "--help"]);
        assert!(
            output.status.success(),
            "`{name} --help` failed: {}",
            stderr(&output)
        );
    }
}

/// `done`/`snooze`/`mute`/`unmute`/`defer`/`undefer` reach nothing but the
/// ledger, so a missing PR is reported the same clear way for all of them,
/// against a hermetic, empty one. `done` additionally needs no network for this
/// case: it fails on the same existence check before ever reaching the forge.
///
/// `track` is not in the list: it fetches what the ledger doesn't have, so an
/// unknown number is the normal case rather than an error.
/// The documentation has to reach somebody whose config is broken — that is
/// when it is most wanted, and every other command refuses to run.
#[test]
fn help_is_readable_without_a_working_config() {
    let (_dir, config, db) = workspace();
    std::fs::write(&config, "this is not toml [[[").expect("write a broken config");

    let index = run_in(&config, &db, &["help"]);
    assert!(index.status.success(), "{}", stderr(&index));
    let listing = String::from_utf8_lossy(&index.stdout);
    assert!(listing.contains("reviewq help verbs"), "{listing}");

    // And a verb reaches its page without knowing which page it is on.
    let page = run_in(&config, &db, &["help", "done"]);
    assert!(page.status.success(), "{}", stderr(&page));
    let shown = String::from_utf8_lossy(&page.stdout);
    assert!(shown.contains("accounted for"), "{shown}");
    assert!(shown.contains("mute"), "{shown}");
    // Piped, so no escapes: the colour is for a terminal, not for a file.
    assert!(!shown.contains('\x1b'), "{shown:?}");
}

/// A picture in the documentation is a screenshot of a text interface, so in
/// the terminal it is drawn rather than linked.
#[test]
fn a_help_page_draws_the_interface_rather_than_naming_a_file() {
    let (_dir, config, db) = workspace();

    let output = run_in(&config, &db, &["help", "keys"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let page = String::from_utf8_lossy(&output.stdout);
    assert!(
        page.contains("on the queue"),
        "the header of a drawn screen"
    );
    assert!(page.contains('╭'), "and its panes: {page}");
    assert!(!page.contains(".svg"), "no file paths: {page}");
    assert!(!page.contains("[img]"), "and no placeholders: {page}");
}

/// A typo in a config is silent by construction — unknown keys have to load, or
/// a config written for a newer reviewq would refuse to start. So it is said out
/// loud instead, by whatever command you happened to run.
#[test]
fn a_setting_reviewq_does_not_read_is_named_on_every_command() {
    let (_dir, config, db) = workspace();
    let existing = std::fs::read_to_string(&config).expect("read");
    std::fs::write(
        &config,
        format!("{existing}\n[[project.interest]]\nlabels = [\"x\"]\nlables = [\"y\"]\n"),
    )
    .expect("write a typo");

    let output = run_in(&config, &db, &["list"]);

    let err = stderr(&output);
    assert!(err.contains("not read"), "{err}");
    assert!(err.contains("lables"), "it names the key: {err}");
    assert!(err.contains("reviewq doctor"), "and where to look: {err}");

    // And `doctor` counts it against a clean bill of health.
    let doctor = run_in(&config, &db, &["doctor"]);
    let shown = String::from_utf8_lossy(&doctor.stdout);
    assert!(shown.contains("is not a setting reviewq reads"), "{shown}");
}

/// The version says where the build sits, not just which release it followed —
/// a binary reporting the last tag while running code from past it is how a
/// missing feature looks like a broken one.
#[test]
fn the_version_describes_the_build_it_came_from() {
    let output = run(&["--version"]);

    assert!(output.status.success(), "{}", stderr(&output));
    let shown = String::from_utf8_lossy(&output.stdout);
    let version = shown
        .split_whitespace()
        .nth(1)
        .expect("a version after the name");

    assert!(
        !version.starts_with('v'),
        "the tag's `v` is the tag's: {shown:?}"
    );

    // Built from a tarball, or from a tag, the version is the release itself —
    // give or take a `+dirty` for a modified tree, which semver reads as the
    // same version and a human reads as the warning it is.
    let manifest = env!("CARGO_PKG_VERSION");
    if version.split('+').next() == Some(manifest) {
        return;
    }

    // Otherwise this build is past the release, and must say so in the one way
    // semver can order: a *higher* version, qualified. `0.2.0-10-gdeadbee`
    // would sort below 0.2.0, which is the whole thing being avoided.
    let (heading_for, qualifier) = version.split_once('-').unwrap_or_else(|| {
        panic!("a build past {manifest} says which release it is heading for: {shown:?}")
    });
    assert!(
        release(heading_for) > release(manifest),
        "{heading_for} does not sort above the release behind it: {shown:?}"
    );
    assert!(
        qualifier.starts_with("dev"),
        "and that it has not got there yet: {shown:?}"
    );
    assert!(
        qualifier.contains("+g"),
        "and which commit it is: {shown:?}"
    );
}

/// `0.2.1` as something orderable.
fn release(version: &str) -> (u64, u64, u64) {
    let parts: Vec<u64> = version
        .split('.')
        .map(|part| part.parse().expect("a numeric version part"))
        .collect();
    match parts[..] {
        [major, minor, patch] => (major, minor, patch),
        _ => panic!("a three-part version, not {version:?}"),
    }
}

#[test]
fn an_unknown_help_topic_says_what_there_is() {
    let (_dir, config, db) = workspace();

    let output = run_in(&config, &db, &["help", "nonesuch"]);

    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("no help topic"), "{err}");
    assert!(err.contains("verbs"), "it lists them: {err}");
}

#[test]
fn an_action_on_an_unknown_pr_is_a_clear_error() {
    let (_dir, config, db) = workspace();

    for args in [
        vec!["done", "999"],
        vec!["snooze", "999", "3d"],
        vec!["mute", "999"],
        vec!["unmute", "999"],
        vec!["defer", "999"],
        vec!["undefer", "999"],
        vec!["untrack", "999"],
    ] {
        let output = run_in(&config, &db, &args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr(&output).contains("not in the ledger"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

/// Config is loaded and validated before a command runs, so a broken one is
/// reported as itself rather than as whatever the command tripped over later.
/// It applies to every command, including the ones that read only the ledger.
#[test]
fn a_broken_config_stops_every_command_early() {
    let (dir, config, db) = workspace();
    std::fs::write(&config, "this is not toml = = =").expect("write config");

    for args in [
        vec!["list"],
        vec!["show", "1"],
        vec!["mute", "1"],
        vec!["sync"],
        vec!["doctor"],
    ] {
        let output = run_in(&config, &db, &args);
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        let stderr = stderr(&output);
        assert!(stderr.contains("parsing config"), "{args:?}: {stderr}");
    }
    drop(dir);
}

#[test]
fn snooze_rejects_a_bad_duration_before_touching_the_ledger() {
    // REVIEWQ_DB points under a directory that cannot exist: if the duration
    // were ever validated after opening the ledger instead of before, this
    // would fail loudly (a ledger-open error) rather than silently passing.
    let (_dir, config, _db) = workspace();
    let output = run_in(
        &config,
        Path::new("/nonexistent/reviewq/reviewq.db"),
        &["snooze", "1", "not-a-duration"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("invalid duration"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn list_rejects_contradictory_buckets() {
    let output = run(&["list", "--all", "--waiting"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("cannot be used with"));
}

#[test]
fn an_empty_queue_reports_itself_and_exits_empty() {
    // `list` with no flag is the queue. Against a fresh ledger it is empty, and
    // must say so with the dedicated exit code rather than printing nothing.
    let (_dir, config, db) = workspace();
    let output = run_in(&config, &db, &["list"]);

    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("queue is empty"));
}

#[test]
fn no_subcommand_is_a_usage_error() {
    let output = run(&[]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Usage"));
}

#[test]
fn an_unknown_subcommand_is_rejected() {
    let output = run(&["frobnicate"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unrecognized subcommand"));
}

#[test]
fn a_missing_explicit_config_is_reported_not_created() {
    let output = run(&["--config", "/nonexistent/nope.toml", "doctor"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("config not found"));
}

/// The same PR number tracked in two repos is ambiguous by number alone, but
/// not when named by its full URL — `show` prefers the URL's own repo over
/// searching, exactly so this case has an answer.
#[test]
fn show_disambiguates_a_shared_pr_number_by_url() {
    let db = std::env::temp_dir().join(format!(
        "reviewq-cli-show-disambiguates-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);
    {
        let pr = |title: &str| PrSnapshot {
            number: 42,
            title: title.to_string(),
            author: "octocat".into(),
            author_association: "CONTRIBUTOR".into(),
            head_sha: "abc123".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: reviewq_core::model::PrState::Open,
            updated_at: "2026-08-05T12:00:00Z".parse().unwrap(),
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        };
        let ledger = Ledger::open(&db).unwrap();
        let airflow = ledger
            .ensure_repo(&reviewq_ledger::RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
            .unwrap();
        let astro = ledger
            .ensure_repo(&reviewq_ledger::RepoKey {
                host: "github.com".into(),
                owner: "astronomer".into(),
                name: "astro".into(),
            })
            .unwrap();
        ledger
            .upsert_pr(
                airflow,
                &pr("Airflow #42"),
                None,
                "2026-08-05T12:00:00Z".parse().unwrap(),
            )
            .unwrap();
        ledger
            .upsert_pr(
                astro,
                &pr("Astro #42"),
                None,
                "2026-08-05T12:00:00Z".parse().unwrap(),
            )
            .unwrap();
    }

    let (_dir, config, _) = workspace();
    let bare = run_in(&config, &db, &["show", "42", "--json"]);
    assert!(!bare.status.success(), "a bare shared number is ambiguous");
    assert!(stderr(&bare).contains("more than one configured repo"));

    let by_url = run_in(
        &config,
        &db,
        &[
            "show",
            "https://github.com/astronomer/astro/pull/42",
            "--json",
        ],
    );
    let _ = std::fs::remove_file(&db);
    assert!(by_url.status.success(), "{}", stderr(&by_url));
    assert!(String::from_utf8_lossy(&by_url.stdout).contains("Astro #42"));
}
