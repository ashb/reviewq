//! The data types forge operations return.
//!
//! These are the [`Forge`](crate::Forge) trait's vocabulary. They lean on
//! GitHub's shape today (the GraphQL point budget in [`RateLimit`], notably); a
//! second provider is where they would earn a more neutral form.

use jiff::Timestamp;
use reviewq_core::model::PrSnapshot;
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
