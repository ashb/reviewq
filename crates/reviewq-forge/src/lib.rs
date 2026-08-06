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
}

/// Build the adapter for `host` authenticated with `token`, choosing it by
/// provider. No I/O happens here — it just constructs a client; the first
/// real request is whatever the caller makes with it.
///
/// `host` is expected to have come from [`resolve_host`], which already rejects
/// unsupported providers; the match here is the defensive backstop and the
/// single place a new adapter is registered.
pub fn build(host: &ForgeHost, token: &str) -> Result<Box<dyn Forge>> {
    match host.provider.as_deref() {
        Some("github") => Ok(Box::new(github::GithubForge::new(host, token)?)),
        Some(other) => bail!("no forge adapter for provider {other:?}"),
        None => bail!("forge host has no provider"),
    }
}
