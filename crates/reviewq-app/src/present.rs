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
