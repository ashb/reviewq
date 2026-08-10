//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted, then refresh that PR's detail so a review made during the
//! handoff shows up right away. reviewq only ever hands off — it never decides
//! a review is finished, so this does not imply `done`.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::cli::NumberArgs;

pub async fn run(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
    let handoff = reviewq_app::review::handoff_for(config_path, args.number)?;

    let status = handoff
        .command()
        .status()
        .with_context(|| format!("running {:?}", handoff.argv[0]))?;

    match status.code() {
        Some(0) => {
            if let Err(err) = refresh_after_review(config_path, args.number).await {
                tracing::warn!(number = args.number, %err, "could not refresh PR state after review");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("{:?} was terminated by a signal", handoff.argv[0]),
    }
}

/// Refresh this PR's tier-2 detail right after handing it off, so a review
/// made during the handoff shows up immediately rather than waiting for the
/// next `reviewq sync`.
///
/// A PR the ledger has never heard of is skipped rather than an error —
/// `review` names any PR, tracked or not — which is exactly what
/// [`Refreshed::Untracked`] reports. Best-effort overall: token or network
/// trouble here must not turn a successful review session into a failing
/// `reviewq review` exit, so the caller only warns.
async fn refresh_after_review(config_path: Option<&Path>, number: u64) -> Result<()> {
    reviewq_app::sync::sync_one(config_path, number).await?;
    Ok(())
}
