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
    /// Draft PRs are suppressed except for mentions.
    pub is_draft: bool,
    /// Open, merged or closed.
    pub state: PrState,
    /// GitHub's `updatedAt`; drives whether a detail fetch is needed.
    pub updated_at: Timestamp,
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
