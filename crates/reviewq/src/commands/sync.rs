//! `reviewq sync`: run the sync engine and print what it's doing.
//!
//! The engine itself lives in `reviewq-app`; all that's here is the CLI's
//! rendering of its progress — page counts on stderr, one summary line per repo
//! on stdout.

use std::process::ExitCode;

use anyhow::Result;
use reviewq_app::config::{Config, Loaded};
use reviewq_app::sync::{Refreshed, RepoSummary, SyncProgress, summary_line};
use reviewq_core::model::PrState;

use crate::cli::SyncArgs;
use crate::colour::Output;
use crate::commands::EXIT_EMPTY;

pub async fn run(
    loaded: &Loaded,
    args: &SyncArgs,
    logging: bool,
    output: &impl Output,
) -> Result<ExitCode> {
    if let Some(number) = args.number {
        return one(&loaded.config, number, output).await;
    }
    let mut progress = StderrProgress::new(logging, output);
    let which = match args.all {
        true => reviewq_ledger::Detail::Every,
        false => reviewq_ledger::Detail::Stale,
    };
    reviewq_app::sync::run(&loaded.config, args.labels, which, &mut progress).await
}

/// `reviewq sync <number>`: refresh one PR's detail and say what changed.
async fn one(cfg: &Config, number: u64, output: &impl Output) -> Result<ExitCode> {
    match reviewq_app::sync::sync_one(cfg, number).await? {
        Refreshed::Untracked => {
            output.eprintln(format!(
                "#{number} is not in the ledger — run `reviewq sync` first"
            ));
            Ok(ExitCode::from(EXIT_EMPTY))
        }
        Refreshed::Gone => {
            output.eprintln(format!(
                "#{number} no longer exists on the forge — dropped from the queue"
            ));
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
            output.println(format!(
                "sync {repo}#{number}: {standing}; {cost} pts, {remaining} left"
            ));
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The CLI's progress sink: pages on stderr, so stdout carries only the
/// per-repo summary and stays pipeable.
struct StderrProgress<'a, O> {
    /// Rewrite a single line rather than printing one per page. Only tidy when
    /// fancy output is enabled and nothing else is writing to stderr.
    in_place: bool,
    /// An in-place line is on screen without its newline yet.
    open_line: bool,
    output: &'a O,
}

impl<'a, O: Output> StderrProgress<'a, O> {
    fn new(logging: bool, output: &'a O) -> Self {
        Self {
            in_place: output.stderr_is_terminal() && output.colour_enabled() && !logging,
            open_line: false,
            output,
        }
    }
}

impl<O: Output> SyncProgress for StderrProgress<'_, O> {
    fn page(&mut self, what: &str, fetched: usize, total: u32) {
        let msg = format!("{what}: {fetched}/{total} PRs");
        if self.in_place {
            let _ = self.output.replace_stderr_line(&format!("  {msg}"));
            self.open_line = true;
        } else {
            self.output.eprintln(format!("  {msg}"));
        }
        let _ = self.output.flush();
    }

    fn repo_finished(&mut self, summary: &RepoSummary) {
        // Close the progress line before the summary goes to stdout — but only
        // if one was actually left open, so a repo that reported no pages at
        // all doesn't emit a stray blank line.
        if std::mem::take(&mut self.open_line) {
            self.output.eprintln("");
        }
        self.output.println(summary_line(summary));
    }
}

#[cfg(test)]
mod tests {
    use reviewq_app::sync::SyncProgress as _;

    use super::*;
    use crate::colour::testing::FakeOutput;

    #[test]
    fn progress_rewrites_terminal_stderr_when_fancy_output_is_enabled() {
        let output = FakeOutput::new(true).with_stderr_terminal();
        let mut progress = StderrProgress::new(false, &output);

        progress.page("search", 20, 100);

        assert_eq!(&*output.stderr.borrow(), "  search: 20/100 PRs");
        assert_eq!(output.flushes.get(), 1);
    }

    #[test]
    fn progress_uses_complete_lines_when_fancy_output_is_disabled() {
        let output = FakeOutput::new(false).with_stderr_terminal();
        let mut progress = StderrProgress::new(false, &output);

        progress.page("search", 20, 100);

        assert_eq!(&*output.stderr.borrow(), "  search: 20/100 PRs\n");
        assert_eq!(output.flushes.get(), 1);
    }
}
