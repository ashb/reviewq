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

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, Unit};
use reviewq_core::model::{ActivityKind, ActivityPayload, ActivitySource};
use reviewq_forge::Forge;
use reviewq_ledger::{Ledger, NewActivityEvent, RepoId};

use crate::config::{Config, RepoRef};

/// Mark a PR handled at `head_sha`, and clear the reasons `done` is allowed to.
///
/// Not `review_requested`: only submitting a review, or the request being
/// withdrawn, clears that one — so a `done` on a PR you were asked to review
/// leaves it asking, which is the point.
pub fn done(ledger: &Ledger, repo_id: RepoId, number: u64, head_sha: &str) -> Result<()> {
    let now = Timestamp::now();
    Ok(ledger.record_done_action(repo_id, number, head_sha, now, &done_event(head_sha, now))?)
}

/// Tell GitHub the PR's notifications have been read.
///
/// Best-effort and separate from [`done`]: a token or the network being
/// unavailable must not stop the local record, and must not delay it either.
/// Callers log a failure rather than surfacing it.
pub async fn mark_notifications_read(cfg: &Config, repo: &RepoRef, number: u64) -> Result<()> {
    let forge = cfg.forge_for(&repo.host)?;
    Ok(forge
        .mark_pr_notifications_read(&repo.owner, &repo.name, number)
        .await?)
}

/// Suppress everything on a PR until `until`, mentions included.
///
/// Takes an instant rather than a duration, so a caller can choose one however
/// suits it — typed as `3d`, or picked from presets — without this knowing about
/// either. [`snooze_until`] turns the CLI's syntax into one.
pub fn snooze(
    ledger: &Ledger,
    repo_id: RepoId,
    number: u64,
    until: Timestamp,
) -> Result<Timestamp> {
    let now = Timestamp::now();
    ledger.record_snooze_action(
        repo_id,
        number,
        until,
        &local_event(
            ActivityKind::Snoozed,
            now,
            ActivityPayload::Snoozed { until },
        ),
    )?;
    Ok(until.round(Unit::Second).unwrap_or(until))
}

/// Set or clear a PR's mute.
///
/// Keeps the attention it holds, deliberately. A mute is a statement about what
/// you want shown — the queue is what hides it (see [`Ledger::muted`]) — so the
/// reasons stay computed, which is what lets the interface show you what you
/// have silenced and why. It also makes unmuting immediate: this used to clear
/// them, so a PR came back empty and stayed that way until the next sync
/// rediscovered what had been true all along.
pub fn set_muted(ledger: &Ledger, repo_id: RepoId, number: u64, muted: bool) -> Result<()> {
    let now = Timestamp::now();
    let kind = if muted {
        ActivityKind::Muted
    } else {
        ActivityKind::Unmuted
    };
    ledger.record_muted_action(
        repo_id,
        number,
        muted,
        &local_event(kind, now, ActivityPayload::None),
    )?;
    Ok(())
}

/// Stop watching a PR: drop what it was tracked for, and the attention with it.
///
/// The verb `done` is not. `done` says "handled at this head" and leaves the PR
/// waiting on somebody, to come back when they push or reply; this says "I am
/// finished with it", and a rule that still matches may not take it back.
/// `false` if the ledger has no such PR, so a caller can say so rather than
/// reporting a success it didn't have.
///
/// [`track`] is the undo, which is why this stops short of deleting anything.
pub fn untrack(ledger: &Ledger, repo_id: RepoId, number: u64) -> Result<bool> {
    let now = Timestamp::now();
    Ok(ledger.record_untrack_action(
        repo_id,
        number,
        now,
        &local_event(ActivityKind::Untracked, now, ActivityPayload::None),
    )?)
}

/// Set or clear a PR's defer, which sinks it to the bottom of the queue without
/// hiding it. It clears itself once something new happens on the PR.
pub fn set_deferred(ledger: &Ledger, repo_id: RepoId, number: u64, deferred: bool) -> Result<()> {
    let now = Timestamp::now();
    let at = deferred.then_some(now);
    let kind = if deferred {
        ActivityKind::Deferred
    } else {
        ActivityKind::Undeferred
    };
    ledger.record_deferred_action(
        repo_id,
        number,
        at,
        &local_event(kind, now, ActivityPayload::None),
    )?;
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
    repo_id: RepoId,
    repo: &RepoRef,
    number: u64,
    forge: &dyn Forge,
) -> Result<Tracked> {
    let now = Timestamp::now();
    if ledger.show(repo_id, number)?.is_some() {
        return Ok(
            if ledger.record_track_action(
                repo_id,
                number,
                &local_event(ActivityKind::Tracked, now, ActivityPayload::None),
            )? {
                Tracked::Marked
            } else {
                Tracked::Already
            },
        );
    }

    let fetched = forge
        .fetch_pr(&repo.owner, &repo.name, number)
        .await?
        .with_context(|| format!("{}/{} has no pull request #{number}", repo.owner, repo.name))?;
    // The colours arrive with it, because a sweep may never come for this one:
    // tracking is what you do for a PR outside the sweep's window, and it is one
    // of only two roads a PR takes into the ledger.
    ledger.set_label_colours(
        repo_id,
        &fetched
            .labels
            .into_iter()
            .map(|label| (label.name, label.color))
            .collect::<Vec<_>>(),
    )?;
    ledger.record_fetched_track_action(
        repo_id,
        &fetched.pr,
        now,
        &local_event(ActivityKind::Tracked, now, ActivityPayload::None),
    )?;
    Ok(Tracked::Fetched)
}

fn local_event(kind: ActivityKind, at: Timestamp, payload: ActivityPayload) -> NewActivityEvent {
    NewActivityEvent {
        relation: reviewq_core::model::ActivityRelation::Own,
        source: ActivitySource::Local,
        kind,
        occurred_at: at,
        recorded_at: at,
        actor: None,
        head_sha: None,
        external_id: None,
        permalink: None,
        payload,
    }
}

fn done_event(head_sha: &str, at: Timestamp) -> NewActivityEvent {
    NewActivityEvent {
        head_sha: Some(head_sha.into()),
        ..local_event(ActivityKind::Done, at, ActivityPayload::None)
    }
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
    use reviewq_core::model::{
        ActivityKind, ActivityPayload, Attention, AttentionReason, MyState, PrSnapshot, PrState,
    };
    use reviewq_ledger::{ActivityScope, RepoKey, TrackedReason};

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
            created_at: None,
            state_changed_at: None,
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
    fn queued(reason: AttentionReason) -> (Ledger, RepoId, u64) {
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
                Some(TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
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
            .expect("detail")
            .expect_applied();
        (ledger, repo_id, 1)
    }

    fn mention() -> AttentionReason {
        AttentionReason::Mention { by: "kaxil".into() }
    }

    fn own_activity(
        ledger: &Ledger,
        repo_id: RepoId,
        number: u64,
    ) -> Vec<(ActivityKind, ActivityPayload)> {
        ledger
            .activity_page(ActivityScope::Pr { repo_id, number }, None, 100)
            .unwrap()
            .events
            .into_iter()
            .filter(|event| event.kind != ActivityKind::AttentionChanged)
            .map(|event| (event.kind, event.payload))
            .collect()
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
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Done, ActivityPayload::None)]
        );
        assert_eq!(
            ledger
                .activity_page(ActivityScope::Pr { repo_id, number }, None, 1)
                .unwrap()
                .events[0]
                .head_sha
                .as_deref(),
            Some("abc1234")
        );
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
    fn repeating_done_records_each_decision() {
        let (ledger, repo_id, number) = queued(mention());

        done(&ledger, repo_id, number, "abc1234").unwrap();
        done(&ledger, repo_id, number, "abc1234").unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Done, ActivityPayload::None),
                (ActivityKind::Done, ActivityPayload::None),
            ]
        );
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
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Snoozed, ActivityPayload::Snoozed { until })]
        );
    }

    #[test]
    fn repeating_snooze_records_each_decision() {
        let (ledger, repo_id, number) = queued(mention());
        let until = ts("2026-08-14T12:00:00Z");

        snooze(&ledger, repo_id, number, until).unwrap();
        snooze(&ledger, repo_id, number, until).unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Snoozed, ActivityPayload::Snoozed { until }),
                (ActivityKind::Snoozed, ActivityPayload::Snoozed { until }),
            ]
        );
    }

    #[test]
    fn muting_hides_a_pr_without_forgetting_why_it_was_there() {
        let (ledger, repo_id, number) = queued(mention());

        set_muted(&ledger, repo_id, number, true).unwrap();

        assert!(ledger.queue(repo_id).unwrap().is_empty(), "off the queue");
        assert!(ledger.my_state(repo_id, number).unwrap().muted);
        let hidden = ledger.muted(repo_id).unwrap();
        assert_eq!(hidden.len(), 1, "but findable, and it says why");
        assert_eq!(hidden[0].top.reason, mention());
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Muted, ActivityPayload::None)]
        );
    }

    #[test]
    fn unmuting_puts_a_pr_straight_back() {
        // It used to come back empty and stay that way until the next sync
        // rediscovered what had been true the whole time.
        let (ledger, repo_id, number) = queued(mention());
        set_muted(&ledger, repo_id, number, true).unwrap();

        set_muted(&ledger, repo_id, number, false).unwrap();

        assert!(!ledger.my_state(repo_id, number).unwrap().muted);
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);
        assert!(ledger.muted(repo_id).unwrap().is_empty());
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Unmuted, ActivityPayload::None),
                (ActivityKind::Muted, ActivityPayload::None),
            ]
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
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Undeferred, ActivityPayload::None),
                (ActivityKind::Deferred, ActivityPayload::None),
            ]
        );
    }

    #[test]
    fn an_expired_defer_can_be_renewed() {
        let (ledger, repo_id, number) = queued(mention());
        ledger
            .set_deferred_at(repo_id, number, Some(ts("2026-08-11T08:00:00Z")))
            .unwrap();
        assert!(!ledger.queue(repo_id).unwrap()[0].deferred);

        set_deferred(&ledger, repo_id, number, true).unwrap();

        assert!(ledger.queue(repo_id).unwrap()[0].deferred);
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Deferred, ActivityPayload::None)]
        );
    }

    #[test]
    fn muting_an_already_muted_pr_records_no_extra_event() {
        let (ledger, repo_id, number) = queued(mention());

        set_muted(&ledger, repo_id, number, true).unwrap();
        set_muted(&ledger, repo_id, number, true).unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Muted, ActivityPayload::None)]
        );
    }

    #[test]
    fn unmuting_an_unmuted_pr_records_no_extra_event() {
        let (ledger, repo_id, number) = queued(mention());

        set_muted(&ledger, repo_id, number, true).unwrap();
        set_muted(&ledger, repo_id, number, false).unwrap();
        set_muted(&ledger, repo_id, number, false).unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Unmuted, ActivityPayload::None),
                (ActivityKind::Muted, ActivityPayload::None),
            ]
        );
    }

    #[test]
    fn deferring_an_already_deferred_pr_records_no_extra_event() {
        let (ledger, repo_id, number) = queued(mention());

        set_deferred(&ledger, repo_id, number, true).unwrap();
        set_deferred(&ledger, repo_id, number, true).unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Deferred, ActivityPayload::None)]
        );
    }

    #[test]
    fn undefering_an_undeferred_pr_records_no_extra_event() {
        let (ledger, repo_id, number) = queued(mention());

        set_deferred(&ledger, repo_id, number, true).unwrap();
        set_deferred(&ledger, repo_id, number, false).unwrap();
        set_deferred(&ledger, repo_id, number, false).unwrap();

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [
                (ActivityKind::Undeferred, ActivityPayload::None),
                (ActivityKind::Deferred, ActivityPayload::None),
            ]
        );
    }

    #[test]
    fn untracking_records_the_action_without_losing_the_pr_or_history() {
        let (ledger, repo_id, number) = queued(mention());

        assert!(untrack(&ledger, repo_id, number).unwrap());

        assert!(ledger.show(repo_id, number).unwrap().is_some());
        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Untracked, ActivityPayload::None)]
        );
    }

    #[test]
    fn untracking_an_untracked_pr_records_no_extra_event() {
        let (ledger, repo_id, number) = queued(mention());

        assert!(untrack(&ledger, repo_id, number).unwrap());
        assert!(untrack(&ledger, repo_id, number).unwrap());

        assert_eq!(
            own_activity(&ledger, repo_id, number),
            [(ActivityKind::Untracked, ActivityPayload::None)]
        );
    }

    #[tokio::test]
    async fn tracking_an_unknown_pr_learns_its_repo_s_colours() {
        // The one road into the ledger a sweep may never travel: `track` is what
        // you reach for when a PR is outside the sweep's window, so the colours
        // have to arrive with the fetch or not at all.
        let (ledger, repo_id, _) = queued(mention());
        let repo = RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: None,
        };
        let forge = crate::fake_forge::FakeForge::new(vec![])
            .with_fetched_labels(&[("area:task-sdk", "0e8a16")]);

        let tracked = track(&ledger, repo_id, &repo, 4242, &forge).await.unwrap();

        assert_eq!(tracked, Tracked::Fetched);
        assert_eq!(
            ledger.label_colours(repo_id).unwrap()["area:task-sdk"],
            "0e8a16"
        );
        assert_eq!(
            own_activity(&ledger, repo_id, 4242),
            [(ActivityKind::Tracked, ActivityPayload::None)]
        );
    }

    #[tokio::test]
    async fn tracking_an_already_tracked_pr_records_no_event() {
        let (ledger, repo_id, number) = queued(mention());
        let repo = RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: None,
        };
        let forge = crate::fake_forge::FakeForge::new(vec![]);

        assert_eq!(
            track(&ledger, repo_id, &repo, number, &forge)
                .await
                .unwrap(),
            Tracked::Already
        );

        assert!(own_activity(&ledger, repo_id, number).is_empty());
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
