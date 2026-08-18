//! `reviewq list` and `reviewq next`: read the ledger and show the queue. Never
//! hits the network — the queue is whatever the last `sync` computed.
//!
//! Default is the queue: PRs with a live attention reason, most-urgent first.
//! `--all` groups everything tracked by state; `--waiting` shows the tracked
//! PRs that want nothing right now.
//!
//! Every repo the ledger knows about is queried and merged into one view —
//! whatever `sync` actually populated, rather than what config lists today. A
//! valid config is still required to get this far, like every command: one that
//! doesn't parse means the rules and identity behind the queue on screen are
//! unknown.

use std::process::ExitCode;

use anyhow::Result;
use crossterm::style::Stylize;
use jiff::Timestamp;
use reviewq_app::present;
use reviewq_core::model::{MyState, PrSnapshot, PrState};
use reviewq_ledger::{Ledger, Located, QueueItem, RepoKey, TrackedPr};
use serde::Serialize;

use reviewq_app::config::{Loaded, Marks};

use crate::cli::{ListArgs, NextArgs};
use crate::colour::{self, Output, Span};
use crate::commands::EXIT_EMPTY;
use reviewq_app::paths;

pub fn run(loaded: &Loaded, args: &ListArgs, output: &impl Output) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;
    let now = Timestamp::now();
    let marks = &loaded.config.output.marks;

    if args.all {
        let tracked = ledger.tracked_all()?;
        if args.json {
            print_tracked_json(output, &tracked)?;
        } else if tracked.is_empty() {
            output.eprintln("nothing tracked yet — run `reviewq sync`");
        } else {
            print_grouped(output, multi, now, marks, &tracked);
        }
        return Ok(empty_code(tracked.is_empty()));
    }

    if args.waiting {
        let waiting = ledger.waiting_all()?;
        if args.json {
            print_tracked_json(output, &waiting)?;
        } else if waiting.is_empty() {
            output.eprintln("nothing waiting");
        } else {
            for item in &waiting {
                output.line(tracked_row(multi, now, marks, item));
            }
        }
        return Ok(empty_code(waiting.is_empty()));
    }

    // What a mute hides, rather than what it leaves. Same rows, same order, and
    // each still carries the reason it would have been listed for — a mute stops
    // it being shown, not being computed.
    if args.muted {
        let muted = ledger.muted_all()?;
        if args.json {
            print_queue_json(output, &muted)?;
        } else if muted.is_empty() {
            output.eprintln("nothing muted");
        } else {
            for item in &muted {
                output.line(queue_row(multi, now, marks, item));
            }
        }
        return Ok(empty_code(muted.is_empty()));
    }

    let queue = ledger.queue_all()?;
    if args.json {
        print_queue_json(output, &queue)?;
    } else if queue.is_empty() {
        output.eprintln("queue is empty — run `reviewq sync`, or try `reviewq list --waiting`");
    } else {
        for item in &queue {
            output.line(queue_row(multi, now, marks, item));
        }
    }
    Ok(empty_code(queue.is_empty()))
}

pub fn next(loaded: &Loaded, args: &NextArgs, output: &impl Output) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;
    let now = Timestamp::now();
    let marks = &loaded.config.output.marks;
    let top = ledger.queue_all()?.into_iter().next();

    match top {
        None => {
            if args.json {
                output.println("null");
            } else {
                output.eprintln("queue is empty");
            }
            Ok(ExitCode::from(EXIT_EMPTY))
        }
        Some(item) => {
            if args.json {
                output.println(serde_json::to_string_pretty(&queue_json(&item))?);
            } else {
                output.line(queue_row(multi, now, marks, &item));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn empty_code(empty: bool) -> ExitCode {
    if empty {
        ExitCode::from(EXIT_EMPTY)
    } else {
        ExitCode::SUCCESS
    }
}

/// `#42`, or `owner/name#42` when more than one repo is known — with one
/// repo there's nothing to disambiguate, so the label stays exactly as before.
fn number_label(multi: bool, repo: &RepoKey, number: u64) -> String {
    if multi {
        format!("{}#{number}", repo.slug())
    } else {
        format!("#{number}")
    }
}

/// Paint a reason the way its emphasis asks, in the terminal's own palette.
///
/// ANSI names rather than the interface's hexes on purpose: this output goes
/// wherever a terminal is, including ones with sixteen colours and a palette
/// their owner chose. What is shared with the interface is which rows shout —
/// [`present::emphasis`] — not what red is.
fn painted(reason: &str, emphasis: present::Emphasis) -> Span {
    match emphasis {
        present::Emphasis::Urgent => reason.to_string().dark_red().into(),
        present::Emphasis::Normal => reason.to_string().dark_cyan().into(),
        present::Emphasis::Quiet => reason.to_string().dim().into(),
    }
}

/// `[snoozed until 2026-08-15] ` for a PR that is, and nothing at all for the
/// many that are not.
///
/// In front of the title with the state tag, because both say what is true of
/// the PR rather than why it is on the list — and because a row that is quiet
/// for a reason should say so before the reader wonders why nothing is
/// happening on it. `--waiting` is where these mostly land: a snooze clears the
/// attention, which is exactly what puts a PR there.
fn snooze_tag(my: &MyState, now: Timestamp) -> Vec<Span> {
    present::snoozed_tag(my, now).map_or_else(Vec::new, |tag| {
        vec![format!("[{tag}]").dim().into(), colour::plain(" ")]
    })
}

/// The one column in front of a row saying where you stand with the PR — the
/// same glyphs, from the same config, that the interface draws: `✓` a review you
/// submitted, `·` a `done` of your own, and the deferred mark for one you sank.
///
/// Occupies its column marked or not, so the numbers beside it line up either
/// way. Dimmed once the PR has moved past the head the mark names, and for a
/// deferred row — which is how the interface says the same two things, in the
/// colours it has.
fn mark_cell(pr: &PrSnapshot, my: &MyState, deferred: bool, marks: &Marks) -> Span {
    let Some(mark) = present::mark(pr, my, deferred) else {
        return colour::plain(" ");
    };
    let glyph = marks.glyph(mark).to_string();
    match mark {
        present::Mark::Handled { current: true, .. } => glyph.dark_green().into(),
        present::Mark::Handled { .. } | present::Mark::Deferred => glyph.dim().into(),
    }
}

fn queue_row(multi: bool, now: Timestamp, marks: &Marks, item: &Located<QueueItem>) -> Vec<Span> {
    // A merged PR only reaches the queue when a project opted into post-merge
    // review; tag it so it doesn't read as still-open.
    let tag: Vec<Span> = if item.item.pr.state.is_open() {
        Vec::new()
    } else {
        vec![
            format!("[{}]", item.item.pr.state.as_str())
                .dark_yellow()
                .into(),
            colour::plain(" "),
        ]
    };
    // Padded before it is painted: a width applies to what it is given, and
    // styling only ever happens at render time, on a copy — so padding the
    // plain value first always lines the columns up correctly.
    let mut line = vec![
        colour::plain("  "),
        mark_cell(
            &item.item.pr,
            &item.item.my_state,
            item.item.deferred,
            marks,
        ),
        colour::plain(" "),
        format!(
            "{:>7}",
            number_label(multi, &item.repo, item.item.pr.number)
        )
        .dim()
        .into(),
        colour::plain("  "),
        painted(
            &item.item.top.reason.to_string(),
            present::emphasis(Some(item.item.top.priority()), item.item.deferred),
        ),
        colour::plain("  "),
    ];
    line.extend(tag);
    line.extend(snooze_tag(&item.item.my_state, now));
    line.push(colour::plain(truncate(&item.item.pr.title, 60)));
    line
}

fn tracked_row(multi: bool, now: Timestamp, marks: &Marks, item: &Located<TrackedPr>) -> Vec<Span> {
    let mut line = vec![
        colour::plain("  "),
        // A tracked row's defer comes from `my_state`: there is no attention for
        // it to still be standing against, which is what the queue's own
        // `deferred` means.
        mark_cell(
            &item.item.pr,
            &item.item.my_state,
            item.item.my_state.deferred_at.is_some(),
            marks,
        ),
        colour::plain(" "),
        format!(
            "{:>7}",
            number_label(multi, &item.repo, item.item.pr.number)
        )
        .dim()
        .into(),
        colour::plain("  "),
        // No attention at all — this row is saying why it is watched, which is
        // the interface's quiet case too.
        painted(
            &format!("{:<44}", item.item.tracked_reason),
            present::emphasis(None, item.item.my_state.deferred_at.is_some()),
        ),
        colour::plain("  "),
    ];
    line.extend(snooze_tag(&item.item.my_state, now));
    line.push(colour::plain(truncate(&item.item.pr.title, 60)));
    line
}

/// Open first, then merged, then closed; each group by number ascending (which
/// `list_tracked` already guarantees).
const STATE_ORDER: [PrState; 3] = [PrState::Open, PrState::Merged, PrState::Closed];

fn print_grouped(
    output: &impl Output,
    multi: bool,
    now: Timestamp,
    marks: &Marks,
    tracked: &[Located<TrackedPr>],
) {
    let mut first = true;
    for state in STATE_ORDER {
        let group: Vec<&Located<TrackedPr>> = tracked
            .iter()
            .filter(|t| t.item.pr.state == state)
            .collect();
        if group.is_empty() {
            continue;
        }
        // Blank line between groups so a header doesn't sit flush against the
        // previous group's last row.
        if !first {
            output.println("");
        }
        first = false;
        output.println(Span::from(state.as_str().to_string().bold()));
        for item in group {
            output.line(tracked_row(multi, now, marks, item));
        }
    }
}

#[derive(Serialize)]
struct QueueJson<'a> {
    repo: String,
    number: u64,
    reason: &'a str,
    /// Rendered on the way out — the ledger stores the reason, not its prose.
    detail: String,
    since: String,
    tracked_reason: &'a str,
    title: &'a str,
    author: &'a str,
}

fn queue_json(item: &Located<QueueItem>) -> QueueJson<'_> {
    QueueJson {
        repo: item.repo.slug(),
        number: item.item.pr.number,
        reason: item.item.top.reason.discriminant(),
        detail: item.item.top.reason.to_string(),
        since: item.item.top.since.to_string(),
        tracked_reason: &item.item.tracked_reason,
        title: &item.item.pr.title,
        author: &item.item.pr.author,
    }
}

fn print_queue_json(output: &impl Output, queue: &[Located<QueueItem>]) -> Result<()> {
    let items: Vec<QueueJson<'_>> = queue.iter().map(queue_json).collect();
    output.println(serde_json::to_string_pretty(&items)?);
    Ok(())
}

#[derive(Serialize)]
struct TrackedJson<'a> {
    repo: String,
    number: u64,
    state: &'a str,
    tracked_reason: &'a str,
    title: &'a str,
    author: &'a str,
    is_draft: bool,
    updated_at: String,
}

fn print_tracked_json(output: &impl Output, tracked: &[Located<TrackedPr>]) -> Result<()> {
    let items: Vec<TrackedJson<'_>> = tracked
        .iter()
        .map(|t| TrackedJson {
            repo: t.repo.slug(),
            number: t.item.pr.number,
            state: t.item.pr.state.as_str(),
            tracked_reason: &t.item.tracked_reason,
            title: &t.item.pr.title,
            author: &t.item.pr.author,
            is_draft: t.item.pr.is_draft,
            updated_at: t.item.pr.updated_at.to_string(),
        })
        .collect();
    output.println(serde_json::to_string_pretty(&items)?);
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colour::testing::FakeOutput;
    use reviewq_core::model::AttentionReason;
    use reviewq_ledger::AttentionRow;

    /// Rendering with an explicit colour setting keeps this assertion
    /// independent of the terminal running the test.
    #[test]
    fn painting_a_reason_changes_nothing_but_its_colour() {
        for emphasis in [
            present::Emphasis::Urgent,
            present::Emphasis::Normal,
            present::Emphasis::Quiet,
        ] {
            assert_eq!(
                colour::render(false, painted("@kaxil mentioned you", emphasis)),
                "@kaxil mentioned you"
            );
            assert_ne!(
                colour::render(true, painted("@kaxil mentioned you", emphasis)),
                "@kaxil mentioned you",
                "{emphasis:?} is supposed to carry a colour"
            );
        }
    }

    #[test]
    fn a_padded_reason_keeps_its_width_through_painting() {
        // The bug this shape avoids: a width applied to an already-coloured
        // string pads by counting escape bytes, so the columns only line up
        // when nothing is coloured.
        let padded = format!("{:<44}", "interest: label area:task-sdk");
        assert_eq!(
            colour::render(false, painted(&padded, present::Emphasis::Quiet)).len(),
            44
        );
    }

    #[test]
    fn a_snoozed_row_says_so_until_the_snooze_lapses() {
        let my = MyState {
            snoozed_until: Some("2026-08-15T09:00:00Z".parse().unwrap()),
            ..MyState::default()
        };

        assert_eq!(
            colour::render(
                false,
                snooze_tag(&my, "2026-08-13T10:00:00Z".parse().unwrap())
            ),
            "[snoozed until 2026-08-15] ",
            "with its own trailing space, so the title needs no separator of its own"
        );
        assert_eq!(
            colour::render(
                false,
                snooze_tag(&my, "2026-08-16T10:00:00Z".parse().unwrap())
            ),
            "",
            "and nothing once it has lapsed"
        );
        assert_eq!(
            colour::render(
                false,
                snooze_tag(&MyState::default(), "2026-08-13T10:00:00Z".parse().unwrap())
            ),
            ""
        );
    }

    fn pr() -> PrSnapshot {
        PrSnapshot {
            number: 62922,
            title: "AIP-104".into(),
            author: "dabla".into(),
            author_association: "MEMBER".into(),
            head_sha: "head0000".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: "2026-08-10T09:00:00Z".parse().unwrap(),
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    #[test]
    fn a_row_carries_the_same_mark_the_interface_would_draw() {
        // The gap this closes: a deferred PR sorts to the bottom of both, and
        // only one of them said why it was down there.
        let marks = Marks::default();
        let untouched = mark_cell(&pr(), &MyState::default(), false, &marks);
        assert_eq!(
            colour::render(false, untouched),
            " ",
            "the column is held whether marked or not"
        );

        let deferred = mark_cell(&pr(), &MyState::default(), true, &marks);
        assert_eq!(colour::render(false, deferred), marks.deferred);

        let reviewed = MyState {
            last_reviewed_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(
            colour::render(false, mark_cell(&pr(), &reviewed, false, &marks)),
            marks.reviewed
        );

        let done = MyState {
            done_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(
            colour::render(false, mark_cell(&pr(), &done, false, &marks)),
            marks.done
        );
    }

    #[test]
    fn a_configured_mark_is_what_a_row_draws() {
        // A terminal without a patched font says so in config, and the list has
        // to honour that as much as the interface does.
        let marks = Marks {
            deferred: "z".into(),
            ..Marks::default()
        };
        assert_eq!(
            colour::render(false, mark_cell(&pr(), &MyState::default(), true, &marks)),
            "z"
        );
    }

    fn located<T>(item: T) -> Located<T> {
        Located {
            repo: RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            },
            repo_id: 1,
            item,
        }
    }

    #[test]
    fn queue_row_preserves_column_spacing() {
        let now: Timestamp = "2026-08-13T10:00:00Z".parse().unwrap();
        let marks = Marks::default();
        let item = located(QueueItem {
            pr: pr(),
            tracked_reason: "interest: label x".into(),
            top: AttentionRow {
                reason: AttentionReason::Mention { by: "kaxil".into() },
                since: now,
            },
            my_state: MyState::default(),
            deferred: false,
        });

        assert_eq!(
            colour::render(false, queue_row(false, now, &marks, &item)),
            "     #62922  @kaxil mentioned you  AIP-104"
        );
    }

    #[test]
    fn tracked_row_preserves_column_spacing() {
        let now: Timestamp = "2026-08-13T10:00:00Z".parse().unwrap();
        let marks = Marks::default();
        let item = located(TrackedPr {
            pr: pr(),
            tracked_reason: "interest: label x".into(),
            after_merge: false,
            my_state: MyState::default(),
        });

        assert_eq!(
            colour::render(false, tracked_row(false, now, &marks, &item)),
            "     #62922  interest: label x                             AIP-104"
        );
    }

    #[test]
    fn grouped_output_writes_separators_through_the_output_sink() {
        let now: Timestamp = "2026-08-13T10:00:00Z".parse().unwrap();
        let marks = Marks::default();
        let open = located(TrackedPr {
            pr: pr(),
            tracked_reason: "interest: label x".into(),
            after_merge: false,
            my_state: MyState::default(),
        });
        let mut merged = open.clone();
        merged.item.pr.state = PrState::Merged;
        let output = FakeOutput::new(false);

        print_grouped(&output, false, now, &marks, &[open, merged]);

        assert_eq!(output.lines.borrow()[0], "OPEN");
        assert_eq!(output.lines.borrow()[2], "");
        assert_eq!(output.lines.borrow()[3], "MERGED");
    }

    #[test]
    fn truncate_keeps_short_titles_and_ellipsises_long_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 10), "0123456789");
        assert_eq!(truncate("0123456789a", 10), "012345678…");
    }
}
