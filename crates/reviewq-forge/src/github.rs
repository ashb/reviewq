//! The GitHub adapter.
//!
//! An octocrab wrapper that returns plain data types; no model or ledger types
//! cross this boundary. When a `Forge` trait exists this is what implements it,
//! which is why the type is named for the provider rather than called `Client`.

use anyhow::{Context, Result};
use jiff::Timestamp;
use octocrab::Octocrab;
use serde::Deserialize;

use crate::ForgeHost;

/// A GitHub connection bound to one host.
pub struct GithubForge {
    inner: Octocrab,
}

/// GraphQL point budget. Every query we send asks for this, so cost is always
/// observable rather than inferred.
#[derive(Debug, Clone, Deserialize)]
pub struct RateLimit {
    /// Total points available per reset window.
    pub limit: u32,
    /// Points the most recent query cost.
    pub cost: u32,
    /// Points remaining in the current window.
    pub remaining: u32,
    /// When the window resets and `remaining` returns to `limit`.
    #[serde(rename = "resetAt")]
    pub reset_at: Timestamp,
}

/// The authenticated account, with the budget reported alongside it.
#[derive(Debug, Clone)]
pub struct Viewer {
    /// The account's login.
    pub login: String,
    /// The GraphQL budget as of this call.
    pub rate_limit: RateLimit,
}

#[derive(Debug, Deserialize)]
struct ViewerQuery {
    viewer: ViewerNode,
    #[serde(rename = "rateLimit")]
    rate_limit: RateLimit,
}

#[derive(Debug, Deserialize)]
struct ViewerNode {
    login: String,
}

impl GithubForge {
    /// Build an adapter for `host`, using its `api_base` when set so a GitHub
    /// Enterprise instance works without code changes.
    pub fn new(host: &ForgeHost, token: &str) -> Result<Self> {
        let mut builder = Octocrab::builder().personal_token(token.to_string());
        if let Some(api_base) = &host.api_base {
            builder = builder
                .base_uri(api_base.as_str())
                .with_context(|| format!("invalid api_base {api_base:?}"))?;
        }
        let inner = builder.build().context("building GitHub client")?;
        Ok(Self { inner })
    }

    /// Cheapest possible authenticated call: proves the token works, names the
    /// account it belongs to, and reports the GraphQL budget.
    pub async fn viewer(&self) -> Result<Viewer> {
        const QUERY: &str = r"
            query {
              viewer { login }
              rateLimit { limit cost remaining resetAt }
            }
        ";

        let data: ViewerQuery = self.graphql(QUERY, serde_json::Map::new()).await?;
        Ok(Viewer {
            login: data.viewer.login,
            rate_limit: data.rate_limit,
        })
    }

    /// REST rate limit for the `core` resource — the notifications endpoints
    /// draw on this, not the GraphQL budget. Returns `(remaining, limit)`.
    pub async fn rest_core_remaining(&self) -> Result<(u32, u32)> {
        let limits = self
            .inner
            .ratelimit()
            .get()
            .await
            .context("fetching REST rate limit")?;
        Ok((
            limits.resources.core.remaining as u32,
            limits.resources.core.limit as u32,
        ))
    }

    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Map<String, serde_json::Value>,
    ) -> Result<T> {
        let payload = serde_json::json!({ "query": query, "variables": variables });
        self.inner
            .graphql(&payload)
            .await
            .context("GitHub GraphQL request failed")
    }
}

impl RateLimit {
    /// Log every query's cost so a runaway sync is visible in `-v` output.
    pub fn trace(&self, query: &str) {
        tracing::debug!(
            query,
            cost = self.cost,
            remaining = self.remaining,
            limit = self.limit,
            reset_at = %self.reset_at,
            "graphql rate limit"
        );
    }
}

/// Deserialization tests against responses captured from the real API with
/// `gh api graphql`.
///
/// These are the guard against the failure mode this project is most exposed
/// to: a guessed or drifted GraphQL field name deserializing into a default and
/// producing a silently empty queue rather than an error. Re-capture a fixture
/// with the query in the corresponding method whenever one changes.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewer_response_deserializes() {
        let raw = include_str!("../tests/fixtures/graphql/viewer.json");
        let data: ViewerQuery = serde_json::from_str(raw).expect("captured response parses");

        assert_eq!(data.viewer.login, "ashb");
        assert_eq!(data.rate_limit.limit, 5000);
        assert_eq!(data.rate_limit.cost, 1);
        assert_eq!(
            data.rate_limit.reset_at,
            "2026-08-05T16:48:03Z".parse::<Timestamp>().unwrap()
        );
    }

    /// serde would otherwise accept a response missing `rateLimit` by leaving
    /// the field at its default, which is exactly the silent-truncation class of
    /// bug the fixtures exist to catch.
    #[test]
    fn a_response_missing_rate_limit_is_an_error() {
        let raw = r#"{"viewer": {"login": "ashb"}}"#;
        serde_json::from_str::<ViewerQuery>(raw).expect_err("rateLimit is required");
    }
}
