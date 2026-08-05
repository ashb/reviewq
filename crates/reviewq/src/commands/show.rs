//! `reviewq show <number>`: everything the ledger knows about one PR — why it's
//! tracked, every attention reason it holds, and its review threads. Read-only.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_ledger::{Ledger, PrShow};
use serde::Serialize;

use crate::cli::ShowArgs;
use crate::commands::EXIT_EMPTY;
use crate::paths;

pub fn run(_config_path: Option<&Path>, args: &ShowArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let Some(show) = ledger.show(args.number)? else {
        if args.json {
            println!("null");
        } else {
            eprintln!("#{} is not in the ledger — run `reviewq sync`", args.number);
        }
        return Ok(ExitCode::from(EXIT_EMPTY));
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json(&show))?);
    } else {
        print_human(&show);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_human(show: &PrShow) {
    let pr = &show.pr;
    println!(
        "{} {}",
        format!("#{}", pr.number).if_supports_color(Stdout, |s| s.bold().to_string()),
        pr.title,
    );
    println!(
        "  {} · @{} · {}",
        pr.state.as_str(),
        pr.author,
        show.tracked_reason.as_deref().unwrap_or("untracked"),
    );

    if show.attention.is_empty() {
        println!("  attention: none");
    } else {
        println!("  attention:");
        for a in &show.attention {
            println!(
                "    {} {}",
                format!("[p{}]", a.priority).if_supports_color(Stdout, |s| s.dimmed().to_string()),
                a.detail.if_supports_color(Stdout, |s| s.cyan().to_string()),
            );
        }
    }

    let owned = show.threads.iter().filter(|t| t.i_own).count();
    if !show.threads.is_empty() {
        println!(
            "  threads: {} ({} you own, {} resolved)",
            show.threads.len(),
            owned,
            show.threads.iter().filter(|t| t.is_resolved).count(),
        );
    }
}

#[derive(Serialize)]
struct ShowJson<'a> {
    number: u64,
    state: &'a str,
    title: &'a str,
    author: &'a str,
    tracked_reason: Option<&'a str>,
    attention: Vec<AttentionJson<'a>>,
    threads: usize,
    threads_i_own: usize,
}

#[derive(Serialize)]
struct AttentionJson<'a> {
    reason: &'a str,
    detail: &'a str,
    priority: u8,
    since: String,
}

fn json(show: &PrShow) -> ShowJson<'_> {
    ShowJson {
        number: show.pr.number,
        state: show.pr.state.as_str(),
        title: &show.pr.title,
        author: &show.pr.author,
        tracked_reason: show.tracked_reason.as_deref(),
        attention: show
            .attention
            .iter()
            .map(|a| AttentionJson {
                reason: &a.reason,
                detail: &a.detail,
                priority: a.priority,
                since: a.since.to_string(),
            })
            .collect(),
        threads: show.threads.len(),
        threads_i_own: show.threads.iter().filter(|t| t.i_own).count(),
    }
}
