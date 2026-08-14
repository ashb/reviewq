use std::fmt;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Why a PR is in the queue, and since when.
///
/// A PR may hold several of these; its queue position is set by the
/// highest-priority one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Attention {
    /// The rule that fired, with its evidence.
    pub reason: AttentionReason,
    /// When the triggering event happened. Older is more urgent within a
    /// priority band, so staleness escalates without a scoring function.
    pub since: Timestamp,
}

/// An attention reason together with the evidence that produced it.
///
/// Every variant renders, via [`Display`](fmt::Display), to a human-readable
/// string quoting its evidence. Those strings are a stable API — they are
/// snapshot-tested, and changing one is a user-visible change.
///
/// The rendering deliberately does not name the rule that fired: the caller
/// already has that as [`discriminant`](Self::discriminant), and repeating it
/// spent a third of a narrow column restating what the evidence says anyway.
/// "@kaxil mentioned you" needs no `mention:` in front of it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AttentionReason {
    /// Someone @mentioned me in a comment newer than my last action.
    Mention {
        /// Login of whoever mentioned me.
        by: String,
    },

    /// Someone else replied in an unresolved thread I own.
    ThreadReply {
        /// Login of the most recent replier.
        by: String,
        /// How many of my threads have new replies.
        threads: usize,
    },

    /// A thread I own was resolved by someone else without answering me. This
    /// is a "go verify the fix" state, so only an explicit ack clears it.
    ResolvedUnanswered {
        /// Login of whoever resolved it.
        by: String,
        /// How many of my threads were resolved this way.
        threads: usize,
    },

    /// The head has moved since I reviewed.
    ReReview {
        /// Commits pushed since my review.
        new_commits: u32,
        /// Head SHA at the time of my review.
        since_sha: String,
    },

    /// Somebody I am waiting on has spoken on a PR I reviewed — the author of
    /// it, or somebody I pulled into the discussion myself.
    ///
    /// The case no other reason covers: you review, the author answers in a
    /// comment of their own rather than in your thread, and never types your
    /// name. Nothing about that is a mention, a thread reply or a new commit,
    /// and until this existed the PR simply went quiet.
    AnsweredAfterReview {
        /// Login of whoever spoke.
        by: String,
        /// Whether they are the PR's author, as opposed to somebody I asked.
        author: bool,
    },

    /// A formal review request names me or a team I'm in.
    ReviewRequested {
        /// Team slug, if the request was to a team rather than to me directly.
        team: Option<String>,
    },

    /// Matches an interest rule and I have never acted on it.
    NeedsFirstLook {
        /// The rule that matched, without the `interest:` prefix that the
        /// ledger's `tracked_reason` carries: e.g. `label area:task-sdk`,
        /// `path task-sdk/**`, `author FIRST_TIME_CONTRIBUTOR`.
        rule: String,
    },
}

impl AttentionReason {
    /// Queue priority; 1 is most urgent. Matches the reason table in the design
    /// doc, and is the primary sort key for the queue.
    pub fn priority(&self) -> u8 {
        match self {
            Self::Mention { .. } => 1,
            Self::ThreadReply { .. } => 2,
            Self::ResolvedUnanswered { .. } => 3,
            Self::ReReview { .. } => 4,
            // Above a fresh request, below new commits: this is a PR you are
            // already in, and somebody is waiting on your answer to what you
            // already said.
            Self::AnsweredAfterReview { .. } => 5,
            Self::ReviewRequested { .. } => 6,
            Self::NeedsFirstLook { .. } => 7,
        }
    }

    /// Stable identifier stored in the ledger's `attention.reason` column, and
    /// reported as `reason` in `--json` output.
    ///
    /// The ledger keys a PR's attention rows on this, so one reason kind holds
    /// at most one row per PR. It is not what a person reads — that's
    /// [`Display`](fmt::Display) — so renaming a reason in prose never touches
    /// stored data.
    pub fn discriminant(&self) -> &'static str {
        match self {
            Self::Mention { .. } => "mention",
            Self::ThreadReply { .. } => "thread_reply",
            Self::ResolvedUnanswered { .. } => "resolved_unanswered",
            Self::ReReview { .. } => "re_review",
            Self::AnsweredAfterReview { .. } => "answered_after_review",
            Self::ReviewRequested { .. } => "review_requested",
            Self::NeedsFirstLook { .. } => "needs_first_look",
        }
    }
}

impl fmt::Display for AttentionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mention { by } => write!(f, "@{by} mentioned you"),

            Self::ThreadReply { by, threads } => {
                write!(f, "@{by} replied in ")?;
                match threads {
                    1 => write!(f, "a thread you own"),
                    n => write!(f, "{n} threads you own"),
                }
            }

            Self::ResolvedUnanswered { by, threads } => {
                write!(f, "@{by} resolved ")?;
                match threads {
                    1 => write!(f, "your thread"),
                    n => write!(f, "{n} of your threads"),
                }?;
                write!(f, " without replying")
            }

            Self::ReReview {
                new_commits,
                since_sha,
            } => {
                let sha = short_sha(since_sha);
                match new_commits {
                    1 => write!(f, "1 new commit since your review of {sha}"),
                    n => write!(f, "{n} new commits since your review of {sha}"),
                }
            }

            Self::AnsweredAfterReview { by, author: true } => {
                write!(f, "@{by} answered your review")
            }
            Self::AnsweredAfterReview { by, author: false } => {
                write!(f, "@{by} replied, as you asked")
            }

            // "you were asked to review" spent 24 characters saying what the
            // queue is for. Whose queue this is never needs restating.
            Self::ReviewRequested { team: None } => write!(f, "review requested"),
            Self::ReviewRequested { team: Some(team) } => {
                write!(f, "review requested via @{team}")
            }

            Self::NeedsFirstLook { rule } => write!(f, "matches {rule}"),
        }
    }
}

impl Attention {
    /// Queue sort key: priority band first, then oldest-first inside the band.
    pub fn sort_key(&self) -> (u8, Timestamp) {
        (self.reason.priority(), self.since)
    }
}

impl Ord for Attention {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for Attention {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// GitHub's own abbreviation length, so reason strings match what the web UI
/// shows and can be pasted into `git show`.
fn short_sha(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(7)
        .map_or(sha.len(), |(index, _)| index);
    &sha[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each variant, so a test covering "all of them" keeps covering all
    /// of them when a variant is added.
    fn every_variant() -> [AttentionReason; 7] {
        [
            AttentionReason::Mention { by: "a".into() },
            AttentionReason::ThreadReply {
                by: "a".into(),
                threads: 1,
            },
            AttentionReason::ResolvedUnanswered {
                by: "a".into(),
                threads: 1,
            },
            AttentionReason::ReReview {
                new_commits: 1,
                since_sha: "a".into(),
            },
            AttentionReason::AnsweredAfterReview {
                by: "a".into(),
                author: true,
            },
            AttentionReason::ReviewRequested { team: None },
            AttentionReason::NeedsFirstLook { rule: "a".into() },
        ]
    }

    #[test]
    fn priorities_are_unique_and_contiguous() {
        let all = every_variant();

        let mut priorities: Vec<u8> = all.iter().map(AttentionReason::priority).collect();
        priorities.sort_unstable();
        assert_eq!(priorities, (1..=7).collect::<Vec<u8>>());
    }

    #[test]
    fn every_variant_survives_a_round_trip_through_json() {
        // The ledger stores reasons as JSON and renders them on read, so a
        // variant that didn't round-trip would come back as a lost queue row.
        for reason in every_variant() {
            let json = serde_json::to_string(&reason).expect("serialises");
            let back: AttentionReason = serde_json::from_str(&json).expect("deserialises");
            assert_eq!(back, reason, "{json}");
        }
    }

    #[test]
    fn discriminants_are_unique() {
        let all = every_variant();

        let mut names: Vec<&str> = all.iter().map(AttentionReason::discriminant).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn short_sha_handles_short_input() {
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha("abc123f89012345"), "abc123f");
    }

    #[test]
    fn priority_beats_staleness_in_the_sort() {
        let old_low = Attention {
            reason: AttentionReason::NeedsFirstLook { rule: "x".into() },
            since: "2026-01-01T00:00:00Z".parse().unwrap(),
        };
        let new_high = Attention {
            reason: AttentionReason::Mention { by: "a".into() },
            since: "2026-08-01T00:00:00Z".parse().unwrap(),
        };
        assert!(new_high < old_low);
    }

    #[test]
    fn staleness_orders_within_a_priority_band() {
        let older = Attention {
            reason: AttentionReason::Mention { by: "a".into() },
            since: "2026-01-01T00:00:00Z".parse().unwrap(),
        };
        let newer = Attention {
            reason: AttentionReason::Mention { by: "b".into() },
            since: "2026-08-01T00:00:00Z".parse().unwrap(),
        };
        assert!(older < newer);
    }
}
