//! `reviewq list` and `reviewq next`: read the ledger and show the queue. Never
//! hits the network — the queue is whatever the last `sync` computed.
//!
//! Default is the queue: PRs with a live attention reason, most-urgent first.
//! `--all` groups everything tracked by state; `--waiting` shows the tracked
//! PRs that want nothing right now.
//!
//! Ledger-only, like every other read/write command: every repo the ledger
//! knows about (whatever `sync` has actually populated, not what a possibly
//! stale config currently lists) is queried and merged into one view.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::PrState;
use reviewq_ledger::{Ledger, Located, QueueItem, RepoKey, TrackedPr};
use serde::Serialize;

use crate::cli::{ListArgs, NextArgs};
use crate::commands::EXIT_EMPTY;
use reviewq_app::paths;

pub fn run(_config_path: Option<&Path>, args: &ListArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;

    if args.all {
        let tracked = ledger.tracked_all()?;
        if args.json {
            print_tracked_json(&tracked)?;
        } else {
            print_grouped(multi, &tracked);
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
                print_tracked_row(multi, item);
            }
        }
        return Ok(empty_code(waiting.is_empty()));
    }

    let queue = ledger.queue_all()?;
    if args.json {
        print_queue_json(&queue)?;
    } else if queue.is_empty() {
        eprintln!("queue is empty — run `reviewq sync`, or try `reviewq list --waiting`");
    } else {
        for item in &queue {
            print_queue_row(multi, item);
        }
    }
    Ok(empty_code(queue.is_empty()))
}

pub fn next(_config_path: Option<&Path>, args: &NextArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let multi = ledger.repos()?.len() > 1;
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
                print_queue_row(multi, &item);
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

fn print_queue_row(multi: bool, item: &Located<QueueItem>) {
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
    println!(
        "  {:>7}  {}  {}{}",
        number_label(multi, &item.repo, item.item.pr.number)
            .if_supports_color(Stdout, |s| s.dimmed().to_string()),
        item.item
            .top
            .reason
            .to_string()
            .if_supports_color(Stdout, |s| s.cyan().to_string()),
        tag,
        truncate(&item.item.pr.title, 60),
    );
}

fn print_tracked_row(multi: bool, item: &Located<TrackedPr>) {
    println!(
        "  {:>7}  {:<44}  {}",
        number_label(multi, &item.repo, item.item.pr.number)
            .if_supports_color(Stdout, |s| s.dimmed().to_string()),
        item.item
            .tracked_reason
            .if_supports_color(Stdout, |s| s.cyan().to_string()),
        truncate(&item.item.pr.title, 60),
    );
}

/// Open first, then merged, then closed; each group by number ascending (which
/// `list_tracked` already guarantees).
const STATE_ORDER: [PrState; 3] = [PrState::Open, PrState::Merged, PrState::Closed];

fn print_grouped(multi: bool, tracked: &[Located<TrackedPr>]) {
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
            print_tracked_row(multi, item);
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
