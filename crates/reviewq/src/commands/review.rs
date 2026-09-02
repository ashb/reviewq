//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted, then refresh that PR's detail so a review made during the
//! handoff shows up right away. reviewq only ever hands off — it never decides
//! a review is finished, so this does not imply `done`.

use std::process::ExitCode;

use anyhow::{Result, bail};
use jiff::Timestamp;
use reviewq_app::config::{Config, Loaded};
use reviewq_ledger::RepoKey;

use crate::cli::NumberArgs;
use crate::colour::Output;

pub async fn run(loaded: &Loaded, args: &NumberArgs, output: &impl Output) -> Result<ExitCode> {
    let handoff = reviewq_app::review::prepare_handoff_for(&loaded.config, args.number).await?;
    let (repo, number) = handoff.target()?;
    let program = handoff.argv[0].clone();
    let outcome = handoff.run(Timestamp::now())?;
    finish_handoff(outcome, &repo, number, output, |repo, number| {
        refresh_after_review(&loaded.config, repo, number)
    })
    .await
    .map_err(|error| error.context(format!("finishing handoff through {program:?}")))
}

async fn finish_handoff<F, Fut>(
    outcome: reviewq_app::review::HandoffOutcome,
    repo: &RepoKey,
    number: u64,
    output: &impl Output,
    refresh: F,
) -> Result<ExitCode>
where
    F: FnOnce(RepoKey, u64) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    if let Some(error) = outcome.history_error {
        output.eprintln(format!(
            "warning: review started, but its history was not recorded: {error:#}"
        ));
    }

    match outcome.status.code() {
        Some(0) => {
            if let Err(err) = refresh(repo.clone(), number).await {
                tracing::warn!(number, %err, "could not refresh PR state after review");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("review handoff was terminated by a signal"),
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
async fn refresh_after_review(cfg: &Config, repo: RepoKey, number: u64) -> Result<()> {
    reviewq_app::sync::sync_one_for(cfg, &repo, number).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::process::{Command, ExitCode};

    use anyhow::anyhow;
    use reviewq_app::review::HandoffOutcome;
    use reviewq_ledger::RepoKey;

    use super::finish_handoff;
    use crate::colour::testing::FakeOutput;

    fn outcome(code: i32) -> HandoffOutcome {
        let status = Command::new("sh")
            .args(["-c", &format!("exit {code}")])
            .status()
            .expect("child");
        HandoffOutcome {
            status,
            history_error: Some(anyhow!("history is read-only")),
        }
    }

    fn repo() -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        }
    }

    #[tokio::test]
    async fn a_history_failure_warns_but_a_successful_child_still_refreshes() {
        let output = FakeOutput::new(false);
        let refreshed = Cell::new(false);

        let target = repo();
        let seen = RefCell::new(None);
        let exit = finish_handoff(outcome(0), &target, 7, &output, |repo, number| {
            seen.replace(Some((repo, number)));
            refreshed.set(true);
            async { Ok(()) }
        })
        .await
        .expect("successful review");

        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(refreshed.get(), "post-review refresh still ran");
        assert_eq!(*seen.borrow(), Some((target, 7)));
        assert_eq!(
            &*output.stderr.borrow(),
            "warning: review started, but its history was not recorded: history is read-only\n"
        );
    }

    #[tokio::test]
    async fn a_nonzero_child_exit_keeps_precedence_over_a_history_failure() {
        let output = FakeOutput::new(false);
        let refreshed = Cell::new(false);

        let exit = finish_handoff(outcome(23), &repo(), 7, &output, |_, _| {
            refreshed.set(true);
            async { Ok(()) }
        })
        .await
        .expect("child exit returned");

        assert_eq!(exit, ExitCode::from(23));
        assert!(!refreshed.get(), "a failed handoff is not refreshed");
        assert_eq!(
            &*output.stderr.borrow(),
            "warning: review started, but its history was not recorded: history is read-only\n"
        );
    }
}
