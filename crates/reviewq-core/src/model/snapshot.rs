use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Everything known about a PR from the cheap sweep plus, if fetched, its file
/// list. Mirrors the `prs` ledger table; also the fixture format used by the
/// classification tests.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrSnapshot {
    /// PR number.
    pub number: u64,
    /// PR title, as displayed in the queue.
    pub title: String,
    /// Author's login.
    pub author: String,
    /// GitHub `authorAssociation`, e.g. `FIRST_TIME_CONTRIBUTOR`.
    pub author_association: String,
    /// Current head commit.
    pub head_sha: String,
    /// The branch the PR would merge into — GitHub's `baseRefName`.
    ///
    /// Nothing classifies on it: it is worth knowing which branch a change is
    /// aimed at, because a fix landing on a maintenance branch and the same fix
    /// aimed at `main` are different reviews.
    ///
    /// Defaulted rather than required so the fixture format and any ledger row
    /// written before this was captured still read — an unknown target branch is
    /// an empty string, which display treats as "don't say".
    #[serde(default)]
    pub base_ref: String,
    /// Draft PRs are suppressed except for mentions.
    pub is_draft: bool,
    /// Open, merged or closed.
    pub state: PrState,
    /// When the forge last changed the PR's lifecycle state, if known.
    #[serde(default)]
    pub state_changed_at: Option<Timestamp>,
    /// GitHub's `updatedAt`; drives whether a detail fetch is needed.
    pub updated_at: Timestamp,
    /// GitHub's `createdAt` — when the PR was opened on the forge.
    ///
    /// Distinct from the ledger's `first_seen_at`, which is when *this* ledger
    /// first swept it: a PR opened last year and swept this morning has both,
    /// and only one of them says how long its author has been waiting.
    ///
    /// `None` on a row written before this was captured, until the next sweep
    /// rewrites it — the same treatment `base_ref` gets, and for the same
    /// reason: an unknown date is not a date, and display says nothing rather
    /// than inventing one.
    #[serde(default)]
    pub created_at: Option<Timestamp>,
    /// Label names.
    #[serde(default)]
    pub labels: Vec<String>,
    /// Milestone title, if any.
    #[serde(default)]
    pub milestone: Option<String>,
    /// Changed paths. `None` means never fetched, which is distinct from an
    /// empty list.
    #[serde(default)]
    pub files: Option<Vec<String>>,
    /// Set when GitHub returned fewer files than the PR actually has. A
    /// truncated list that matched no path rule is *unknown*, not *no match*.
    #[serde(default)]
    pub files_truncated: bool,
}

/// Lifecycle state. Merged and closed PRs are archived rather than queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    /// Still open.
    Open,
    /// Merged.
    Merged,
    /// Closed without merging.
    Closed,
}

/// Where an activity event originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    /// An action reviewq performed locally.
    Local,
    /// An action observed from the forge.
    Forge,
}

/// How an event relates to the configured user, captured when it is ingested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityRelation {
    /// An action performed by the user.
    Own,
    /// An action or observation that directly concerns the user's attention.
    Relevant,
    /// Other activity retained for the full history of this PR.
    #[default]
    Context,
}

/// A meaningful action or lifecycle event concerning a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    /// The reasons requiring the user's attention changed during an observation.
    AttentionChanged,
    /// `reviewq done` succeeded.
    Done,
    /// A snooze was set.
    Snoozed,
    /// A mute was set.
    Muted,
    /// A mute was cleared.
    Unmuted,
    /// A defer was set.
    Deferred,
    /// A defer was cleared.
    Undeferred,
    /// The PR was tracked.
    Tracked,
    /// The PR was untracked.
    Untracked,
    /// A review handoff command started.
    ReviewStarted,
    /// The user resolved for this forge submitted a review.
    ReviewSubmitted,
    /// The user resolved for this forge posted a comment.
    Commented,
    /// The user resolved for this forge posted in a review thread.
    ReviewThreadCommented,
    /// The pull request closed without merging.
    PrClosed,
    /// The pull request reopened.
    PrReopened,
    /// The pull request merged.
    PrMerged,
    /// A review thread was observed to become resolved.
    ThreadResolved,
    /// A review thread was observed to become unresolved again.
    ThreadReopened,
}

/// The result a forge reports for a submitted review.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewResult {
    /// The review approved the change.
    Approved,
    /// The review requested changes.
    ChangesRequested,
    /// The review only commented.
    Commented,
    /// A provider-specific review result reviewq does not interpret.
    Other(String),
}

/// Kind-specific activity details.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPayload {
    /// Attention evidence preserved at the time of an observed change.
    AttentionChanged {
        /// Reasons before the change.
        before: Vec<super::Attention>,
        /// Reasons after the change.
        after: Vec<super::Attention>,
    },
    /// The event has no additional details.
    None,
    /// A snooze's expiry.
    Snoozed {
        /// When the snooze expires.
        until: Timestamp,
    },
    /// A submitted review's result and the head it covered.
    ReviewSubmitted {
        /// The provider-neutral review result.
        result: ReviewResult,
        /// The reviewed commit, when the forge supplied one.
        reviewed_sha: Option<String>,
    },
    /// A comment added to a review thread.
    ReviewThreadCommented {
        /// The provider's opaque thread identifier, when supplied.
        thread_id: Option<String>,
    },
    /// A thread transition, with observation time when the provider has no event time.
    ThreadStateChanged {
        /// Provider identity of the thread.
        thread_id: String,
        /// Whether the thread is resolved.
        resolved: bool,
        /// The timestamp is an observation, not a provider event time.
        observed: bool,
    },
    /// A pull request lifecycle transition.
    StateChanged {
        /// The previous state.
        from: PrState,
        /// The current state.
        to: PrState,
    },
}

impl PrState {
    /// GitHub's own spelling, and the value stored in the ledger's `state`
    /// column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "OPEN",
            Self::Merged => "MERGED",
            Self::Closed => "CLOSED",
        }
    }

    /// Parse the wire/ledger spelling back to a state.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "OPEN" => Some(Self::Open),
            "MERGED" => Some(Self::Merged),
            "CLOSED" => Some(Self::Closed),
            _ => None,
        }
    }

    /// Whether the PR is still open. Merged/closed PRs are archived out of the
    /// queue.
    pub fn is_open(&self) -> bool {
        matches!(self, Self::Open)
    }
}

/// My own history on a PR. Mirrors the `my_state` ledger table.
///
/// This is the state GitHub does not track for me, and the reason reviewq
/// exists: chiefly [`last_reviewed_sha`](Self::last_reviewed_sha), which is
/// what makes "has this changed since I looked?" answerable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct MyState {
    /// Head SHA as of my last review.
    pub last_reviewed_sha: Option<String>,
    /// Verdict of that review.
    pub last_verdict: Option<Verdict>,
    /// My most recent comment or review on the PR.
    pub last_action_at: Option<Timestamp>,
    /// Head SHA at my last `reviewq done`.
    pub done_sha: Option<String>,
    /// Suppress everything until this instant.
    pub snoozed_until: Option<Timestamp>,
    /// Suppress everything forever, mentions included.
    pub muted: bool,
    /// `reviewq defer` was called and nothing has happened on the PR since:
    /// push it to the bottom of the queue without hiding it. Purely a queue-
    /// ordering hint — `classify` never reads it.
    pub deferred_at: Option<Timestamp>,
    /// When `reviewq done` last ran. Distinct from
    /// [`last_action_at`](Self::last_action_at), which GitHub itself derives
    /// (a comment or review) and which a sync overwrites wholesale on every
    /// run: `done_at` is the only record of a purely local acknowledgement, so
    /// nothing else may ever assign it, or the next sync silently undoes the
    /// `done`.
    pub done_at: Option<Timestamp>,
}

impl MyState {
    /// Whether the stored defer still applies to the highest-priority attention.
    /// With no attention, nothing has invalidated the defer yet.
    pub fn is_deferred(&self, attention_since: Option<Timestamp>) -> bool {
        self.deferred_at
            .is_some_and(|at| attention_since.is_none_or(|since| since <= at))
    }
}

/// My last review verdict on a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    /// Approved.
    Approved,
    /// Changes requested.
    ChangesRequested,
    /// Commented without a verdict.
    Commented,
}

impl Verdict {
    /// GitHub's own spelling, and the value stored in the ledger's
    /// `last_verdict` column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Approved => "APPROVED",
            Self::ChangesRequested => "CHANGES_REQUESTED",
            Self::Commented => "COMMENTED",
        }
    }

    /// Parse the wire/ledger spelling back to a verdict. GitHub also emits
    /// `DISMISSED` and `PENDING`, which are not verdicts we record.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "APPROVED" => Some(Self::Approved),
            "CHANGES_REQUESTED" => Some(Self::ChangesRequested),
            "COMMENTED" => Some(Self::Commented),
            _ => None,
        }
    }
}

/// One reviewer's most recent submitted verdict on a PR — everyone who has
/// reviewed, not just me. Mirrors the `reviewers` ledger table. Purely
/// informational: `classify` never reads this, only [`MyState`]'s own review
/// fields.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewerVerdict {
    /// The reviewer's login.
    pub login: String,
    /// Their most recent submitted verdict.
    pub verdict: Verdict,
    /// When they submitted it.
    pub at: Timestamp,
}

/// One review thread. Mirrors the `threads` ledger table.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ThreadState {
    /// GraphQL node id.
    pub thread_id: String,
    /// Whether the thread is mine: I started it, or I was the last non-author
    /// voice in it. Deliberately crude in v1; see `classify`'s doc comment.
    pub i_own: bool,
    /// Whether GitHub considers the thread resolved.
    pub is_resolved: bool,
    /// Who resolved it, if resolved.
    #[serde(default)]
    pub resolved_by: Option<String>,
    /// Author of the most recent comment.
    #[serde(default)]
    pub last_comment_author: Option<String>,
    /// Timestamp of the most recent comment.
    #[serde(default)]
    pub last_comment_at: Option<Timestamp>,
    /// Timestamp of my most recent comment in this thread.
    #[serde(default)]
    pub my_last_comment_at: Option<Timestamp>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_results_preserve_known_and_provider_specific_values() {
        for result in [
            ReviewResult::Approved,
            ReviewResult::ChangesRequested,
            ReviewResult::Commented,
            ReviewResult::Other("needs-security-signoff".into()),
        ] {
            let payload = ActivityPayload::ReviewSubmitted {
                result: result.clone(),
                reviewed_sha: Some("abc123".into()),
            };
            let encoded = serde_json::to_string(&payload).unwrap();
            let decoded: ActivityPayload = serde_json::from_str(&encoded).unwrap();

            assert_eq!(
                decoded,
                ActivityPayload::ReviewSubmitted {
                    result,
                    reviewed_sha: Some("abc123".into()),
                }
            );
        }
    }
}
