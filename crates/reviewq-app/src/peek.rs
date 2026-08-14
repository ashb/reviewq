//! Looking at a PR that isn't on the queue.
//!
//! Everything else in reviewq answers "what wants me now?", and a PR that has
//! merged, was never tracked, or was never even swept is deliberately not part of
//! that answer. But knowing a number and being told no is a dead end: the PR is
//! right there on the forge, and reading it changes nothing.
//!
//! So a peek shows one, whatever its state, and writes nothing. A PR the ledger
//! already holds is served from it; one it has never seen is fetched into a
//! [`Peeked`] and dropped when the caller is done with it. That is the whole
//! difference from [`track_one`](crate::sync::track_one), which is the same fetch
//! followed by a commitment to keep the thing.

use anyhow::{Context, Result, bail};
use reviewq_ledger::{PrShow, RepoKey};

use crate::config::Config;

/// A PR to look at, and where it came from.
#[derive(Debug, Clone)]
pub struct Peeked {
    /// The repo it lives in — the identity a browser or clipboard hook needs.
    pub repo: RepoKey,
    /// Everything there is to show.
    pub show: PrShow,
    /// It was fetched for this look alone: nothing about it was written to the
    /// ledger, and it holds no attention because nothing has classified it.
    pub scratch: bool,
}

/// Read `number` for display, fetching it if the ledger has never seen it.
///
/// Never writes. A fetched PR is *not* stored — that is what `track` is for, and
/// conflating the two would mean every glance at a merged PR quietly grew the
/// ledger and the next sync's detail pass.
///
/// Which repo an unknown number belongs to comes from config, since the ledger
/// has nothing to say about it; with more than one configured that is ambiguous
/// and refused rather than guessed, exactly as `track` refuses it.
pub async fn peek_one(cfg: &Config, number: u64) -> Result<Peeked> {
    let ledger = crate::resolve::open()?;
    if let Some(key) = crate::resolve::repo_with_pr(&ledger, number)? {
        let repo_id = ledger
            .repo_id(&key)?
            .context("the number resolved to this repo a moment ago")?;
        if let Some(show) = ledger.show(repo_id, number)? {
            return Ok(Peeked {
                repo: key,
                show,
                scratch: false,
            });
        }
    }

    let mut repos = cfg.repos();
    let repo = repos.next().context("no repos configured")?.clone();
    if repos.next().is_some() {
        bail!(
            "#{number} is not in the ledger, and more than one repo is configured \
             — paste its full pull-request URL"
        );
    }

    let forge = cfg.forge_for(&repo.host)?;
    // Reading a PR nobody has tracked still needs to know whose reviews are
    // whose, and the host is the one that knows.
    let me = crate::identity::Logins::new()
        .on(cfg, &repo.host, forge.as_ref())
        .await?;
    fetched(&repo, number, &me, forge.as_ref()).await
}

/// Assemble a [`Peeked`] from what the forge can tell us about one PR.
///
/// Split out from [`peek_one`] so it is reachable with a forge a test supplies:
/// what it has to get right is which fields of a stored PR a fetch can stand in
/// for, and which it cannot.
async fn fetched(
    repo: &crate::config::RepoRef,
    number: u64,
    login: &str,
    forge: &dyn reviewq_forge::Forge,
) -> Result<Peeked> {
    let fetched = forge
        .fetch_pr(&repo.owner, &repo.name, number)
        .await?
        .with_context(|| format!("{} has no pull request #{number}", repo.slug()))?;
    let detail = forge
        .fetch_pr_detail(&repo.owner, &repo.name, number, login)
        .await?
        .with_context(|| format!("{} has no pull request #{number}", repo.slug()))?;

    // A peek stores nothing, the colours it saw included: it is a look at a PR,
    // and the ledger has no row for any of it to belong to.
    //
    // The detail fetch saw the head more recently than the snapshot did.
    let mut pr = fetched.pr;
    pr.head_sha = detail.head_sha.clone();
    let my_state = reviewq_core::model::MyState {
        last_reviewed_sha: detail.last_reviewed_sha,
        last_verdict: detail.last_verdict,
        last_action_at: detail.last_action_at,
        ..Default::default()
    };

    Ok(Peeked {
        repo: repo.key(),
        show: PrShow {
            pr,
            body: Some(detail.body),
            tracked_reason: None,
            after_merge: false,
            my_state,
            threads: detail.threads,
            reviewers: detail.reviewers,
            // Empty, and honestly so: attention is what a detail pass computes
            // and stores, and this PR has had neither.
            attention: Vec::new(),
        },
        scratch: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoRef;
    use crate::fake_forge::{FakeForge, Page};

    fn repo() -> RepoRef {
        RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: None,
        }
    }

    #[tokio::test]
    async fn a_fetched_pr_carries_its_description_and_admits_it_is_untracked() {
        let forge = FakeForge::new(vec![Page::of(vec![])]).with_detail(7, 4900);

        let peeked = fetched(&repo(), 7, "ashb", &forge).await.expect("peeked");

        assert!(peeked.scratch);
        assert_eq!(peeked.repo.owner, "apache");
        assert_eq!(peeked.show.pr.number, 7);
        assert_eq!(
            peeked.show.pr.head_sha, "sha7",
            "the head the detail fetch saw, not the snapshot's"
        );
        assert_eq!(peeked.show.body.as_deref(), Some(""));
        assert_eq!(peeked.show.tracked_reason, None);
        assert!(
            peeked.show.attention.is_empty(),
            "nothing has classified it, so it claims no reasons"
        );
    }

    #[tokio::test]
    async fn a_pr_the_forge_does_not_have_is_an_error_rather_than_a_blank_view() {
        // Nothing is scripted for #9, which is the fake saying the PR is gone.
        let forge = FakeForge::new(vec![Page::of(vec![])]);

        let err = fetched(&repo(), 9, "ashb", &forge)
            .await
            .expect_err("no such PR");

        assert!(
            format!("{err:#}").contains("has no pull request #9"),
            "{err:#}"
        );
    }
}
