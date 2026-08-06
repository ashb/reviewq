//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted. reviewq only ever hands off — it never decides a review is
//! finished, so this does not imply `done`.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

use crate::cli::NumberArgs;
use crate::config;

pub fn run(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
    let loaded = config::load(config_path)?;
    let number = args.number.to_string();
    // Non-empty is enforced at config load.
    let argv: Vec<String> = loaded
        .config
        .handoff
        .review_command
        .iter()
        .map(|arg| arg.replace("{number}", &number))
        .collect();

    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {:?}", argv[0]))?;

    match status.code() {
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("{:?} was terminated by a signal", argv[0]),
    }
}
