use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// A deterministic PR review queue.
///
/// Every queue item names the rule that produced it.
#[derive(Debug, Parser)]
#[command(name = "reviewq", version, about, long_about = None)]
pub struct Cli {
    /// Config file to use (default: $XDG_CONFIG_HOME/reviewq/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Raise log level; repeat for more (-v = info, -vv = debug, -vvv = trace).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch updates from the forge and rebuild the ledger.
    Sync,

    /// Show the queue: PRs that want attention, most-urgent first.
    List(ListArgs),

    /// Show just the single most-urgent PR.
    Next(NextArgs),

    /// Show everything known about one PR: why it's tracked, its attention
    /// reasons and its threads.
    Show(ShowArgs),

    /// Record the current head as handled and mark matching GitHub
    /// notifications read. Drops the PR off the queue until something new
    /// happens on it.
    Done(NumberArgs),

    /// Suppress everything on a PR — including mentions — until `duration` has
    /// passed, e.g. `3d`, `12h`, `1w`.
    Snooze(SnoozeArgs),

    /// Suppress everything on a PR, including mentions, until `unmute`.
    Mute(NumberArgs),

    /// Undo `mute`.
    Unmute(NumberArgs),

    /// Push a PR to the bottom of the queue without hiding it. Clears itself
    /// the next time something new happens on the PR.
    Defer(NumberArgs),

    /// Undo `defer`.
    Undefer(NumberArgs),

    /// Force-track a PR that matched no interest rule.
    Track(NumberArgs),

    /// Exec `handoff.review_command` with the PR number substituted. Does not
    /// imply `done`.
    Review(NumberArgs),

    /// Check the token, the rate-limit budget and where things live on disk.
    Doctor,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Show everything tracked, grouped by state, rather than just the queue.
    #[arg(long, conflicts_with = "waiting")]
    pub all: bool,

    /// Show the tracked PRs that want nothing right now — seen, waiting on
    /// someone else.
    #[arg(long)]
    pub waiting: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct NextArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    /// The PR number.
    pub number: u64,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Shared by every action command that names one PR and takes no other input.
#[derive(Debug, Args)]
pub struct NumberArgs {
    /// The PR number.
    pub number: u64,
}

#[derive(Debug, Args)]
pub struct SnoozeArgs {
    /// The PR number.
    pub number: u64,

    /// How long to suppress it, e.g. `3d`, `12h`, `1w2d`.
    pub duration: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn global_flags_are_accepted_after_the_subcommand() {
        let cli = Cli::try_parse_from(["reviewq", "doctor", "-vv"]).expect("parses");
        assert_eq!(cli.verbose, 2);
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(Cli::try_parse_from(["reviewq"]).is_err());
    }
}
