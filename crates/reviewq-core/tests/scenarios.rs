//! Fixture plumbing for the classification suite.
//!
//! Each file in `tests/fixtures/` is one case classification has to get right,
//! and the suite is one snapshot of classified output per fixture — so a
//! behaviour change shows up as a diff against the scenario it broke.
//!
//! The loader enforces the corpus's own consistency as it reads: a hand-written
//! fixture can encode a state GitHub would never produce and then "prove"
//! behaviour that cannot happen. Those checks live in [`load_all`] rather than in
//! tests of their own, because they say nothing about `classify` — they are the
//! price of admission for a fixture.

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
    /// The discriminants classification produces for this scenario.
    fn reasons(&self) -> Vec<&'static str> {
        classify(&self.pr, &self.mine, &self.threads, self.now, &self.ctx())
            .iter()
            .map(|a| a.reason.discriminant())
            .collect()
    }

    fn ctx(&self) -> ClassifyCtx<'_> {
        ClassifyCtx {
            bots: &self.ctx.bots,
            interest: self.ctx.interest.as_deref(),
            mentions: &self.ctx.mentions,
            review_request: self.ctx.review_request.clone(),
            new_commits: self.ctx.new_commits,
            include_merged: self.ctx.include_merged,
        }
    }

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
    for (name, scenario) in &loaded {
        check_is_a_state_github_could_produce(name, scenario);
    }
    loaded
}

/// Refuse a fixture that describes something the forge could not report, since a
/// snapshot of it would pin behaviour for a state that never arrives.
fn check_is_a_state_github_could_produce(name: &str, scenario: &Scenario) {
    assert!(
        !scenario.description.is_empty(),
        "{name} has no description"
    );
    assert!(scenario.pr.number > 0, "{name} has no PR number");
    assert!(
        scenario.now >= scenario.pr.updated_at,
        "{name}: `now` predates the PR's updatedAt"
    );
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

/// Every reason classification can produce is produced by some fixture.
///
/// Asserted against what `classify` actually returns, rather than against a list
/// of file names: a new variant in [`AttentionReason`] with no scenario exercising
/// it fails here, where a directory listing could only notice a deleted file.
#[test]
fn every_attention_reason_is_covered_by_a_fixture() {
    let produced: std::collections::BTreeSet<&'static str> = load_all()
        .iter()
        .flat_map(|(_, scenario)| scenario.reasons())
        .collect();

    let expected = [
        "mention",
        "needs_first_look",
        "re_review",
        "resolved_unanswered",
        "review_requested",
        "thread_reply",
    ];
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|reason| !produced.contains(reason))
        .collect();

    assert!(
        missing.is_empty(),
        "no fixture produces these reasons: {missing:?}"
    );
}

/// Every suppression is exercised too: a fixture that classifies to nothing.
///
/// Snoozed and draft PRs have to come back empty, and a suppression that
/// silently stopped working would otherwise only show as a snapshot diff nobody
/// reads as a suppression.
///
/// A *mute* is deliberately not in this list: it is the queue's business rather
/// than the state machine's, and `muted_still_classifies` is the fixture that
/// holds it to that.
#[test]
fn suppressions_are_exercised_by_fixtures_that_classify_to_nothing() {
    let silent: Vec<String> = load_all()
        .into_iter()
        .filter(|(_, scenario)| scenario.reasons().is_empty())
        .map(|(name, _)| name)
        .collect();

    for expected in ["bot_comment_suppressed", "draft_suppressed", "snoozed"] {
        assert!(
            silent.iter().any(|name| name == expected),
            "{expected} should classify to nothing, silent were {silent:?}"
        );
    }
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
