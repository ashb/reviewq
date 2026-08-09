//! Resolving a bare PR number to the repo it belongs to, without config.
//!
//! `done`/`snooze`/`mute`/`unmute`/`defer`/`undefer`/`track` are ledger-only —
//! no config needed, so a broken or missing config never blocks them. With
//! more than one repo now possible, a bare number alone no longer says which
//! repo it's on; the ledger itself is asked instead, since every PR it knows
//! about already carries its own repo identity.

use anyhow::{Context, Result, bail};
use reviewq_ledger::{Ledger, PrShow, RepoKey};

use crate::paths;

/// The one repo (already known to the ledger) that has PR `number`. Errors
/// clearly on not-found or on ambiguity (the same number tracked in more than
/// one configured repo) rather than picking one silently.
pub fn repo_for(number: u64) -> Result<RepoKey> {
    let path = paths::database_file()?;
    let mut repos = reviewq_ledger::repos_with_pr(&path, number)?;
    match repos.len() {
        0 => bail!("#{number} is not in the ledger — run `reviewq sync` first"),
        1 => Ok(repos.remove(0)),
        _ => bail!(
            "#{number} exists in more than one configured repo ({}) — not supported yet",
            repos
                .iter()
                .map(|r| format!("{}/{}", r.owner, r.name))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}

/// [`repo_for`], then the ledger and that repo's id, with the PR's full detail
/// already loaded — what every action command needs before it writes.
pub fn open_for_number(number: u64) -> Result<(Ledger, i64, PrShow)> {
    let repo = repo_for(number)?;
    let ledger = Ledger::open(&paths::database_file()?)?;
    let repo_id = ledger.ensure_repo(&repo)?;
    let show = ledger
        .show(repo_id, number)?
        .context("repo_for just confirmed this PR is in the ledger")?;
    Ok((ledger, repo_id, show))
}
