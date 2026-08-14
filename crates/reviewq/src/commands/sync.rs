//! `reviewq sync`: run the sync engine and print what it's doing.
//!
//! The engine itself lives in `reviewq-app`; all that's here is the CLI's
//! rendering of its progress — page counts on stderr, one summary line per repo
//! on stdout.

use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::Result;
use reviewq_app::config::{Config, Loaded};
use reviewq_app::sync::{Refreshed, RepoSummary, SyncProgress, summary_line};
use reviewq_core::model::PrState;

use crate::cli::SyncArgs;
use crate::commands::EXIT_EMPTY;

pub async fn run(loaded: &Loaded, args: &SyncArgs, logging: bool) -> Result<ExitCode> {
    if let Some(number) = args.number {
        return one(&loaded.config, number).await;
    }
    let mut progress = StderrProgress::new(logging);
    let which = match args.all {
        true => reviewq_ledger::Detail::Every,
        false => reviewq_ledger::Detail::Stale,
    };
    reviewq_app::sync::run(&loaded.config, args.labels, which, &mut progress).await
}

/// `reviewq sync <number>`: refresh one PR's detail and say what changed.
async fn one(cfg: &Config, number: u64) -> Result<ExitCode> {
    match reviewq_app::sync::sync_one(cfg, number).await? {
        Refreshed::Untracked => {
            eprintln!("#{number} is not in the ledger — run `reviewq sync` first");
            Ok(ExitCode::from(EXIT_EMPTY))
        }
        Refreshed::Gone => {
            eprintln!("#{number} no longer exists on the forge — dropped from the queue");
            Ok(ExitCode::SUCCESS)
        }
        Refreshed::Updated {
            repo,
            state,
            queued,
            cost,
            remaining,
        } => {
            // A PR that is no longer open wants nothing whatever its reasons
            // once said, so saying which it is beats reporting the absence.
            let standing = match state {
                PrState::Closed => "closed on the forge",
                PrState::Merged => "merged",
                PrState::Open if queued => "wants attention",
                PrState::Open => "wants nothing",
            };
            println!("sync {repo}#{number}: {standing}; {cost} pts, {remaining} left");
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The CLI's progress sink: pages on stderr, so stdout carries only the
/// per-repo summary and stays pipeable.
struct StderrProgress {
    /// Rewrite a single line with `\r` rather than printing one per page. Only
    /// tidy when nothing else is writing to stderr, so it's off when logs are
    /// interleaved (`-v`) or stderr isn't a terminal.
    in_place: bool,
    /// An in-place line is on screen without its newline yet.
    open_line: bool,
}

impl StderrProgress {
    fn new(logging: bool) -> Self {
        Self {
            in_place: std::io::stderr().is_terminal() && !logging,
            open_line: false,
        }
    }
}

impl SyncProgress for StderrProgress {
    fn page(&mut self, what: &str, fetched: usize, total: u32) {
        let msg = format!("{what}: {fetched}/{total} PRs");
        let mut err = std::io::stderr().lock();
        if self.in_place {
            // \x1b[K clears the rest of the line after the (possibly shorter) update.
            let _ = write!(err, "\r  {msg}\x1b[K");
            self.open_line = true;
        } else {
            let _ = writeln!(err, "  {msg}");
        }
        let _ = err.flush();
    }

    fn repo_finished(&mut self, summary: &RepoSummary) {
        // Close the progress line before the summary goes to stdout — but only
        // if one was actually left open, so a repo that reported no pages at
        // all doesn't emit a stray blank line.
        if std::mem::take(&mut self.open_line) {
            let _ = writeln!(std::io::stderr());
        }
        println!("{}", summary_line(summary));
    }
}
