mod doctor;
mod list;
mod show;
mod sync;

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::{Cli, Command};

/// The read commands exit with this when there is nothing to show, so shell
/// wrappers can branch on "nothing to do" without parsing output.
pub const EXIT_EMPTY: u8 = 2;

pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    let config = cli.config.as_deref();
    // Logging shares stderr with sync's progress line; the in-place rewrite is
    // only tidy when nothing else is writing there.
    let logging = cli.verbose > 0;
    match cli.command {
        Command::Sync => sync::run(config, logging).await,
        Command::List(args) => list::run(config, &args),
        Command::Next(args) => list::next(config, &args),
        Command::Show(args) => show::run(config, &args),
        Command::Doctor => doctor::run(config).await,
    }
}
