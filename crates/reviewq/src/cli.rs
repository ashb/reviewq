use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Parse a PR number, tolerating a leading `#` so a number copied straight out
/// of `list`/`show` output pastes in unedited.
fn pr_number(s: &str) -> Result<u64, String> {
    s.strip_prefix('#')
        .unwrap_or(s)
        .parse()
        .map_err(|_| format!("{s:?} is not a PR number"))
}

/// A PR number, and — when named by a full URL rather than a bare number —
/// the repo it's on. `show` prefers that over asking the ledger to search
/// for which configured repo the number belongs to: the URL already says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrTarget {
    pub number: u64,
    pub repo: Option<RepoUrl>,
}

/// A repo as named by a pull-request URL's own host/owner/name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoUrl {
    pub host: String,
    pub owner: String,
    pub name: String,
}

/// Like [`pr_number`], but also accepts a full pull-request URL pasted
/// straight from a browser — `https://github.com/owner/repo/pull/N` (or the
/// same on a GitHub Enterprise host).
fn pr_number_or_url(s: &str) -> Result<PrTarget, String> {
    let bad_url = || format!("{s:?} doesn't look like a pull request URL");
    let Some(after_scheme) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    else {
        return pr_number(s).map(|number| PrTarget { number, repo: None });
    };
    let (host, rest) = after_scheme.split_once('/').ok_or_else(bad_url)?;
    let (repo_path, tail) = rest.split_once("/pull/").ok_or_else(bad_url)?;
    let mut repo_parts = repo_path.split('/');
    let owner = repo_parts.next().ok_or_else(bad_url)?;
    let name = repo_parts.next().ok_or_else(bad_url)?;
    let number = tail
        .split('/')
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(bad_url)?;
    Ok(PrTarget {
        number,
        repo: Some(RepoUrl {
            host: host.to_string(),
            owner: owner.to_string(),
            name: name.to_string(),
        }),
    })
}

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

    /// Browse the queue interactively.
    Tui,

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
    /// The PR number, or a full pull-request URL.
    #[arg(value_parser = pr_number_or_url)]
    pub target: PrTarget,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

/// Shared by every action command that names one PR and takes no other input.
#[derive(Debug, Args)]
pub struct NumberArgs {
    /// The PR number.
    #[arg(value_parser = pr_number)]
    pub number: u64,
}

#[derive(Debug, Args)]
pub struct SnoozeArgs {
    /// The PR number.
    #[arg(value_parser = pr_number)]
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

    #[test]
    fn a_pr_number_sheds_a_leading_hash_so_pasted_output_just_works() {
        let cli = Cli::try_parse_from(["reviewq", "show", "#42"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Show(ShowArgs {
                target: PrTarget { number: 42, .. },
                ..
            })
        ));
    }

    #[test]
    fn a_bare_pr_number_still_parses() {
        let cli = Cli::try_parse_from(["reviewq", "mute", "42"]).expect("parses");
        assert!(matches!(
            cli.command,
            Command::Mute(NumberArgs { number: 42 })
        ));
    }

    #[test]
    fn garbage_after_the_hash_is_rejected() {
        assert!(Cli::try_parse_from(["reviewq", "mute", "#nope"]).is_err());
    }

    #[test]
    fn show_accepts_a_full_pull_request_url() {
        let cli = Cli::try_parse_from([
            "reviewq",
            "show",
            "https://github.com/apache/airflow/pull/70135",
        ])
        .expect("parses");
        let Command::Show(ShowArgs { target, .. }) = cli.command else {
            panic!("expected Show");
        };
        assert_eq!(target.number, 70135);
        assert_eq!(
            target.repo,
            Some(RepoUrl {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
        );
    }

    #[test]
    fn show_accepts_a_pull_request_url_on_an_enterprise_host() {
        let cli = Cli::try_parse_from([
            "reviewq",
            "show",
            "https://github.acme.example/acme/widgets/pull/7",
        ])
        .expect("parses");
        let Command::Show(ShowArgs { target, .. }) = cli.command else {
            panic!("expected Show");
        };
        assert_eq!(target.number, 7);
        assert_eq!(
            target.repo,
            Some(RepoUrl {
                host: "github.acme.example".into(),
                owner: "acme".into(),
                name: "widgets".into(),
            })
        );
    }

    #[test]
    fn show_still_accepts_a_bare_number_and_a_hash_number() {
        for arg in ["42", "#42"] {
            let cli = Cli::try_parse_from(["reviewq", "show", arg]).expect("parses");
            let Command::Show(ShowArgs { target, .. }) = cli.command else {
                panic!("expected Show");
            };
            assert_eq!(target.number, 42);
            assert_eq!(target.repo, None);
        }
    }

    #[test]
    fn a_url_without_pull_in_it_is_rejected() {
        assert!(
            Cli::try_parse_from(["reviewq", "show", "https://github.com/apache/airflow"]).is_err()
        );
    }

    #[test]
    fn a_url_with_a_non_numeric_pull_segment_is_rejected() {
        let err = Cli::try_parse_from([
            "reviewq",
            "show",
            "https://github.com/apache/airflow/pull/abc",
        ])
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("doesn't look like a pull request URL")
        );
    }
}
