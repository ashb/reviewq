//! `reviewq sync`: fetch updates and rebuild the ledger.
//!
//! The sweep fetches every PR updated since the cursor, each with its changed
//! files, and classifies it against the interest rules; then a handful of
//! involvement searches (`review-requested:me`, `mentions:me`, ...) mark the
//! PRs that name me. Everything is an idempotent upsert, so a re-sync over an
//! overlapping window is a near-no-op.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use futures::{StreamExt as _, stream};
use jiff::{Timestamp, ToSpan};
use reviewq_core::model::{ActivitySource, ClassifyCtx, PrSnapshot, PrState};
use reviewq_core::rules::{Evaluation, Interest};
use reviewq_forge::{
    ActivityRateLimit, DETAIL_BUDGET_FLOOR, Forge, ForgeActivity, ForgeActivityPage, PrDetail,
    RateLimitUnit,
};
use reviewq_ledger::{
    ActivityPageCommit, ActivityRateLimitUnit as StoredRateLimitUnit, Committed, Detail, Ledger,
    NewActivityEvent, RepoId, RepoKey, TrackedReason,
};

use crate::config::{Config, Project, RepoRef};
use crate::identity::Logins;
use crate::priority::{self, Priority};
use crate::{actions, paths};

/// Cursor: the high-water mark of `updatedAt` we have swept up to.
pub const CURSOR_KEY: &str = "last_sync_at";
/// Whether the most recent sweep hit the search cap; surfaced by `doctor`.
pub const TRUNCATED_KEY: &str = "last_sweep_truncated";
const ACTIVITY_CONCURRENCY: usize = 8;

/// Sync every repo in every configured project, reporting through `progress`.
///
/// Repos are synced one at a time, each against its own forge connection, all
/// writing through one ledger handle. A failure on any repo aborts the run —
/// but everything committed before it stays committed, and the cursor means the
/// next sync resumes rather than starts over.
pub async fn run(
    cfg: &Config,
    labels: bool,
    teams: bool,
    which: Detail,
    progress: &mut dyn SyncProgress,
) -> Result<ExitCode> {
    let now = Timestamp::now();
    // One handle for the whole run: every configured repo's sync writes
    // through it, each scoped by its own `repo_id`.
    let ledger = Ledger::open(&paths::database_file()?)?;

    // Asked once per host and remembered: a project's six repos on one forge
    // resolve one login between them.
    let mut logins = Logins::new();

    for project in &cfg.projects {
        for repo in &project.repos {
            let repo_id = ledger.ensure_repo(&repo.key())?;
            // Built here rather than inside the per-repo sync, so that sync is
            // reachable with a forge a test supplies.
            let forge = cfg.forge_for(&repo.host)?;
            let me = logins.on(cfg, &repo.host, forge.as_ref()).await?;
            // Compiled per repo rather than per project, because a rule saying
            // `mine` compiles the login in — and the same project's repos may
            // sit on forges that know you by different names.
            let rules = cfg.interest_for_login(project, &me)?;
            sync_repo(
                cfg,
                forge.as_ref(),
                &ledger,
                repo_id,
                project,
                repo,
                &rules,
                &me,
                labels,
                teams,
                which,
                now,
                progress,
            )
            .await?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One repo's whole sync: sweep, involvement search, detail pass, and the
/// archived-attention sweep, each scoped to `repo_id` in the shared `ledger`
/// and using its own forge connection (a different repo may live on a
/// different host).
#[allow(clippy::too_many_arguments)]
async fn sync_repo(
    cfg: &Config,
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    project: &Project,
    repo: &RepoRef,
    rules: &Interest,
    me: &str,
    labels: bool,
    teams: bool,
    which: Detail,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) -> Result<()> {
    let priority = priority::resolve(repo, forge, ledger, teams, now).await?;
    ledger.rank_attention(repo_id, &priority.authors, &priority.requesters)?;

    // The repo's whole palette, when asked for. Not on every sync: a colour
    // changes about never, and this is a query per repo to learn what is almost
    // always what we already knew. What it is *for* is the hole an incidental
    // approach leaves — label names in the ledger go back to whenever each PR
    // was last swept, so a label not on a recently-updated PR would otherwise
    // have no colour indefinitely.
    if labels {
        let palette = forge.fetch_labels(&repo.owner, &repo.name).await?;
        progress.page("labels", palette.len(), palette.len() as u32);
        ledger.set_label_colours(
            repo_id,
            &palette
                .into_iter()
                .map(|label| (label.name, label.color))
                .collect::<Vec<_>>(),
        )?;
    }

    let since = sweep_since(ledger, repo_id, cfg, now)?;

    // Oldest-updated first, so the cursor watermark advances monotonically and
    // an interrupted sweep resumes from where it stopped. It also makes the
    // 1000-result cap self-draining: each sync consumes the oldest window and
    // the next continues.
    let query = format!(
        "repo:{}/{} is:pr sort:updated-asc updated:>{}",
        repo.owner,
        repo.name,
        search_time(since),
    );
    tracing::info!(%query, "tier-1 sweep");

    let mut stats = Stats::default();
    let mut after: Option<String> = None;

    loop {
        let page = forge
            .search_prs_page(&query, cfg.sync.page_size, after.as_deref())
            .await?;
        stats.total_count = page.total_count;
        stats.cost += page.cost;
        stats.remaining = Some(page.remaining);

        // Files arrive with the sweep, so classification is pure — no per-PR
        // round trip that could fail mid-page.
        let mut batch = Vec::with_capacity(page.prs.len());
        let mut watermark: Option<Timestamp> = None;
        for pr in page.prs {
            let reason = match rules.evaluate(&pr) {
                Evaluation::Match(rule) => {
                    stats.interest += 1;
                    // Asked of every matching rule, not just the one that named
                    // it — see `Interest::keeps_after_merge`.
                    Some(TrackedReason::Interest {
                        rule,
                        after_merge: rules.keeps_after_merge(&pr),
                    })
                }
                Evaluation::Unknown => {
                    stats.truncated_unknown += 1;
                    None
                }
                // NeedsFiles cannot occur — the sweep always carries files.
                Evaluation::NoMatch | Evaluation::NeedsFiles => None,
            };
            watermark = Some(watermark.map_or(pr.updated_at, |w| w.max(pr.updated_at)));
            batch.push((pr, reason));
        }
        stats.swept += batch.len();
        // Persist the page and advance the cursor to the newest updatedAt in
        // it, atomically. A ^C leaves the cursor at the last committed page, so
        // the next sync resumes rather than re-sweeps.
        if let Some(watermark) = watermark {
            stats.new +=
                ledger.commit_sweep_page(repo_id, &batch, CURSOR_KEY, &watermark.to_string())?;
        }
        progress.page("updated", stats.swept, stats.total_count);

        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    let truncated = stats.total_count > stats.swept as u32;
    ledger.set_meta(repo_id, TRUNCATED_KEY, if truncated { "1" } else { "0" })?;

    let review_requested = involvement_search(
        forge,
        ledger,
        repo_id,
        repo,
        me,
        cfg.involving_reasons(project),
        cfg.sync.page_size,
        &mut stats,
        progress,
    )
    .await?;

    detail_pass(
        forge,
        ledger,
        repo_id,
        repo,
        me,
        &cfg.bots.logins,
        &priority,
        rules,
        which,
        project.include_merged,
        &review_requested,
        now,
        &mut stats,
        progress,
    )
    .await?;

    sync_activity(
        forge,
        ledger,
        repo_id,
        repo,
        me,
        None,
        stats.remaining,
        now,
        &mut stats,
        progress,
    )
    .await;

    // Merged/closed PRs are not re-fetched, so drop any attention they still
    // carry — bar the merged ones post-merge review keeps, whether that is this
    // project's `include_merged` or their own rule's `after_merge`.
    ledger.clear_archived_attention(repo_id, project.include_merged, now)?;

    let (tracked, total) = ledger.counts(repo_id)?;
    let summary = RepoSummary {
        repo: repo.slug(),
        stats,
        tracked,
        total,
        truncated,
    };
    progress.repo_finished(&summary);
    if summary.truncated {
        tracing::warn!(
            repo = repo.slug(),
            total = summary.stats.total_count,
            cap = reviewq_forge::SEARCH_CAP,
            "sweep hit the search cap; some PRs in this window were missed \
             (narrow sync.bootstrap_days or sync more often)"
        );
    }

    Ok(())
}

/// The lower bound for this sweep: the stored cursor minus an overlap buffer,
/// or a bootstrap window on the first-ever run.
fn sweep_since(
    ledger: &Ledger,
    repo_id: RepoId,
    cfg: &Config,
    now: Timestamp,
) -> Result<Timestamp> {
    match ledger.get_meta(repo_id, CURSOR_KEY)? {
        Some(stored) => {
            let cursor: Timestamp = stored
                .parse()
                .with_context(|| format!("parsing stored cursor {stored:?}"))?;
            Ok(cursor - (cfg.sync.overlap_minutes as i64).minutes())
        }
        // A lookback window is a fixed span of hours: jiff refuses calendar
        // `day` units in zoneless Timestamp arithmetic (and would panic).
        None => Ok(now - (cfg.sync.bootstrap_days as i64 * 24).hours()),
    }
}

/// Format a timestamp for a GitHub search `updated:>` bound: whole seconds and
/// an explicit numeric offset, the form GitHub's docs specify. jiff renders a
/// `Z` with sub-second precision, which we normalise away here.
fn search_time(ts: Timestamp) -> String {
    let rendered = ts.to_string();
    let head = rendered.strip_suffix('Z').unwrap_or(&rendered);
    let seconds = head.split('.').next().unwrap_or(head);
    format!("{seconds}+00:00")
}

/// Find PRs I'm involved in via search qualifiers — one query per configured
/// relationship — and mark them `involved:`. Returns the set of PR numbers where
/// a review is currently requested of me, so the detail pass can raise
/// `review-requested` even when the request went to a *team* I'm on (which
/// tier-2 can't attribute to me directly, but this search resolves).
///
/// This is what replaces scanning the notifications firehose: each qualifier
/// (`review-requested:me`, `mentions:me`, `assignee:me`, ...) returns only the
/// PRs where it holds, through the same resumable search path as the sweep. No
/// window is applied — a review request from weeks ago still matters — but the
/// result sets are small, so a full re-run each sync is cheap.
#[allow(clippy::too_many_arguments)]
async fn involvement_search(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    login: &str,
    reasons: &[String],
    page_size: u32,
    stats: &mut Stats,
    progress: &mut dyn SyncProgress,
) -> Result<HashSet<u64>> {
    let mut involved = HashSet::new();
    let mut review_requested = HashSet::new();

    for reason in reasons {
        let Some(qualifier) = involvement_qualifier(reason, login) else {
            tracing::warn!(
                reason,
                "unknown involvement reason; skipping (expected one of \
                 review_requested/mention/assign/author/comment)"
            );
            continue;
        };
        let query = format!(
            "repo:{}/{} is:pr is:open {qualifier}",
            repo.owner, repo.name
        );
        tracing::info!(%query, reason, "involvement search");

        let mut after: Option<String> = None;
        let mut fetched = 0usize;
        loop {
            let page = forge
                .search_prs_page(&query, page_size, after.as_deref())
                .await?;
            stats.cost += page.cost;
            stats.remaining = Some(page.remaining);
            for pr in &page.prs {
                if ledger.upsert_pr(repo_id, pr, Some(TrackedReason::Involved(reason.clone())))? {
                    stats.new += 1;
                }
                involved.insert(pr.number);
                if reason == "review_requested" {
                    review_requested.insert(pr.number);
                }
            }
            fetched += page.prs.len();
            progress.page(reason, fetched, page.total_count);
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
    }

    stats.involved = involved.len() as u64;
    Ok(review_requested)
}

/// Counts accumulated while initially filling pull-request activity history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillStats {
    /// Pull requests the provider reported complete.
    pub prs: u64,
    /// New events stored after deduplication.
    pub events: u64,
    /// Provider pages committed.
    pub pages: usize,
    /// GraphQL-style points spent.
    pub point_cost: u32,
    /// Request-count budget spent.
    pub request_cost: u32,
    /// Last reported GraphQL-style point balance.
    pub points_remaining: Option<u32>,
    /// Last reported request-count balance.
    pub requests_remaining: Option<u32>,
    /// The pass stopped before a request would cross the safety floor.
    pub stopped_for_budget: bool,
}

/// One pull request whose provider activity could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillFailure {
    /// Pull request number.
    pub number: u64,
    /// Contextual provider or validation error.
    pub message: String,
}

/// A partial backfill result that retains all successfully committed counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillFailed {
    /// Work committed before and after individual pull-request failures.
    pub stats: BackfillStats,
    /// Pull requests that remain incomplete.
    pub failures: Vec<BackfillFailure>,
}

impl fmt::Display for BackfillFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "activity sync failed:")?;
        for failure in &self.failures {
            writeln!(f, "#{}: {}", failure.number, failure.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for BackfillFailed {}

/// Automatic activity work attempted after a pull request became tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackActivity {
    /// Work committed before the provider completed or failed.
    pub stats: BackfillStats,
    /// A provider, identity, validation, or ledger failure isolated from the
    /// successful tracking and detail refresh.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct ActivityBudgets {
    points: Option<u32>,
    requests: Option<u32>,
}

#[derive(Debug, Default)]
struct ActivityBudgetErrors {
    points: Option<String>,
    requests: Option<String>,
}

impl ActivityBudgets {
    fn update(&mut self, rate_limit: ActivityRateLimit) {
        match rate_limit.unit {
            RateLimitUnit::Points => keep_lowest(&mut self.points, rate_limit.remaining),
            RateLimitUnit::Requests => keep_lowest(&mut self.requests, rate_limit.remaining),
        }
    }

    fn is_low(self, unit: Option<RateLimitUnit>) -> bool {
        match unit {
            Some(RateLimitUnit::Points) => budget_is_low(self.points),
            Some(RateLimitUnit::Requests) => budget_is_low(self.requests),
            None => false,
        }
    }
}

fn keep_lowest(current: &mut Option<u32>, observed: u32) {
    *current = Some(current.map_or(observed, |current| current.min(observed)));
}

impl ActivityBudgetErrors {
    fn for_unit(&self, unit: Option<RateLimitUnit>) -> Option<&str> {
        match unit {
            Some(RateLimitUnit::Points) => self.points.as_deref(),
            Some(RateLimitUnit::Requests) => self.requests.as_deref(),
            None => None,
        }
    }
}

async fn activity_budgets(
    forge: &dyn Forge,
    known_points: Option<u32>,
) -> (ActivityBudgets, ActivityBudgetErrors) {
    let (points, point_error) = match known_points {
        Some(remaining) => (Some(remaining), None),
        None => match forge.viewer().await {
            Ok(viewer) => (Some(viewer.rate_limit.remaining), None),
            Err(error) => (None, Some(error.to_string())),
        },
    };
    let (requests, request_error) = match forge.rest_core_remaining().await {
        Ok((remaining, _)) => (Some(remaining), None),
        Err(error) => (None, Some(error.to_string())),
    };
    (
        ActivityBudgets { points, requests },
        ActivityBudgetErrors {
            points: point_error,
            requests: request_error,
        },
    )
}

impl BackfillStats {
    fn record_rate_limit(&mut self, rate_limit: ActivityRateLimit) {
        match rate_limit.unit {
            RateLimitUnit::Points => {
                self.point_cost += rate_limit.cost;
                keep_lowest(&mut self.points_remaining, rate_limit.remaining);
            }
            RateLimitUnit::Requests => {
                self.request_cost += rate_limit.cost;
                keep_lowest(&mut self.requests_remaining, rate_limit.remaining);
            }
        }
    }
}

/// Backfill every currently tracked pull request, resuming each provider cursor.
///
/// Pages commit independently. Provider failures are collected so safe work on
/// other pull requests can finish before the function returns an error.
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
async fn backfill_activity(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) -> Result<BackfillStats> {
    let (stats, errors, _) = backfill_activity_inner(
        forge, ledger, repo_id, repo, actor, None, None, now, progress,
    )
    .await?;
    if errors.is_empty() {
        Ok(stats)
    } else {
        Err(BackfillFailed {
            stats,
            failures: errors,
        }
        .into())
    }
}

#[allow(clippy::too_many_arguments)]
async fn backfill_activity_for_pr(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    number: u64,
    known_points: Option<u32>,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) -> Result<BackfillStats> {
    let stored = ledger.activity_backfill(repo_id, number)?;
    if stored
        .as_ref()
        .is_some_and(|progress| progress.completed_at.is_some())
    {
        return Ok(BackfillStats::default());
    }
    let cursor = stored.as_ref().and_then(|progress| progress.cursor.clone());
    let next_rate_limit = stored.and_then(|progress| progress.next_rate_limit);
    let (stats, errors, _) = backfill_pending(
        forge,
        ledger,
        repo_id,
        repo,
        actor,
        known_points,
        now,
        vec![(number, cursor, next_rate_limit)],
        1,
        progress,
    )
    .await?;
    if errors.is_empty() {
        Ok(stats)
    } else {
        Err(BackfillFailed {
            stats,
            failures: errors,
        }
        .into())
    }
}

struct QuietProgress;

impl SyncProgress for QuietProgress {
    fn page(&mut self, _what: &str, _fetched: usize, _total: u32) {}

    fn repo_finished(&mut self, _summary: &RepoSummary) {}
}

#[allow(clippy::too_many_arguments)]
async fn backfill_newly_tracked_activity(
    tracked: &actions::Tracked,
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    number: u64,
    now: Timestamp,
) -> Option<TrackActivity> {
    if *tracked == actions::Tracked::Already {
        return None;
    }
    let result = backfill_activity_for_pr(
        forge,
        ledger,
        repo_id,
        repo,
        actor,
        number,
        None,
        now,
        &mut QuietProgress,
    )
    .await;
    Some(match result {
        Ok(stats) => TrackActivity { stats, error: None },
        Err(error) => TrackActivity {
            stats: error
                .downcast_ref::<BackfillFailed>()
                .map_or_else(BackfillStats::default, |failed| failed.stats.clone()),
            error: Some(format!("{error:#}")),
        },
    })
}

#[allow(clippy::too_many_arguments)]
async fn backfill_activity_inner(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    only: Option<u64>,
    known_points: Option<u32>,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) -> Result<(BackfillStats, Vec<BackfillFailure>, usize)> {
    let numbers = match only {
        Some(number) if ledger.show(repo_id, number)?.is_some() => vec![number],
        Some(_) => Vec::new(),
        None => ledger.activity_candidates(repo_id)?,
    };
    let mut pending = Vec::new();
    for number in numbers {
        let stored = ledger.activity_backfill(repo_id, number)?;
        if !stored
            .as_ref()
            .is_some_and(|progress| progress.completed_at.is_some())
        {
            let cursor = stored.as_ref().and_then(|progress| progress.cursor.clone());
            let next_rate_limit = stored.and_then(|progress| progress.next_rate_limit);
            pending.push((number, cursor, next_rate_limit));
        }
    }
    if pending.is_empty() {
        return Ok((BackfillStats::default(), Vec::new(), 0));
    }
    let total = pending.len() as u32;

    backfill_pending(
        forge,
        ledger,
        repo_id,
        repo,
        actor,
        known_points,
        now,
        pending,
        total,
        progress,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn backfill_pending(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    known_points: Option<u32>,
    now: Timestamp,
    pending: Vec<(u64, Option<String>, Option<StoredRateLimitUnit>)>,
    total: u32,
    progress: &mut dyn SyncProgress,
) -> Result<(BackfillStats, Vec<BackfillFailure>, usize)> {
    let (mut budgets, budget_errors) = activity_budgets(forge, known_points).await;
    let mut stats = BackfillStats {
        points_remaining: budgets.points,
        requests_remaining: budgets.requests,
        ..Default::default()
    };
    let mut errors = Vec::new();
    let mut completed = 0;
    let mut pending = pending
        .into_iter()
        .map(|(number, cursor, saved_rate_limit)| {
            let next_rate_limit = saved_rate_limit.map(provider_rate_limit).or_else(|| {
                cursor
                    .is_none()
                    .then(|| forge.initial_activity_rate_limit())
            });
            (number, cursor, next_rate_limit)
        })
        .collect::<VecDeque<_>>();

    let mut in_flight = stream::FuturesUnordered::new();
    while !pending.is_empty() || !in_flight.is_empty() {
        while in_flight.len() < ACTIVITY_CONCURRENCY && !pending.is_empty() {
            let (number, cursor, next_rate_limit) = pending.pop_front().expect("counted pending");
            if let Some(error) = budget_errors.for_unit(next_rate_limit) {
                tracing::warn!(repo = repo.slug(), number, error = %error, "activity sync failed");
                errors.push(BackfillFailure {
                    number,
                    message: error.to_string(),
                });
                completed += 1;
                progress.page("activity", completed, total);
            } else if budgets.is_low(next_rate_limit) {
                stats.stopped_for_budget = true;
            } else {
                in_flight.push(async move {
                    let page =
                        fetch_activity_page(forge, repo, number, actor, cursor.as_deref()).await;
                    (number, cursor, next_rate_limit, page)
                });
            }
        }
        if let Some((number, cursor, _next_rate_limit, page)) = in_flight.next().await {
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(repo = repo.slug(), number, error = %error, "activity sync failed");
                    errors.push(BackfillFailure {
                        number,
                        message: error.to_string(),
                    });
                    completed += 1;
                    progress.page("activity", completed, total);
                    continue;
                }
            };
            let mut events = Vec::new();
            let participated_at = first_own_activity(&page.activities, actor);
            let mine = authored_by_viewer(ledger, repo_id, number, actor)?;
            for activity in &page.activities {
                let mut event = new_activity(activity, now);
                event.relation = activity_relation(
                    ledger,
                    repo_id,
                    number,
                    actor,
                    activity,
                    participated_at,
                    mine,
                )?;
                events.push(event);
            }
            if let Some(rate_limit) = page.rate_limit {
                budgets.update(rate_limit);
                stats.record_rate_limit(rate_limit);
            }
            let committed = ledger.commit_activity_page(
                repo_id,
                number,
                cursor.as_deref(),
                &events,
                page.next.as_deref(),
                page.next_rate_limit.map(stored_rate_limit),
                now,
            )?;
            let ActivityPageCommit::Applied { inserted } = committed else {
                completed += 1;
                progress.page("activity", completed, total);
                continue;
            };
            stats.events += inserted;
            stats.pages += 1;
            if page.next.is_none() {
                stats.prs += 1;
            }
            match page.next {
                Some(next) => pending.push_back((number, Some(next), page.next_rate_limit)),
                None => {
                    completed += 1;
                    progress.page("activity", completed, total);
                }
            }
        }
    }

    Ok((stats, errors, completed))
}

async fn fetch_activity_page(
    forge: &dyn Forge,
    repo: &RepoRef,
    number: u64,
    actor: &str,
    cursor: Option<&str>,
) -> reviewq_forge::Result<ForgeActivityPage> {
    let page = forge
        .fetch_pr_activity(&repo.owner, &repo.name, number, actor, cursor)
        .await?;
    page.validate()?;
    Ok(page)
}

fn new_activity(activity: &ForgeActivity, recorded_at: Timestamp) -> NewActivityEvent {
    NewActivityEvent {
        relation: activity.relation,
        source: ActivitySource::Forge,
        kind: activity.kind,
        occurred_at: activity.occurred_at,
        recorded_at,
        actor: activity.actor.clone(),
        head_sha: activity.head_sha.clone(),
        external_id: activity.external_id.clone(),
        permalink: activity.permalink.clone(),
        payload: activity.payload.clone(),
    }
}

fn stored_rate_limit(unit: RateLimitUnit) -> StoredRateLimitUnit {
    match unit {
        RateLimitUnit::Requests => StoredRateLimitUnit::Requests,
        RateLimitUnit::Points => StoredRateLimitUnit::Points,
    }
}

fn provider_rate_limit(unit: StoredRateLimitUnit) -> RateLimitUnit {
    match unit {
        StoredRateLimitUnit::Requests => RateLimitUnit::Requests,
        StoredRateLimitUnit::Points => RateLimitUnit::Points,
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_activity(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    only: Option<u64>,
    known_points: Option<u32>,
    now: Timestamp,
    stats: &mut Stats,
    progress: &mut dyn SyncProgress,
) {
    let (backfill_count, incremental) = match (|| {
        let candidates = match only {
            Some(number) if ledger.show(repo_id, number)?.is_some() => vec![number],
            Some(_) => Vec::new(),
            None => ledger.activity_candidates(repo_id)?,
        };
        let mut backfill_count = 0;
        for number in candidates {
            if ledger
                .activity_backfill(repo_id, number)?
                .is_none_or(|state| state.completed_at.is_none())
            {
                backfill_count += 1;
            }
        }
        let refresh = match only {
            Some(number) => vec![number],
            None => ledger.activity_refresh_candidates(repo_id)?,
        };
        let mut incremental = Vec::new();
        for number in refresh {
            if ledger
                .activity_backfill(repo_id, number)?
                .is_some_and(|state| state.completed_at.is_some())
            {
                incremental.push((
                    number,
                    ledger.begin_incremental_activity(repo_id, number, now)?,
                ));
            }
        }
        Ok::<_, reviewq_ledger::LedgerError>((backfill_count, incremental))
    })() {
        Ok(progress) => progress,
        Err(error) => {
            stats.activity_errors += 1;
            tracing::warn!(%error, "could not prepare activity progress");
            return;
        }
    };
    let total = backfill_count + incremental.len();

    let mut activity_done = match backfill_activity_inner(
        forge,
        ledger,
        repo_id,
        repo,
        actor,
        only,
        known_points,
        now,
        progress,
    )
    .await
    {
        Ok((backfill, errors, attempted)) => {
            stats.activity_events += backfill.events;
            stats.activity_request_cost += backfill.request_cost;
            stats.cost += backfill.point_cost;
            if let Some(remaining) = backfill.points_remaining {
                stats.remaining = Some(remaining);
            }
            stats.activity_errors += errors.len() as u64;
            attempted
        }
        Err(error) => {
            stats.activity_errors += 1;
            tracing::warn!(repo = repo.slug(), %error, "activity sync could not run");
            return;
        }
    };

    if incremental.is_empty() {
        return;
    }

    let (mut budgets, budget_errors) = activity_budgets(forge, stats.remaining).await;
    let total = total as u32;
    let mut pending = incremental
        .into_iter()
        .map(|(number, checkpoint)| {
            let next_rate_limit =
                checkpoint
                    .next_rate_limit
                    .map(provider_rate_limit)
                    .or_else(|| {
                        checkpoint
                            .cursor
                            .is_none()
                            .then(|| forge.initial_activity_rate_limit())
                    });
            (number, checkpoint, next_rate_limit)
        })
        .collect::<VecDeque<_>>();

    let mut in_flight = stream::FuturesUnordered::new();
    while !pending.is_empty() || !in_flight.is_empty() {
        while in_flight.len() < ACTIVITY_CONCURRENCY && !pending.is_empty() {
            let (number, checkpoint, next_rate_limit) =
                pending.pop_front().expect("counted pending");
            if let Some(error) = budget_errors.for_unit(next_rate_limit) {
                stats.activity_errors += 1;
                tracing::warn!(
                    repo = repo.slug(),
                    number,
                    error,
                    "activity budget unavailable"
                );
                activity_done += 1;
                progress.page("activity", activity_done, total);
            } else if budgets.is_low(next_rate_limit) {
                continue;
            } else {
                in_flight.push(async move {
                    let page = fetch_activity_page(
                        forge,
                        repo,
                        number,
                        actor,
                        checkpoint.cursor.as_deref(),
                    )
                    .await;
                    (number, checkpoint, next_rate_limit, page)
                });
            }
        }
        if let Some((number, checkpoint, _next_rate_limit, page)) = in_flight.next().await {
            let page = match page {
                Ok(page) => page,
                Err(error) => {
                    stats.activity_errors += 1;
                    tracing::warn!(repo = repo.slug(), number, %error, "incremental activity failed");
                    activity_done += 1;
                    progress.page("activity", activity_done, total);
                    continue;
                }
            };
            let processed = match incremental_page_events(
                ledger,
                repo_id,
                number,
                actor,
                &page.activities,
                checkpoint.stop_at,
                now,
            ) {
                Ok(result) => result,
                Err(error) => {
                    stats.activity_errors += 1;
                    tracing::warn!(repo = repo.slug(), number, %error, "could not check activity identity");
                    activity_done += 1;
                    progress.page("activity", activity_done, total);
                    continue;
                }
            };
            if let Some(rate_limit) = page.rate_limit {
                budgets.update(rate_limit);
                match rate_limit.unit {
                    RateLimitUnit::Points => {
                        stats.cost += rate_limit.cost;
                        keep_lowest(&mut stats.remaining, rate_limit.remaining);
                    }
                    RateLimitUnit::Requests => stats.activity_request_cost += rate_limit.cost,
                }
            }
            let checkpoint_next = if processed.reached_boundary {
                None
            } else {
                page.next.as_deref()
            };
            let checkpoint_rate_limit = checkpoint_next
                .and(page.next_rate_limit)
                .map(stored_rate_limit);
            match ledger.commit_incremental_activity_page(
                repo_id,
                number,
                &checkpoint,
                &processed.events,
                checkpoint_next,
                checkpoint_rate_limit,
            ) {
                Ok(ActivityPageCommit::Applied { inserted }) => {
                    stats.activity_events += inserted;
                }
                Ok(ActivityPageCommit::Superseded) => {
                    activity_done += 1;
                    progress.page("activity", activity_done, total);
                    continue;
                }
                Err(error) => {
                    stats.activity_errors += 1;
                    tracing::warn!(repo = repo.slug(), number, %error, "could not store incremental activity");
                    activity_done += 1;
                    progress.page("activity", activity_done, total);
                    continue;
                }
            }
            match (processed.reached_boundary, page.next) {
                (false, Some(next)) => {
                    pending.push_back((
                        number,
                        reviewq_ledger::ActivityIncremental {
                            revision: checkpoint.revision + 1,
                            generation: checkpoint.generation,
                            started_at: checkpoint.started_at,
                            cursor: Some(next),
                            next_rate_limit: checkpoint_rate_limit,
                            stop_at: processed.stop_at,
                        },
                        page.next_rate_limit,
                    ));
                }
                _ => {
                    activity_done += 1;
                    progress.page("activity", activity_done, total);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn sync_activity_for_pr(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    actor: &str,
    number: u64,
    known_points: Option<u32>,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) {
    let mut stats = Stats {
        remaining: known_points,
        ..Default::default()
    };
    sync_activity(
        forge,
        ledger,
        repo_id,
        repo,
        actor,
        Some(number),
        known_points,
        now,
        &mut stats,
        progress,
    )
    .await;
}

struct IncrementalPage {
    events: Vec<NewActivityEvent>,
    stop_at: Option<Timestamp>,
    reached_boundary: bool,
}

fn incremental_page_events(
    ledger: &Ledger,
    repo_id: RepoId,
    number: u64,
    actor: &str,
    activities: &[ForgeActivity],
    stop_at: Option<Timestamp>,
    recorded_at: Timestamp,
) -> reviewq_ledger::Result<IncrementalPage> {
    let mut page = IncrementalPage {
        events: Vec::new(),
        stop_at,
        reached_boundary: false,
    };
    let participated_at = first_own_activity(activities, actor);
    let mine = authored_by_viewer(ledger, repo_id, number, actor)?;
    for activity in activities {
        if page
            .stop_at
            .is_some_and(|boundary| activity.occurred_at < boundary)
        {
            page.reached_boundary = true;
            break;
        }
        if let Some(external_id) = activity.external_id.as_deref()
            && ledger.has_forge_activity(repo_id, activity.kind, external_id)?
        {
            continue;
        }
        let mut event = new_activity(activity, recorded_at);
        event.relation = activity_relation(
            ledger,
            repo_id,
            number,
            actor,
            activity,
            participated_at,
            mine,
        )?;
        page.events.push(event);
    }
    Ok(page)
}

fn first_own_activity(activities: &[ForgeActivity], viewer: &str) -> Option<Timestamp> {
    activities
        .iter()
        .filter(|event| {
            event
                .actor
                .as_deref()
                .is_some_and(|actor| actor.eq_ignore_ascii_case(viewer))
        })
        .map(|event| event.occurred_at)
        .min()
}

fn authored_by_viewer(
    ledger: &Ledger,
    repo_id: RepoId,
    number: u64,
    viewer: &str,
) -> reviewq_ledger::Result<bool> {
    Ok(ledger
        .show(repo_id, number)?
        .is_some_and(|show| show.pr.author.eq_ignore_ascii_case(viewer)))
}

fn activity_relation(
    ledger: &Ledger,
    repo_id: RepoId,
    number: u64,
    viewer: &str,
    activity: &ForgeActivity,
    participated_at: Option<Timestamp>,
    mine: bool,
) -> reviewq_ledger::Result<reviewq_core::model::ActivityRelation> {
    use reviewq_core::model::{ActivityKind, ActivityRelation};
    if activity
        .actor
        .as_deref()
        .is_some_and(|actor| actor.eq_ignore_ascii_case(viewer))
    {
        return Ok(ActivityRelation::Own);
    }
    if activity.relation == ActivityRelation::Relevant || mine {
        return Ok(ActivityRelation::Relevant);
    }
    if matches!(
        activity.kind,
        ActivityKind::PrClosed | ActivityKind::PrMerged | ActivityKind::PrReopened
    ) && (participated_at.is_some_and(|at| at <= activity.occurred_at)
        || ledger.lifecycle_affects_me(
            repo_id,
            number,
            activity.occurred_at,
            activity.kind,
            viewer,
        )?)
    {
        return Ok(ActivityRelation::Relevant);
    }
    Ok(ActivityRelation::Context)
}

/// Whether to stop the detail pass rather than spend the tail of the budget.
///
/// Pulled out of the loop so it can be tested without a forge: it is one
/// comparison, and it got the case it exists for backwards.
fn budget_is_low(remaining: Option<u32>) -> bool {
    remaining.is_some_and(|left| left < DETAIL_BUDGET_FLOOR)
}

/// Tier-2: for every tracked PR whose detail is stale, fetch its threads,
/// reviews and mentions, classify it, and store the resulting attention. This
/// is the expensive pass — one query per PR — so it runs only over the tracked
/// set, and each PR commits independently so a ^C (or a budget stop) keeps
/// finished work.
#[allow(clippy::too_many_arguments)]
async fn detail_pass(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    login: &str,
    bots: &[String],
    priority: &Priority,
    rules: &Interest,
    which: Detail,
    include_merged: bool,
    review_requested: &HashSet<u64>,
    now: Timestamp,
    stats: &mut Stats,
    progress: &mut dyn SyncProgress,
) -> Result<()> {
    let pending = ledger.prs_needing_detail(repo_id, include_merged, which)?;
    let total = pending.len() as u32;
    for (index, tracked) in pending.iter().enumerate() {
        // The floor is checked against the budget the last fetch reported, so we
        // stop before spending the tail rather than after.
        if budget_is_low(stats.remaining) {
            tracing::warn!(
                remaining = stats.remaining,
                done = index,
                total,
                "stopping the detail pass to preserve GraphQL budget; \
                 re-run `reviewq sync` to finish the rest"
            );
            break;
        }

        let Some((detail, queued)) = refresh_one(
            forge,
            ledger,
            repo_id,
            repo,
            login,
            bots,
            priority,
            // Either the project keeps every merged PR, or this one's own rule
            // asked to keep it.
            include_merged || tracked.after_merge,
            review_requested,
            &tracked.pr,
            &tracked.tracked_reason,
            &rules.heard_bots(&tracked.pr),
            now,
        )
        .await?
        else {
            continue;
        };
        stats.cost += detail.cost;
        stats.remaining = Some(detail.remaining);
        if queued {
            stats.queued += 1;
        }
        progress.page("detail", index + 1, total);
    }
    Ok(())
}

/// What refreshing one PR did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refreshed {
    /// Its detail was fetched and stored.
    Updated {
        /// `owner/name` it was fetched from.
        repo: String,
        /// The state the forge has it in, which the fetch may just have learnt:
        /// a PR closed or merged since the last sweep is what a single refresh
        /// most often turns up.
        state: PrState,
        /// It now holds at least one attention reason — it's on the queue.
        queued: bool,
        /// GraphQL points the fetch spent.
        cost: u32,
        /// Points left in the hourly budget afterwards.
        remaining: u32,
    },
    /// The ledger has never heard of this number, so there is nothing to
    /// refresh — a full `sync` has to find it first.
    Untracked,
    /// The forge no longer has it. Recorded as unavailable, so it leaves the
    /// queue and stops being refetched.
    Gone,
}

/// The durable outcomes of explicitly tracking and refreshing one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedOne {
    /// Whether tracking was new, restored, or already in effect.
    pub tracked: actions::Tracked,
    /// The independent current-detail refresh result.
    pub refreshed: Refreshed,
    /// Initial activity sync for a PR that became tracked during this call.
    pub activity: Option<TrackActivity>,
}

/// Refresh one PR by number, resolving from the ledger and config whatever
/// [`refresh_one`] needs.
///
/// The entry point for wanting one PR up to date without a whole sync when the
/// caller only has its number, such as the `sync <number>` command.
///
/// Which repo the number belongs to comes from the ledger, not config, and
/// through the same resolver `show`/`done`/`mute` use — so a number that is
/// ambiguous across repos is refused here too rather than resolving to whichever
/// repo happened to come back first.
pub async fn sync_one(cfg: &Config, number: u64) -> Result<Refreshed> {
    // One handle for the whole call: the resolution below is a read on the same
    // connection the refresh then writes through.
    let ledger = crate::resolve::open()?;
    let Some(key) = crate::resolve::repo_with_pr(&ledger, number)? else {
        return Ok(Refreshed::Untracked);
    };
    sync_one_for_in(cfg, &key, number, &ledger).await
}

/// Refresh one PR whose repository has already been resolved, such as a TUI
/// selection or a review handoff.
pub async fn sync_one_for(cfg: &Config, key: &RepoKey, number: u64) -> Result<Refreshed> {
    let ledger = crate::resolve::open()?;
    sync_one_for_in(cfg, key, number, &ledger).await
}

async fn sync_one_for_in(
    cfg: &Config,
    key: &RepoKey,
    number: u64,
    ledger: &Ledger,
) -> Result<Refreshed> {
    let repo = cfg
        .repos()
        .find(|r| r.key() == *key)
        .cloned()
        .with_context(|| {
            format!(
                "#{number} was last synced from {}/{}, which is no longer configured",
                key.owner, key.name
            )
        })?;
    let project = cfg
        .projects
        .iter()
        .find(|p| p.repos.contains(&repo))
        .with_context(|| format!("{} is no longer configured", repo.slug()))?;

    let repo_id = ledger
        .repo_id(key)?
        .with_context(|| format!("{} is not in the ledger", key.slug()))?;
    let Some(show) = ledger.show(repo_id, number)? else {
        return Ok(Refreshed::Untracked);
    };

    let forge = cfg.forge_for(&repo.host)?;
    let me = Logins::new().on(cfg, &repo.host, forge.as_ref()).await?;
    let now = Timestamp::now();
    let priority = priority::resolve(&repo, forge.as_ref(), ledger, false, now).await?;
    ledger.rank_attention(repo_id, &priority.authors, &priority.requesters)?;
    let outcome = refresh_one(
        forge.as_ref(),
        ledger,
        repo_id,
        &repo,
        &me,
        &cfg.bots.logins,
        &priority,
        project.include_merged || show.after_merge,
        // No involvement search has run, so a review requested of a *team* isn't
        // known here. A full sync is what resolves those; this only refreshes
        // what one PR's own detail can say.
        &HashSet::new(),
        &show.pr,
        show.tracked_reason.as_deref().unwrap_or(""),
        &cfg.interest_for_login(project, &me)?.heard_bots(&show.pr),
        Timestamp::now(),
    )
    .await?;

    if let Some((detail, _)) = outcome.as_ref() {
        sync_activity_for_pr(
            forge.as_ref(),
            ledger,
            repo_id,
            &repo,
            &me,
            number,
            Some(detail.remaining),
            Timestamp::now(),
            &mut QuietProgress,
        )
        .await;
    }

    Ok(match outcome {
        None => Refreshed::Gone,
        Some((detail, queued)) => Refreshed::Updated {
            repo: repo.slug(),
            state: detail.state,
            queued,
            cost: detail.cost,
            remaining: detail.remaining,
        },
    })
}

/// Start tracking `number`, fetching it if the ledger has never seen it, then
/// give it a detail pass so it can reach the queue.
///
/// The entry point for "put this PR in my queue" from a bare number or a pasted
/// URL: `reviewq track`/`add`, and the TUI's offer to fetch a PR you asked to go
/// to and which turned out to be unknown.
///
/// Which repo it belongs to comes from config, not the ledger — the whole point
/// is that the ledger may know nothing about it. With more than one repo
/// configured a bare number is ambiguous, so `repo` names one.
pub async fn track_one(cfg: &Config, repo: Option<&RepoRef>, number: u64) -> Result<TrackedOne> {
    // Always reaches the forge, whether or not the ledger already has the PR:
    // `track` means "track it and go and get it", unlike the purely local actions.
    let repo = match repo {
        Some(repo) => repo.clone(),
        None => {
            let mut repos = cfg.repos();
            let first = repos.next().context("no repos configured")?.clone();
            if repos.next().is_some() {
                bail!(
                    "more than one repo is configured — name one, or give a full \
                     pull-request URL"
                );
            }
            first
        }
    };

    let ledger = Ledger::open(&paths::database_file()?)?;
    let repo_id = ledger.ensure_repo(&repo.key())?;
    let forge = cfg.forge_for(&repo.host)?;
    let tracked = actions::track(&ledger, repo_id, &repo, number, forge.as_ref()).await?;

    // A freshly-stored PR holds no attention until something classifies it, so
    // the detail pass is what actually puts it on the queue.
    let refreshed = sync_one_for(cfg, &repo.key(), number).await?;
    let activity = if tracked == actions::Tracked::Already {
        None
    } else {
        match Logins::new().on(cfg, &repo.host, forge.as_ref()).await {
            Ok(actor) => {
                backfill_newly_tracked_activity(
                    &tracked,
                    forge.as_ref(),
                    &ledger,
                    repo_id,
                    &repo,
                    &actor,
                    number,
                    Timestamp::now(),
                )
                .await
            }
            Err(error) => Some(TrackActivity {
                stats: BackfillStats::default(),
                error: Some(format!("{error:#}")),
            }),
        }
    };
    Ok(TrackedOne {
        tracked,
        refreshed,
        activity,
    })
}

/// Fetch one PR's tier-2 detail, classify it against what the fetch saw, and
/// commit the result — the per-item body `detail_pass` runs over the whole
/// tracked set, factored out so [`sync_one`] can refresh a single PR without
/// waiting for a whole sync.
///
/// `None` when the forge has no such PR, having first recorded that via
/// [`Ledger::mark_detail_unavailable`] — one unreachable PR must not abort a
/// sync, and must not be retried on every subsequent one. Otherwise the bool
/// reports whether the PR now holds attention.
#[allow(clippy::too_many_arguments)]
async fn refresh_one(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: RepoId,
    repo: &RepoRef,
    login: &str,
    bots: &[String],
    priority: &Priority,
    include_merged: bool,
    review_requested: &HashSet<u64>,
    pr: &PrSnapshot,
    tracked_reason: &str,
    heard_bots: &[String],
    now: Timestamp,
) -> Result<Option<(PrDetail, bool)>> {
    let number = pr.number;
    let Some(detail) = forge
        .fetch_pr_detail(&repo.owner, &repo.name, number, login)
        .await?
    else {
        // The forge has no such PR. Record that so the queue stops advertising
        // something nobody can open, and so the next sync doesn't spend another
        // query rediscovering it.
        ledger.mark_detail_unavailable(repo_id, number, now)?;
        return Ok(None);
    };

    // Classify against what the detail fetch saw, not what the sweep did: both
    // can move between the two, and a PR closed since the sweep must classify
    // as closed rather than as the open one the ledger still holds.
    let mut pr = pr.clone();
    pr.head_sha = detail.head_sha.clone();
    pr.state = detail.state;

    // GitHub owns my review history; the ledger owns done/snooze/mute. Read
    // the local state and overlay only the forge-derived fields.
    let mut mine = ledger.my_state(repo_id, number)?;
    mine.last_reviewed_sha = detail.last_reviewed_sha.clone();
    mine.last_verdict = detail.last_verdict;
    mine.last_action_at = detail.last_action_at;

    // A review requested of me — directly (tier-2) or via a team I'm on (the
    // involvement search) — is the same actionable request.
    let direct_request = detail
        .review_requests
        .iter()
        .find(|request| request.team.is_none())
        .cloned();
    let team_requests = detail
        .review_requests
        .iter()
        .filter(|request| request.team.is_some())
        .collect::<Vec<_>>();
    let review_request = direct_request.or_else(|| {
        (review_requested.contains(&number) && team_requests.len() == 1)
            .then(|| (*team_requests[0]).clone())
    });
    let inferred_review_request = review_requested.contains(&number) && review_request.is_none();

    let interest = interest_detail(tracked_reason);
    let ctx = ClassifyCtx {
        viewer: Some(login),
        bots,
        interest: interest.as_deref(),
        mentions: &detail.mentions,
        said: &detail.said,
        invited: &detail.invited,
        mine: pr.author.eq_ignore_ascii_case(login),
        heard_bots,
        review_request,
        inferred_review_request,
        priority_authors: &priority.authors,
        priority_review_requesters: &priority.requesters,
        new_commits: detail.new_commits,
        include_merged,
        ..Default::default()
    };
    let events: Vec<_> = detail
        .activities
        .iter()
        .map(|event| {
            let mut retained = new_activity(event, now);
            if event
                .actor
                .as_deref()
                .is_some_and(|actor| actor.eq_ignore_ascii_case(login))
            {
                retained.relation = reviewq_core::model::ActivityRelation::Own;
            } else if ctx.mine {
                retained.relation = reviewq_core::model::ActivityRelation::Relevant;
            }
            retained
        })
        .collect();
    let committed = ledger.commit_detail_with_activity(
        repo_id,
        &pr,
        &mine,
        &detail.threads,
        &detail.reviewers,
        &detail.body,
        detail.state_changed_at,
        &events,
        &ctx,
        now,
    )?;
    if let Committed::Superseded { stored } = committed {
        // Somebody stored a newer detail while this one was in flight — a `sync`
        // and the interface's refresh key can be fetching the same PR at once.
        // Theirs is the fresher view of the PR, so this one is dropped rather
        // than winning on commit order.
        tracing::info!(
            number,
            stored = %stored,
            "a newer detail was already stored, so this fetch was dropped"
        );
    }
    let queued = ledger
        .show(repo_id, number)?
        .is_some_and(|show| !show.attention.is_empty());
    Ok(Some((detail, queued)))
}

/// The bare interest rule from a stored `tracked_reason`
/// (`interest: label area:x` → `label area:x`), or `None` for an involvement
/// reason — which never produces `needs-first-look`.
fn interest_detail(tracked_reason: &str) -> Option<String> {
    tracked_reason
        .strip_prefix("interest: ")
        .map(str::to_string)
}

/// Map a configured involvement reason to its GitHub search qualifier.
fn involvement_qualifier(reason: &str, login: &str) -> Option<String> {
    let qualifier = match reason {
        "review_requested" => "review-requested",
        "mention" => "mentions",
        "assign" => "assignee",
        "author" => "author",
        "comment" => "commenter",
        _ => return None,
    };
    Some(format!("{qualifier}:{login}"))
}

/// Per-repo counters accumulated over one sync.
#[derive(Default, Debug, Clone)]
pub struct Stats {
    /// PRs the tier-1 sweep actually pulled down and classified.
    pub swept: usize,
    /// PRs the forge said matched the sweep's search window — larger than
    /// `swept` when the search cap truncated the results.
    pub total_count: u32,
    /// PRs stored for the first time, across every pass.
    pub new: u64,
    /// PRs the sweep tracked because an interest rule matched.
    pub interest: u64,
    /// Distinct PRs an involvement search tracked because they name me.
    pub involved: u64,
    /// PRs that came out of the detail pass holding at least one attention
    /// reason — i.e. that the sync put on the queue.
    pub queued: u64,
    /// PRs the sweep couldn't classify because their file list was truncated,
    /// so a path rule could neither match nor be ruled out.
    pub truncated_unknown: u64,
    /// New forge activity events stored during this sync.
    pub activity_events: u64,
    /// Pull requests whose activity could not be read or stored.
    pub activity_errors: u64,
    /// Request-count budget spent on activity retrieval.
    pub activity_request_cost: u32,
    /// GraphQL points this repo's sync spent.
    pub cost: u32,
    /// Points left in the hourly budget, as of the last response. `None` until
    /// one has arrived.
    ///
    /// Distinct from `Some(0)`, which is the budget genuinely exhausted — the two
    /// shared a `0` before, so the guard that exists to stop before the budget
    /// runs out skipped itself in exactly that case.
    pub remaining: Option<u32>,
}

/// One repo's sync outcome: the counters it accumulated, plus what the ledger
/// holds now that it's done.
#[derive(Debug, Clone)]
pub struct RepoSummary {
    /// The repo's `owner/name`.
    pub repo: String,
    /// What this repo's sync counted.
    pub stats: Stats,
    /// Tracked PRs in the ledger afterwards.
    pub tracked: u64,
    /// PRs stored for this repo in total, tracked or not.
    pub total: u64,
    /// The sweep hit the forge's search cap, so some PRs in the window were
    /// missed.
    pub truncated: bool,
}

/// What a sync reports as it runs, so its caller — not the sync itself —
/// decides where that goes.
///
/// [`run`] writes to neither stdout nor stderr. The CLI implements this over
/// stderr and stdout; a frontend that owns the terminal (a TUI) can implement
/// the same two methods over a channel instead.
pub trait SyncProgress {
    /// A page of a paginated pass landed. `what` names the pass (`updated`, an
    /// involvement reason such as `review_requested`, or `detail`), `fetched`
    /// is the running count and `total` is what the forge said there is.
    fn page(&mut self, what: &str, fetched: usize, total: u32);

    /// One repo finished — called once per repo, after its last page.
    fn repo_finished(&mut self, summary: &RepoSummary);
}

/// The one-line per-repo summary a frontend can print verbatim.
///
/// Lives here rather than in the CLI because it's the canonical rendering of a
/// [`RepoSummary`] — a second frontend showing the same numbers should show
/// them the same way, not reinvent the wording.
///
/// `swept`/`total_count` count what matched the search window; `interest`/
/// `involved` count why PRs are tracked.
pub fn summary_line(summary: &RepoSummary) -> String {
    let s = &summary.stats;
    let mut line = format!(
        "sync {}: swept {} of {} in window, tracked {}/{} (+{} new), \
         {} interest, {} involved, {} on the queue",
        summary.repo,
        s.swept,
        s.total_count,
        summary.tracked,
        summary.total,
        s.new,
        s.interest,
        s.involved,
        s.queued,
    );
    if s.truncated_unknown > 0 {
        line.push_str(&format!(", {} unknown (truncated)", s.truncated_unknown));
    }
    if s.activity_events > 0 || s.activity_errors > 0 || s.activity_request_cost > 0 {
        line.push_str(&format!(
            "; activity {} new, {} failed, {} requests",
            s.activity_events, s.activity_errors, s.activity_request_cost
        ));
    }
    match s.remaining {
        Some(left) => line.push_str(&format!("; {} pts, {left} left", s.cost)),
        None => line.push_str(&format!("; {} pts", s.cost)),
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_time_uses_a_numeric_offset_and_whole_seconds() {
        let ts: Timestamp = "2026-08-05T18:30:00.123456Z".parse().unwrap();
        assert_eq!(search_time(ts), "2026-08-05T18:30:00+00:00");
    }

    #[test]
    fn search_time_handles_no_fractional_part() {
        let ts: Timestamp = "2026-08-05T18:30:00Z".parse().unwrap();
        assert_eq!(search_time(ts), "2026-08-05T18:30:00+00:00");
    }

    fn summary() -> RepoSummary {
        RepoSummary {
            repo: "apache/airflow".into(),
            stats: Stats {
                swept: 12,
                total_count: 12,
                new: 3,
                interest: 5,
                involved: 2,
                queued: 4,
                truncated_unknown: 0,
                activity_events: 0,
                activity_errors: 0,
                activity_request_cost: 0,
                cost: 61,
                remaining: Some(4823),
            },
            tracked: 7,
            total: 90,
            truncated: false,
        }
    }

    #[test]
    fn summary_line_reports_every_counter() {
        assert_eq!(
            summary_line(&summary()),
            "sync apache/airflow: swept 12 of 12 in window, tracked 7/90 (+3 new), \
             5 interest, 2 involved, 4 on the queue; 61 pts, 4823 left"
        );
    }

    #[test]
    fn summary_line_omits_the_budget_before_any_response_reports_one() {
        let mut unreported = summary();
        unreported.stats.remaining = None;
        let line = summary_line(&unreported);
        assert!(line.ends_with("61 pts"), "{line}");
        assert!(!line.contains("left"), "{line}");
    }

    #[test]
    fn an_exhausted_budget_stops_the_detail_pass() {
        // The case the guard exists for. It read `remaining != 0 && remaining <
        // FLOOR`, so nought — the budget actually gone — skipped the check.
        assert!(budget_is_low(Some(0)));
        assert!(budget_is_low(Some(DETAIL_BUDGET_FLOOR - 1)));
    }

    #[test]
    fn a_healthy_budget_and_an_unreported_one_both_let_the_pass_run() {
        assert!(!budget_is_low(Some(DETAIL_BUDGET_FLOOR)));
        assert!(!budget_is_low(Some(5000)));
        assert!(
            !budget_is_low(None),
            "nothing has reported a budget yet, so there is nothing to be low"
        );
    }

    #[test]
    fn summary_line_mentions_unclassifiable_prs_only_when_there_are_some() {
        let mut with_unknown = summary();
        with_unknown.stats.truncated_unknown = 2;
        assert!(
            summary_line(&with_unknown).contains("4 on the queue, 2 unknown (truncated); 61 pts")
        );
        assert!(!summary_line(&summary()).contains("unknown"));
    }

    #[test]
    fn summary_line_reports_activity_successes_and_failures_separately() {
        let mut with_activity = summary();
        with_activity.stats.activity_events = 3;
        with_activity.stats.activity_errors = 2;
        with_activity.stats.activity_request_cost = 4;

        assert_eq!(
            summary_line(&with_activity),
            "sync apache/airflow: swept 12 of 12 in window, tracked 7/90 (+3 new), \
             5 interest, 2 involved, 4 on the queue; activity 3 new, 2 failed, \
             4 requests; 61 pts, 4823 left"
        );
    }

    /// A sink that records what it was told, standing in for the CLI's stderr one
    /// wherever a test drives a sync without printing. What it recorded is
    /// asserted by the engine tests that actually run a sync through it.
    #[derive(Default)]
    pub(super) struct RecordingProgress {
        pub(super) pages: Vec<(String, usize, u32)>,
        pub(super) finished: Vec<String>,
        pub(super) summaries: Vec<RepoSummary>,
    }

    impl SyncProgress for RecordingProgress {
        fn page(&mut self, what: &str, fetched: usize, total: u32) {
            self.pages.push((what.to_string(), fetched, total));
        }

        fn repo_finished(&mut self, summary: &RepoSummary) {
            self.finished.push(summary.repo.clone());
            self.summaries.push(summary.clone());
        }
    }
}

/// The sync engine driven against a forge a test supplies.
///
/// Every pass here takes `&dyn Forge` and an open [`Ledger`], so all of it is
/// reachable without a network: what these cover is the sweep's pagination and
/// cursor, the search cap, the budget floor, and what survives a failure
/// part-way through — none of which the shape of the code was enough to
/// guarantee.
#[cfg(test)]
mod engine_tests {
    use super::tests::RecordingProgress;
    use super::*;
    use crate::fake_forge::{FakeForge, Page, pr, ts};
    use reviewq_core::model::{
        ActivityKind, ActivityPayload, Attention, AttentionReason, MyState, PrState, Verdict,
    };
    use reviewq_forge::{ActivityRateLimit, ForgeActivity, ForgeActivityPage, RateLimitUnit};
    use reviewq_ledger::{ActivityPageCommit, ActivityRateLimitUnit, ActivityScope, RepoKey};

    fn now() -> Timestamp {
        ts("2026-08-11T12:00:00Z")
    }

    /// A config with one repo, one label rule, and no involvement searches — so a
    /// test that only cares about the sweep isn't also scripting those.
    fn config(extra: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [[project]]
            repos = [{{ owner = "apache", name = "airflow" }}]
            [[project.interest]]
            labels = ["area:task-sdk"]
            [involvement]
            reasons = []
            {extra}
            "#
        ))
        .expect("config parses")
    }

    fn repo_key() -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        }
    }

    fn activity(id: &str, occurred_at: &str) -> ForgeActivity {
        ForgeActivity {
            relation: reviewq_core::model::ActivityRelation::Own,
            kind: ActivityKind::Commented,
            occurred_at: ts(occurred_at),
            actor: Some("ashb".into()),
            head_sha: None,
            external_id: Some(id.into()),
            permalink: Some(format!("https://forge.example/comments/{id}")),
            payload: ActivityPayload::None,
        }
    }

    fn activity_page(
        activities: Vec<ForgeActivity>,
        next: Option<&str>,
        remaining: u32,
        next_rate_limit: Option<RateLimitUnit>,
    ) -> ForgeActivityPage {
        ForgeActivityPage {
            activities,
            next: next.map(str::to_string),
            rate_limit: Some(ActivityRateLimit {
                unit: RateLimitUnit::Points,
                cost: 1,
                remaining,
            }),
            next_rate_limit,
        }
    }

    fn track_for_backfill(ledger: &Ledger, repo_id: RepoId, number: u64) {
        ledger
            .upsert_pr(
                repo_id,
                &pr(number, "2026-08-09T09:00:00Z"),
                Some(TrackedReason::Involved("manual".into())),
            )
            .expect("tracked PR");
    }

    fn complete_initial_activity(ledger: &Ledger, repo_id: RepoId, number: u64) {
        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    number,
                    None,
                    &[],
                    None,
                    None,
                    "2026-08-11T10:05:00Z".parse().unwrap()
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
    }

    fn give_attention(ledger: &Ledger, repo_id: RepoId, number: u64) {
        let _ = ledger
            .commit_detail(
                repo_id,
                number,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    priority: false,
                    reason: AttentionReason::ReviewRequested {
                        team: None,
                        requested_by: None,
                    },
                    since: now(),
                }],
                None,
                now(),
            )
            .expect("attention");
    }

    /// Run one repo's whole sync against `forge`, returning the ledger it wrote.
    async fn sync(cfg: &Config, forge: &dyn Forge) -> (Ledger, RepoId, RecordingProgress) {
        synced(cfg, forge, false).await
    }

    /// A sync that also asks for the repo's palette, as `--labels` does.
    async fn synced(
        cfg: &Config,
        forge: &dyn Forge,
        labels: bool,
    ) -> (Ledger, RepoId, RecordingProgress) {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        let mut progress = RecordingProgress::default();
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").expect("rules");
        sync_repo(
            cfg,
            forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            labels,
            false,
            Detail::Stale,
            now(),
            &mut progress,
        )
        .await
        .expect("sync");
        (ledger, repo_id, progress)
    }

    #[tokio::test]
    async fn an_ordinary_sync_does_not_ask_for_the_palette() {
        // A colour changes about never, so this is a query per repo to learn
        // what we already knew. `--labels` is how you ask.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_repo_labels(&[("stale", "e8b955")])
            .with_detail(1, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        assert!(ledger.label_colours(repo_id).expect("colours").is_empty());
    }

    #[tokio::test]
    async fn asking_for_labels_records_every_one_the_repo_defines() {
        // Every label, not only those on PRs this sweep touched: a label's name
        // reaches the ledger whenever the PR carrying it was last swept, which
        // may be long before colours were ever asked for — which is how `stale`
        // came to be the one label drawn in grey.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_repo_labels(&[("area:task-sdk", "0e8a16"), ("stale", "e8b955")])
            .with_detail(1, 4900);

        let (ledger, repo_id, _) = synced(&cfg, &forge, true).await;

        let colours = ledger.label_colours(repo_id).expect("colours");
        assert_eq!(colours["area:task-sdk"], "0e8a16");
        assert_eq!(
            colours["stale"], "e8b955",
            "including one no PR on this page carries"
        );
    }

    #[tokio::test]
    async fn the_sweep_follows_every_page_and_leaves_the_cursor_at_the_newest_seen() {
        let cfg = config("");
        let forge = FakeForge::new(vec![
            Page::of(vec![
                pr(1, "2026-08-09T09:00:00Z"),
                pr(2, "2026-08-09T10:00:00Z"),
            ])
            .then("cursor-1")
            .of_total(4),
            // Deliberately not in ascending order: the watermark is the newest
            // `updatedAt` on the page, not the first row of the last one.
            Page::of(vec![
                pr(3, "2026-08-09T08:00:00Z"),
                pr(4, "2026-08-10T11:00:00Z"),
            ])
            .of_total(4),
        ])
        .with_detail(1, 4900)
        .with_detail(2, 4900)
        .with_detail(3, 4900)
        .with_detail(4, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        let asked = forge.searches();
        assert_eq!(asked.len(), 2, "both pages fetched: {asked:?}");
        assert_eq!(asked[0].1, None, "first page asks with no cursor");
        assert_eq!(
            asked[1].1.as_deref(),
            Some("cursor-1"),
            "the second follows the first's cursor"
        );
        assert_eq!(ledger.list_tracked(repo_id).expect("tracked").len(), 4);
        assert_eq!(
            ledger.get_meta(repo_id, CURSOR_KEY).expect("cursor"),
            Some("2026-08-10T11:00:00Z".to_string()),
            "the watermark is the newest updatedAt swept, not the last page's first row"
        );
    }

    #[tokio::test]
    async fn backfill_activity_resumes_each_pr_from_its_committed_provider_cursor() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        track_for_backfill(&ledger, repo_id, 2);
        let repo = &config("").projects[0].repos[0];
        let interrupted = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("one-a", "2026-08-11T11:00:00Z")],
                    Some("cursor one / next"),
                    DETAIL_BUDGET_FLOOR - 1,
                    Some(RateLimitUnit::Points),
                ),
            )
            .with_activity_page(
                2,
                None,
                activity_page(
                    vec![activity("two-a", "2026-08-11T09:00:00Z")],
                    Some("cursor two / next"),
                    DETAIL_BUDGET_FLOOR - 1,
                    Some(RateLimitUnit::Points),
                ),
            );

        let first = backfill_activity(
            &interrupted,
            &ledger,
            repo_id,
            repo,
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("budget stop is resumable");
        assert!(first.stopped_for_budget);
        let mut asked = interrupted.activities_asked();
        asked.sort();
        assert_eq!(asked, vec![(1, None), (2, None)]);
        assert_eq!(
            ledger
                .activity_backfill(repo_id, 1)
                .expect("progress")
                .expect("started")
                .cursor
                .as_deref(),
            Some("cursor one / next")
        );

        let resumed = FakeForge::new(vec![])
            .with_activity_page(
                1,
                Some("cursor one / next"),
                activity_page(
                    vec![activity("one-b", "2026-08-11T10:00:00Z")],
                    None,
                    4900,
                    None,
                ),
            )
            .with_activity_page(
                2,
                Some("cursor two / next"),
                activity_page(
                    vec![activity("two-b", "2026-08-11T08:00:00Z")],
                    None,
                    4899,
                    None,
                ),
            );
        let second = backfill_activity(
            &resumed,
            &ledger,
            repo_id,
            repo,
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("resumed backfill");

        assert!(!second.stopped_for_budget);
        assert_eq!(second.points_remaining, Some(4899));
        assert_eq!(second.requests_remaining, Some(5000));
        let mut asked = resumed.activities_asked();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                (1, Some("cursor one / next".into())),
                (2, Some("cursor two / next".into())),
            ]
        );
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .expect("activity")
                .events
                .len(),
            4
        );
        assert!(
            ledger
                .activity_backfill(repo_id, 1)
                .unwrap()
                .unwrap()
                .completed_at
                .is_some()
        );
        assert!(
            ledger
                .activity_backfill(repo_id, 2)
                .unwrap()
                .unwrap()
                .completed_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn activity_backfill_fetches_different_prs_concurrently() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        for number in 1..=9 {
            track_for_backfill(&ledger, repo_id, number);
        }
        let forge = FakeForge::new(vec![]);

        backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("backfill");

        assert_eq!(forge.max_activity_in_flight(), ACTIVITY_CONCURRENCY);
    }

    #[tokio::test]
    async fn activity_backfill_commits_fast_results_without_waiting_for_a_slow_peer() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        for number in 1..=9 {
            track_for_backfill(&ledger, repo_id, number);
        }
        let forge = FakeForge::new(vec![]).delaying_activity_page(
            1,
            None,
            std::time::Duration::from_millis(100),
        );
        let mut progress = RecordingProgress::default();
        let future = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            now(),
            &mut progress,
        );

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), future)
                .await
                .is_err()
        );
        assert!(
            (2..=9).any(|number| {
                ledger
                    .activity_backfill(repo_id, number)
                    .expect("progress")
                    .is_some_and(|state| state.completed_at.is_some())
            }),
            "fast results should be committed while a slow request remains in flight"
        );
    }

    #[tokio::test]
    async fn backfill_activity_keeps_other_pr_progress_and_reports_malformed_pages() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        track_for_backfill(&ledger, repo_id, 2);
        let repo = &config("").projects[0].repos[0];
        let mut malformed = activity("missing-id", "2026-08-11T11:00:00Z");
        malformed.external_id = None;
        let forge = FakeForge::new(vec![])
            .with_activity_page(1, None, activity_page(vec![malformed], None, 4900, None))
            .with_activity_page(
                2,
                None,
                activity_page(
                    vec![activity("safe", "2026-08-11T10:00:00Z")],
                    None,
                    4899,
                    None,
                ),
            );

        let error = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect_err("malformed page is reported after safe work");

        assert!(error.to_string().contains("#1"), "{error:#}");
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .iter()
                .filter_map(|event| event.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["safe"]
        );
        assert!(ledger.activity_backfill(repo_id, 1).unwrap().is_none());
        assert!(
            ledger
                .activity_backfill(repo_id, 2)
                .unwrap()
                .unwrap()
                .completed_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn backfill_activity_drains_free_cursor_pages_below_the_rate_floor() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        let repo = &config("").projects[0].repos[0];
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("buffered-a", "2026-08-11T11:00:00Z")],
                    Some("buffered cursor"),
                    0,
                    None,
                ),
            )
            .with_activity_page(
                1,
                Some("buffered cursor"),
                ForgeActivityPage {
                    activities: vec![activity("buffered-b", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: None,
                    next_rate_limit: None,
                },
            );

        let stats = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("buffered cursor does not spend budget");

        assert!(!stats.stopped_for_budget);
        assert_eq!(forge.activities_asked().len(), 2);
        assert_eq!(stats.events, 2);
    }

    #[tokio::test]
    async fn backfill_activity_checks_the_budget_pool_named_for_the_next_request() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        let repo = &config("").projects[0].repos[0];
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("graphql", "2026-08-11T11:00:00Z")],
                    Some("rest cursor"),
                    0,
                    Some(RateLimitUnit::Requests),
                ),
            )
            .with_activity_page(
                1,
                Some("rest cursor"),
                ForgeActivityPage {
                    activities: vec![activity("rest", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            );

        let stats = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("the request pool remains healthy");

        assert!(!stats.stopped_for_budget);
        assert_eq!(forge.activities_asked().len(), 2);
        assert_eq!(stats.points_remaining, Some(0));
        assert_eq!(stats.requests_remaining, Some(4999));
    }

    #[tokio::test]
    async fn fresh_backfill_uses_the_provider_declared_initial_request_pool() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        let forge = FakeForge::new(vec![])
            .with_initial_activity_rate_limit(RateLimitUnit::Requests)
            .failing_point_budget()
            .with_activity_page(
                1,
                None,
                ForgeActivityPage {
                    activities: vec![activity("request-first", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            );

        let stats = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("the healthy request pool is enough for a fresh traversal");

        assert_eq!(stats.events, 1);
        assert_eq!(stats.request_cost, 1);
        assert_eq!(forge.activities_asked(), vec![(1, None)]);
    }

    #[tokio::test]
    async fn exact_pr_backfill_does_not_fetch_other_tracked_pull_requests() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        track_for_backfill(&ledger, repo_id, 2);
        let forge = FakeForge::new(vec![]).with_activity_page(
            2,
            None,
            activity_page(
                vec![activity("two", "2026-08-11T10:00:00Z")],
                None,
                4900,
                None,
            ),
        );

        let stats = backfill_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            2,
            None,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();

        assert_eq!(stats.prs, 1);
        assert_eq!(stats.events, 1);
        assert_eq!(forge.activities_asked(), vec![(2, None)]);
        assert!(ledger.activity_backfill(repo_id, 1).unwrap().is_none());
    }

    #[tokio::test]
    async fn exact_pr_backfill_resumes_its_saved_provider_cursor() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 7);
        ledger
            .commit_activity_page(
                repo_id,
                7,
                None,
                &[],
                Some("saved cursor"),
                Some(ActivityRateLimitUnit::Requests),
                now(),
            )
            .unwrap();
        let forge = FakeForge::new(vec![]).with_activity_page(
            7,
            Some("saved cursor"),
            ForgeActivityPage {
                activities: vec![activity("resumed", "2026-08-11T10:00:00Z")],
                next: None,
                rate_limit: Some(ActivityRateLimit {
                    unit: RateLimitUnit::Requests,
                    cost: 1,
                    remaining: 4999,
                }),
                next_rate_limit: None,
            },
        );

        let stats = backfill_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            7,
            None,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();

        assert_eq!(stats.events, 1);
        assert_eq!(stats.request_cost, 1);
        assert_eq!(
            forge.activities_asked(),
            vec![(7, Some("saved cursor".into()))]
        );
    }

    #[tokio::test]
    async fn exact_pr_backfill_uses_the_provider_declared_rate_pool() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 9);
        let forge = FakeForge::new(vec![])
            .with_initial_activity_rate_limit(RateLimitUnit::Requests)
            .failing_point_budget()
            .with_activity_page(
                9,
                None,
                ForgeActivityPage {
                    activities: vec![],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            );

        let stats = backfill_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            9,
            None,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();

        assert_eq!(stats.prs, 1);
        assert_eq!(stats.request_cost, 1);
    }

    #[tokio::test]
    async fn exact_pr_backfill_failure_keeps_the_committed_page_and_resume_cursor() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 11);
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                11,
                None,
                activity_page(
                    vec![activity("kept", "2026-08-11T10:00:00Z")],
                    Some("resume here"),
                    4900,
                    Some(RateLimitUnit::Points),
                ),
            )
            .failing_activity_page(11, Some("resume here"));

        let error = backfill_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            11,
            None,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap_err();
        let failure = error.downcast_ref::<BackfillFailed>().unwrap();

        assert_eq!(failure.stats.events, 1);
        assert_eq!(failure.stats.pages, 1);
        assert_eq!(failure.failures.len(), 1);
        assert_eq!(
            ledger
                .activity_backfill(repo_id, 11)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("resume here")
        );
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn newly_tracked_activity_failure_is_returned_without_undoing_tracking_or_progress() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 13);
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                13,
                None,
                activity_page(
                    vec![activity("kept", "2026-08-11T10:00:00Z")],
                    Some("resume after tracking"),
                    4900,
                    Some(RateLimitUnit::Points),
                ),
            )
            .failing_activity_page(13, Some("resume after tracking"));

        let activity = backfill_newly_tracked_activity(
            &actions::Tracked::Fetched,
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            13,
            now(),
        )
        .await
        .expect("a newly tracked PR schedules activity");

        assert_eq!(activity.stats.events, 1);
        assert_eq!(activity.stats.pages, 1);
        assert!(
            activity
                .error
                .as_deref()
                .is_some_and(|error| error.contains("fetching activity for #13"))
        );
        assert_eq!(ledger.list_tracked(repo_id).unwrap().len(), 1);
        assert_eq!(
            ledger
                .activity_backfill(repo_id, 13)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("resume after tracking")
        );
    }

    #[tokio::test]
    async fn already_tracked_pr_does_not_schedule_an_automatic_backfill() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 14);
        let forge = FakeForge::new(vec![]);

        let activity = backfill_newly_tracked_activity(
            &actions::Tracked::Already,
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            14,
            now(),
        )
        .await;

        assert!(activity.is_none());
        assert!(forge.activities_asked().is_empty());
    }

    #[tokio::test]
    async fn backfill_low_point_pool_still_runs_later_request_and_free_cursors() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        for number in [1, 2, 3] {
            track_for_backfill(&ledger, repo_id, number);
        }
        ledger
            .commit_activity_page(
                repo_id,
                2,
                None,
                &[],
                Some("request cursor"),
                Some(ActivityRateLimitUnit::Requests),
                now(),
            )
            .unwrap();
        ledger
            .commit_activity_page(repo_id, 3, None, &[], Some("free cursor"), None, now())
            .unwrap();
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("point", "2026-08-11T11:00:00Z")],
                    Some("low point cursor"),
                    DETAIL_BUDGET_FLOOR - 1,
                    Some(RateLimitUnit::Points),
                ),
            )
            .with_activity_page(
                2,
                Some("request cursor"),
                ForgeActivityPage {
                    activities: vec![activity("request", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            )
            .with_activity_page(
                3,
                Some("free cursor"),
                ForgeActivityPage {
                    activities: vec![activity("free", "2026-08-11T09:00:00Z")],
                    next: None,
                    rate_limit: None,
                    next_rate_limit: None,
                },
            );

        let stats = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();

        assert!(stats.stopped_for_budget);
        assert_eq!(stats.events, 3);
        let mut asked = forge.activities_asked();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                (1, None),
                (2, Some("request cursor".into())),
                (3, Some("free cursor".into())),
            ]
        );
    }

    #[tokio::test]
    async fn backfill_failed_point_budget_still_runs_saved_request_cursors() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        for number in [1, 2] {
            track_for_backfill(&ledger, repo_id, number);
        }
        ledger
            .commit_activity_page(
                repo_id,
                2,
                None,
                &[],
                Some("request cursor"),
                Some(ActivityRateLimitUnit::Requests),
                now(),
            )
            .unwrap();
        let forge = FakeForge::new(vec![])
            .failing_point_budget()
            .with_activity_page(
                2,
                Some("request cursor"),
                ForgeActivityPage {
                    activities: vec![activity("request", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            );

        let error = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap_err();
        let failure = error.downcast_ref::<BackfillFailed>().unwrap();

        assert_eq!(failure.stats.events, 1);
        assert_eq!(failure.failures.len(), 1);
        assert_eq!(failure.failures[0].number, 1);
        assert_eq!(
            forge.activities_asked(),
            vec![(2, Some("request cursor".into()))]
        );
    }

    #[test]
    fn a_detail_inserted_event_does_not_end_history_fetching() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        let known = activity("detail-event", "2026-08-11T11:00:00Z");
        ledger
            .record_forge_activity(repo_id, 1, &new_activity(&known, now()))
            .unwrap();
        let missing = activity("missed-between-syncs", "2026-08-11T10:00:00Z");
        let page =
            incremental_page_events(&ledger, repo_id, 1, "ashb", &[known, missing], None, now())
                .unwrap();
        assert!(!page.reached_boundary);
        assert_eq!(page.events.len(), 1);
        assert_eq!(
            page.events[0].external_id.as_deref(),
            Some("missed-between-syncs")
        );
    }

    #[test]
    fn contextual_history_is_retained_without_hiding_the_page_boundary() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        let mut unrelated = activity("someone-else", "2026-08-11T11:00:00Z");
        unrelated.actor = Some("other".into());
        let mut review = unrelated.clone();
        review.kind = ActivityKind::ReviewSubmitted;
        let mut merge = unrelated.clone();
        merge.kind = ActivityKind::PrMerged;
        let mut old = activity("older", "2026-08-10T11:00:00Z");
        old.actor = Some("other".into());
        let boundary = "2026-08-11T00:00:00Z".parse().unwrap();
        let page = incremental_page_events(
            &ledger,
            repo_id,
            1,
            "ashb",
            &[unrelated, review, merge, old],
            Some(boundary),
            now(),
        )
        .unwrap();
        assert_eq!(page.events.len(), 3);
        assert!(
            page.events
                .iter()
                .all(|event| event.relation == reviewq_core::model::ActivityRelation::Context)
        );
        assert!(page.reached_boundary);
        let mut own = activity("own-merge", "2026-08-11T11:00:00Z");
        own.kind = ActivityKind::PrMerged;
        assert_eq!(
            activity_relation(&ledger, repo_id, 1, "ashb", &own, None, false).unwrap(),
            reviewq_core::model::ActivityRelation::Own
        );
    }

    #[tokio::test]
    async fn ordinary_sync_stops_incremental_activity_at_the_saved_coverage_boundary() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        let repo = &cfg.projects[0].repos[0];
        let initial = FakeForge::new(vec![]).with_activity_page(
            1,
            None,
            activity_page(
                vec![activity("known", "2026-08-11T10:00:00Z")],
                None,
                4900,
                None,
            ),
        );
        backfill_activity(
            &initial,
            &ledger,
            repo_id,
            repo,
            "ashb",
            "2026-08-11T10:05:00Z".parse().unwrap(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();
        give_attention(&ledger, repo_id, 1);

        let forge = FakeForge::new(vec![]).with_activity_page(
            1,
            None,
            activity_page(
                vec![
                    activity("new", "2026-08-11T11:00:00Z"),
                    activity("known", "2026-08-11T10:00:00Z"),
                    activity("older-unseen", "2026-08-11T09:00:00Z"),
                ],
                Some("must not be fetched"),
                4899,
                Some(RateLimitUnit::Points),
            ),
        );
        sync_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            Some(4900),
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert_eq!(forge.activities_asked(), vec![(1, None)]);
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .iter()
                .filter_map(|event| event.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["new", "known"]
        );
    }

    #[tokio::test]
    async fn a_review_clearing_attention_still_refreshes_activity() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        complete_initial_activity(&ledger, repo_id, 1);
        give_attention(&ledger, repo_id, 1);
        let review = ForgeActivity {
            relation: reviewq_core::model::ActivityRelation::Own,
            kind: ActivityKind::ReviewSubmitted,
            head_sha: Some("sha1".into()),
            payload: ActivityPayload::ReviewSubmitted {
                result: reviewq_core::model::ReviewResult::Commented,
                reviewed_sha: Some("sha1".into()),
            },
            ..activity("review", "2026-08-11T12:01:00Z")
        };
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-11T12:01:00Z")])])
            .with_detail(1, 4900)
            .with_current_head_review(1)
            .with_activity_page(1, None, activity_page(vec![review], None, 4899, None));
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").unwrap();

        sync_repo(
            &cfg,
            &forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            false,
            false,
            Detail::Every,
            ts("2026-08-11T12:02:00Z"),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();

        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        assert!(
            ledger
                .has_forge_activity(repo_id, ActivityKind::ReviewSubmitted, "review")
                .unwrap()
        );
    }

    #[tokio::test]
    async fn activity_refresh_survives_restart_after_detail_clears_attention() {
        let cfg = config("");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = Ledger::open(&path).unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        complete_initial_activity(&ledger, repo_id, 1);
        give_attention(&ledger, repo_id, 1);
        let repo = &cfg.projects[0].repos[0];
        let forge = FakeForge::new(vec![])
            .with_detail(1, 4900)
            .with_current_head_review(1);
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        refresh_one(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            &[],
            &Priority::default(),
            false,
            &HashSet::new(),
            &show.pr,
            show.tracked_reason.as_deref().unwrap(),
            &[],
            now(),
        )
        .await
        .unwrap();
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let forge = FakeForge::new(vec![]).with_activity_page(
            1,
            None,
            activity_page(
                vec![activity("after-restart", "2026-08-11T11:00:00Z")],
                None,
                4900,
                None,
            ),
        );

        sync_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            Some(0),
            now(),
            &mut Stats {
                remaining: Some(0),
                ..Stats::default()
            },
            &mut RecordingProgress::default(),
        )
        .await;
        assert!(forge.activities_asked().is_empty());
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        sync_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            Some(4900),
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert!(
            ledger
                .has_forge_activity(repo_id, ActivityKind::Commented, "after-restart")
                .unwrap()
        );
        assert!(
            ledger
                .activity_refresh_candidates(repo_id)
                .unwrap()
                .is_empty()
        );
        sync_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            Some(4900),
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;
        assert_eq!(forge.activities_asked(), [(1, None)]);
    }

    #[tokio::test]
    async fn equal_timestamp_activity_survives_a_page_failure_and_resume() {
        let cfg = config("");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = Ledger::open(&path).unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        complete_initial_activity(&ledger, repo_id, 1);
        give_attention(&ledger, repo_id, 1);
        let known = activity("z-known", "2026-08-11T10:00:00Z");
        ledger
            .record_forge_activity(repo_id, 1, &new_activity(&known, now()))
            .unwrap();
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![known, activity("b-same-page", "2026-08-11T10:00:00Z")],
                    Some("same timestamp"),
                    4900,
                    Some(RateLimitUnit::Points),
                ),
            )
            .failing_activity_page(1, Some("same timestamp"));
        let repo = &cfg.projects[0].repos[0];
        sync_activity(
            &forge,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            None,
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;
        assert!(
            ledger
                .has_forge_activity(repo_id, ActivityKind::Commented, "b-same-page")
                .unwrap()
        );
        ledger.untrack(repo_id, 1, now()).unwrap();
        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());
        drop(ledger);
        let ledger = Ledger::open(&path).unwrap();
        let resumed = FakeForge::new(vec![]).with_activity_page(
            1,
            Some("same timestamp"),
            activity_page(
                vec![
                    activity("a-next-page", "2026-08-11T10:00:00Z"),
                    activity("older", "2026-08-11T09:00:00Z"),
                ],
                Some("must not fetch"),
                4899,
                Some(RateLimitUnit::Points),
            ),
        );

        sync_activity(
            &resumed,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            None,
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert!(
            ledger
                .has_forge_activity(repo_id, ActivityKind::Commented, "a-next-page")
                .unwrap()
        );
        assert!(
            !ledger
                .has_forge_activity(repo_id, ActivityKind::Commented, "older")
                .unwrap()
        );
        assert_eq!(
            resumed.activities_asked(),
            [(1, Some("same timestamp".into()))]
        );
    }

    #[tokio::test]
    async fn targeted_activity_sync_fetches_only_the_selected_pr() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 1);
        track_for_backfill(&ledger, repo_id, 2);
        let forge = FakeForge::new(vec![])
            .failing_point_budget()
            .with_activity_page(
                2,
                None,
                activity_page(
                    vec![activity("selected", "2026-08-11T11:00:00Z")],
                    None,
                    4899,
                    None,
                ),
            );

        sync_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            2,
            Some(4900),
            now(),
            &mut QuietProgress,
        )
        .await;

        assert_eq!(forge.activities_asked(), vec![(2, None)]);
        assert!(ledger.activity_backfill(repo_id, 1).unwrap().is_none());
        assert!(
            ledger
                .activity_backfill(repo_id, 2)
                .unwrap()
                .is_some_and(|state| state.completed_at.is_some())
        );
        assert_eq!(
            ledger
                .activity_page(ActivityScope::Pr { repo_id, number: 2 }, None, 10)
                .unwrap()
                .events
                .iter()
                .map(|event| event.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("selected")]
        );
    }

    #[tokio::test]
    async fn targeted_incremental_activity_reuses_the_detail_budget() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        track_for_backfill(&ledger, repo_id, 2);
        complete_initial_activity(&ledger, repo_id, 2);
        let forge = FakeForge::new(vec![])
            .failing_point_budget()
            .with_activity_page(
                2,
                None,
                activity_page(
                    vec![activity("incremental", "2026-08-11T11:00:00Z")],
                    None,
                    4899,
                    None,
                ),
            );

        sync_activity_for_pr(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            2,
            Some(4900),
            now(),
            &mut QuietProgress,
        )
        .await;

        assert_eq!(forge.activities_asked(), vec![(2, None)]);
        assert_eq!(
            ledger
                .activity_page(ActivityScope::Pr { repo_id, number: 2 }, None, 10)
                .unwrap()
                .events
                .iter()
                .map(|event| event.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("incremental")]
        );
    }

    #[tokio::test]
    async fn incremental_activity_fetches_different_prs_concurrently() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        for number in [1, 2] {
            track_for_backfill(&ledger, repo_id, number);
            complete_initial_activity(&ledger, repo_id, number);
            give_attention(&ledger, repo_id, number);
        }
        let forge = FakeForge::new(vec![]);

        sync_activity(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            None,
            Some(4900),
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert!(forge.max_activity_in_flight() > 1);
    }

    #[tokio::test]
    async fn ordinary_sync_only_refreshes_completed_history_for_attention_prs() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        for number in [1, 2] {
            track_for_backfill(&ledger, repo_id, number);
            complete_initial_activity(&ledger, repo_id, number);
        }
        give_attention(&ledger, repo_id, 2);
        let forge = FakeForge::new(vec![]);

        sync_activity(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            None,
            Some(4900),
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert_eq!(forge.activities_asked(), vec![(2, None)]);
    }

    #[tokio::test]
    async fn incremental_activity_resumes_after_a_committed_page_then_provider_failure() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        complete_initial_activity(&ledger, repo_id, 1);
        give_attention(&ledger, repo_id, 1);
        let repo = &cfg.projects[0].repos[0];
        let interrupted = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("incremental-a", "2026-08-11T11:00:00Z")],
                    Some("incremental cursor"),
                    4900,
                    Some(RateLimitUnit::Points),
                ),
            )
            .failing_activity_page(1, Some("incremental cursor"));
        let mut first_stats = Stats::default();
        let mut first_progress = RecordingProgress::default();

        sync_activity(
            &interrupted,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            None,
            now(),
            &mut first_stats,
            &mut first_progress,
        )
        .await;

        assert_eq!(first_stats.activity_events, 1);
        assert_eq!(first_stats.activity_errors, 1);
        assert_eq!(first_progress.pages, [("activity".into(), 1, 1)]);
        assert_eq!(
            ledger
                .activity_incremental(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("incremental cursor")
        );

        let resumed = FakeForge::new(vec![]).with_activity_page(
            1,
            Some("incremental cursor"),
            activity_page(
                vec![activity("incremental-b", "2026-08-11T10:00:00Z")],
                None,
                4899,
                None,
            ),
        );
        sync_activity(
            &resumed,
            &ledger,
            repo_id,
            repo,
            "ashb",
            None,
            None,
            now(),
            &mut Stats::default(),
            &mut RecordingProgress::default(),
        )
        .await;

        assert_eq!(
            resumed.activities_asked(),
            vec![(1, Some("incremental cursor".into()))]
        );
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .iter()
                .filter_map(|event| event.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec!["incremental-a", "incremental-b"]
        );
        assert_eq!(
            ledger
                .activity_incremental(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor,
            None
        );
    }

    #[tokio::test]
    async fn incremental_budget_stop_keeps_the_cursor_and_runs_later_healthy_pools() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        for number in [1, 2, 3] {
            track_for_backfill(&ledger, repo_id, number);
            complete_initial_activity(&ledger, repo_id, number);
            give_attention(&ledger, repo_id, number);
        }
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    2,
                    &ledger
                        .begin_incremental_activity(repo_id, 2, now())
                        .unwrap(),
                    &[],
                    Some("request cursor"),
                    Some(ActivityRateLimitUnit::Requests)
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        assert_eq!(
            ledger
                .commit_incremental_activity_page(
                    repo_id,
                    3,
                    &ledger
                        .begin_incremental_activity(repo_id, 3, now())
                        .unwrap(),
                    &[],
                    Some("free incremental cursor"),
                    None
                )
                .unwrap(),
            ActivityPageCommit::Applied { inserted: 0 }
        );
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("point-event", "2026-08-11T11:00:00Z")],
                    Some("low-point cursor"),
                    DETAIL_BUDGET_FLOOR - 1,
                    Some(RateLimitUnit::Points),
                ),
            )
            .with_activity_page(
                2,
                Some("request cursor"),
                ForgeActivityPage {
                    activities: vec![activity("request-event", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: Some(ActivityRateLimit {
                        unit: RateLimitUnit::Requests,
                        cost: 1,
                        remaining: 4999,
                    }),
                    next_rate_limit: None,
                },
            )
            .with_activity_page(
                3,
                Some("free incremental cursor"),
                ForgeActivityPage {
                    activities: vec![activity("free-event", "2026-08-11T10:00:00Z")],
                    next: None,
                    rate_limit: None,
                    next_rate_limit: None,
                },
            );
        let mut stats = Stats::default();

        sync_activity(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            None,
            None,
            now(),
            &mut stats,
            &mut RecordingProgress::default(),
        )
        .await;

        let mut asked = forge.activities_asked();
        asked.sort();
        assert_eq!(
            asked,
            vec![
                (1, None),
                (2, Some("request cursor".into())),
                (3, Some("free incremental cursor".into())),
            ]
        );
        assert_eq!(stats.activity_events, 3);
        assert_eq!(stats.activity_request_cost, 1);
        assert_eq!(
            ledger
                .activity_incremental(repo_id, 1)
                .unwrap()
                .unwrap()
                .cursor
                .as_deref(),
            Some("low-point cursor")
        );
    }

    #[tokio::test]
    async fn backfill_error_retains_partial_success_statistics() {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        track_for_backfill(&ledger, repo_id, 2);
        let mut malformed = activity("missing-id", "2026-08-11T10:00:00Z");
        malformed.external_id = None;
        let forge = FakeForge::new(vec![])
            .with_activity_page(
                1,
                None,
                activity_page(
                    vec![activity("stored", "2026-08-11T11:00:00Z")],
                    None,
                    4900,
                    None,
                ),
            )
            .with_activity_page(2, None, activity_page(vec![malformed], None, 4899, None));

        let error = backfill_activity(
            &forge,
            &ledger,
            repo_id,
            &config("").projects[0].repos[0],
            "ashb",
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap_err();
        let failure = error
            .downcast_ref::<BackfillFailed>()
            .expect("typed activity failure");

        assert_eq!(failure.stats.events, 1);
        assert_eq!(failure.stats.pages, 1);
        assert_eq!(failure.stats.prs, 1);
        assert_eq!(failure.failures.len(), 1);
        assert_eq!(failure.failures[0].number, 2);
    }

    #[tokio::test]
    async fn activity_failure_is_reported_without_blocking_detail_commit() {
        let cfg = config("");
        let mut malformed = activity("missing-id", "2026-08-11T11:00:00Z");
        malformed.external_id = None;
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-11T11:00:00Z")])])
            .with_review_request(1, 4900)
            .with_activity_page(1, None, activity_page(vec![malformed], None, 4899, None));

        let (ledger, repo_id, progress) = sync(&cfg, &forge).await;

        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);
        assert_eq!(progress.summaries[0].stats.activity_errors, 1);
        assert!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .iter()
                .all(|event| event.kind == ActivityKind::AttentionChanged)
        );
        assert_eq!(
            progress
                .pages
                .into_iter()
                .filter(|(what, _, _)| what == "activity")
                .collect::<Vec<_>>(),
            [("activity".into(), 1, 1)]
        );
    }

    #[tokio::test]
    async fn a_window_with_more_matches_than_it_served_is_recorded_as_truncated() {
        // The forge caps a search at 1000 results however many match. A window
        // that blew past it means PRs were silently missed, which `doctor`
        // reports and which must not read as a clean sync.
        let cfg = config("");
        let forge = FakeForge::new(vec![
            Page::of(vec![pr(1, "2026-08-09T09:00:00Z")]).of_total(reviewq_forge::SEARCH_CAP + 5),
        ])
        .with_detail(1, 4900);

        let (ledger, repo_id, progress) = sync(&cfg, &forge).await;

        assert_eq!(
            ledger.get_meta(repo_id, TRUNCATED_KEY).expect("flag"),
            Some("1".to_string())
        );
        assert!(progress.finished.contains(&"apache/airflow".to_string()));
    }

    #[tokio::test]
    async fn a_window_it_served_whole_clears_a_previous_truncation() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        ledger
            .set_meta(repo_id, TRUNCATED_KEY, "1")
            .expect("stale flag");

        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_detail(1, 4900);
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").expect("rules");
        sync_repo(
            &cfg,
            &forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            false,
            false,
            Detail::Stale,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect("sync");

        assert_eq!(
            ledger.get_meta(repo_id, TRUNCATED_KEY).expect("flag"),
            Some("0".to_string()),
            "a full window must not leave yesterday's truncation standing"
        );
    }

    #[tokio::test]
    async fn the_detail_pass_stops_before_spending_the_budget_and_keeps_what_it_did() {
        // Each PR commits on its own, and the sweep watermark is already stored,
        // so stopping short costs nothing but a second run.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![
            pr(1, "2026-08-09T09:00:00Z"),
            pr(2, "2026-08-09T10:00:00Z"),
            pr(3, "2026-08-09T11:00:00Z"),
        ])])
        // The first detail comes back reporting the budget *gone*, so the pass
        // must stop rather than fetch the other two. Nought rather than merely
        // low: that is the value the guard used to skip itself on.
        .with_review_request(1, 0)
        .with_detail(2, 4900)
        .with_detail(3, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        assert_eq!(
            forge.details_asked(),
            vec![1],
            "it stopped after the response that reported the low budget"
        );
        assert_eq!(
            ledger.queue(repo_id).expect("queue").len(),
            1,
            "what it did finish is committed"
        );
        assert_eq!(
            ledger
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .expect("pending")
                .len(),
            2,
            "and the rest are still due, so the next sync finishes them"
        );
    }

    #[tokio::test]
    async fn a_pr_the_forge_no_longer_has_is_recorded_and_not_retried() {
        // A deleted PR used to abort the whole sync. It has to be remembered as
        // unavailable, or every later sync would ask again and fail again.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![
            pr(1, "2026-08-09T09:00:00Z"),
            pr(2, "2026-08-09T10:00:00Z"),
        ])])
        // #1 has no detail scripted at all, which is the forge saying it's gone.
        .with_review_request(2, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        assert_eq!(
            forge.details_asked(),
            vec![1, 2],
            "the sync carried on past it"
        );
        assert_eq!(
            ledger.queue(repo_id).expect("queue").len(),
            1,
            "only the PR that still exists is on the queue"
        );
        let pending: Vec<u64> = ledger
            .prs_needing_detail(repo_id, false, Detail::Stale)
            .expect("pending")
            .iter()
            .map(|t| t.pr.number)
            .collect();
        assert!(
            !pending.contains(&1),
            "a PR known to be gone must not be asked for again, was {pending:?}"
        );
    }

    #[tokio::test]
    async fn a_detail_fetch_that_fails_stops_the_sync_and_keeps_the_pages_it_committed() {
        // Unlike a deleted PR, a forge error is not something to absorb: it could
        // be a token or an outage, and carrying on would write a queue computed
        // from half the data.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![
            pr(1, "2026-08-09T09:00:00Z"),
            pr(2, "2026-08-09T10:00:00Z"),
        ])])
        .with_review_request(1, 4900)
        .failing_detail(2);

        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").expect("rules");
        let err = sync_repo(
            &cfg,
            &forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            false,
            false,
            Detail::Stale,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect_err("the forge failed");

        assert!(err.to_string().contains("detail for #2"), "{err:#}");
        assert_eq!(
            ledger.list_tracked(repo_id).expect("tracked").len(),
            2,
            "the sweep page committed before the failure stays committed"
        );
        assert_eq!(
            ledger.get_meta(repo_id, CURSOR_KEY).expect("cursor"),
            Some("2026-08-09T10:00:00Z".to_string()),
            "so does the cursor, which is what makes the next run resume"
        );
        assert_eq!(
            ledger.queue(repo_id).expect("queue").len(),
            1,
            "the PR whose detail did land is on the queue"
        );
    }

    #[tokio::test]
    async fn an_involvement_search_tracks_a_pr_no_interest_rule_matched() {
        // The rules here match `area:task-sdk`; this PR carries no labels, so
        // only being asked to review it puts it in the ledger.
        let cfg = config("");
        let mut unmatched = pr(7, "2026-08-09T09:00:00Z");
        unmatched.labels.clear();
        let forge = FakeForge::new(vec![Page::of(vec![unmatched])]).with_review_request(7, 4900);

        // Ask for the involvement pass this time.
        let cfg_involved = Config {
            involvement: crate::config::Involvement {
                reasons: vec!["review_requested".into()],
            },
            ..cfg
        };
        let (ledger, repo_id, progress) = sync(&cfg_involved, &forge).await;

        let searches = forge.searches();
        assert!(
            searches
                .iter()
                .any(|(query, _)| query.contains("review-requested:ashb")),
            "the involvement search ran: {searches:?}"
        );
        assert_eq!(
            ledger.list_tracked(repo_id).expect("tracked").len(),
            1,
            "tracked by involvement, not by a rule"
        );
        assert!(
            progress
                .pages
                .iter()
                .any(|(what, _, _)| what == "review_requested"),
            "and reported under its own name: {:?}",
            progress.pages
        );
    }

    #[tokio::test]
    async fn a_full_sync_surfaces_a_live_request_after_reviewing_the_head() {
        let cfg = Config {
            involvement: crate::config::Involvement {
                reasons: vec!["review_requested".into()],
            },
            ..config("")
        };
        let forge = FakeForge::new(vec![Page::of(vec![pr(7, "2026-08-09T09:00:00Z")])])
            .with_review_request(7, 4900)
            .with_current_head_review(7);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        let queue = ledger.queue(repo_id).expect("queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].top.reason.discriminant(), "review_requested");
    }

    #[tokio::test]
    async fn detail_sync_clears_an_old_resolution_without_waiting_for_history() {
        let cfg = config("");
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo_key()).unwrap();
        track_for_backfill(&ledger, repo_id, 1);
        let thread = reviewq_core::model::ThreadState {
            thread_id: "resolved".into(),
            i_own: true,
            is_resolved: true,
            resolved_by: Some("author".into()),
            last_comment_author: Some("ashb".into()),
            last_comment_at: Some(now()),
            my_last_comment_at: Some(now()),
        };
        let first = FakeForge::new(vec![])
            .with_detail(1, 4900)
            .with_detail_activity(1, vec![thread.clone()], vec![]);
        let snapshot = ledger.show(repo_id, 1).unwrap().unwrap().pr;
        let result = refresh_one(
            &first,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            &[],
            &Priority::default(),
            false,
            &HashSet::new(),
            &snapshot,
            "manual",
            &[],
            now(),
        )
        .await
        .unwrap();
        assert!(matches!(result, Some((_, true))));
        let review = ForgeActivity {
            relation: reviewq_core::model::ActivityRelation::Own,
            kind: ActivityKind::ReviewSubmitted,
            payload: ActivityPayload::ReviewSubmitted {
                result: reviewq_core::model::ReviewResult::Commented,
                reviewed_sha: Some("sha1".into()),
            },
            ..activity("my-review", "2026-08-12T10:00:00Z")
        };
        let second = FakeForge::new(vec![])
            .with_detail(1, 4900)
            .with_current_head_review(1)
            .with_detail_activity(1, vec![thread], vec![review]);
        let result = refresh_one(
            &second,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            &[],
            &Priority::default(),
            false,
            &HashSet::new(),
            &snapshot,
            "manual",
            &[],
            "2026-08-12T11:00:00Z".parse().unwrap(),
        )
        .await
        .unwrap();
        assert!(matches!(result, Some((_, false))));
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert_eq!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .len(),
            4
        );
        assert!(first.activities_asked().is_empty());
        assert!(second.activities_asked().is_empty());
    }

    #[tokio::test]
    async fn refreshing_one_pr_surfaces_a_live_request_after_reviewing_the_head() {
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![pr(7, "2026-08-09T09:00:00Z")])])
            .with_review_request(7, 4900)
            .with_current_head_review(7);
        let (ledger, repo_id, _) = sync(&cfg, &forge).await;
        ledger.clear_attention(repo_id, 7).expect("clear attention");

        let show = ledger.show(repo_id, 7).expect("show").expect("tracked PR");
        let refreshed = refresh_one(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            &[],
            &Priority::default(),
            false,
            &HashSet::new(),
            &show.pr,
            show.tracked_reason.as_deref().unwrap_or(""),
            &[],
            now(),
        )
        .await
        .expect("refresh");

        assert!(matches!(refreshed, Some((_, true))));
        let queue = ledger.queue(repo_id).expect("queue");
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].top.reason.discriminant(), "review_requested");
    }

    #[tokio::test]
    async fn a_pr_whose_files_were_truncated_is_counted_rather_than_guessed_at() {
        // A path rule can neither match nor be ruled out against a partial file
        // list, so the PR is left untracked and counted — not silently dropped.
        let cfg = config("");
        let mut partial = pr(9, "2026-08-09T09:00:00Z");
        partial.labels.clear();
        partial.files = Some(vec!["something.py".into()]);
        partial.files_truncated = true;

        let with_paths: Config = toml::from_str(
            r#"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            paths = ["task-sdk/**"]
            [involvement]
            reasons = []
            "#,
        )
        .expect("config parses");
        let forge = FakeForge::new(vec![Page::of(vec![partial])]);

        let (ledger, repo_id, progress) = sync(&with_paths, &forge).await;

        assert!(
            ledger.list_tracked(repo_id).expect("tracked").is_empty(),
            "unknown is not a match"
        );
        assert_eq!(
            ledger.count_truncated_untracked(repo_id).expect("counted"),
            1
        );
        assert!(
            progress.finished.contains(&"apache/airflow".to_string()),
            "the repo still finished"
        );
        let _ = &cfg;
    }

    #[tokio::test]
    async fn a_rule_asking_for_post_merge_review_keeps_its_own_prs_after_they_merge() {
        // The targeted opt-in: this project does not set `include_merged`, so
        // everything else still leaves the queue at merge — but the rule that
        // tracked this PR asked to keep it, and a review request on it survives.
        let cfg: Config = toml::from_str(
            r#"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            labels = ["area:task-sdk"]
            after_merge = true
            [involvement]
            reasons = []
            "#,
        )
        .expect("config parses");
        let mut merged = pr(4, "2026-08-09T09:00:00Z");
        merged.state = PrState::Merged;
        let forge = FakeForge::new(vec![Page::of(vec![merged])]).with_review_request(4, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        assert!(!cfg.projects[0].include_merged, "no blunt opt-in here");
        let queue = ledger.queue(repo_id).expect("queue");
        assert_eq!(queue.len(), 1, "the rule kept it");
        assert_eq!(queue[0].pr.number, 4);
    }

    #[tokio::test]
    async fn being_asked_to_review_a_pr_does_not_cost_it_its_post_merge_rule() {
        // The involvement search runs after the sweep and outranks it, so its
        // reason is the one displayed. It evaluates no rules, though, so it must
        // not be what decides the PR stops mattering once it merged.
        let cfg: Config = toml::from_str(
            r#"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            labels = ["area:task-sdk"]
            after_merge = true
            [involvement]
            reasons = ["review_requested"]
            "#,
        )
        .expect("config parses");
        let mut merged = pr(4, "2026-08-09T09:00:00Z");
        merged.state = PrState::Merged;
        let forge = FakeForge::new(vec![Page::of(vec![merged])]).with_review_request(4, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        let queue = ledger.queue(repo_id).expect("queue");
        assert_eq!(queue.len(), 1, "the rule still keeps it");
        assert_eq!(queue[0].tracked_reason, "involved: review_requested");
    }

    #[tokio::test]
    async fn refreshing_one_pr_learns_it_was_closed_and_stops_calling_it_waiting() {
        // The sweep is what usually notices a PR closing, and a refresh never
        // sweeps — so before the detail fetch carried the state, pressing `r`
        // on a PR closed on the forge left it stored as open, and it went on
        // being listed as waiting on somebody forever.
        let cfg = config("");
        let forge = FakeForge::new(vec![Page::of(vec![pr(7, "2026-08-09T09:00:00Z")])])
            .with_detail(7, 4900);
        let (ledger, repo_id, _) = sync(&cfg, &forge).await;
        // Reviewed and answered, so it wants nothing: tracked, open, waiting on
        // the author — which is where the PR this was reported against sat.
        ledger.clear_attention(repo_id, 7).expect("reviewed");
        ledger
            .set_done(repo_id, 7, "head", ts("2026-08-10T00:00:00Z"))
            .unwrap();
        assert_eq!(
            ledger
                .waiting(repo_id)
                .expect("waiting")
                .iter()
                .map(|tracked| tracked.pr.number)
                .collect::<Vec<_>>(),
            vec![7],
        );

        // Somebody closes it, and the only thing asked of the forge is this
        // one PR's detail.
        let forge = FakeForge::new(vec![])
            .with_detail(7, 4800)
            .with_detail_transition(7, PrState::Closed, Some(ts("2026-08-11T11:45:00Z")));
        let show = ledger.show(repo_id, 7).expect("show").expect("stored");
        let outcome = refresh_one(
            &forge,
            &ledger,
            repo_id,
            &cfg.projects[0].repos[0],
            "ashb",
            &cfg.bots.logins,
            &Priority::default(),
            false,
            &HashSet::new(),
            &show.pr,
            show.tracked_reason.as_deref().unwrap_or(""),
            &[],
            now(),
        )
        .await
        .expect("refresh");

        assert!(outcome.is_some(), "the PR still exists, it is just closed");
        assert_eq!(
            ledger
                .show(repo_id, 7)
                .expect("show")
                .expect("stored")
                .pr
                .state,
            PrState::Closed,
            "and the state the fetch found is what the ledger now holds"
        );
        assert!(
            ledger.waiting(repo_id).expect("waiting").is_empty(),
            "nobody is waiting on a closed PR"
        );
        assert!(ledger.queue(repo_id).expect("queue").is_empty());
        let show = ledger.show(repo_id, 7).unwrap().unwrap();
        assert_eq!(show.pr.state_changed_at, Some(ts("2026-08-11T11:45:00Z")));
        assert_eq!(
            ledger
                .activity_page(ActivityScope::Pr { repo_id, number: 7 }, None, 10)
                .unwrap()
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec![ActivityKind::AttentionChanged, ActivityKind::PrClosed]
        );
    }

    #[tokio::test]
    async fn a_merged_pr_loses_the_attention_it_was_holding() {
        let cfg = config("");
        let mut merged = pr(4, "2026-08-09T09:00:00Z");
        merged.state = PrState::Merged;
        let forge = FakeForge::new(vec![Page::of(vec![merged])]).with_review_request(4, 4900);

        let (ledger, repo_id, _) = sync(&cfg, &forge).await;

        assert!(
            ledger.queue(repo_id).expect("queue").is_empty(),
            "a merged PR is archived out of the queue unless the project opts in"
        );
        let _ = Verdict::Approved;
    }
    #[tokio::test]
    async fn team_priority_reranks_unchanged_prs_and_removed_members_without_detail_fetches() {
        let mut cfg = config("");
        cfg.projects[0].repos[0].priority_authors = vec!["apache/airflow-committers".into()];
        let mut other = pr(2, "2026-08-09T09:00:00Z");
        other.author = "other".into();
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z"), other])])
            .with_detail(1, 4900)
            .with_detail(2, 4900)
            .with_team("apache", "airflow-committers", &["other"]);
        let (ledger, repo_id, _) = sync(&cfg, &forge).await;
        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(
            queue.iter().map(|item| item.pr.number).collect::<Vec<_>>(),
            [2, 1]
        );
        assert_eq!(queue[0].top.priority(), 2);
        assert_eq!(queue[1].top.priority(), 8);
        let before = ledger
            .activity_page(ActivityScope::PrAll { repo_id, number: 2 }, None, 100)
            .unwrap();
        let updated = FakeForge::new(vec![Page::of(vec![])]).with_team(
            "apache",
            "airflow-committers",
            &["potiuk"],
        );
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").unwrap();
        sync_repo(
            &cfg,
            &updated,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            false,
            true,
            Detail::Stale,
            now() + jiff::SignedDuration::from_hours(1),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();
        assert!(updated.details_asked().is_empty());
        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(
            queue.iter().map(|item| item.pr.number).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(queue[0].top.priority(), 2);
        assert_eq!(queue[1].top.priority(), 8);
        assert_eq!(
            ledger.show(repo_id, 2).unwrap().unwrap().attention[0].priority(),
            8
        );
        let after = ledger
            .activity_page(ActivityScope::PrAll { repo_id, number: 2 }, None, 100)
            .unwrap();
        assert_eq!(before.events.len(), after.events.len());
        cfg.projects[0].repos[0].priority_authors.clear();
        let project = &cfg.projects[0];
        sync_repo(
            &cfg,
            &updated,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            "ashb",
            false,
            false,
            Detail::Stale,
            now() + jiff::SignedDuration::from_hours(2),
            &mut RecordingProgress::default(),
        )
        .await
        .unwrap();
        assert!(
            ledger
                .queue_all()
                .unwrap()
                .iter()
                .all(|item| item.item.top.priority() == 8)
        );
    }

    #[tokio::test]
    async fn team_membership_prioritizes_the_requester_independently_of_the_author() {
        let mut cfg = config("");
        cfg.projects[0].repos[0].priority_review_requesters =
            vec!["apache/airflow-committers".into()];
        let forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_review_request(1, 4900)
            .with_requester(1, "kaxil")
            .with_team("apache", "airflow-committers", &["kaxil"]);
        let (ledger, repo_id, _) = sync(&cfg, &forge).await;
        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue[0].top.priority(), 2);
        assert_eq!(queue[0].top.reason.discriminant(), "review_requested");
    }
    #[tokio::test]
    async fn repository_policies_are_isolated_while_team_membership_is_shared() {
        let mut cfg = config("");
        let mut authors = cfg.projects[0].repos[0].clone();
        authors.priority_authors = vec!["apache/airflow-committers".into()];
        let mut requesters = cfg.projects[0].repos[0].clone();
        requesters.name = "other".into();
        requesters.priority_review_requesters = vec!["apache/airflow-committers".into()];
        let mut unconfigured = cfg.projects[0].repos[0].clone();
        unconfigured.owner = "acme".into();
        unconfigured.host = "github.acme.example".into();
        cfg.projects[0].repos = vec![authors, requesters, unconfigured];
        let project = &cfg.projects[0];
        let rules = cfg.interest_for_login(project, "ashb").unwrap();
        let ledger = Ledger::open_in_memory().unwrap();
        let team_forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_review_request(1, 4900)
            .with_requester(1, "kaxil")
            .with_team("apache", "airflow-committers", &["potiuk", "kaxil"]);
        let other_forge = FakeForge::new(vec![Page::of(vec![pr(1, "2026-08-09T09:00:00Z")])])
            .with_review_request(1, 4900)
            .with_requester(1, "kaxil");
        for (index, expected_author_band, expected_request_band) in
            [(0, 2, 2), (1, 8, 2), (2, 8, 7)]
        {
            let repo = &project.repos[index];
            let repo_id = ledger.ensure_repo(&repo.key()).unwrap();
            let forge = if index == 0 {
                &team_forge
            } else {
                &other_forge
            };
            sync_repo(
                &cfg,
                forge,
                &ledger,
                repo_id,
                project,
                repo,
                &rules,
                "ashb",
                false,
                true,
                Detail::Stale,
                now(),
                &mut RecordingProgress::default(),
            )
            .await
            .unwrap();
            let attention = ledger.show(repo_id, 1).unwrap().unwrap().attention;
            assert_eq!(
                attention
                    .iter()
                    .find(|a| a.reason.discriminant() == "needs_first_look")
                    .unwrap()
                    .priority(),
                expected_author_band
            );
            assert_eq!(
                attention
                    .iter()
                    .find(|a| a.reason.discriminant() == "review_requested")
                    .unwrap()
                    .priority(),
                expected_request_band
            );
        }
        assert_eq!(team_forge.team_calls(), 1);
        assert_eq!(other_forge.team_calls(), 0);
        assert!(
            ledger
                .team_members("github.acme.example", "apache", "airflow-committers")
                .unwrap()
                .is_none()
        );
    }
}
