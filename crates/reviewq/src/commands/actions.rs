//! `reviewq done`/`snooze`/`mute`/`unmute`/`defer`/`undefer`/`track`: the
//! ledger-write actions. Each names one PR already in the ledger and updates
//! its `my_state` row; `done` is the one that also reaches the network, and
//! does so best-effort — a notification API hiccup must not stop it from
//! recording locally.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;

use super::resolve::{open_for_number, repo_for};
use crate::cli::{NumberArgs, SnoozeArgs};
use crate::config;

pub async fn done(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, show) = open_for_number(args.number)?;

    ledger.set_done(repo_id, args.number, &show.pr.head_sha, Timestamp::now())?;
    ledger.clear_done_attention(repo_id, args.number)?;

    if let Err(err) = mark_notifications_read(config_path, args.number).await {
        tracing::warn!(
            number = args.number,
            %err,
            "could not mark GitHub notifications read"
        );
    }

    println!("#{} marked done at {}", args.number, show.pr.head_sha);
    Ok(ExitCode::SUCCESS)
}

/// Best-effort: config or the network being unavailable should not stop
/// `done` from recording locally, so the caller only warns on error.
async fn mark_notifications_read(config_path: Option<&Path>, number: u64) -> Result<()> {
    let repo = repo_for(number)?;
    let loaded = config::load(config_path)?;
    let forge = loaded.config.forge_for(&repo.host)?;
    forge
        .mark_pr_notifications_read(&repo.owner, &repo.name, number)
        .await
}

pub fn snooze(args: &SnoozeArgs) -> Result<ExitCode> {
    // Validate the duration before touching the ledger, so a typo is reported
    // as itself rather than as an unrelated "PR not found".
    let until = snooze_until(Timestamp::now(), &args.duration)?;
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    ledger.set_snoozed_until(repo_id, args.number, until)?;
    ledger.clear_attention(repo_id, args.number)?;

    println!(
        "#{} snoozed until {}",
        args.number,
        until.round(jiff::Unit::Second).unwrap_or(until)
    );
    Ok(ExitCode::SUCCESS)
}

pub fn mute(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    ledger.set_muted(repo_id, args.number, true)?;
    ledger.clear_attention(repo_id, args.number)?;

    println!("#{} muted", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn unmute(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    ledger.set_muted(repo_id, args.number, false)?;

    println!(
        "#{} unmuted — its reasons return on the next sync",
        args.number
    );
    Ok(ExitCode::SUCCESS)
}

pub fn defer(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    ledger.set_deferred_at(repo_id, args.number, Some(Timestamp::now()))?;

    println!("#{} deferred to the bottom of the queue", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn undefer(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    ledger.set_deferred_at(repo_id, args.number, None)?;

    println!("#{} undeferred", args.number);
    Ok(ExitCode::SUCCESS)
}

pub fn track(args: &NumberArgs) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    if ledger.track(repo_id, args.number)? {
        println!(
            "#{} force-tracked — run `reviewq sync` to fetch its detail and queue it",
            args.number
        );
    } else {
        println!("#{} is already tracked", args.number);
    }
    Ok(ExitCode::SUCCESS)
}

/// Parse a friendly duration (`3d`, `12h`, `1w2d`) into the instant it reaches
/// past `now`.
fn snooze_until(now: Timestamp, duration: &str) -> Result<Timestamp> {
    let span: jiff::Span = duration
        .parse()
        .with_context(|| format!("invalid duration {duration:?} (try `3d`, `12h`, `1w2d`)"))?;
    let until = now
        .to_zoned(jiff::tz::TimeZone::UTC)
        .checked_add(span)
        .with_context(|| format!("duration {duration:?} out of range"))?
        .timestamp();
    if until <= now {
        bail!("duration {duration:?} must be positive");
    }
    Ok(until)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn snooze_until_parses_a_friendly_duration() {
        let until = snooze_until(ts("2026-08-05T12:00:00Z"), "3d").unwrap();
        assert_eq!(until, ts("2026-08-08T12:00:00Z"));
    }

    #[test]
    fn snooze_until_rejects_a_non_positive_duration() {
        let now = ts("2026-08-05T12:00:00Z");
        assert!(snooze_until(now, "0s").is_err());
        assert!(snooze_until(now, "-3d").is_err());
    }

    #[test]
    fn snooze_until_rejects_garbage() {
        assert!(snooze_until(ts("2026-08-05T12:00:00Z"), "soon").is_err());
    }
}
