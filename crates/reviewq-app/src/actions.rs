//! The things you can do to a PR: done, snooze, mute, defer, track.
//!
//! Each writes the PR's `my_state` row and adjusts what it holds on the queue.
//! They live here rather than in a frontend because two frontends perform them,
//! and a `done` that cleared different reasons depending on where it was pressed
//! would be a difference nobody asked for.
//!
//! Every one takes an open [`Ledger`] and a `repo_id` rather than resolving its
//! own. A frontend showing a queue already has both, and opening a second
//! connection per action would mean a TUI acting on a different database than the
//! one it is displaying — including, in a test, the real one on disk.
//!
//! Nothing here touches the network. `done`'s other half — marking the PR's
//! GitHub notifications read — is [`mark_notifications_read`], deliberately
//! separate: it is best-effort and unbounded, so a caller runs it behind the
//! local record rather than in front of it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, Unit};
use reviewq_forge::Forge;
use reviewq_ledger::Ledger;

use crate::config::{self, RepoRef};

/// Mark a PR handled at `head_sha`, and clear the reasons `done` is allowed to.
///
/// Not `review_requested`: only submitting a review, or the request being
/// withdrawn, clears that one — so a `done` on a PR you were asked to review
/// leaves it asking, which is the point.
pub fn done(ledger: &Ledger, repo_id: i64, number: u64, head_sha: &str) -> Result<()> {
    ledger.set_done(repo_id, number, head_sha, Timestamp::now())?;
    ledger.clear_done_attention(repo_id, number)?;
    Ok(())
}

/// Tell GitHub the PR's notifications have been read.
///
/// Best-effort and separate from [`done`]: config, a token or the network being
/// unavailable must not stop the local record, and must not delay it either.
/// Callers log a failure rather than surfacing it.
pub async fn mark_notifications_read(
    config_path: Option<&Path>,
    repo: &crate::config::RepoRef,
    number: u64,
) -> Result<()> {
    let loaded = config::load(config_path)?;
    let forge = loaded.config.forge_for(&repo.host)?;
    forge
        .mark_pr_notifications_read(&repo.owner, &repo.name, number)
        .await
}

/// Suppress everything on a PR until `until`, mentions included.
///
/// Takes an instant rather than a duration, so a caller can choose one however
/// suits it — typed as `3d`, or picked from presets — without this knowing about
/// either. [`snooze_until`] turns the CLI's syntax into one.
pub fn snooze(ledger: &Ledger, repo_id: i64, number: u64, until: Timestamp) -> Result<Timestamp> {
    ledger.set_snoozed_until(repo_id, number, until)?;
    ledger.clear_attention(repo_id, number)?;
    Ok(until.round(Unit::Second).unwrap_or(until))
}

/// Set or clear a PR's mute.
///
/// Muting clears what it currently holds. Unmuting does not put it back, because
/// the reasons are recomputed by the next sync rather than remembered.
pub fn set_muted(ledger: &Ledger, repo_id: i64, number: u64, muted: bool) -> Result<()> {
    ledger.set_muted(repo_id, number, muted)?;
    if muted {
        ledger.clear_attention(repo_id, number)?;
    }
    Ok(())
}

/// Set or clear a PR's defer, which sinks it to the bottom of the queue without
/// hiding it. It clears itself once something new happens on the PR.
pub fn set_deferred(ledger: &Ledger, repo_id: i64, number: u64, deferred: bool) -> Result<()> {
    let at = deferred.then(Timestamp::now);
    ledger.set_deferred_at(repo_id, number, at)?;
    Ok(())
}

/// What tracking a PR did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    /// It was already tracked; nothing to do.
    Already,
    /// It was stored but untracked — a sweep had seen it and no rule matched.
    /// Now force-tracked, with no network needed.
    Marked,
    /// The ledger had never seen it, so it was fetched from the forge and
    /// stored. Its detail still needs a pass before it can hold attention.
    Fetched,
}

/// Track a PR, fetching it first if the ledger has never seen one.
///
/// The plain ledger flag only reaches a PR some sweep already stored, which
/// leaves a real gap: a PR outside your sweep window, or in a repo you watch
/// narrowly, can't be tracked at all. So an unknown number is fetched from the
/// forge and inserted — the same snapshot a sweep would have produced.
///
/// It arrives with no attention: what a PR wants comes from the detail pass, so
/// the caller runs one (via [`sync_one`](crate::sync::sync_one)) to put it on the
/// queue.
pub async fn track(
    ledger: &Ledger,
    repo_id: i64,
    repo: &RepoRef,
    number: u64,
    forge: &dyn Forge,
    now: Timestamp,
) -> Result<Tracked> {
    if ledger.show(repo_id, number)?.is_some() {
        return Ok(if ledger.track(repo_id, number)? {
            Tracked::Marked
        } else {
            Tracked::Already
        });
    }

    let snapshot = forge
        .fetch_pr(&repo.owner, &repo.name, number)
        .await?
        .with_context(|| format!("{}/{} has no pull request #{number}", repo.owner, repo.name))?;
    ledger.upsert_pr(
        repo_id,
        &snapshot,
        Some(reviewq_ledger::TrackedReason::Involved("manual".into())),
        now,
    )?;
    Ok(Tracked::Fetched)
}

/// Turn a friendly duration (`3d`, `12h`, `1w2d`) into the instant it reaches
/// past `now`.
///
/// Rejects a non-positive span: `snooze 0s` asks to suppress a PR until a moment
/// already gone, which would silently do nothing.
pub fn snooze_until(now: Timestamp, duration: &str) -> Result<Timestamp> {
    let span: jiff::Span = duration
        .parse()
        .with_context(|| format!("invalid duration {duration:?} (try `3d`, `12h`, `1w2d`)"))?;
    let until = now
        .to_zoned(jiff::tz::TimeZone::UTC)
        .checked_add(span)
        .with_context(|| format!("duration {duration:?} out of range"))?
        .timestamp();
    if until <= now {
        bail!("duration {duration:?} must be positive");
    }
    Ok(until)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reviewq_core::model::{Attention, AttentionReason, MyState, PrSnapshot, PrState};
    use reviewq_ledger::{RepoKey, TrackedReason};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn pr(number: u64) -> PrSnapshot {
        PrSnapshot {
            number,
            title: format!("PR {number}"),
            author: "potiuk".into(),
            author_association: "MEMBER".into(),
            head_sha: "abc1234".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-11T09:00:00Z"),
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    /// A ledger holding one queued PR, and the repo it belongs to.
    ///
    /// A real ledger rather than a fake: these functions exist to pin what the
    /// writes do to the queue, which only the real thing can answer.
    fn queued(reason: AttentionReason) -> (Ledger, i64, u64) {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
            .expect("repo");
        let now = ts("2026-08-11T12:00:00Z");
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Interest("label x".into())),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason,
                    since: ts("2026-08-11T09:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail");
        (ledger, repo_id, 1)
    }

    fn mention() -> AttentionReason {
        AttentionReason::Mention { by: "kaxil".into() }
    }

    #[test]
    fn done_records_the_head_and_takes_it_off_the_queue() {
        let (ledger, repo_id, number) = queued(mention());
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);

        done(&ledger, repo_id, number, "abc1234").unwrap();

        assert!(ledger.queue(repo_id).unwrap().is_empty());
        let mine = ledger.my_state(repo_id, number).unwrap();
        assert_eq!(mine.done_sha.as_deref(), Some("abc1234"));
        assert!(mine.done_at.is_some());
    }

    #[test]
    fn done_leaves_a_review_request_asking() {
        // Only reviewing, or the request being withdrawn, clears this one — so a
        // `done` on it must not make the PR look handled.
        let (ledger, repo_id, number) = queued(AttentionReason::ReviewRequested { team: None });

        done(&ledger, repo_id, number, "abc1234").unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 1, "the review request should survive `done`");
        assert_eq!(queue[0].top.reason.discriminant(), "review_requested");
    }

    #[test]
    fn snooze_clears_the_queue_entry_and_reports_whole_seconds() {
        let (ledger, repo_id, number) = queued(mention());
        let until = ts("2026-08-14T12:00:00.123456Z");

        let reported = snooze(&ledger, repo_id, number, until).unwrap();

        assert_eq!(reported, ts("2026-08-14T12:00:00Z"), "rounded for display");
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert_eq!(
            ledger.my_state(repo_id, number).unwrap().snoozed_until,
            Some(until),
            "stored at full precision, only the report is rounded"
        );
    }

    #[test]
    fn muting_clears_attention_and_unmuting_does_not_restore_it() {
        let (ledger, repo_id, number) = queued(mention());

        set_muted(&ledger, repo_id, number, true).unwrap();
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert!(ledger.my_state(repo_id, number).unwrap().muted);

        set_muted(&ledger, repo_id, number, false).unwrap();
        assert!(!ledger.my_state(repo_id, number).unwrap().muted);
        assert!(
            ledger.queue(repo_id).unwrap().is_empty(),
            "the reasons come back from the next sync, not from unmuting"
        );
    }

    #[test]
    fn deferring_keeps_it_on_the_queue_but_at_the_bottom() {
        let (ledger, repo_id, number) = queued(mention());

        set_deferred(&ledger, repo_id, number, true).unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 1, "deferred is sunk, not hidden");
        assert!(queue[0].deferred);

        set_deferred(&ledger, repo_id, number, false).unwrap();
        assert!(!ledger.queue(repo_id).unwrap()[0].deferred);
    }

    #[test]
    fn snooze_until_parses_a_friendly_duration() {
        let until = snooze_until(ts("2026-08-05T12:00:00Z"), "3d").unwrap();
        assert_eq!(until, ts("2026-08-08T12:00:00Z"));
    }

    #[test]
    fn snooze_until_handles_a_compound_duration() {
        let until = snooze_until(ts("2026-08-05T12:00:00Z"), "1w2d").unwrap();
        assert_eq!(until, ts("2026-08-14T12:00:00Z"));
    }

    #[test]
    fn snooze_until_rejects_a_non_positive_duration() {
        let now = ts("2026-08-05T12:00:00Z");
        assert!(snooze_until(now, "0s").is_err());
        assert!(snooze_until(now, "-3d").is_err());
    }

    #[test]
    fn snooze_until_rejects_garbage() {
        assert!(snooze_until(ts("2026-08-05T12:00:00Z"), "soon").is_err());
    }
}
