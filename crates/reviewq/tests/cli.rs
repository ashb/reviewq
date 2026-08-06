//! The CLI's observable contract: exit codes and stderr wording. Scripts and
//! cron jobs branch on these, so they are tested rather than assumed.
//!
//! Nothing here touches the network. `doctor`'s successful path is exercised by
//! hand against real GitHub, since faking it would only test the fake.

use std::process::{Command, Output};

/// Cargo builds the binary before running integration tests and hands us its
/// path, so no dependency on `assert_cmd` is needed.
const BIN: &str = env!("CARGO_BIN_EXE_reviewq");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        // Point at a path that cannot exist, so a stray config load fails loudly
        // rather than reading (or creating) the real user config.
        .env("REVIEWQ_CONFIG", "/nonexistent/reviewq/config.toml")
        .env("NO_COLOR", "1")
        .output()
        .expect("binary runs")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
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
        "track", "review", "doctor",
    ] {
        let output = run(&[name, "--help"]);
        assert!(
            output.status.success(),
            "`{name} --help` failed: {}",
            stderr(&output)
        );
    }
}

/// `done`/`snooze`/`mute`/`unmute`/`defer`/`undefer`/`track` are ledger-only —
/// no config needed — so a missing PR is reported the same clear way for all
/// of them, against a hermetic, empty ledger. `done` additionally needs no
/// network for this case: it fails on the same existence check before ever
/// touching config.
#[test]
fn an_action_on_an_unknown_pr_is_a_clear_error() {
    // Unique per test run, not just per file name, so concurrent `cargo test`
    // invocations (or a leftover file from a killed run) can't collide.
    let db = std::env::temp_dir().join(format!(
        "reviewq-cli-action-unknown-pr-{}.db",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&db);

    for args in [
        vec!["done", "999"],
        vec!["snooze", "999", "3d"],
        vec!["mute", "999"],
        vec!["unmute", "999"],
        vec!["defer", "999"],
        vec!["undefer", "999"],
        vec!["track", "999"],
    ] {
        let output = Command::new(BIN)
            .args(&args)
            .env("REVIEWQ_CONFIG", "/nonexistent/reviewq/config.toml")
            .env("REVIEWQ_DB", &db)
            .env("NO_COLOR", "1")
            .output()
            .expect("binary runs");
        assert!(!output.status.success(), "{args:?} unexpectedly succeeded");
        assert!(
            stderr(&output).contains("not in the ledger"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    let _ = std::fs::remove_file(&db);
}

#[test]
fn snooze_rejects_a_bad_duration_before_touching_the_ledger() {
    // REVIEWQ_DB points under a directory that cannot exist: if the duration
    // were ever validated after opening the ledger instead of before, this
    // would fail loudly (a ledger-open error) rather than silently passing.
    let output = Command::new(BIN)
        .args(["snooze", "1", "not-a-duration"])
        .env("REVIEWQ_CONFIG", "/nonexistent/reviewq/config.toml")
        .env("REVIEWQ_DB", "/nonexistent/reviewq/reviewq.db")
        .env("NO_COLOR", "1")
        .output()
        .expect("binary runs");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("invalid duration"));
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
    let db =
        std::env::temp_dir().join(format!("reviewq-cli-empty-queue-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db);
    let output = Command::new(BIN)
        .args(["list"])
        .env("REVIEWQ_CONFIG", "/nonexistent/reviewq/config.toml")
        .env("REVIEWQ_DB", &db)
        .env("NO_COLOR", "1")
        .output()
        .expect("binary runs");
    let _ = std::fs::remove_file(&db);

    assert_eq!(output.status.code(), Some(2));
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
