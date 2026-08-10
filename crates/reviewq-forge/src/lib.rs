//! Forge access for reviewq.
//!
//! Everything that knows how to reach a pull request's host lives here: which
//! host a repo is on, which adapter speaks to it, where its token comes from,
//! and the adapters themselves. The [`Forge`] trait is the one interface the
//! layers above use; [`build`] hands back an implementation for a resolved
//! host. Only GitHub has an adapter today, but the seam is drawn so a second
//! provider slots in beside it without the crates above changing.

mod host;
mod types;

pub mod github;

pub use host::{
    DEFAULT_HOST, ForgeHost, ForgeTable, Token, TokenSource, resolve_host, resolve_token,
};
pub use types::{PrDetail, RateLimit, SEARCH_CAP, SweepPage, Viewer};

use anyhow::{Result, bail};
use async_trait::async_trait;
use reviewq_core::model::PrSnapshot;

/// One forge's operations. Each is roughly a single logical request; the
/// implementation handles pagination and wire formats.
///
/// Read-only but for one exception: reviewq never comments, reviews, labels or
/// approves, but [`mark_pr_notifications_read`](Self::mark_pr_notifications_read)
/// marks my own notification threads read, which `reviewq done` calls.
#[async_trait]
pub trait Forge: Send + Sync {
    /// The authenticated account and the current rate-limit budget. The
    /// cheapest call that proves the token works.
    async fn viewer(&self) -> Result<Viewer>;

    /// The REST `core` budget as `(remaining, limit)` — a different pool from
    /// the GraphQL points in [`Viewer`], and what the notifications endpoint
    /// draws on.
    async fn rest_core_remaining(&self) -> Result<(u32, u32)>;

    /// One page of a tier-1 sweep: PRs matching `query` (each with its changed
    /// files), plus the cursor for the next page. `after` is the previous
    /// page's cursor, or `None` to start. The caller paginates so it can
    /// persist and checkpoint each page — an interrupted sweep then resumes
    /// rather than restarts. `query` is the full forge search string, built by
    /// the caller so nothing here hardcodes a repo (or a sort order).
    async fn search_prs_page(
        &self,
        query: &str,
        page_size: u32,
        after: Option<&str>,
    ) -> Result<SweepPage>;

    /// One PR by number. `None` if it no longer exists.
    async fn fetch_pr(&self, owner: &str, name: &str, number: u64) -> Result<Option<PrSnapshot>>;

    /// Tier-2 detail for one PR — threads, my review history, mentions — from
    /// the point of view of `login` (the authenticated viewer). `None` if the
    /// PR no longer exists. This is the expensive per-PR fetch the queue needs
    /// beyond the sweep, so the caller runs it only for tracked PRs.
    async fn fetch_pr_detail(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        login: &str,
    ) -> Result<Option<PrDetail>>;

    /// Mark every unread notification thread for this PR read. The one write
    /// operation reviewq performs; `reviewq done` calls it best-effort — a
    /// failure here should not stop `done` from recording locally.
    async fn mark_pr_notifications_read(&self, owner: &str, name: &str, number: u64) -> Result<()>;

    /// The web URL for pull request `number` in `owner/name` — e.g.
    /// `https://github.com/apache/airflow/pull/12345`. Each provider's own
    /// layout (GitHub's `/pull/N`, a future provider's own) lives in its
    /// adapter; nothing above this trait renders one itself. No I/O, so it
    /// isn't `async` — and no credential either, so a caller that only wants to
    /// show someone where a PR is never resolves a token to do it.
    fn web_url(&self, owner: &str, name: &str, number: u64) -> String;

    /// The env var name and value to hand this forge's token to an external
    /// tool that does its own separate credential resolution — `review`'s
    /// handoff forwards a token it already resolved rather than requiring a
    /// second, separate login. Which env var name that tool expects is a
    /// provider convention (GitHub tooling — `gh`, `wiff` — reads
    /// `GITHUB_TOKEN`), so each adapter answers for itself rather than the
    /// caller guessing.
    ///
    /// Fallible because this is one of the few operations that genuinely needs
    /// the token, and an adapter resolves one only when asked — which can fail,
    /// or prompt.
    fn handoff_credentials(&self) -> Result<(&str, &str)>;
}

/// A pull request named by its web URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRef {
    /// The host it lives on, from the URL itself.
    pub host: String,
    /// The repo owner.
    pub owner: String,
    /// The repo name.
    pub name: String,
    /// The pull request number.
    pub number: u64,
}

/// Read a pull-request URL, using the layout its host's provider uses.
///
/// Dispatched on the provider exactly as [`build`] is, because the path shape is
/// the provider's business — GitHub's `/owner/name/pull/N` is not GitLab's
/// `/owner/name/-/merge_requests/N`. It needs no token, so a caller can resolve a
/// pasted URL before deciding whether to connect.
///
/// `Ok(None)` when `url` simply isn't a URL, so a caller can fall back to reading
/// it as a bare number. An error means it *looked* like one on a host that
/// nothing configured knows about, which is worth saying rather than shrugging at.
pub fn parse_pull_request_url(forges: &ForgeTable, url: &str) -> Result<Option<PullRequestRef>> {
    let Some(after_scheme) = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))
    else {
        return Ok(None);
    };
    // Splitting scheme and host off is URL syntax, not forge knowledge; what the
    // rest of the path means is the provider's.
    let (host_name, path) = after_scheme.split_once('/').unwrap_or((after_scheme, ""));
    let host = resolve_host(forges, host_name)?;

    let parsed = match host.provider.as_deref() {
        Some("github") => github::GithubForge::parse_web_path(path),
        Some(other) => bail!("no forge adapter for provider {other:?}"),
        None => bail!("forge host {host_name:?} has no provider"),
    };
    Ok(parsed.map(|(owner, name, number)| PullRequestRef {
        host: host_name.to_string(),
        owner,
        name,
        number,
    }))
}

/// Build the adapter for `host_name` (resolved to `host`), choosing it by
/// provider.
///
/// Nothing happens here but construction: no I/O, and no credential resolution
/// either. An adapter resolves its own token the first time an operation actually
/// needs one, so building one to ask for a URL costs nothing and cannot prompt.
///
/// `token` presets that resolution for a caller that has already done it and
/// wants to report on it — `doctor` — so the reporting doesn't cost a second
/// resolution. `None` leaves it to the adapter.
///
/// `host` is expected to have come from [`resolve_host`], which already rejects
/// unsupported providers; the match here is the defensive backstop and the
/// single place a new adapter is registered.
pub fn build(host: &ForgeHost, host_name: &str, token: Option<Token>) -> Result<Box<dyn Forge>> {
    match host.provider.as_deref() {
        Some("github") => Ok(Box::new(match token {
            Some(token) => github::GithubForge::with_token(host, host_name, token),
            None => github::GithubForge::new(host, host_name),
        })),
        Some(other) => bail!("no forge adapter for provider {other:?}"),
        None => bail!("forge host has no provider"),
    }
}
