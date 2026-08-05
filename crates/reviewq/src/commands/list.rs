//! `reviewq list`: show tracked PRs. Reads the ledger only; never hits the
//! network.
//!
//! `--all` groups everything tracked by state. The queue (`list` with no flag)
//! and `--waiting` need the attention state machine, which is not built yet, so
//! they say so rather than pretending.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, bail};
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::PrState;
use reviewq_ledger::{Ledger, TrackedPr};
use serde::Serialize;

use crate::cli::ListArgs;
use crate::commands::EXIT_EMPTY;
use crate::paths;

pub fn run(_config_path: Option<&Path>, args: &ListArgs) -> Result<ExitCode> {
    if !args.all {
        bail!(
            "the queue and --waiting need the attention state machine, which isn't built yet; \
             use `reviewq list --all`"
        );
    }

    let ledger = Ledger::open(&paths::database_file()?)?;
    let tracked = ledger.list_tracked()?;

    if args.json {
        print_json(&tracked)?;
    } else {
        print_grouped(&tracked);
    }

    Ok(if tracked.is_empty() {
        ExitCode::from(EXIT_EMPTY)
    } else {
        ExitCode::SUCCESS
    })
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
            println!(
                "  {:>7}  {:<44}  {}",
                format!("#{}", item.pr.number)
                    .if_supports_color(Stdout, |s| s.dimmed().to_string()),
                item.tracked_reason
                    .if_supports_color(Stdout, |s| s.cyan().to_string()),
                truncate(&item.pr.title, 60),
            );
        }
    }
}

#[derive(Serialize)]
struct JsonItem<'a> {
    number: u64,
    state: &'a str,
    tracked_reason: &'a str,
    title: &'a str,
    author: &'a str,
    is_draft: bool,
    updated_at: String,
}

fn print_json(tracked: &[TrackedPr]) -> Result<()> {
    let items: Vec<JsonItem<'_>> = tracked
        .iter()
        .map(|t| JsonItem {
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
