//! `reviewq sync`: fetch updates and rebuild the ledger.
//!
//! The sweep fetches every PR updated since the cursor, each with its changed
//! files, and classifies it against the interest rules; then a handful of
//! involvement searches (`review-requested:me`, `mentions:me`, ...) mark the
//! PRs that name me. Everything is an idempotent upsert, so a re-sync over an
//! overlapping window is a near-no-op.

use std::path::Path;
use std::process::ExitCode;

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use jiff::{Timestamp, ToSpan};
use reviewq_core::rules::Evaluation;
use reviewq_forge::{Forge, build, resolve_token};
use reviewq_ledger::{Ledger, TrackedReason};

use crate::config::{Config, RepoRef};
use crate::{config, paths};

/// Cursor: the high-water mark of `updatedAt` we have swept up to.
const CURSOR_KEY: &str = "last_sync_at";
/// Whether the most recent sweep hit the search cap; surfaced by `doctor`.
const TRUNCATED_KEY: &str = "last_sweep_truncated";

pub async fn run(config_path: Option<&Path>, logging: bool) -> Result<ExitCode> {
    let loaded = config::load(config_path)?;
    let cfg = &loaded.config;
    let (project, repo) = cfg.sole_repo()?;
    let host = cfg.forge_host_for(repo)?;
    let token = resolve_token(&host)?;
    let forge = build(&host, &token.value)?;
    let ledger = Ledger::open(&paths::database_file()?)?;

    let now = Timestamp::now();
    let since = sweep_since(&ledger, cfg, now)?;
    let rules = cfg.interest_for(project)?;

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

    // In-place progress only when stderr is an unshared terminal; with logs
    // interleaved (`-v`) or piped, print a line per page instead.
    let in_place = std::io::stderr().is_terminal() && !logging;
    let mut progress = progress_reporter(in_place);
    let mut stats = Stats::default();
    let mut after: Option<String> = None;

    loop {
        let page = forge
            .search_prs_page(&query, cfg.sync.page_size, after.as_deref())
            .await?;
        stats.total_count = page.total_count;
        stats.cost += page.cost;
        stats.remaining = page.remaining;

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
            stats.new +=
                ledger.commit_sweep_page(&batch, now, CURSOR_KEY, &watermark.to_string())?;
        }
        progress("updated", stats.swept, stats.total_count);

        match page.next {
            Some(cursor) => after = Some(cursor),
            None => break,
        }
    }

    let truncated = stats.total_count > stats.swept as u32;
    ledger.set_meta(TRUNCATED_KEY, if truncated { "1" } else { "0" })?;

    involvement_search(
        forge.as_ref(),
        &ledger,
        repo,
        &cfg.identity.login,
        cfg.involving_reasons(project),
        cfg.sync.page_size,
        now,
        &mut stats,
        &mut progress,
    )
    .await?;

    finish_progress(in_place);
    let (tracked, total) = ledger.counts()?;
    print_summary(&stats, tracked, total);
    if truncated {
        tracing::warn!(
            total = stats.total_count,
            cap = reviewq_forge::SEARCH_CAP,
            "sweep hit the search cap; some PRs in this window were missed \
             (narrow sync.bootstrap_days or sync more often)"
        );
    }

    Ok(ExitCode::SUCCESS)
}

/// The lower bound for this sweep: the stored cursor minus an overlap buffer,
/// or a bootstrap window on the first-ever run.
fn sweep_since(ledger: &Ledger, cfg: &Config, now: Timestamp) -> Result<Timestamp> {
    match ledger.get_meta(CURSOR_KEY)? {
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
/// relationship — and mark them `involved:`.
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
    repo: &RepoRef,
    login: &str,
    reasons: &[String],
    page_size: u32,
    now: Timestamp,
    stats: &mut Stats,
    progress: &mut impl FnMut(&str, usize, u32),
) -> Result<()> {
    let mut involved = std::collections::HashSet::new();

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
            stats.remaining = page.remaining;
            for pr in &page.prs {
                if ledger.upsert_pr(pr, Some(TrackedReason::Involved(reason.clone())), now)? {
                    stats.new += 1;
                }
                involved.insert(pr.number);
            }
            fetched += page.prs.len();
            progress(reason, fetched, page.total_count);
            match page.next {
                Some(cursor) => after = Some(cursor),
                None => break,
            }
        }
    }

    stats.involved = involved.len() as u64;
    Ok(())
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

#[derive(Default)]
struct Stats {
    swept: usize,
    total_count: u32,
    new: u64,
    interest: u64,
    involved: u64,
    truncated_unknown: u64,
    cost: u32,
    remaining: u32,
}

/// A progress sink for the paginated searches, on stderr so stdout (the
/// summary) stays clean. `what` names the search (`updated`, `review-requested`,
/// ...) so it's clear which class of PRs is streaming. When `in_place`, it
/// rewrites one line with `\r`; otherwise it prints a line per page (so it
/// doesn't collide with interleaved log lines, which have their own newlines).
fn progress_reporter(in_place: bool) -> impl FnMut(&str, usize, u32) {
    move |what: &str, fetched: usize, total: u32| {
        let msg = format!("{what}: {fetched}/{total} PRs");
        let mut err = std::io::stderr().lock();
        if in_place {
            // \x1b[K clears the rest of the line after the (possibly shorter) update.
            let _ = write!(err, "\r  {msg}\x1b[K");
        } else {
            let _ = writeln!(err, "  {msg}");
        }
        let _ = err.flush();
    }
}

/// Close the in-place progress line once paginated work is done.
fn finish_progress(in_place: bool) {
    if in_place {
        let _ = writeln!(std::io::stderr());
    }
}

fn print_summary(stats: &Stats, tracked: u64, total: u64) {
    // `swept`/`total_count` count what matched the search window; `interest`/
    // `involved` count why PRs are tracked.
    let mut line = format!(
        "sync: swept {} of {} in window, tracked {tracked}/{total} (+{} new), \
         {} interest, {} involved",
        stats.swept, stats.total_count, stats.new, stats.interest, stats.involved
    );
    if stats.truncated_unknown > 0 {
        line.push_str(&format!(
            ", {} unknown (truncated)",
            stats.truncated_unknown
        ));
    }
    line.push_str(&format!("; {} pts, {} left", stats.cost, stats.remaining));
    println!("{line}");
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
}
