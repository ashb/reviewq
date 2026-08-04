mod doctor;

use std::process::ExitCode;

use anyhow::Result;

use crate::cli::{Cli, Command};

pub async fn dispatch(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
    }
}
