use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use reviewq_forge::Forge;
use reviewq_ledger::Ledger;

use crate::config::RepoRef;

const MEMBERSHIP_TTL: SignedDuration = SignedDuration::from_hours(24);

#[derive(Debug, Default)]
pub(crate) struct Priority {
    pub(crate) authors: Vec<String>,
    pub(crate) requesters: Vec<String>,
}

pub(crate) fn validate(selector: &str) -> Result<()> {
    let selector_to_validate = if selector.contains('/') {
        selector
    } else {
        selector.strip_suffix("[bot]").unwrap_or(selector)
    };
    let parts: Vec<_> = selector_to_validate.split('/').collect();
    if parts.len() > 2
        || parts.iter().any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        })
    {
        bail!("invalid priority identity {selector:?}: expected a login or org/team");
    }
    Ok(())
}

pub(crate) async fn resolve(
    repo: &RepoRef,
    forge: &dyn Forge,
    ledger: &Ledger,
    force: bool,
    now: Timestamp,
) -> Result<Priority> {
    let host = repo.host.as_str();
    let mut teams = HashMap::new();
    for selector in repo
        .priority_authors
        .iter()
        .chain(&repo.priority_review_requesters)
    {
        validate(selector)?;
        let selector = selector.to_ascii_lowercase();
        let Some((org, team)) = selector.split_once('/') else {
            continue;
        };
        if teams.contains_key(&selector) {
            continue;
        }
        let cached = ledger.team_members(host, org, team)?;
        let due = cached.as_ref().is_none_or(|cached| {
            if force {
                cached.refreshed_at < now
            } else {
                now.duration_since(cached.refreshed_at) >= MEMBERSHIP_TTL
            }
        });
        let members = if due {
            let members = forge
                .fetch_team_members(org, team)
                .await
                .with_context(|| format!("refreshing priority team {host}/{org}/{team}"))?;
            ledger.set_team_members(host, org, team, &members, now)?;
            tracing::info!(%host, %org, %team, members = members.len(), "refreshed priority team");
            // Another sync may have completed a newer snapshot while this fetched.
            ledger
                .team_members(host, org, team)?
                .expect("stored team snapshot")
                .members
        } else {
            cached.expect("fresh team snapshot").members
        };
        teams.insert(selector, members);
    }
    let expand = |selectors: &[String]| -> Vec<String> {
        selectors
            .iter()
            .flat_map(|selector| {
                let selector = selector.to_ascii_lowercase();
                if selector.contains('/') {
                    teams[&selector].clone()
                } else {
                    vec![selector]
                }
            })
            .map(|login| login.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    Ok(Priority {
        authors: expand(&repo.priority_authors),
        requesters: expand(&repo.priority_review_requesters),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_forge::{FakeForge, ts};

    fn repo() -> RepoRef {
        toml::from_str(
            r#"owner = "apache"
name = "airflow""#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn teams_are_cached_shared_and_refreshed_only_when_due_or_requested() {
        let ledger = Ledger::open_in_memory().unwrap();
        let cfg = RepoRef {
            priority_authors: vec!["alice".into(), "Apache/Airflow-Committers".into()],
            priority_review_requesters: vec!["apache/airflow-committers".into()],
            ..repo()
        };
        let forge = FakeForge::new(vec![]).with_team("apache", "airflow-committers", &["ashb"]);
        let now = ts("2026-09-17T10:00:00Z");
        let first = resolve(&cfg, &forge, &ledger, false, now).await.unwrap();
        assert_eq!(first.authors, ["alice", "ashb"]);
        assert_eq!(first.requesters, ["ashb"]);
        assert_eq!(forge.team_calls(), 1);
        let changed = FakeForge::new(vec![]).with_team("apache", "airflow-committers", &["bob"]);
        let fresh = resolve(
            &cfg,
            &changed,
            &ledger,
            false,
            now + jiff::SignedDuration::from_hours(23),
        )
        .await
        .unwrap();
        assert_eq!(fresh.requesters, ["ashb"]);
        assert_eq!(changed.team_calls(), 0);
        let forced_at = now + jiff::SignedDuration::from_hours(23);
        let forced = resolve(&cfg, &changed, &ledger, true, forced_at)
            .await
            .unwrap();
        assert_eq!(forced.requesters, ["bob"]);
        resolve(&cfg, &changed, &ledger, true, forced_at)
            .await
            .unwrap();
        assert_eq!(changed.team_calls(), 1);
        let empty = FakeForge::new(vec![]).with_team("apache", "airflow-committers", &[]);
        let expired = resolve(
            &cfg,
            &empty,
            &ledger,
            false,
            forced_at + jiff::SignedDuration::from_hours(24),
        )
        .await
        .unwrap();
        assert_eq!(expired.authors, ["alice"]);
        assert!(expired.requesters.is_empty());
        assert_eq!(empty.team_calls(), 1);
    }

    #[tokio::test]
    async fn failed_refresh_preserves_the_last_successful_snapshot() {
        let ledger = Ledger::open_in_memory().unwrap();
        let now = ts("2026-09-17T10:00:00Z");
        ledger
            .set_team_members(
                "github.com",
                "apache",
                "airflow-committers",
                &["ashb".into()],
                now,
            )
            .unwrap();
        let cfg = RepoRef {
            priority_authors: vec!["apache/airflow-committers".into()],
            ..repo()
        };
        let unavailable = FakeForge::new(vec![]);
        let error = resolve(
            &cfg,
            &unavailable,
            &ledger,
            true,
            now + jiff::SignedDuration::from_hours(1),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("apache/airflow-committers"));
        let cached = ledger
            .team_members("github.com", "apache", "airflow-committers")
            .unwrap()
            .unwrap();
        assert_eq!(cached.members, ["ashb"]);
        assert_eq!(cached.refreshed_at, now);
        let other_host = RepoRef {
            host: "another.example".into(),
            ..cfg
        };
        assert!(
            resolve(&other_host, &unavailable, &ledger, false, now)
                .await
                .is_err()
        );
    }
}
