//! The CLI's observable contract: exit codes and stderr wording. Scripts and
//! cron jobs branch on these, so they are tested rather than assumed.
//!
//! Nothing here touches the network. `doctor`'s successful path is exercised by
//! hand against real GitHub, since faking it would only test the fake.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use reviewq_core::model::{
    ActivityKind, ActivityPayload, ActivitySource, PrSnapshot, PrState, ReviewResult,
};
use reviewq_ledger::{ActivityScope, Ledger, NewActivityEvent, RepoId, RepoKey};

use schema::activity_retention;

mod schema {
    diesel::table! {
        activity_retention (singleton) {
            singleton -> Integer,
            cutoff -> Text,
        }
    }
}

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
    command_in(config, db, args).output().expect("binary runs")
}

fn command_in(config: &Path, db: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env("REVIEWQ_CONFIG", config)
        .env("REVIEWQ_DB", db)
        .env("NO_COLOR", "1");
    command
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
        .filter(|line| line.trim().ends_with("Command::new(BIN);"))
        .count();
    assert_eq!(
        spawns, 1,
        "found {spawns} spawns; only `command_in` may construct one"
    );

    let helper = source
        .split_once("fn command_in(")
        .expect("command_in exists")
        .1;
    assert!(
        helper.contains("REVIEWQ_CONFIG") && helper.contains("REVIEWQ_DB"),
        "command_in must set both isolation variables"
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
        "track", "untrack", "review", "doctor", "help", "history",
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
            state_changed_at: None,
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
        ledger.upsert_pr(airflow, &pr("Airflow #42"), None).unwrap();
        ledger.upsert_pr(astro, &pr("Astro #42"), None).unwrap();
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

fn pr(number: u64, title: &str) -> PrSnapshot {
    PrSnapshot {
        number,
        title: title.to_string(),
        author: "octocat".into(),
        author_association: "CONTRIBUTOR".into(),
        head_sha: "abc123456789".into(),
        base_ref: "main".into(),
        is_draft: false,
        state: PrState::Open,
        updated_at: "2026-08-20T12:00:00Z".parse().unwrap(),
        created_at: None,
        state_changed_at: None,
        labels: vec![],
        milestone: None,
        files: None,
        files_truncated: false,
    }
}

fn add_pr(ledger: &Ledger, repo: &RepoKey, number: u64, title: &str) -> RepoId {
    let repo_id = ledger.ensure_repo(repo).unwrap();
    ledger.upsert_pr(repo_id, &pr(number, title), None).unwrap();
    repo_id
}

fn activity(kind: ActivityKind, at: &str, payload: ActivityPayload) -> NewActivityEvent {
    NewActivityEvent {
        relation: reviewq_core::model::ActivityRelation::Own,
        source: if matches!(
            kind,
            ActivityKind::ReviewSubmitted
                | ActivityKind::Commented
                | ActivityKind::ReviewThreadCommented
                | ActivityKind::PrClosed
                | ActivityKind::PrReopened
                | ActivityKind::PrMerged
        ) {
            ActivitySource::Forge
        } else {
            ActivitySource::Local
        },
        kind,
        occurred_at: at.parse().unwrap(),
        recorded_at: "2026-08-20T12:30:00Z".parse().unwrap(),
        actor: None,
        head_sha: None,
        external_id: None,
        permalink: None,
        payload,
    }
}

fn history_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (dir, config, db) = workspace();
    let ledger = Ledger::open(&db).unwrap();
    let airflow = RepoKey {
        host: "github.com".into(),
        owner: "apache".into(),
        name: "airflow".into(),
    };
    let astro = RepoKey {
        host: "github.com".into(),
        owner: "astronomer".into(),
        name: "astro".into(),
    };
    let airflow_id = add_pr(&ledger, &airflow, 42, "Airflow #42");
    let astro_id = add_pr(&ledger, &astro, 42, "Astro #42");

    let mut done = activity(
        ActivityKind::Done,
        "2026-08-20T10:00:00Z",
        ActivityPayload::None,
    );
    done.head_sha = Some("abc123456789".into());
    ledger.record_activity(airflow_id, 42, &done).unwrap();

    let mut review = activity(
        ActivityKind::ReviewSubmitted,
        "2026-08-20T11:00:00Z",
        ActivityPayload::ReviewSubmitted {
            result: ReviewResult::ChangesRequested,
            reviewed_sha: Some("def456789012".into()),
        },
    );
    review.actor = Some("ashb".into());
    review.head_sha = Some("def456789012".into());
    review.external_id = Some("review-1".into());
    review.permalink = Some("https://example.test/reviews/1".into());
    ledger
        .record_forge_activity(airflow_id, 42, &review)
        .unwrap();

    let merged = activity(
        ActivityKind::PrMerged,
        "2026-08-20T12:00:00Z",
        ActivityPayload::StateChanged {
            from: PrState::Open,
            to: PrState::Merged,
        },
    );
    ledger.record_forge_activity(astro_id, 42, &merged).unwrap();

    (dir, config, db)
}

#[test]
fn history_prints_global_and_per_pr_events_newest_first() {
    let (_dir, config, db) = history_workspace();

    let global = run_in(&config, &db, &["history"]);
    assert!(global.status.success(), "{}", stderr(&global));
    assert_eq!(
        String::from_utf8_lossy(&global.stdout),
        "2026-08-20T12:00:00Z  astronomer/astro #42  merged (open → merged)\n\
2026-08-20T11:00:00Z  apache/airflow #42  you requested changes at def4567\n\
2026-08-20T10:00:00Z  apache/airflow #42  you marked done at abc1234\n"
    );

    let per_pr = run_in(
        &config,
        &db,
        &["history", "https://github.com/apache/airflow/pull/42"],
    );
    assert!(per_pr.status.success(), "{}", stderr(&per_pr));
    assert_eq!(
        String::from_utf8_lossy(&per_pr.stdout),
        "2026-08-20T11:00:00Z  #42  you requested changes at def4567\n\
2026-08-20T10:00:00Z  #42  you marked done at abc1234\n"
    );
}

#[test]
fn history_explains_when_no_activity_has_been_recorded() {
    let (_dir, config, db) = workspace();

    let human = run_in(&config, &db, &["history"]);
    assert!(human.status.success(), "{}", stderr(&human));
    assert_eq!(
        String::from_utf8_lossy(&human.stdout),
        "No activity history.\n"
    );

    let json = run_in(&config, &db, &["history", "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    assert_eq!(String::from_utf8_lossy(&json.stdout), "[]\n");
}

#[test]
fn history_json_is_typed_and_does_not_expose_the_stored_payload_string() {
    let (_dir, config, db) = history_workspace();

    let output = run_in(
        &config,
        &db,
        &[
            "history",
            "https://github.com/apache/airflow/pull/42",
            "--json",
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        r#"[
  {
    "repository": {
      "host": "github.com",
      "owner": "apache",
      "name": "airflow"
    },
    "pr_number": 42,
    "pr_title": "Airflow #42",
    "source": "forge",
    "kind": "review_submitted",
    "relation": "own",
    "occurred_at": "2026-08-20T11:00:00Z",
    "recorded_at": "2026-08-20T12:30:00Z",
    "actor": "ashb",
    "head_sha": "def456789012",
    "external_id": "review-1",
    "permalink": "https://example.test/reviews/1",
    "payload": {
      "review_submitted": {
        "result": "changes_requested",
        "reviewed_sha": "def456789012"
      }
    }
  },
  {
    "repository": {
      "host": "github.com",
      "owner": "apache",
      "name": "airflow"
    },
    "pr_number": 42,
    "pr_title": "Airflow #42",
    "source": "local",
    "kind": "done",
    "relation": "own",
    "occurred_at": "2026-08-20T10:00:00Z",
    "recorded_at": "2026-08-20T12:30:00Z",
    "actor": null,
    "head_sha": "abc123456789",
    "external_id": null,
    "permalink": null,
    "payload": "none"
  }
]
"#
    );
}

#[test]
fn history_reads_every_keyset_page() {
    let (_dir, config, db) = workspace();
    let ledger = Ledger::open(&db).unwrap();
    let repo_id = add_pr(
        &ledger,
        &RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        },
        7,
        "Many events",
    );
    for second in 0..101 {
        ledger
            .record_activity(
                repo_id,
                7,
                &activity(
                    ActivityKind::Done,
                    &format!("2026-08-20T10:{:02}:{:02}Z", second / 60, second % 60),
                    ActivityPayload::None,
                ),
            )
            .unwrap();
    }

    let human = run_in(&config, &db, &["history"]);
    assert!(human.status.success(), "{}", stderr(&human));
    let human = String::from_utf8_lossy(&human.stdout);
    assert_eq!(human.lines().count(), 101);
    assert!(
        human
            .lines()
            .next()
            .unwrap()
            .starts_with("2026-08-20T10:01:40Z")
    );
    assert!(
        human
            .lines()
            .last()
            .unwrap()
            .starts_with("2026-08-20T10:00:00Z")
    );

    let json = run_in(&config, &db, &["history", "--json"]);
    assert!(json.status.success(), "{}", stderr(&json));
    let events: Vec<serde_json::Value> = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(events.len(), 101);
    assert_eq!(events[0]["occurred_at"], "2026-08-20T10:01:40Z");
    assert_eq!(events[100]["occurred_at"], "2026-08-20T10:00:00Z");
}

#[test]
fn history_requires_a_url_when_a_number_exists_in_multiple_repos() {
    let (_dir, config, db) = history_workspace();

    let output = run_in(&config, &db, &["history", "42"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("pass its full URL to pick one"));
}

#[test]
fn history_backfill_is_not_a_command() {
    let (_dir, config, db) = workspace();

    let output = run_in(&config, &db, &["history", "backfill"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("is not a PR number"));
    assert_eq!(
        Ledger::open(&db)
            .unwrap()
            .repo_id(&RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
            .unwrap(),
        None
    );
}

fn cleanup_workspace() -> (tempfile::TempDir, PathBuf, PathBuf, RepoId) {
    let (dir, config, db) = workspace();
    let ledger = Ledger::open(&db).unwrap();
    let repo_id = add_pr(
        &ledger,
        &RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        },
        1,
        "Cleanup",
    );
    ledger
        .record_activity(
            repo_id,
            1,
            &activity(
                ActivityKind::Done,
                "2024-01-01T00:00:00Z",
                ActivityPayload::None,
            ),
        )
        .unwrap();
    let recent = jiff::Timestamp::now().to_string();
    ledger
        .record_activity(
            repo_id,
            1,
            &activity(ActivityKind::Done, &recent, ActivityPayload::None),
        )
        .unwrap();
    (dir, config, db, repo_id)
}

#[test]
fn history_cleanup_dry_run_reports_without_deleting() {
    let (_dir, config, db, repo_id) = cleanup_workspace();

    let output = run_in(
        &config,
        &db,
        &["history", "clean", "--older-than", "52w", "--dry-run"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Would remove 1 event across 1 pull request.\n"
    );
    assert_eq!(
        Ledger::open(&db)
            .unwrap()
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10,)
            .unwrap()
            .events
            .len(),
        2
    );
}

#[test]
fn history_cleanup_refuses_noninteractive_deletion_without_yes() {
    let (_dir, config, db, repo_id) = cleanup_workspace();

    let output = run_in(&config, &db, &["history", "clean", "--older-than", "52w"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("requires --yes when input is not interactive"));
    assert_eq!(
        Ledger::open(&db)
            .unwrap()
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10,)
            .unwrap()
            .events
            .len(),
        2
    );
}

#[test]
fn history_cleanup_rejects_an_invalid_duration_before_opening_the_ledger() {
    let (_dir, config, _db) = workspace();

    let output = run_in(
        &config,
        Path::new("/nonexistent/reviewq/reviewq.db"),
        &[
            "history",
            "clean",
            "--older-than",
            "not-a-duration",
            "--dry-run",
        ],
    );

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("invalid duration"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn history_cleanup_with_yes_deletes_only_events_before_the_cutoff() {
    let (_dir, config, db, repo_id) = cleanup_workspace();

    let output = run_in(
        &config,
        &db,
        &["history", "clean", "--older-than", "52w", "--yes"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Would remove 1 event across 1 pull request.\nRemoved 1 event across 1 pull request.\n"
    );
    let remaining = Ledger::open(&db)
        .unwrap()
        .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
        .unwrap()
        .events;
    assert_eq!(remaining.len(), 1);
}

#[test]
fn confirmed_empty_cleanup_after_upgrading_from_main_filters_later_ingestion() {
    use diesel::{
        Connection as _, QueryDsl as _, RunQueryDsl as _, connection::SimpleConnection as _,
    };

    let (_dir, config, db) = workspace();
    let mut conn = diesel::SqliteConnection::establish(db.to_str().unwrap()).unwrap();
    let migrations_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../reviewq-ledger/migrations");
    let mut migrations = std::fs::read_dir(migrations_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    migrations.sort();
    for migration in migrations.into_iter().take(11) {
        conn.batch_execute(&std::fs::read_to_string(migration.join("up.sql")).unwrap())
            .unwrap();
    }
    conn.batch_execute(
        "INSERT INTO repos (id, host, owner, name) VALUES (1, 'github.com', 'apache', 'airflow');
         INSERT INTO prs (repo_id, number, title, author, author_association, head_sha,
             is_draft, state, updated_at, labels, first_seen_at)
         VALUES (1, 1, 'Cleanup', 'ashb', 'MEMBER', 'abc123', 0, 'OPEN',
             '2026-08-11T09:00:00Z', '[]', '2026-08-11T09:00:00Z');
         PRAGMA user_version = 11;",
    )
    .unwrap();
    drop(conn);

    let output = run_in(
        &config,
        &db,
        &["history", "clean", "--older-than", "52w", "--yes"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Would remove 0 events across 0 pull requests.\nRemoved 0 events across 0 pull requests.\n"
    );
    let mut conn = diesel::SqliteConnection::establish(db.to_str().unwrap()).unwrap();
    let cutoff = activity_retention::table
        .select(activity_retention::cutoff)
        .first::<String>(&mut conn)
        .unwrap();
    drop(conn);
    let ledger = Ledger::open(&db).unwrap();
    let repo_id = ledger
        .repo_id(&RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        })
        .unwrap()
        .unwrap();
    ledger
        .record_activity(
            repo_id,
            1,
            &activity(
                ActivityKind::Done,
                "2020-01-01T00:00:00Z",
                ActivityPayload::None,
            ),
        )
        .unwrap();
    ledger
        .record_activity(
            repo_id,
            1,
            &activity(ActivityKind::Muted, &cutoff, ActivityPayload::None),
        )
        .unwrap();
    let mut old_backfill = activity(
        ActivityKind::Commented,
        "2020-01-01T00:00:00Z",
        ActivityPayload::None,
    );
    old_backfill.external_id = Some("old-comment".into());
    let mut new_backfill = activity(
        ActivityKind::Commented,
        "2030-01-01T00:00:00Z",
        ActivityPayload::None,
    );
    new_backfill.external_id = Some("new-comment".into());
    assert_eq!(
        ledger
            .commit_activity_page(
                repo_id,
                1,
                None,
                &[old_backfill, new_backfill],
                None,
                None,
                "2030-01-01T00:01:00Z".parse().unwrap(),
            )
            .unwrap(),
        reviewq_ledger::ActivityPageCommit::Applied { inserted: 1 }
    );

    let events = ledger
        .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
        .unwrap()
        .events;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].occurred_at,
        "2030-01-01T00:00:00Z".parse().unwrap()
    );
    assert_eq!(events[1].occurred_at, cutoff.parse().unwrap());
}

fn review_workspace(review_command: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (dir, config, db) = workspace();
    let mut contents = std::fs::read_to_string(&config).unwrap();
    let command = review_command
        .iter()
        .map(|arg| toml::Value::String((*arg).to_string()).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    contents.push_str(&format!("\n[handoff]\nreview_command = [{command}]\n"));
    std::fs::write(&config, contents).unwrap();
    let ledger = Ledger::open(&db).unwrap();
    add_pr(
        &ledger,
        &RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        },
        7,
        "Review me",
    );
    (dir, config, db)
}

fn activity_kinds(db: &Path) -> Vec<ActivityKind> {
    Ledger::open(db)
        .unwrap()
        .activity_page(ActivityScope::All, None, 10)
        .unwrap()
        .events
        .into_iter()
        .map(|event| event.kind)
        .collect()
}

#[test]
fn review_started_is_recorded_before_the_handoff_child_exits() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let script = "printf started > \"$1\"; read _";
    let (_workspace, config, db) = review_workspace(&[
        "sh",
        "-c",
        script,
        "reviewq-test",
        &started.display().to_string(),
    ]);
    let mut child = command_in(&config, &db, &["review", "7"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    for _ in 0..200 {
        if started.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(started.exists(), "handoff child started");
    for _ in 0..200 {
        if activity_kinds(&db) == [ActivityKind::ReviewStarted] {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let still_running = child.try_wait().unwrap().is_none();
    let kinds_before_exit = activity_kinds(&db);
    use std::io::Write as _;
    child.stdin.as_mut().unwrap().write_all(b"go\n").unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    assert!(still_running, "reviewq is still waiting for the handoff");
    assert_eq!(kinds_before_exit, [ActivityKind::ReviewStarted]);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn review_started_is_not_recorded_when_the_handoff_cannot_spawn() {
    let (_dir, config, db) = review_workspace(&["/definitely/not/a/review-command"]);

    let output = run_in(&config, &db, &["review", "7"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("/definitely/not/a/review-command"));
    assert!(activity_kinds(&db).is_empty());
}

#[test]
fn review_started_is_retained_when_the_handoff_exits_nonzero() {
    let (_dir, config, db) = review_workspace(&["sh", "-c", "exit 23"]);

    let output = run_in(&config, &db, &["review", "7"]);

    assert_eq!(output.status.code(), Some(23), "{}", stderr(&output));
    assert_eq!(activity_kinds(&db), [ActivityKind::ReviewStarted]);
}

#[test]
fn per_pr_all_history_reads_context_across_pages_with_actor_and_relation() {
    let (_dir, config, db) = workspace();
    let ledger = Ledger::open(&db).unwrap();
    let repo_id = add_pr(
        &ledger,
        &RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        },
        7,
        "Context",
    );
    for second in 0..101 {
        let mut event = activity(
            ActivityKind::Commented,
            &format!("2026-08-20T10:{:02}:{:02}Z", second / 60, second % 60),
            ActivityPayload::None,
        );
        event.relation = reviewq_core::model::ActivityRelation::Context;
        event.actor = Some("other".into());
        event.external_id = Some(format!("context-{second}"));
        ledger.record_activity(repo_id, 7, &event).unwrap();
    }
    for args in [vec!["history", "--json"], vec!["history", "7", "--json"]] {
        let output = run_in(&config, &db, &args);
        assert!(output.status.success(), "{}", stderr(&output));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "[]\n");
    }
    let output = run_in(&config, &db, &["history", "7", "--all", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let events: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(events.len(), 101);
    assert!(events.iter().all(|event| event["relation"] == "context"));
    let output = run_in(&config, &db, &["history", "7", "--all"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .all(|line| line.ends_with("other commented"))
    );
}
