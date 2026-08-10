//! `reviewq done`/`snooze`/`mute`/`unmute`/`defer`/`undefer`/`track`: name one
//! PR and act on it.
//!
//! What each one does to the ledger lives in `reviewq_app::actions`, shared with
//! the TUI. What's here is the CLI's half: resolving the number, and saying what
//! happened.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use jiff::Timestamp;
use reviewq_app::actions;
use reviewq_app::resolve::{open_for_number, repo_for};

use crate::cli::{NumberArgs, SnoozeArgs, TrackArgs};

pub async fn done(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, show) = open_for_number(args.number)?;
    actions::done(&ledger, repo_id, args.number, &show.pr.head_sha)?;

    // After the local record, never in front of it: the PR is marked done
    // whether or not GitHub can be reached.
    if let Err(err) = mark_read(config_path, args.number).await {
        tracing::warn!(
            number = args.number,
            %err,
            "could not mark GitHub notifications read"
        );
    }

    println!("#{} marked done at {}", args.number, show.pr.head_sha);
    Ok(ExitCode::SUCCESS)
}

/// Resolve the PR's repo from config and hand off to the shared best-effort
/// notification marking.
///
/// One load, used for both halves: finding the repo and reaching its forge.
async fn mark_read(config_path: Option<&Path>, number: u64) -> Result<()> {
    let key = repo_for(number)?;
    let loaded = reviewq_app::config::load(config_path)?;
    let repo = loaded
        .config
        .repos()
        .find(|r| r.key() == key)
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "#{number} was last synced from {}/{}, which is no longer configured",
                key.owner,
                key.name
            )
        })?;
    actions::mark_notifications_read(&loaded.config, &repo, number).await
}

pub fn snooze(args: &SnoozeArgs) -> Result<ExitCode> {
    // Validate the duration before touching the ledger, so a typo is reported
    // as itself rather than as an unrelated "PR not found".
    let until = actions::snooze_until(Timestamp::now(), &args.duration)?;
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    let until = actions::snooze(&ledger, repo_id, args.number, until)?;

    println!("#{} snoozed until {until}", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn mute(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_muted(&ledger, repo_id, args.number, true)?;

    println!("#{} muted", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn unmute(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_muted(&ledger, repo_id, args.number, false)?;

    println!(
        "#{} unmuted — its reasons return on the next sync",
        args.number
    );
    Ok(ExitCode::SUCCESS)
}

pub fn defer(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_deferred(&ledger, repo_id, args.number, true)?;

    println!("#{} deferred to the bottom of the queue", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn undefer(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_deferred(&ledger, repo_id, args.number, false)?;

    println!("#{} undeferred", args.number);
    Ok(ExitCode::SUCCESS)
}

pub async fn track(config_path: Option<&Path>, args: &TrackArgs) -> Result<ExitCode> {
    let number = args.target.number;
    // `track` fetches, so it needs config either way — loaded once here and used
    // for both naming the repo and reaching it.
    let loaded = reviewq_app::config::load(config_path)
        .with_context(|| format!("tracking #{number} needs a usable config to fetch it"))?;
    // A URL names its own repo, which is the only way to reach one that isn't
    // the single configured repo.
    let named = args.target.repo.as_ref().and_then(|url| {
        loaded
            .config
            .repos()
            .find(|repo| repo.host == url.host && repo.owner == url.owner && repo.name == url.name)
            .cloned()
    });

    let (tracked, refreshed) =
        reviewq_app::sync::track_one(&loaded.config, named.as_ref(), number).await?;

    let what = match tracked {
        actions::Tracked::Already => "was already tracked",
        actions::Tracked::Marked => "force-tracked",
        actions::Tracked::Fetched => "fetched from the forge and tracked",
    };
    let queued = match refreshed {
        reviewq_app::sync::Refreshed::Updated { queued: true, .. } => " — it wants attention",
        reviewq_app::sync::Refreshed::Updated { queued: false, .. } => " — it wants nothing yet",
        reviewq_app::sync::Refreshed::Gone => " — but the forge no longer has it",
        reviewq_app::sync::Refreshed::Untracked => "",
    };
    println!("#{number} {what}{queued}");
    Ok(ExitCode::SUCCESS)
}
