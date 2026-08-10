//! `reviewq sync`: fetch updates and rebuild the ledger.
//!
//! The sweep fetches every PR updated since the cursor, each with its changed
//! files, and classifies it against the interest rules; then a handful of
//! involvement searches (`review-requested:me`, `mentions:me`, ...) mark the
//! PRs that name me. Everything is an idempotent upsert, so a re-sync over an
//! overlapping window is a near-no-op.

use std::collections::HashSet;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, ToSpan};
use reviewq_core::model::{ClassifyCtx, PrSnapshot, classify};
use reviewq_core::rules::{Evaluation, Interest};
use reviewq_forge::{Forge, PrDetail};
use reviewq_ledger::{Committed, Ledger, TrackedReason};

use crate::config::{Config, Project, RepoRef};
use crate::{actions, paths};

/// Cursor: the high-water mark of `updatedAt` we have swept up to.
pub const CURSOR_KEY: &str = "last_sync_at";
/// Whether the most recent sweep hit the search cap; surfaced by `doctor`.
pub const TRUNCATED_KEY: &str = "last_sweep_truncated";

/// Sync every repo in every configured project, reporting through `progress`.
///
/// Repos are synced one at a time, each against its own forge connection, all
/// writing through one ledger handle. A failure on any repo aborts the run —
/// but everything committed before it stays committed, and the cursor means the
/// next sync resumes rather than starts over.
pub async fn run(cfg: &Config, progress: &mut dyn SyncProgress) -> Result<ExitCode> {
    let now = Timestamp::now();
    // One handle for the whole run: every configured repo's sync writes
    // through it, each scoped by its own `repo_id`.
    let ledger = Ledger::open(&paths::database_file()?)?;

    for project in &cfg.projects {
        let rules = cfg.interest_for(project)?;
        for repo in &project.repos {
            let repo_id = ledger.ensure_repo(&repo.key())?;
            // Built here rather than inside the per-repo sync, so that sync is
            // reachable with a forge a test supplies.
            let forge = cfg.forge_for(&repo.host)?;
            sync_repo(
                cfg,
                forge.as_ref(),
                &ledger,
                repo_id,
                project,
                repo,
                &rules,
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
    repo_id: i64,
    project: &Project,
    repo: &RepoRef,
    rules: &Interest,
    now: Timestamp,
    progress: &mut dyn SyncProgress,
) -> Result<()> {
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
                Evaluation::Match(detail) => {
                    stats.interest += 1;
                    Some(TrackedReason::Interest(detail))
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
            stats.new += ledger.commit_sweep_page(
                repo_id,
                &batch,
                now,
                CURSOR_KEY,
                &watermark.to_string(),
            )?;
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
        &cfg.identity.login,
        cfg.involving_reasons(project),
        cfg.sync.page_size,
        now,
        &mut stats,
        progress,
    )
    .await?;

    detail_pass(
        forge,
        ledger,
        repo_id,
        repo,
        &cfg.identity.login,
        &cfg.bots.logins,
        project.include_merged,
        &review_requested,
        now,
        &mut stats,
        progress,
    )
    .await?;

    // Merged/closed PRs are not re-fetched, so drop any attention they still
    // carry (unless this project keeps merged PRs on the queue).
    ledger.clear_archived_attention(repo_id, project.include_merged)?;

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
fn sweep_since(ledger: &Ledger, repo_id: i64, cfg: &Config, now: Timestamp) -> Result<Timestamp> {
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
    repo_id: i64,
    repo: &RepoRef,
    login: &str,
    reasons: &[String],
    page_size: u32,
    now: Timestamp,
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
                if ledger.upsert_pr(
                    repo_id,
                    pr,
                    Some(TrackedReason::Involved(reason.clone())),
                    now,
                )? {
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

/// Stop the detail pass when the GraphQL budget falls below this. The pass is
/// resumable (each PR commits independently and the sync watermark is already
/// advanced), so stopping short just means the next `sync` finishes the rest —
/// far better than running the budget to zero and erroring out.
const DETAIL_BUDGET_FLOOR: u32 = 100;

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
    repo_id: i64,
    repo: &RepoRef,
    login: &str,
    bots: &[String],
    include_merged: bool,
    review_requested: &HashSet<u64>,
    now: Timestamp,
    stats: &mut Stats,
    progress: &mut dyn SyncProgress,
) -> Result<()> {
    let pending = ledger.prs_needing_detail(repo_id, include_merged)?;
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
            include_merged,
            review_requested,
            &tracked.pr,
            &tracked.tracked_reason,
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

/// Refresh one PR by number, resolving from the ledger and config whatever
/// [`refresh_one`] needs.
///
/// The entry point for wanting one PR up to date without a whole sync: the
/// `sync <number>` command, the TUI's sync key, and `review`'s refresh after a
/// handoff all come through here rather than repeating the resolution dance.
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

    let repo = cfg
        .repos()
        .find(|r| r.key() == key)
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
        .repo_id(&key)?
        .context("the number resolved to this repo a moment ago")?;
    let Some(show) = ledger.show(repo_id, number)? else {
        return Ok(Refreshed::Untracked);
    };

    let forge = cfg.forge_for(&repo.host)?;
    let outcome = refresh_one(
        forge.as_ref(),
        &ledger,
        repo_id,
        &repo,
        &cfg.identity.login,
        &cfg.bots.logins,
        project.include_merged,
        // No involvement search has run, so a review requested of a *team* isn't
        // known here. A full sync is what resolves those; this only refreshes
        // what one PR's own detail can say.
        &HashSet::new(),
        &show.pr,
        show.tracked_reason.as_deref().unwrap_or(""),
        Timestamp::now(),
    )
    .await?;

    Ok(match outcome {
        None => Refreshed::Gone,
        Some((detail, queued)) => Refreshed::Updated {
            repo: repo.slug(),
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
pub async fn track_one(
    cfg: &Config,
    repo: Option<&RepoRef>,
    number: u64,
) -> Result<(actions::Tracked, Refreshed)> {
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
    let tracked = actions::track(
        &ledger,
        repo_id,
        &repo,
        number,
        forge.as_ref(),
        Timestamp::now(),
    )
    .await?;

    // A freshly-stored PR holds no attention until something classifies it, so
    // the detail pass is what actually puts it on the queue.
    let refreshed = sync_one(cfg, number).await?;
    Ok((tracked, refreshed))
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
pub async fn refresh_one(
    forge: &dyn Forge,
    ledger: &Ledger,
    repo_id: i64,
    repo: &RepoRef,
    login: &str,
    bots: &[String],
    include_merged: bool,
    review_requested: &HashSet<u64>,
    pr: &PrSnapshot,
    tracked_reason: &str,
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

    // Classify against the head the detail fetch saw, not the sweep's: the
    // head can move between the two, and re-review keys on it.
    let mut pr = pr.clone();
    pr.head_sha = detail.head_sha.clone();

    // GitHub owns my review history; the ledger owns done/snooze/mute. Read
    // the local state and overlay only the forge-derived fields.
    let mut mine = ledger.my_state(repo_id, number)?;
    mine.last_reviewed_sha = detail.last_reviewed_sha.clone();
    mine.last_verdict = detail.last_verdict;
    mine.last_action_at = detail.last_action_at;

    // A review requested of me — directly (tier-2) or via a team I'm on (the
    // involvement search) — is the same actionable request.
    let review_request = detail
        .review_request
        .clone()
        .or_else(|| review_requested.contains(&number).then(Default::default));

    let interest = interest_detail(tracked_reason);
    let ctx = ClassifyCtx {
        bots,
        interest: interest.as_deref(),
        mentions: &detail.mentions,
        review_request,
        new_commits: detail.new_commits,
        include_merged,
    };
    let attention = classify(&pr, &mine, &detail.threads, now, &ctx);
    let queued = !attention.is_empty();
    let committed = ledger.commit_detail(
        repo_id,
        number,
        &mine,
        &detail.threads,
        &detail.reviewers,
        &attention,
        Some(&detail.body),
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
        // Report what the winning detail concluded, not what this one did.
        let queued = ledger
            .show(repo_id, number)?
            .is_some_and(|show| !show.attention.is_empty());
        return Ok(Some((detail, queued)));
    }
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

    /// A sink that records what it was told, standing in for the CLI's stderr one
    /// wherever a test drives a sync without printing. What it recorded is
    /// asserted by the engine tests that actually run a sync through it.
    #[derive(Default)]
    pub(super) struct RecordingProgress {
        pub(super) pages: Vec<(String, usize, u32)>,
        pub(super) finished: Vec<String>,
    }

    impl SyncProgress for RecordingProgress {
        fn page(&mut self, what: &str, fetched: usize, total: u32) {
            self.pages.push((what.to_string(), fetched, total));
        }

        fn repo_finished(&mut self, summary: &RepoSummary) {
            self.finished.push(summary.repo.clone());
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
    use reviewq_core::model::{PrState, Verdict};
    use reviewq_ledger::RepoKey;

    fn now() -> Timestamp {
        ts("2026-08-11T12:00:00Z")
    }

    /// A config with one repo, one label rule, and no involvement searches — so a
    /// test that only cares about the sweep isn't also scripting those.
    fn config(extra: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [identity]
            login = "ashb"
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

    /// Run one repo's whole sync against `forge`, returning the ledger it wrote.
    async fn sync(cfg: &Config, forge: &dyn Forge) -> (Ledger, i64, RecordingProgress) {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo_key()).expect("repo");
        let mut progress = RecordingProgress::default();
        let project = &cfg.projects[0];
        let rules = cfg.interest_for(project).expect("rules");
        sync_repo(
            cfg,
            forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            now(),
            &mut progress,
        )
        .await
        .expect("sync");
        (ledger, repo_id, progress)
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
        let rules = cfg.interest_for(project).expect("rules");
        sync_repo(
            &cfg,
            &forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
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
                .prs_needing_detail(repo_id, false)
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
            .prs_needing_detail(repo_id, false)
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
        let rules = cfg.interest_for(project).expect("rules");
        let err = sync_repo(
            &cfg,
            &forge,
            &ledger,
            repo_id,
            project,
            &project.repos[0],
            &rules,
            now(),
            &mut RecordingProgress::default(),
        )
        .await
        .expect_err("the forge failed");

        assert!(err.to_string().contains("fell over"), "{err:#}");
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
            [identity]
            login = "ashb"
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
}
