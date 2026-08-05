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
    for name in ["sync", "list", "next", "show", "doctor"] {
        let output = run(&[name, "--help"]);
        assert!(
            output.status.success(),
            "`{name} --help` failed: {}",
            stderr(&output)
        );
    }
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
    let db = std::env::temp_dir().join("reviewq-cli-empty-queue.db");
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
