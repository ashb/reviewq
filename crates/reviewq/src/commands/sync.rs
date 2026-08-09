//! `reviewq sync`: run the sync engine and print what it's doing.
//!
//! The engine itself lives in `reviewq-app`; all that's here is the CLI's
//! rendering of its progress — page counts on stderr, one summary line per repo
//! on stdout.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use reviewq_app::sync::{RepoSummary, SyncProgress, summary_line};

pub async fn run(config_path: Option<&Path>, logging: bool) -> Result<ExitCode> {
    let mut progress = StderrProgress::new(logging);
    reviewq_app::sync::run(config_path, &mut progress).await
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
