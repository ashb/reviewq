//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted. reviewq only ever hands off — it never decides a review is
//! finished, so this does not imply `done`.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use reviewq_forge::resolve_token;

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

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some((var, token)) = handoff_token(&loaded.config) {
        command.env(var, token);
    }

    let status = command
        .status()
        .with_context(|| format!("running {:?}", argv[0]))?;

    match status.code() {
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("{:?} was terminated by a signal", argv[0]),
    }
}

/// The env var name and value the handoff command should see its forge token
/// under. The handoff command is a separate process with its own credential
/// resolution (`wiff` looks for `GITHUB_TOKEN`, matching the host's own
/// `token_env` convention) — this forwards whatever reviewq itself resolved
/// rather than requiring a second, separate login. `None` if config or token
/// resolution fails; the handoff command then falls back to its own
/// resolution and reports its own error, same as before this existed.
fn handoff_token(config: &config::Config) -> Option<(String, String)> {
    let (_project, repo) = config.sole_repo().ok()?;
    let host = config.forge_host_for(repo).ok()?;
    let token = resolve_token(&host).ok()?;
    let var = host.token_env.unwrap_or_else(|| "GITHUB_TOKEN".to_string());
    Some((var, token.value))
}
