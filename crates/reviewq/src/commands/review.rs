//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted, then refresh that PR's detail so a review made during the
//! handoff shows up right away. reviewq only ever hands off — it never decides
//! a review is finished, so this does not imply `done`.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use reviewq_forge::{build, resolve_token};
use reviewq_ledger::Ledger;

use crate::cli::NumberArgs;
use crate::{config, paths};

pub async fn run(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
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
        Some(0) => {
            if let Err(err) = refresh_after_review(&loaded.config, args.number).await {
                tracing::warn!(number = args.number, %err, "could not refresh PR state after review");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("{:?} was terminated by a signal", argv[0]),
    }
}

/// Refresh this PR's tier-2 detail right after handing it off, so a review
/// made during the handoff shows up immediately rather than waiting for the
/// next `reviewq sync`. Skipped (not an error) for a PR the ledger has never
/// heard of — `review` names any PR, tracked or not. Best-effort: config,
/// token or network trouble here must not turn a successful review session
/// into a failing `reviewq review` exit.
async fn refresh_after_review(config: &config::Config, number: u64) -> Result<()> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let Some(show) = ledger.show(number)? else {
        return Ok(());
    };

    let (project, repo) = config.sole_repo()?;
    let host = config.forge_host_for(repo)?;
    let token = resolve_token(&host)?;
    let forge = build(&host, &token.value)?;

    super::sync::refresh_one(
        forge.as_ref(),
        &ledger,
        repo,
        &config.identity.login,
        &config.bots.logins,
        project.include_merged,
        &HashSet::new(),
        &show.pr,
        show.tracked_reason.as_deref().unwrap_or(""),
        Timestamp::now(),
    )
    .await?;
    Ok(())
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
