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
use jiff::Timestamp;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_app::present;
use reviewq_core::model::{MyState, PrSnapshot, PrState};
use reviewq_ledger::{Ledger, Located, QueueItem, RepoKey, TrackedPr};
use serde::Serialize;

use reviewq_app::config::{Loaded, Marks};

use crate::cli::{ListArgs, NextArgs};
use crate::commands::EXIT_EMPTY;
use reviewq_app::paths;

pub fn run(loaded: &Loaded, args: &ListArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;
    let now = Timestamp::now();
    let marks = &loaded.config.output.marks;

    if args.all {
        let tracked = ledger.tracked_all()?;
        if args.json {
            print_tracked_json(&tracked)?;
        } else {
            print_grouped(multi, now, marks, &tracked);
        }
        return Ok(empty_code(tracked.is_empty()));
    }

    if args.waiting {
        let waiting = ledger.waiting_all()?;
        if args.json {
            print_tracked_json(&waiting)?;
        } else if waiting.is_empty() {
            eprintln!("nothing waiting");
        } else {
            for item in &waiting {
                print_tracked_row(multi, now, marks, item);
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
            print_queue_json(&muted)?;
        } else if muted.is_empty() {
            eprintln!("nothing muted");
        } else {
            for item in &muted {
                print_queue_row(multi, now, marks, item);
            }
        }
        return Ok(empty_code(muted.is_empty()));
    }

    let queue = ledger.queue_all()?;
    if args.json {
        print_queue_json(&queue)?;
    } else if queue.is_empty() {
        eprintln!("queue is empty — run `reviewq sync`, or try `reviewq list --waiting`");
    } else {
        for item in &queue {
            print_queue_row(multi, now, marks, item);
        }
    }
    Ok(empty_code(queue.is_empty()))
}

pub fn next(loaded: &Loaded, args: &NextArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;
    let now = Timestamp::now();
    let marks = &loaded.config.output.marks;
    let top = ledger.queue_all()?.into_iter().next();

    match top {
        None => {
            if args.json {
                println!("null");
            } else {
                eprintln!("queue is empty");
            }
            Ok(ExitCode::from(EXIT_EMPTY))
        }
        Some(item) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&queue_json(&item))?);
            } else {
                print_queue_row(multi, now, marks, &item);
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
fn painted(reason: &str, emphasis: present::Emphasis) -> String {
    match emphasis {
        present::Emphasis::Urgent => reason
            .if_supports_color(Stdout, |s| s.red().to_string())
            .to_string(),
        present::Emphasis::Normal => reason
            .if_supports_color(Stdout, |s| s.cyan().to_string())
            .to_string(),
        present::Emphasis::Quiet => reason
            .if_supports_color(Stdout, |s| s.dimmed().to_string())
            .to_string(),
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
fn snooze_tag(my: &MyState, now: Timestamp) -> String {
    present::snoozed_tag(my, now).map_or_else(String::new, |tag| {
        format!(
            "{} ",
            format!("[{tag}]").if_supports_color(Stdout, |s| s.dimmed().to_string())
        )
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
fn mark_cell(pr: &PrSnapshot, my: &MyState, deferred: bool, marks: &Marks) -> String {
    let Some(mark) = present::mark(pr, my, deferred) else {
        return " ".to_string();
    };
    let glyph = marks.glyph(mark);
    match mark {
        present::Mark::Handled { current: true, .. } => glyph
            .if_supports_color(Stdout, |s| s.green().to_string())
            .to_string(),
        present::Mark::Handled { .. } | present::Mark::Deferred => glyph
            .if_supports_color(Stdout, |s| s.dimmed().to_string())
            .to_string(),
    }
}

fn print_queue_row(multi: bool, now: Timestamp, marks: &Marks, item: &Located<QueueItem>) {
    // A merged PR only reaches the queue when a project opted into post-merge
    // review; tag it so it doesn't read as still-open.
    let tag = if item.item.pr.state.is_open() {
        String::new()
    } else {
        format!(
            "{} ",
            format!("[{}]", item.item.pr.state.as_str())
                .if_supports_color(Stdout, |s| s.yellow().to_string())
        )
    };
    // Padded before it is painted: a width applies to what it is given, and
    // what a coloured string is given is escapes as well as text — so padding
    // the coloured one lines the columns up by counting characters nobody can
    // see. Only visible on a terminal, which is the only place it matters.
    println!(
        "  {} {}  {}  {}{}{}",
        mark_cell(
            &item.item.pr,
            &item.item.my_state,
            item.item.deferred,
            marks
        ),
        format!(
            "{:>7}",
            number_label(multi, &item.repo, item.item.pr.number)
        )
        .if_supports_color(Stdout, |s| s.dimmed().to_string()),
        painted(
            &item.item.top.reason.to_string(),
            present::emphasis(Some(item.item.top.priority()), item.item.deferred),
        ),
        tag,
        snooze_tag(&item.item.my_state, now),
        truncate(&item.item.pr.title, 60),
    );
}

fn print_tracked_row(multi: bool, now: Timestamp, marks: &Marks, item: &Located<TrackedPr>) {
    println!(
        "  {} {}  {}  {}{}",
        // A tracked row's defer comes from `my_state`: there is no attention for
        // it to still be standing against, which is what the queue's own
        // `deferred` means.
        mark_cell(
            &item.item.pr,
            &item.item.my_state,
            item.item.my_state.deferred_at.is_some(),
            marks
        ),
        format!(
            "{:>7}",
            number_label(multi, &item.repo, item.item.pr.number)
        )
        .if_supports_color(Stdout, |s| s.dimmed().to_string()),
        // No attention at all — this row is saying why it is watched, which is
        // the interface's quiet case too.
        painted(
            &format!("{:<44}", item.item.tracked_reason),
            present::emphasis(None, item.item.my_state.deferred_at.is_some()),
        ),
        snooze_tag(&item.item.my_state, now),
        truncate(&item.item.pr.title, 60),
    );
}

/// Open first, then merged, then closed; each group by number ascending (which
/// `list_tracked` already guarantees).
const STATE_ORDER: [PrState; 3] = [PrState::Open, PrState::Merged, PrState::Closed];

fn print_grouped(multi: bool, now: Timestamp, marks: &Marks, tracked: &[Located<TrackedPr>]) {
    if tracked.is_empty() {
        eprintln!("nothing tracked yet — run `reviewq sync`");
        return;
    }

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
            println!();
        }
        first = false;
        println!(
            "{}",
            state
                .as_str()
                .if_supports_color(Stdout, |s| s.bold().to_string())
        );
        for item in group {
            print_tracked_row(multi, now, marks, item);
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

fn print_queue_json(queue: &[Located<QueueItem>]) -> Result<()> {
    let items: Vec<QueueJson<'_>> = queue.iter().map(queue_json).collect();
    println!("{}", serde_json::to_string_pretty(&items)?);
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

fn print_tracked_json(tracked: &[Located<TrackedPr>]) -> Result<()> {
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
    println!("{}", serde_json::to_string_pretty(&items)?);
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

    /// `if_supports_color` asks about real stdout, which is never a terminal
    /// under `cargo test` — so every emphasis comes back as plain text here.
    /// What is worth asserting is what survives that: the text, and its width.
    #[test]
    fn painting_a_reason_changes_nothing_but_its_colour() {
        for emphasis in [
            present::Emphasis::Urgent,
            present::Emphasis::Normal,
            present::Emphasis::Quiet,
        ] {
            assert_eq!(
                painted("@kaxil mentioned you", emphasis),
                "@kaxil mentioned you"
            );
        }
    }

    #[test]
    fn a_padded_reason_keeps_its_width_through_painting() {
        // The bug this shape avoids: a width applied to an already-coloured
        // string pads by counting escape bytes, so the columns only line up
        // when nothing is coloured.
        let padded = format!("{:<44}", "interest: label area:task-sdk");
        assert_eq!(painted(&padded, present::Emphasis::Quiet).len(), 44);
    }

    #[test]
    fn a_snoozed_row_says_so_until_the_snooze_lapses() {
        let my = MyState {
            snoozed_until: Some("2026-08-15T09:00:00Z".parse().unwrap()),
            ..MyState::default()
        };

        assert_eq!(
            snooze_tag(&my, "2026-08-13T10:00:00Z".parse().unwrap()),
            "[snoozed until 2026-08-15] ",
            "with its own trailing space, so the title needs no separator of its own"
        );
        assert_eq!(
            snooze_tag(&my, "2026-08-16T10:00:00Z".parse().unwrap()),
            "",
            "and nothing once it has lapsed"
        );
        assert_eq!(
            snooze_tag(&MyState::default(), "2026-08-13T10:00:00Z".parse().unwrap()),
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
        assert_eq!(untouched, " ", "the column is held whether marked or not");

        let deferred = mark_cell(&pr(), &MyState::default(), true, &marks);
        assert_eq!(deferred, marks.deferred);

        let reviewed = MyState {
            last_reviewed_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(mark_cell(&pr(), &reviewed, false, &marks), marks.reviewed);

        let done = MyState {
            done_sha: Some("head0000".into()),
            ..MyState::default()
        };
        assert_eq!(mark_cell(&pr(), &done, false, &marks), marks.done);
    }

    #[test]
    fn a_configured_mark_is_what_a_row_draws() {
        // A terminal without a patched font says so in config, and the list has
        // to honour that as much as the interface does.
        let marks = Marks {
            deferred: "z".into(),
            ..Marks::default()
        };
        assert_eq!(mark_cell(&pr(), &MyState::default(), true, &marks), "z");
    }

    #[test]
    fn truncate_keeps_short_titles_and_ellipsises_long_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789", 10), "0123456789");
        assert_eq!(truncate("0123456789a", 10), "012345678…");
    }
}
