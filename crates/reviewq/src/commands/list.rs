//! `reviewq list` and `reviewq next`: read the ledger and show the queue. Never
//! hits the network — the queue is whatever the last `sync` computed.
//!
//! Default is the queue: PRs with a live attention reason, most-urgent first.
//! `--all` groups everything tracked by state; `--waiting` shows the tracked
//! PRs that want nothing right now.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::PrState;
use reviewq_ledger::{Ledger, QueueItem, TrackedPr};
use serde::Serialize;

use crate::cli::{ListArgs, NextArgs};
use crate::commands::EXIT_EMPTY;
use crate::paths;

pub fn run(_config_path: Option<&Path>, args: &ListArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;

    if args.all {
        let tracked = ledger.list_tracked()?;
        if args.json {
            print_tracked_json(&tracked)?;
        } else {
            print_grouped(&tracked);
        }
        return Ok(empty_code(tracked.is_empty()));
    }

    if args.waiting {
        let waiting = ledger.waiting()?;
        if args.json {
            print_tracked_json(&waiting)?;
        } else if waiting.is_empty() {
            eprintln!("nothing waiting");
        } else {
            for item in &waiting {
                print_tracked_row(item);
            }
        }
        return Ok(empty_code(waiting.is_empty()));
    }

    let queue = ledger.queue()?;
    if args.json {
        print_queue_json(&queue)?;
    } else if queue.is_empty() {
        eprintln!("queue is empty — run `reviewq sync`, or try `reviewq list --waiting`");
    } else {
        for item in &queue {
            print_queue_row(item);
        }
    }
    Ok(empty_code(queue.is_empty()))
}

pub fn next(_config_path: Option<&Path>, args: &NextArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let top = ledger.queue()?.into_iter().next();

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
                print_queue_row(&item);
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

fn print_queue_row(item: &QueueItem) {
    // A merged PR only reaches the queue when a project opted into post-merge
    // review; tag it so it doesn't read as still-open.
    let tag = if item.pr.state.is_open() {
        String::new()
    } else {
        format!(
            "{} ",
            format!("[{}]", item.pr.state.as_str())
                .if_supports_color(Stdout, |s| s.yellow().to_string())
        )
    };
    println!(
        "  {:>7}  {}  {}{}",
        format!("#{}", item.pr.number).if_supports_color(Stdout, |s| s.dimmed().to_string()),
        item.top
            .detail
            .if_supports_color(Stdout, |s| s.cyan().to_string()),
        tag,
        truncate(&item.pr.title, 60),
    );
}

fn print_tracked_row(item: &TrackedPr) {
    println!(
        "  {:>7}  {:<44}  {}",
        format!("#{}", item.pr.number).if_supports_color(Stdout, |s| s.dimmed().to_string()),
        item.tracked_reason
            .if_supports_color(Stdout, |s| s.cyan().to_string()),
        truncate(&item.pr.title, 60),
    );
}

/// Open first, then merged, then closed; each group by number ascending (which
/// `list_tracked` already guarantees).
const STATE_ORDER: [PrState; 3] = [PrState::Open, PrState::Merged, PrState::Closed];

fn print_grouped(tracked: &[TrackedPr]) {
    if tracked.is_empty() {
        eprintln!("nothing tracked yet — run `reviewq sync`");
        return;
    }

    let mut first = true;
    for state in STATE_ORDER {
        let group: Vec<&TrackedPr> = tracked.iter().filter(|t| t.pr.state == state).collect();
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
            print_tracked_row(item);
        }
    }
}

#[derive(Serialize)]
struct QueueJson<'a> {
    number: u64,
    reason: &'a str,
    detail: &'a str,
    since: String,
    tracked_reason: &'a str,
    title: &'a str,
    author: &'a str,
}

fn queue_json(item: &QueueItem) -> QueueJson<'_> {
    QueueJson {
        number: item.pr.number,
        reason: &item.top.reason,
        detail: &item.top.detail,
        since: item.top.since.to_string(),
        tracked_reason: &item.tracked_reason,
        title: &item.pr.title,
        author: &item.pr.author,
    }
}

fn print_queue_json(queue: &[QueueItem]) -> Result<()> {
    let items: Vec<QueueJson<'_>> = queue.iter().map(queue_json).collect();
    println!("{}", serde_json::to_string_pretty(&items)?);
    Ok(())
}

#[derive(Serialize)]
struct TrackedJson<'a> {
    number: u64,
    state: &'a str,
    tracked_reason: &'a str,
    title: &'a str,
    author: &'a str,
    is_draft: bool,
    updated_at: String,
}

fn print_tracked_json(tracked: &[TrackedPr]) -> Result<()> {
    let items: Vec<TrackedJson<'_>> = tracked
        .iter()
        .map(|t| TrackedJson {
            number: t.pr.number,
            state: t.pr.state.as_str(),
            tracked_reason: &t.tracked_reason,
            title: &t.pr.title,
            author: &t.pr.author,
            is_draft: t.pr.is_draft,
            updated_at: t.pr.updated_at.to_string(),
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
