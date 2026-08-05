//! Fixture plumbing for the classification suite.
//!
//! Each file in `tests/fixtures/` is one case classification has to get right.
//! Until classification exists these tests assert that the fixtures parse, that
//! they describe states GitHub could actually produce, and that the set covers
//! every case; snapshotting classified output per scenario then hangs off the
//! same loader.

use std::path::{Path, PathBuf};

use reviewq_core::model::{
    ClassifyCtx, Mention, MyState, PrSnapshot, ReviewRequest, ThreadState, classify,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Scenario {
    /// What this case is and what it should produce.
    description: String,
    /// The instant classification runs at, so snooze expiry is deterministic.
    now: jiff::Timestamp,
    pr: PrSnapshot,
    #[serde(default)]
    mine: MyState,
    #[serde(default)]
    threads: Vec<ThreadState>,
    /// Signals that come from config or a tier-2 fetch rather than the PR's own
    /// activity — see [`ClassifyCtx`].
    #[serde(default)]
    ctx: ScenarioCtx,
}

/// The [`ClassifyCtx`] inputs, in an owned form the fixture can deserialize.
#[derive(Debug, Default, Deserialize)]
struct ScenarioCtx {
    #[serde(default)]
    bots: Vec<String>,
    #[serde(default)]
    interest: Option<String>,
    #[serde(default)]
    mentions: Vec<Mention>,
    #[serde(default)]
    review_request: Option<ReviewRequest>,
    #[serde(default)]
    new_commits: u32,
    #[serde(default)]
    include_merged: bool,
}

impl Scenario {
    /// Classify this fixture and render each fired reason as one line, so the
    /// snapshot is a diffable statement of what the queue would show.
    fn classified(&self) -> String {
        let ctx = ClassifyCtx {
            bots: &self.ctx.bots,
            interest: self.ctx.interest.as_deref(),
            mentions: &self.ctx.mentions,
            review_request: self.ctx.review_request.clone(),
            new_commits: self.ctx.new_commits,
            include_merged: self.ctx.include_merged,
        };
        let attention = classify(&self.pr, &self.mine, &self.threads, self.now, &ctx);
        if attention.is_empty() {
            return "(nothing)".to_string();
        }
        attention
            .iter()
            .map(|a| {
                format!(
                    "[p{}] {} (since {})",
                    a.reason.priority(),
                    a.reason,
                    a.since
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_all() -> Vec<(String, Scenario)> {
    let mut loaded = Vec::new();
    for entry in std::fs::read_dir(fixture_dir()).expect("fixture dir is readable") {
        let path = entry.expect("readable dir entry").path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let name = path
            .file_stem()
            .expect("fixture has a stem")
            .to_string_lossy()
            .into_owned();
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("reading {}: {err}", path.display()));
        let scenario: Scenario = serde_json::from_str(&raw)
            .unwrap_or_else(|err| panic!("parsing {}: {err}", path.display()));
        loaded.push((name, scenario));
    }
    loaded.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!loaded.is_empty(), "no fixtures found");
    loaded
}

#[test]
fn every_fixture_parses() {
    for (name, scenario) in load_all() {
        assert!(
            !scenario.description.is_empty(),
            "{name} has no description"
        );
        assert!(scenario.pr.number > 0, "{name} has no PR number");
        assert!(
            scenario.now >= scenario.pr.updated_at,
            "{name}: `now` predates the PR's updatedAt"
        );
    }
}

/// Fixtures are hand-written, so they can encode states GitHub would never
/// produce and then "prove" behaviour that cannot happen. These are the
/// consistency rules worth enforcing on the corpus.
#[test]
fn no_fixture_describes_an_impossible_state() {
    for (name, scenario) in load_all() {
        if scenario.mine.last_verdict.is_some() {
            assert!(
                scenario.mine.last_reviewed_sha.is_some(),
                "{name}: has a verdict but no reviewed SHA"
            );
            assert!(
                scenario.mine.last_action_at.is_some(),
                "{name}: has a verdict but no action timestamp"
            );
        }

        for thread in &scenario.threads {
            assert!(!thread.thread_id.is_empty(), "{name}: thread with no id");
            assert_eq!(
                thread.is_resolved,
                thread.resolved_by.is_some(),
                "{name}: thread {} resolution and resolver disagree",
                thread.thread_id
            );
            if let (Some(mine), Some(last)) = (thread.my_last_comment_at, thread.last_comment_at) {
                assert!(
                    mine <= last,
                    "{name}: thread {} has my comment after the last comment",
                    thread.thread_id
                );
            }
        }
    }
}

/// The cases classification must handle, each a rule from the reason table or
/// one of its suppressions. A name here without a fixture is a case nobody is
/// testing, which is why the list is asserted rather than merely documented.
#[test]
fn every_required_scenario_has_a_fixture() {
    let required = [
        "bot_comment_suppressed",
        "direct_mention",
        "draft_suppressed",
        "fresh_interesting_pr",
        "merged_post_merge_reply",
        "mute_beats_mention",
        "my_thread_resolved_silently",
        "reply_in_my_thread",
        "review_requested",
        "reviewed_then_new_commits",
        "snooze_expired",
        "snoozed",
    ];

    let present: Vec<String> = load_all().into_iter().map(|(name, _)| name).collect();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|name| !present.iter().any(|p| p == name))
        .collect();

    assert!(missing.is_empty(), "missing fixtures: {missing:?}");
}

/// The heart of the suite: what does classification actually produce for each
/// case? One snapshot per fixture, named after it, so a behaviour change shows
/// up as a diff against the specific scenario it broke.
#[test]
fn classification_matches_the_snapshot() {
    for (name, scenario) in load_all() {
        insta::assert_snapshot!(name.clone(), scenario.classified(), &scenario.description);
    }
}
