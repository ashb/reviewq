//! The data types forge operations return.
//!
//! These are the [`Forge`](crate::Forge) trait's vocabulary. They lean on
//! GitHub's shape today (the GraphQL point budget in [`RateLimit`], notably); a
//! second provider is where they would earn a more neutral form.

use jiff::Timestamp;
use reviewq_core::model::{
    Mention, PrSnapshot, PrState, ReviewRequest, ReviewerVerdict, ThreadState, Verdict,
};
use serde::Deserialize;

/// GitHub search returns at most this many results however many match, so a
/// window reporting more than this was silently truncated.
pub const SEARCH_CAP: u32 = 1000;

/// GraphQL point budget. Every query asks for this, so cost is always
/// observable rather than inferred.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    /// Total points available per reset window.
    pub limit: u32,
    /// Points the most recent query cost.
    pub cost: u32,
    /// Points remaining in the current window.
    pub remaining: u32,
    /// When the window resets and `remaining` returns to `limit`.
    #[serde(rename = "resetAt")]
    pub reset_at: Timestamp,
}

impl RateLimit {
    /// Log a query's cost so a runaway sync is visible in `-v` output.
    pub fn trace(&self, query: &str) {
        tracing::debug!(
            query,
            cost = self.cost,
            remaining = self.remaining,
            limit = self.limit,
            reset_at = %self.reset_at,
            "graphql rate limit"
        );
    }
}

/// The authenticated account, with the budget reported alongside it.
#[derive(Debug, Clone)]
pub struct Viewer {
    /// The account's login.
    pub login: String,
    /// The GraphQL budget as of this call.
    pub rate_limit: RateLimit,
}

/// One page of a tier-1 sweep. Each PR already carries its changed-file list,
/// fetched in the same query. The caller drives pagination — persisting each
/// page as it arrives — so an interrupted sweep resumes rather than restarts.
#[derive(Debug, Clone)]
pub struct SweepPage {
    /// PRs on this page, in the query's order.
    pub prs: Vec<PrSnapshot>,
    /// Opaque cursor for the next page, or `None` if this was the last.
    pub next: Option<String>,
    /// How many PRs match the query in total (`issueCount`); may exceed the
    /// number reachable if the window blew past [`SEARCH_CAP`].
    pub total_count: u32,
    /// GraphQL points this page cost.
    pub cost: u32,
    /// Points remaining after it.
    pub remaining: u32,
}

/// One PR as a direct fetch returns it: the snapshot, and the colours its repo
/// paints the labels it carries.
#[derive(Debug, Clone)]
pub struct FetchedPr {
    /// The PR itself.
    pub pr: PrSnapshot,
    /// The colours for its labels — see [`SweepPage::labels`].
    pub labels: Vec<LabelColour>,
}

/// A label as the forge paints it, in one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelColour {
    /// The label's name, as a PR carries it.
    pub name: String,
    /// Its colour, six hex digits and no `#` — the form GitHub reports.
    pub color: String,
}

/// The tier-2 detail for one PR: everything the [`classify`] state machine needs
/// beyond the cheap sweep, derived from the authenticated viewer's point of
/// view. The adapter resolves "me" while shaping this, so nothing above it needs
/// the login to interpret the result.
///
/// [`classify`]: reviewq_core::model::classify
#[derive(Debug, Clone)]
pub struct PrDetail {
    /// PR number.
    pub number: u64,
    /// Whether it is still open, and if not how it ended.
    ///
    /// The sweep learns this too, but a refresh of one PR never runs the sweep
    /// — so without it here, closing a PR on the forge left the ledger calling
    /// it open until a full sync came round.
    pub state: PrState,
    /// Head SHA at fetch time; lets the caller detect a head that moved between
    /// the sweep and this fetch.
    pub head_sha: String,
    /// The PR's description, as raw markdown. Empty when there isn't one.
    ///
    /// Fetched here rather than in the sweep because nothing classifies on it —
    /// it exists to be shown, and only a tracked PR is ever shown.
    pub body: String,
    /// Head SHA as of my most recent review, if I have reviewed.
    pub last_reviewed_sha: Option<String>,
    /// The verdict of that review.
    pub last_verdict: Option<Verdict>,
    /// The most recent thing I did on the PR — a review or any comment.
    pub last_action_at: Option<Timestamp>,
    /// The PR's review threads, from my point of view (`i_own`, my last
    /// comment, ...).
    pub threads: Vec<ThreadState>,
    /// Every reviewer's most recent submitted verdict, not just mine.
    pub reviewers: Vec<ReviewerVerdict>,
    /// @mentions of me, from others, across comments and reviews.
    pub mentions: Vec<Mention>,
    /// Commits pushed since my last review; zero if I have not reviewed.
    pub new_commits: u32,
    /// A live review request naming me directly, if any.
    pub review_request: Option<ReviewRequest>,
    /// GraphQL points this fetch cost.
    pub cost: u32,
    /// Points remaining after it.
    pub remaining: u32,
}
