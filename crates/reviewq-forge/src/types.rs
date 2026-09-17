//! The data types forge operations return.
//!
//! These are the [`Forge`](crate::Forge) trait's vocabulary. They lean on
//! GitHub's shape today (the GraphQL point budget in [`RateLimit`], notably); a
//! second provider is where they would earn a more neutral form.

use jiff::Timestamp;
use reviewq_core::model::{
    ActivityKind, ActivityPayload, ActivityRelation, Mention, PrSnapshot, PrState, ReviewRequest,
    ReviewerVerdict, Said, ThreadState, Verdict,
};
use serde::{Deserialize, Serialize};

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

/// One provider-neutral observation made while reading pull-request activity.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ForgeActivity {
    /// The observed action or lifecycle transition.
    pub kind: ActivityKind,
    /// How this event relates to the configured viewer.
    #[serde(default)]
    pub relation: ActivityRelation,
    /// When the event occurred on the provider.
    pub occurred_at: Timestamp,
    /// The actor, when the provider supplies one.
    pub actor: Option<String>,
    /// The head SHA associated with the event, when the provider supplies one.
    pub head_sha: Option<String>,
    /// The provider's opaque stable event identity, when supplied.
    pub external_id: Option<String>,
    /// The provider's opaque permalink, when supplied.
    pub permalink: Option<String>,
    /// Details specific to this event kind.
    pub payload: ActivityPayload,
}

/// One page of provider activity for a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgeActivityPage {
    /// Pull-request actions and lifecycle transitions from all actors.
    pub activities: Vec<ForgeActivity>,
    /// The provider-owned checkpoint for the next page, when any.
    pub next: Option<String>,
    /// Budget consumed by the provider request, or `None` when this call only
    /// drained events already held by the opaque cursor.
    pub rate_limit: Option<ActivityRateLimit>,
    /// Budget unit the next call will consume, or `None` when it can drain the
    /// cursor without contacting the provider.
    pub next_rate_limit: Option<RateLimitUnit>,
}

/// A provider budget unit. Providers may account for requests and computed
/// query points in separate pools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitUnit {
    /// A count of HTTP/API requests.
    Requests,
    /// A provider-computed query cost.
    Points,
}

/// The budget charged by one activity-page call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityRateLimit {
    /// Which independent provider budget was charged.
    pub unit: RateLimitUnit,
    /// How much the call cost in that unit.
    pub cost: u32,
    /// How much remained in that pool after the call.
    pub remaining: u32,
}

impl ForgeActivityPage {
    /// Reject a page that cannot be deduplicated safely.
    pub fn validate(&self) -> crate::Result<()> {
        for activity in &self.activities {
            if matches!(
                activity.kind,
                ActivityKind::ReviewSubmitted
                    | ActivityKind::Commented
                    | ActivityKind::ReviewThreadCommented
            ) && activity.external_id.is_none()
            {
                return Err(crate::ForgeError::Unreachable {
                    doing: "validating forge activity: user-authored event has no external ID"
                        .into(),
                    source: "the forge response cannot be deduplicated".into(),
                });
            }
        }
        Ok(())
    }
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
    /// Submitted reviews from all actors, captured with the attention inputs.
    pub activities: Vec<ForgeActivity>,
    /// PR number.
    pub number: u64,
    /// Whether it is still open, and if not how it ended.
    ///
    /// The sweep learns this too, but a refresh of one PR never runs the sweep
    /// — so without it here, closing a PR on the forge left the ledger calling
    /// it open until a full sync came round.
    pub state: PrState,
    /// The provider's authoritative timestamp for the latest known transition.
    pub state_changed_at: Option<Timestamp>,
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
    /// What other people said here — top-level comments and submitted reviews,
    /// mine excluded. Whether any of them is somebody I am waiting on is the
    /// classifier's call.
    pub said: Vec<Said>,
    /// Logins I pulled in myself, by @mentioning them in something I wrote.
    /// Quoted mentions are somebody else's words, so they are not here.
    pub invited: Vec<String>,
    /// Commits pushed since my last review; zero if I have not reviewed.
    pub new_commits: u32,
    /// A live review request naming me directly, if any.
    pub review_request: Option<ReviewRequest>,
    /// GraphQL points this fetch cost.
    pub cost: u32,
    /// Points remaining after it.
    pub remaining: u32,
}
