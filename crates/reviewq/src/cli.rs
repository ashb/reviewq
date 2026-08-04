use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
    /// Check the token, the rate-limit budget and where things live on disk.
    Doctor,
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
