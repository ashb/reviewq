//! `reviewq review N`: exec the configured handoff command with the PR number
//! substituted, then refresh that PR's detail so a review made during the
//! handoff shows up right away. reviewq only ever hands off — it never decides
//! a review is finished, so this does not imply `done`.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

use crate::cli::NumberArgs;
use reviewq_app::config::RepoRef;
use reviewq_app::{config, paths};

pub async fn run(config_path: Option<&Path>, args: &NumberArgs) -> Result<ExitCode> {
    let loaded = config::load(config_path)?;
    let repo = resolve_repo(&loaded.config, args.number)?;
    let handoff = resolve_handoff(&loaded.config, &repo, args.number);

    let number = args.number.to_string();
    let url = handoff.as_ref().map(|h| h.url.clone()).unwrap_or_default();
    // Non-empty is enforced at config load.
    let argv: Vec<String> = loaded
        .config
        .handoff
        .review_command
        .iter()
        .map(|arg| arg.replace("{number}", &number).replace("{url}", &url))
        .collect();

    let mut command = Command::new(&argv[0]);
    command.args(&argv[1..]);
    if let Some(handoff) = &handoff {
        command.env(&handoff.token_var, &handoff.token_value);
    }

    let status = command
        .status()
        .with_context(|| format!("running {:?}", argv[0]))?;

    match status.code() {
        Some(0) => {
            if let Err(err) = refresh_after_review(config_path, args.number).await {
                tracing::warn!(number = args.number, %err, "could not refresh PR state after review");
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => bail!("{:?} was terminated by a signal", argv[0]),
    }
}

/// `review`'s target repo: trivial with one repo configured (the common
/// case, and never a new failure mode over the single-repo design). With more
/// than one, `review` — unlike `show`/`done`/etc — can legitimately name a PR
/// the ledger has never heard of (see [`refresh_after_review`]), so a bare
/// number can't always be resolved by asking the ledger; this only falls back
/// to that when it's already tracked.
fn resolve_repo(config: &config::Config, number: u64) -> Result<RepoRef> {
    let repos: Vec<&RepoRef> = config.repos().collect();
    if let [repo] = repos.as_slice() {
        return Ok((*repo).clone());
    }

    let found = reviewq_ledger::repos_with_pr(&paths::database_file()?, number)?;
    match found.as_slice() {
        [key] => repos
            .into_iter()
            .find(|r| r.key() == *key)
            .cloned()
            .with_context(|| {
                format!(
                    "#{number} was last synced from {}/{}, which is no longer configured",
                    key.owner, key.name
                )
            }),
        [] => bail!(
            "#{number} isn't in the ledger yet and more than one repo is configured — \
             run `reviewq sync` first, or configure a single repo"
        ),
        _ => bail!("#{number} is tracked in more than one configured repo — not supported yet"),
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

/// What the handoff command needs from reviewq's own already-resolved forge
/// connection: the PR's web URL (so `{url}` works outside a checkout of the
/// right repo) and the env var/token to forward (the handoff command has its
/// own separate credential resolution otherwise). `None` if config, token or
/// connection resolution fails; the handoff command then falls back to its
/// own resolution — or, for `{url}`, just gets an empty string substituted —
/// rather than this stopping the review outright.
struct Handoff {
    url: String,
    token_var: String,
    token_value: String,
}

fn resolve_handoff(config: &config::Config, repo: &RepoRef, number: u64) -> Option<Handoff> {
    let forge = config.forge_for(&repo.host).ok()?;
    let (var, value) = forge.handoff_credentials();
    Some(Handoff {
        url: forge.web_url(&repo.owner, &repo.name, number),
        token_var: var.to_string(),
        token_value: value.to_string(),
    })
}
