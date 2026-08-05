//! The attention state machine: the pure function turning a PR's stored state
//! into the list of reasons it wants my attention.
//!
//! [`classify`] takes everything reviewq knows about one PR — the cheap
//! snapshot, my own history, its review threads, and a [`ClassifyCtx`] of
//! signals that come from config or a tier-2 fetch rather than the PR's own
//! activity — and returns every [`Attention`] that fires, most-urgent first. It
//! touches no IO and no clock beyond the `now` it is handed, so a fixture plus a
//! `now` reproduces a queue exactly.
//!
//! The reasons and their priorities are the design's reason table; see
//! [`AttentionReason`](crate::model::AttentionReason). Suppressions are checked
//! before any reason: mute beats everything, then snooze, then a closed
//! (unmerged) PR, then draft (which only a mention pierces). A *merged* PR is
//! deliberately not suppressed — post-merge activity can be the signal that
//! something shipped broken.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::model::{Attention, AttentionReason, MyState, PrSnapshot, PrState, ThreadState};

/// An @mention of me, found by scanning comment and review bodies at tier-2.
///
/// Already filtered to mentions *of me*; the classifier only decides whether
/// one is recent enough to matter and whether its author is a bot.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Mention {
    /// Login of whoever mentioned me.
    pub by: String,
    /// When the mentioning comment was posted.
    pub at: Timestamp,
}

/// A formal review request that currently names me.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ReviewRequest {
    /// The team the request went to, if it was a team request rather than a
    /// direct one.
    #[serde(default)]
    pub team: Option<String>,
}

/// Signals classification needs that are not part of a PR's own activity: they
/// come from config (the bot list, why the PR is tracked) or from a tier-2
/// fetch (mentions, review requests, the commit count behind a re-review).
#[derive(Debug, Clone, Default)]
pub struct ClassifyCtx<'a> {
    /// Logins whose comments never raise attention — a bot replying in my
    /// thread, or @mentioning me, is noise.
    pub bots: &'a [String],
    /// The interest rule that caused this PR to be tracked, in the form
    /// [`rules`](crate::rules) renders (`label area:task-sdk`, `path
    /// task-sdk/**`, ...). `None` for a PR tracked only by involvement, which
    /// therefore never produces `needs-first-look`.
    pub interest: Option<&'a str>,
    /// @mentions of me on this PR.
    pub mentions: &'a [Mention],
    /// A live review request naming me, if any.
    pub review_request: Option<ReviewRequest>,
    /// Commits pushed since my last review, for the `re-review` count. Zero when
    /// unknown or not applicable.
    pub new_commits: u32,
    /// Whether merged PRs should still be evaluated. Off by default: most people
    /// want the queue to end at merge. On (a per-project opt-in) for those who
    /// review post-merge to catch things that need reverting — a post-merge
    /// reply or mention then still surfaces. A closed-unmerged PR is silent
    /// regardless.
    pub include_merged: bool,
}

/// Classify one PR: every [`Attention`] it currently warrants, sorted
/// most-urgent first (priority band, then oldest-within-band).
///
/// Pure and clock-free apart from `now`, which decides only whether a snooze has
/// lapsed. An empty result means the PR wants nothing from me right now.
///
/// # Thread ownership is crude in v1
///
/// A thread's [`i_own`](ThreadState::i_own) is set upstream and taken at face
/// value here; "I was the last non-author voice" is an approximation, so a
/// thread I merely commented in late can read as mine. The consequence is an
/// occasional spurious `thread-reply`, never a missed one — acceptable until
/// ownership is tracked precisely.
pub fn classify(
    pr: &PrSnapshot,
    mine: &MyState,
    threads: &[ThreadState],
    now: Timestamp,
    ctx: &ClassifyCtx<'_>,
) -> Vec<Attention> {
    // Mute is absolute — it exists precisely to silence a PR that would
    // otherwise keep firing, mentions included.
    if mine.muted {
        return Vec::new();
    }
    // A snooze still in effect suppresses everything; once it lapses the same
    // reasons reappear unchanged, so nothing about the PR's state is consumed.
    if let Some(until) = mine.snoozed_until
        && now < until
    {
        return Vec::new();
    }
    // A closed-unmerged PR is abandoned — nothing to do. A merged PR is archived
    // too unless this project opts into post-merge review.
    match pr.state {
        PrState::Closed => return Vec::new(),
        PrState::Merged if !ctx.include_merged => return Vec::new(),
        _ => {}
    }

    let mention = mention_attention(mine, now, ctx);

    // A draft is work in progress; only a direct mention pierces it.
    if pr.is_draft {
        return mention.into_iter().collect();
    }

    let mut out = Vec::new();
    out.extend(mention);
    out.extend(thread_reply_attention(threads, ctx));
    out.extend(resolved_unanswered_attention(pr, threads));
    out.extend(re_review_attention(pr, mine, ctx));
    out.extend(review_requested_attention(pr, mine, ctx));
    // needs-first-look is for the open queue only: starting a first review of an
    // already-merged PR is not the point of surfacing it.
    if pr.state.is_open() {
        out.extend(needs_first_look_attention(pr, mine, ctx));
    }

    out.sort();
    out
}

fn is_bot(login: &str, bots: &[String]) -> bool {
    bots.iter().any(|b| b == login)
}

/// The newest non-bot mention of me that lands after my last action (any
/// mention, if I have never acted).
fn mention_attention(mine: &MyState, _now: Timestamp, ctx: &ClassifyCtx<'_>) -> Option<Attention> {
    let latest = ctx
        .mentions
        .iter()
        .filter(|m| !is_bot(&m.by, ctx.bots))
        .filter(|m| mine.last_action_at.is_none_or(|acted| m.at > acted))
        .max_by_key(|m| m.at)?;
    Some(Attention {
        reason: AttentionReason::Mention {
            by: latest.by.clone(),
        },
        since: latest.at,
    })
}

/// Unresolved threads I own in which someone other than me (and not a bot) has
/// spoken since my last comment.
fn thread_reply_attention(threads: &[ThreadState], ctx: &ClassifyCtx<'_>) -> Option<Attention> {
    let replied: Vec<&ThreadState> = threads
        .iter()
        .filter(|t| t.i_own && !t.is_resolved)
        .filter(|t| spoken_after_me(t) && !last_speaker_is_bot(t, ctx.bots))
        .collect();

    let newest = replied.iter().max_by_key(|t| t.last_comment_at)?;
    Some(Attention {
        reason: AttentionReason::ThreadReply {
            by: newest.last_comment_author.clone().unwrap_or_default(),
            threads: replied.len(),
        },
        since: newest.last_comment_at?,
    })
}

/// Threads I own that someone else resolved while I still held the last word —
/// a "go verify the fix" state that only an explicit ack clears.
fn resolved_unanswered_attention(pr: &PrSnapshot, threads: &[ThreadState]) -> Option<Attention> {
    let resolved: Vec<&ThreadState> = threads
        .iter()
        .filter(|t| t.i_own && t.is_resolved)
        .filter(|t| !spoken_after_me(t))
        .collect();

    let by = resolved.iter().find_map(|t| t.resolved_by.clone())?;
    Some(Attention {
        reason: AttentionReason::ResolvedUnanswered {
            by,
            threads: resolved.len(),
        },
        // No per-thread resolve time is stored; the PR's updatedAt is the
        // closest event stamp we have.
        since: pr.updated_at,
    })
}

/// The head has moved since I reviewed and I have not since acknowledged it.
fn re_review_attention(
    pr: &PrSnapshot,
    mine: &MyState,
    ctx: &ClassifyCtx<'_>,
) -> Option<Attention> {
    let reviewed = mine.last_reviewed_sha.as_deref()?;
    mine.last_verdict?;
    if reviewed == pr.head_sha {
        return None;
    }
    // An explicit `done` at the current head clears the re-review until the
    // head moves again.
    if mine.done_sha.as_deref() == Some(pr.head_sha.as_str()) {
        return None;
    }
    Some(Attention {
        reason: AttentionReason::ReReview {
            new_commits: ctx.new_commits,
            since_sha: reviewed.to_string(),
        },
        since: pr.updated_at,
    })
}

/// A live review request names me, and I have not reviewed the current head.
fn review_requested_attention(
    pr: &PrSnapshot,
    mine: &MyState,
    ctx: &ClassifyCtx<'_>,
) -> Option<Attention> {
    let request = ctx.review_request.as_ref()?;
    if mine.last_reviewed_sha.as_deref() == Some(pr.head_sha.as_str()) {
        return None;
    }
    Some(Attention {
        reason: AttentionReason::ReviewRequested {
            team: request.team.clone(),
        },
        since: pr.updated_at,
    })
}

/// Matches an interest rule and I have never touched it.
fn needs_first_look_attention(
    pr: &PrSnapshot,
    mine: &MyState,
    ctx: &ClassifyCtx<'_>,
) -> Option<Attention> {
    let rule = ctx.interest?;
    let untouched = mine.last_action_at.is_none()
        && mine.last_reviewed_sha.is_none()
        && mine.done_sha.is_none();
    if !untouched {
        return None;
    }
    Some(Attention {
        reason: AttentionReason::NeedsFirstLook {
            rule: rule.to_string(),
        },
        since: pr.updated_at,
    })
}

/// Whether the last comment in a thread is newer than my own last comment in
/// it — i.e. someone spoke after me. A thread I have never spoken in counts as
/// spoken-after (there is no "me" to be after).
fn spoken_after_me(t: &ThreadState) -> bool {
    match (t.last_comment_at, t.my_last_comment_at) {
        (Some(last), Some(mine)) => last > mine,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

fn last_speaker_is_bot(t: &ThreadState, bots: &[String]) -> bool {
    t.last_comment_author
        .as_deref()
        .is_some_and(|a| is_bot(a, bots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PrState, Verdict};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn pr() -> PrSnapshot {
        PrSnapshot {
            number: 1,
            title: "t".into(),
            author: "octocat".into(),
            author_association: "MEMBER".into(),
            head_sha: "head0000".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-05T09:00:00Z"),
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    fn now() -> Timestamp {
        ts("2026-08-05T12:00:00Z")
    }

    #[test]
    fn mute_suppresses_a_live_mention() {
        let mine = MyState {
            muted: true,
            ..Default::default()
        };
        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let ctx = ClassifyCtx {
            mentions: &mentions,
            ..Default::default()
        };
        assert!(classify(&pr(), &mine, &[], now(), &ctx).is_empty());
    }

    #[test]
    fn a_live_snooze_suppresses_everything() {
        let mine = MyState {
            snoozed_until: Some(ts("2026-08-06T00:00:00Z")),
            ..Default::default()
        };
        let ctx = ClassifyCtx {
            interest: Some("label area:x"),
            ..Default::default()
        };
        assert!(classify(&pr(), &mine, &[], now(), &ctx).is_empty());
    }

    #[test]
    fn a_lapsed_snooze_lets_reasons_through() {
        let mine = MyState {
            snoozed_until: Some(ts("2026-08-04T00:00:00Z")),
            ..Default::default()
        };
        let ctx = ClassifyCtx {
            interest: Some("label area:x"),
            ..Default::default()
        };
        let out = classify(&pr(), &mine, &[], now(), &ctx);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            out[0].reason,
            AttentionReason::NeedsFirstLook { .. }
        ));
    }

    #[test]
    fn a_draft_lets_only_a_mention_through() {
        let mut draft = pr();
        draft.is_draft = true;
        let ctx_interest = ClassifyCtx {
            interest: Some("label area:x"),
            ..Default::default()
        };
        assert!(classify(&draft, &MyState::default(), &[], now(), &ctx_interest).is_empty());

        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let ctx_mention = ClassifyCtx {
            mentions: &mentions,
            ..Default::default()
        };
        let out = classify(&draft, &MyState::default(), &[], now(), &ctx_mention);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, AttentionReason::Mention { .. }));
    }

    #[test]
    fn a_bot_mention_does_not_fire() {
        let mentions = [Mention {
            by: "github-actions[bot]".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let bots = ["github-actions[bot]".to_string()];
        let ctx = ClassifyCtx {
            bots: &bots,
            mentions: &mentions,
            ..Default::default()
        };
        assert!(classify(&pr(), &MyState::default(), &[], now(), &ctx).is_empty());
    }

    #[test]
    fn a_stale_mention_predating_my_action_does_not_fire() {
        let mine = MyState {
            last_action_at: Some(ts("2026-08-05T10:00:00Z")),
            ..Default::default()
        };
        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let ctx = ClassifyCtx {
            mentions: &mentions,
            ..Default::default()
        };
        assert!(classify(&pr(), &mine, &[], now(), &ctx).is_empty());
    }

    #[test]
    fn re_review_fires_once_the_head_moves_but_not_after_done() {
        let mine = MyState {
            last_reviewed_sha: Some("old00000".into()),
            last_verdict: Some(Verdict::Approved),
            last_action_at: Some(ts("2026-08-01T00:00:00Z")),
            ..Default::default()
        };
        let ctx = ClassifyCtx {
            new_commits: 3,
            ..Default::default()
        };
        let out = classify(&pr(), &mine, &[], now(), &ctx);
        assert_eq!(
            out[0].reason,
            AttentionReason::ReReview {
                new_commits: 3,
                since_sha: "old00000".into(),
            }
        );

        let acked = MyState {
            done_sha: Some("head0000".into()),
            ..mine
        };
        assert!(classify(&pr(), &acked, &[], now(), &ctx).is_empty());
    }

    #[test]
    fn a_closed_pr_is_always_silent() {
        let mut closed = pr();
        closed.state = PrState::Closed;
        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let ctx = ClassifyCtx {
            mentions: &mentions,
            include_merged: true,
            ..Default::default()
        };
        assert!(classify(&closed, &MyState::default(), &[], now(), &ctx).is_empty());
    }

    #[test]
    fn a_merged_pr_is_silent_unless_the_project_opts_in() {
        let mut merged = pr();
        merged.state = PrState::Merged;
        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let off = ClassifyCtx {
            mentions: &mentions,
            ..Default::default()
        };
        assert!(classify(&merged, &MyState::default(), &[], now(), &off).is_empty());

        let on = ClassifyCtx {
            include_merged: true,
            ..off
        };
        let out = classify(&merged, &MyState::default(), &[], now(), &on);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].reason, AttentionReason::Mention { .. }));
    }

    #[test]
    fn a_merged_pr_never_needs_a_first_look() {
        let mut merged = pr();
        merged.state = PrState::Merged;
        let ctx = ClassifyCtx {
            interest: Some("label area:x"),
            include_merged: true,
            ..Default::default()
        };
        assert!(classify(&merged, &MyState::default(), &[], now(), &ctx).is_empty());
    }

    #[test]
    fn reasons_come_back_in_priority_order() {
        let mine = MyState {
            last_reviewed_sha: Some("old00000".into()),
            last_verdict: Some(Verdict::ChangesRequested),
            last_action_at: Some(ts("2026-08-01T00:00:00Z")),
            ..Default::default()
        };
        let mentions = [Mention {
            by: "potiuk".into(),
            at: ts("2026-08-05T08:00:00Z"),
        }];
        let ctx = ClassifyCtx {
            mentions: &mentions,
            new_commits: 1,
            ..Default::default()
        };
        let out = classify(&pr(), &mine, &[], now(), &ctx);
        let priorities: Vec<u8> = out.iter().map(|a| a.reason.priority()).collect();
        assert_eq!(priorities, vec![1, 4]);
    }
}
