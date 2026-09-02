//! `reviewq done`/`snooze`/`mute`/`unmute`/`defer`/`undefer`/`track`: name one
//! PR and act on it.
//!
//! What each one does to the ledger lives in `reviewq_app::actions`, shared with
//! the TUI. What's here is the CLI's half: resolving the number, and saying what
//! happened.

use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use reviewq_app::actions;
use reviewq_app::config::{Config, Loaded};
use reviewq_app::resolve::{open_for_number, repo_for};

use crate::cli::{NumberArgs, SnoozeArgs, TrackArgs};
use crate::colour::Output;

pub async fn done(loaded: &Loaded, args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, show) = open_for_number(args.number)?;
    actions::done(&ledger, repo_id, args.number, &show.pr.head_sha)?;

    // After the local record, never in front of it: the PR is marked done
    // whether or not GitHub can be reached.
    if let Err(err) = mark_read(&loaded.config, args.number).await {
        tracing::warn!(
            number = args.number,
            %err,
            "could not mark GitHub notifications read"
        );
    }

    output.println(format!(
        "#{} marked done at {}",
        args.number, show.pr.head_sha
    ));
    Ok(ExitCode::SUCCESS)
}

/// Resolve the PR's repo from config and hand off to the shared best-effort
/// notification marking.
///
/// One load, used for both halves: finding the repo and reaching its forge.
async fn mark_read(cfg: &Config, number: u64) -> Result<()> {
    let key = repo_for(&reviewq_app::resolve::open()?, number)?;
    let repo = cfg
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
    actions::mark_notifications_read(cfg, &repo, number).await
}

pub fn snooze(args: &SnoozeArgs, output: &impl Output) -> Result<ExitCode> {
    // Validate the duration before touching the ledger, so a typo is reported
    // as itself rather than as an unrelated "PR not found".
    let until = actions::snooze_until(Timestamp::now(), &args.duration)?;
    let (ledger, repo_id, _show) = open_for_number(args.number)?;

    let until = actions::snooze(&ledger, repo_id, args.number, until)?;

    output.println(format!("#{} snoozed until {until}", args.number));
    Ok(ExitCode::SUCCESS)
}

pub fn mute(args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_muted(&ledger, repo_id, args.number, true)?;

    output.println(format!("#{} muted", args.number));
    Ok(ExitCode::SUCCESS)
}

pub fn unmute(args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_muted(&ledger, repo_id, args.number, false)?;

    output.println(format!(
        "#{} unmuted — its reasons return on the next sync",
        args.number
    ));
    Ok(ExitCode::SUCCESS)
}

/// `reviewq untrack N`: stop watching a PR for good.
///
/// Distinct from `done`, which says the current head is handled and leaves the
/// PR waiting on somebody. This says you are finished with it, and no rule may
/// track it again until `reviewq track` puts it back.
pub fn untrack(args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::untrack(&ledger, repo_id, args.number)?;

    output.println(format!(
        "#{} untracked — `reviewq track {}` puts it back",
        args.number, args.number
    ));
    Ok(ExitCode::SUCCESS)
}

pub fn defer(args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_deferred(&ledger, repo_id, args.number, true)?;

    output.println(format!(
        "#{} deferred to the bottom of the queue",
        args.number
    ));
    Ok(ExitCode::SUCCESS)
}

pub fn undefer(args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let (ledger, repo_id, _show) = open_for_number(args.number)?;
    actions::set_deferred(&ledger, repo_id, args.number, false)?;

    output.println(format!("#{} undeferred", args.number));
    Ok(ExitCode::SUCCESS)
}

pub async fn track(loaded: &Loaded, args: &TrackArgs, output: &impl Output) -> Result<ExitCode> {
    let number = args.target.number;
    // A URL names its own repo, which is the only way to reach one that isn't
    // the single configured repo.
    let named = args.target.repo.as_ref().and_then(|url| {
        loaded
            .config
            .repos()
            .find(|repo| repo.host == url.host && repo.owner == url.owner && repo.name == url.name)
            .cloned()
    });

    let reviewq_app::sync::TrackedOne {
        tracked,
        refreshed,
        activity,
    } = reviewq_app::sync::track_one(&loaded.config, named.as_ref(), number).await?;

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
    output.println(format!("#{number} {what}{queued}"));
    if let Some(warning) = activity
        .as_ref()
        .and_then(|activity| track_activity_warning(number, activity))
    {
        output.eprintln(warning);
    }
    Ok(ExitCode::SUCCESS)
}

fn track_activity_warning(
    number: u64,
    activity: &reviewq_app::sync::TrackActivity,
) -> Option<String> {
    let events = activity.stats.events;
    let pages = activity.stats.pages;
    let progress = format!(
        "{events} event{} across {pages} page{}",
        if events == 1 { "" } else { "s" },
        if pages == 1 { "" } else { "s" },
    );
    match activity.error.as_deref() {
        Some(error) => Some(format!(
            "warning: #{number} is tracked and refreshed, but activity sync stopped after {progress}: {error}"
        )),
        None if activity.stats.stopped_for_budget => Some(format!(
            "activity sync for #{number} paused at the provider budget floor after {progress}; the next sync resumes it"
        )),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use reviewq_app::sync::{BackfillStats, TrackActivity};

    use super::track_activity_warning;

    #[test]
    fn automatic_activity_failure_reports_its_retained_partial_statistics() {
        let activity = TrackActivity {
            stats: BackfillStats {
                events: 3,
                pages: 2,
                ..BackfillStats::default()
            },
            error: Some("provider activity failed".into()),
        };

        assert_eq!(
            track_activity_warning(17, &activity).as_deref(),
            Some(
                "warning: #17 is tracked and refreshed, but activity sync stopped after 3 events across 2 pages: provider activity failed"
            )
        );
    }

    #[test]
    fn automatic_activity_budget_pause_reports_resume_progress() {
        let activity = TrackActivity {
            stats: BackfillStats {
                events: 1,
                pages: 4,
                stopped_for_budget: true,
                ..BackfillStats::default()
            },
            error: None,
        };

        assert_eq!(
            track_activity_warning(17, &activity).as_deref(),
            Some(
                "activity sync for #17 paused at the provider budget floor after 1 event across 4 pages; the next sync resumes it"
            )
        );
    }

    #[test]
    fn completed_automatic_activity_needs_no_warning() {
        let activity = TrackActivity {
            stats: BackfillStats {
                prs: 1,
                pages: 1,
                ..BackfillStats::default()
            },
            error: None,
        };

        assert_eq!(track_activity_warning(17, &activity), None);
    }
}
