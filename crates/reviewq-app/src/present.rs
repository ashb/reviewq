//! The wording both frontends use for a PR's state.
//!
//! Not rendering — each frontend paints its own way, one with terminal colours
//! and one with a ratatui theme, and neither has any business owning the other's
//! styling. What lives here is the layer underneath that: which facts are worth
//! saying, in what words, in what order.
//!
//! It exists because the two had drifted. `show` reported `done at abc1234 on
//! 2026-08-11T09:00:00Z — superseded by new commits since` where the interface
//! said `done at abc1234 — superseded by new commits`, and each had its own
//! `short_sha` and its own timestamp formatter. Two frontends disagreeing about
//! what a PR's history says is a difference nobody asked for.
//!
//! Only what both already said lives here. A line where one frontend colours part
//! of the text — the verdict inside a reviewer row — stays where it is: flattening
//! it to one string would share the wording by throwing the colour away.

use jiff::Timestamp;
use reviewq_core::model::{MyState, PrSnapshot, ThreadState};

/// A timestamp as reviewq writes them: whole seconds, RFC 3339.
///
/// Sub-second precision is noise in a queue — nothing here happens twice in a
/// second, and the extra digits push the interesting part off a narrow pane.
pub fn stamp(ts: Timestamp) -> String {
    ts.round(jiff::Unit::Second).unwrap_or(ts).to_string()
}

/// A timestamp as the day it fell on.
///
/// For the dates that answer a day-scale question — when a PR was opened, which
/// is how long its author has been waiting. The hour is real and `--json` keeps
/// it; a pane with two full RFC 3339 stamps on one line has no room for the
/// second, and the minute a PR was opened has never decided anything.
pub fn day(ts: Timestamp) -> String {
    ts.strftime("%Y-%m-%d").to_string()
}

/// A head SHA at GitHub's own abbreviation length, so it can be pasted into
/// `git show`.
pub fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// What is currently keeping a PR quiet, in the order it was asked for.
///
/// Empty for a PR nothing is suppressing — which is most of them, and why the
/// caller decides whether an empty list means printing nothing or a placeholder.
pub fn silenced(my: &MyState) -> Vec<String> {
    let mut bits = Vec::new();
    if my.muted {
        bits.push("muted".to_string());
    }
    if let Some(until) = my.snoozed_until {
        bits.push(format!("snoozed until {}", stamp(until)));
    }
    if let Some(at) = my.deferred_at {
        bits.push(format!("deferred since {}", stamp(at)));
    }
    bits
}

/// A live snooze, as a row-width tag: `snoozed until 2026-08-15`.
///
/// `None` once it has lapsed, which is the whole of what makes it a *live*
/// snooze — a date in the past is history, and a row is not the place for that.
/// The day rather than the instant: a snooze is set in days, and a list row has
/// no column to spare on the minute one runs out.
///
/// Separate from [`silenced`], which is `show`'s roomier account of everything
/// keeping a PR quiet and says so whether or not it still applies.
pub fn snoozed_tag(my: &MyState, now: Timestamp) -> Option<String> {
    let until = my.snoozed_until?;
    (now < until).then(|| format!("snoozed until {}", day(until)))
}

/// How loudly a row's reason should read.
///
/// The classification, not the colour: the two frontends paint it differently on
/// purpose — one has a ratatui theme and one has whatever the terminal's palette
/// is — but *which* rows shout has to be one decision, or a mention looks like a
/// first look in one of them. It did: `list` painted every reason the same cyan
/// while the interface had been telling them apart all along.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    /// Somebody is waiting on a reply from you — a mention, or a thread you own.
    Urgent,
    /// A reason worth acting on, in its turn.
    Normal,
    /// Set aside, or not a reason at all: a deferred row, and one that is only
    /// saying why reviewq watches it. Listed to be seen, not to be acted on.
    Quiet,
}

/// How a row's reason should read, from its priority band and whether it has
/// been deferred.
///
/// `priority` is `None` for a row with no attention at all — a waiting or
/// tracked row, which says why it is watched instead.
pub fn emphasis(priority: Option<u8>, deferred: bool) -> Emphasis {
    match priority {
        _ if deferred => Emphasis::Quiet,
        None => Emphasis::Quiet,
        // Bands 1 to 3 are activity on a PR of yours, a mention, and a reply in
        // a thread you own: the ones where a person is waiting on you rather
        // than a rule pointing.
        Some(p) if p <= 3 => Emphasis::Urgent,
        Some(_) => Emphasis::Normal,
    }
}

/// What I have already done to a PR — the fact a queue wants to show in one
/// column, so "have I been here before?" is answered by scanning rather than by
/// selecting each row in turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handled {
    /// I submitted a review on the forge. GitHub owns this: it survives a fresh
    /// ledger, and everyone else can see it.
    Reviewed,
    /// I marked it done here, and never reviewed it. Local, and mine alone — the
    /// answer to a mention that needed nothing, or to a PR I read and had no
    /// comment on.
    Done,
}

/// Where I stand with a PR, as the one column in front of a list row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// I sank it with `reviewq defer`: still listed, still wanting something,
    /// but put below everything else until it changes.
    Deferred,
    /// I have been through it.
    Handled {
        /// A review of mine, or my own `done`.
        what: Handled,
        /// One of my marks names the PR's current head, so what I did still
        /// stands. False once the PR has moved on, which is most of what reaches
        /// the queue twice.
        current: bool,
    },
}

/// Where I stand with `pr`, or `None` if I have never touched it and it is
/// sitting where the queue put it.
///
/// A defer outranks the rest: it is the thing I most recently decided about the
/// PR, and it is why the row is at the bottom. Between the other two a review
/// wins — it is the stronger statement, and the one other people can see —
/// though `current` asks about both, since a `done` at today's head means
/// today's head is dealt with whichever sha I reviewed.
///
/// `deferred` comes from the queue row rather than from `my`, because a defer
/// only stands while nothing has happened since; the ledger is what works that
/// out.
pub fn mark(pr: &PrSnapshot, my: &MyState, deferred: bool) -> Option<Mark> {
    if deferred {
        return Some(Mark::Deferred);
    }
    let what = match (&my.last_reviewed_sha, &my.done_sha) {
        (Some(_), _) => Handled::Reviewed,
        (None, Some(_)) => Handled::Done,
        (None, None) => return None,
    };
    let at_head = |sha: &Option<String>| sha.as_deref() == Some(pr.head_sha.as_str());
    Some(Mark::Handled {
        what,
        current: at_head(&my.last_reviewed_sha) || at_head(&my.done_sha),
    })
}

/// A local `done`, and whether the PR has moved on since.
pub struct DoneNote {
    /// What to say.
    pub text: String,
    /// The head has moved since, so the `done` no longer covers what is there.
    /// Worth colouring as a caveat rather than as history.
    pub superseded: bool,
}

/// The `done` a PR carries, if any.
pub fn done_note(pr: &PrSnapshot, my: &MyState) -> Option<DoneNote> {
    let sha = my.done_sha.as_deref()?;
    let superseded = sha != pr.head_sha;
    let at = my
        .done_at
        .map_or_else(String::new, |at| format!(" on {}", stamp(at)));
    let note = if superseded {
        " — superseded by new commits"
    } else {
        ""
    };
    Some(DoneNote {
        text: format!("done at {}{at}{note}", short_sha(sha)),
        superseded,
    })
}

/// How a PR's review threads stand.
pub struct ThreadCounts {
    /// Threads in total.
    pub total: usize,
    /// Threads I started or last spoke in.
    pub owned: usize,
    /// Threads GitHub considers resolved.
    pub resolved: usize,
}

/// Count a PR's threads, so a summary line reads the same in both frontends.
pub fn thread_counts(threads: &[ThreadState]) -> ThreadCounts {
    ThreadCounts {
        total: threads.len(),
        owned: threads.iter().filter(|t| t.i_own).count(),
        resolved: threads.iter().filter(|t| t.is_resolved).count(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reviewq_core::model::PrState;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("timestamp")
    }

    fn pr() -> PrSnapshot {
        PrSnapshot {
            number: 1,
            title: "t".into(),
            author: "potiuk".into(),
            author_association: "MEMBER".into(),
            head_sha: "head0000".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-11T09:00:00Z"),
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    #[test]
    fn a_stamp_is_whole_seconds() {
        assert_eq!(
            stamp(ts("2026-08-11T09:00:00.123456Z")),
            "2026-08-11T09:00:00Z"
        );
    }

    #[test]
    fn emphasis_shouts_only_where_a_person_is_waiting_on_you() {
        use reviewq_core::model::AttentionReason;

        // Read off the reason table rather than hardcoded numbers, so a reason
        // that changes band changes this with it.
        let band = |reason: AttentionReason| reason.priority();
        assert_eq!(
            emphasis(
                Some(band(AttentionReason::Mention { by: "kaxil".into() })),
                false
            ),
            Emphasis::Urgent
        );
        assert_eq!(
            emphasis(
                Some(band(AttentionReason::ThreadReply {
                    by: "kaxil".into(),
                    threads: 1
                })),
                false
            ),
            Emphasis::Urgent
        );
        assert_eq!(
            emphasis(
                Some(band(AttentionReason::MyPr {
                    by: "kaxil".into(),
                    what: reviewq_core::model::OnMyPr::Commented,
                })),
                false
            ),
            Emphasis::Urgent
        );
        assert_eq!(
            emphasis(
                Some(band(AttentionReason::ReviewRequested { team: None })),
                false
            ),
            Emphasis::Normal
        );
        assert_eq!(
            emphasis(
                Some(band(AttentionReason::NeedsFirstLook {
                    rule: "label x".into()
                })),
                false
            ),
            Emphasis::Normal
        );
    }

    #[test]
    fn a_set_aside_row_is_quiet_however_urgent_its_reason() {
        // Deferring says "not before the others"; a row that then shouted would
        // be arguing with the person who deferred it.
        assert_eq!(emphasis(Some(1), true), Emphasis::Quiet);
        // And a row with no reason at all is saying why it is watched, which is
        // an answer to a question nobody asked yet.
        assert_eq!(emphasis(None, false), Emphasis::Quiet);
    }

    #[test]
    fn a_snooze_tag_lasts_exactly_as_long_as_the_snooze() {
        let my = MyState {
            snoozed_until: Some(ts("2026-08-15T09:00:00Z")),
            ..MyState::default()
        };

        assert_eq!(
            snoozed_tag(&my, ts("2026-08-13T10:00:00Z")).as_deref(),
            Some("snoozed until 2026-08-15")
        );
        assert_eq!(
            snoozed_tag(&my, ts("2026-08-15T09:00:00Z")),
            None,
            "the instant it lapses it is history, which a row does not carry"
        );
        assert_eq!(
            snoozed_tag(&MyState::default(), ts("2026-08-13T10:00:00Z")),
            None
        );
    }

    #[test]
    fn a_short_sha_tolerates_one_already_short() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test]
    fn nothing_is_silenced_by_default() {
        assert!(silenced(&MyState::default()).is_empty());
    }

    #[test]
    fn every_active_silencer_is_named_in_order() {
        let mine = MyState {
            muted: true,
            snoozed_until: Some(ts("2026-08-14T00:00:00Z")),
            deferred_at: Some(ts("2026-08-11T09:00:00Z")),
            ..MyState::default()
        };

        assert_eq!(
            silenced(&mine),
            vec![
                "muted".to_string(),
                "snoozed until 2026-08-14T00:00:00Z".to_string(),
                "deferred since 2026-08-11T09:00:00Z".to_string(),
            ]
        );
    }

    #[test]
    fn a_done_at_the_current_head_is_not_superseded() {
        let mine = MyState {
            done_sha: Some("head0000".into()),
            done_at: Some(ts("2026-08-11T10:00:00Z")),
            ..MyState::default()
        };

        let note = done_note(&pr(), &mine).expect("a note");

        assert_eq!(note.text, "done at head000 on 2026-08-11T10:00:00Z");
        assert!(!note.superseded);
    }

    #[test]
    fn a_done_left_behind_by_new_commits_says_so() {
        let mine = MyState {
            done_sha: Some("older00".into()),
            done_at: Some(ts("2026-08-11T10:00:00Z")),
            ..MyState::default()
        };

        let note = done_note(&pr(), &mine).expect("a note");

        assert!(
            note.text.ends_with("— superseded by new commits"),
            "{}",
            note.text
        );
        assert!(note.superseded);
    }

    #[test]
    fn a_pr_never_acted_on_has_no_history() {
        assert!(done_note(&pr(), &MyState::default()).is_none());
        assert_eq!(mark(&pr(), &MyState::default(), false), None);
    }

    #[test]
    fn a_review_outranks_a_done_and_either_can_be_the_current_one() {
        // The two are separate fields with separate owners — a sync writes one,
        // `reviewq done` the other — so a PR can carry either, or both at
        // different heads.
        let reviewed = MyState {
            last_reviewed_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(
            mark(&pr(), &reviewed, false),
            Some(Mark::Handled {
                what: Handled::Reviewed,
                current: true
            })
        );

        let done_only = MyState {
            done_sha: Some("older00".into()),
            ..MyState::default()
        };
        assert_eq!(
            mark(&pr(), &done_only, false),
            Some(Mark::Handled {
                what: Handled::Done,
                current: false
            })
        );

        // Reviewed an older head, then acknowledged the one that is there now:
        // the glyph says review, and it is not stale.
        let both = MyState {
            last_reviewed_sha: Some("older00".into()),
            done_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(
            mark(&pr(), &both, false),
            Some(Mark::Handled {
                what: Handled::Reviewed,
                current: true
            })
        );
    }

    #[test]
    fn a_deferred_pr_says_that_before_anything_else() {
        // It is the most recent thing I decided about the PR, and the reason the
        // row is at the bottom of the queue — which is what a reader is asking
        // about when they look at it there.
        let reviewed = MyState {
            last_reviewed_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(mark(&pr(), &reviewed, true), Some(Mark::Deferred));
        assert_eq!(mark(&pr(), &MyState::default(), true), Some(Mark::Deferred));
    }

    #[test]
    fn threads_are_counted_by_whose_they_are_and_whether_they_are_done() {
        let thread = |i_own: bool, is_resolved: bool| ThreadState {
            thread_id: "T".into(),
            i_own,
            is_resolved,
            resolved_by: is_resolved.then(|| "kaxil".to_string()),
            last_comment_author: Some("kaxil".into()),
            last_comment_at: Some(ts("2026-08-11T09:00:00Z")),
            my_last_comment_at: None,
        };

        let counts = thread_counts(&[thread(true, false), thread(false, true), thread(true, true)]);

        assert_eq!(counts.total, 3);
        assert_eq!(counts.owned, 2);
        assert_eq!(counts.resolved, 2);
    }
}
