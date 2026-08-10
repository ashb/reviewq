//! Resolving a bare PR number to the repo it belongs to, and opening the ledger
//! it was resolved against.
//!
//! The *ledger* answers, not config: with more than one repo possible, a bare
//! number alone doesn't say which repo it's on, and every PR the ledger knows
//! already carries its own repo identity — which is also the identity a sync
//! actually populated, rather than whatever config lists today.
//!
//! That is not the same as working without config. Every command loads and
//! validates one before it runs (see `commands::dispatch`); these functions
//! simply don't consult it.

use anyhow::{Context, Result, bail};
use reviewq_ledger::{Ledger, PrShow, RepoKey};

use crate::paths;

/// The one repo (already known to the ledger) that has PR `number`, or `None` if
/// the ledger has never seen it.
///
/// Ambiguity — the same number in more than one repo the ledger knows — is an error,
/// never a silent pick. Every caller resolves a bare number through this or
/// through [`repo_for`], so no two of them can disagree about which PR a number
/// means; a caller that picked the first match would let one key in a frontend
/// act on a different PR than the next key does.
pub fn repo_with_pr(ledger: &Ledger, number: u64) -> Result<Option<RepoKey>> {
    let mut repos = ledger.repos_with_pr(number)?;
    match repos.len() {
        0 => Ok(None),
        1 => Ok(Some(repos.remove(0))),
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

/// [`repo_with_pr`], where the ledger not having the PR is itself an error —
/// what the commands that act on an existing PR need.
pub fn repo_for(ledger: &Ledger, number: u64) -> Result<RepoKey> {
    repo_with_pr(ledger, number)?
        .with_context(|| format!("#{number} is not in the ledger — run `reviewq sync` first"))
}

/// [`repo_for`], then the ledger and that repo's id, with the PR's full detail
/// already loaded — what every action command needs before it writes.
pub fn open_for_number(number: u64) -> Result<(Ledger, i64, PrShow)> {
    let ledger = open()?;
    let repo = repo_for(&ledger, number)?;
    // A read, not `ensure_repo`: the repo came *from* the ledger a line ago, so
    // registering it would only be a write on the way to reading.
    let repo_id = ledger
        .repo_id(&repo)?
        .context("repo_for just confirmed this repo is in the ledger")?;
    let show = ledger
        .show(repo_id, number)?
        .context("repo_for just confirmed this PR is in the ledger")?;
    Ok((ledger, repo_id, show))
}

/// Open the ledger at its configured path.
///
/// One handle per command, threaded from here: the reads that resolve a number
/// and the writes that follow belong to the same connection, and opening a second
/// one to answer a question the first could have is pure cost.
pub fn open() -> Result<Ledger> {
    Ledger::open(&paths::database_file()?)
}
