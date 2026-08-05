//! The GitHub adapter: the one implementation of [`Forge`](crate::Forge) today.
//!
//! An octocrab wrapper that returns the plain data types in [`crate::types`];
//! no model or ledger types cross this boundary. The tier-1 sweep fetches each
//! PR's changed files in the same query, so there is no separate file round
//! trip and a PR arrives ready to classify.

use anyhow::{Context, Result};
use async_trait::async_trait;
use octocrab::Octocrab;
use reviewq_core::model::{PrSnapshot, PrState};
use serde::Deserialize;

use crate::types::{RateLimit, SweepPage, Viewer};
use crate::{Forge, ForgeHost};

/// A GitHub connection bound to one host.
pub struct GithubForge {
    inner: Octocrab,
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

    async fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        query: &str,
        variables: serde_json::Map<String, serde_json::Value>,
    ) -> Result<T> {
        // Our own operation-named line; octocrab's per-request HTTP tracing is
        // silenced at reviewq's -v levels (see the binary's tracing setup).
        tracing::debug!(op, "graphql request");
        let payload = serde_json::json!({ "query": query, "variables": variables });
        self.inner
            .graphql(&payload)
            .await
            .with_context(|| format!("GitHub GraphQL request failed ({op})"))
    }
}

#[async_trait]
impl Forge for GithubForge {
    async fn viewer(&self) -> Result<Viewer> {
        const QUERY: &str = r"
            query {
              viewer { login }
              rateLimit { limit cost remaining resetAt }
            }
        ";
        let data: ViewerQuery = self
            .graphql("viewer", QUERY, serde_json::Map::new())
            .await?;
        Ok(Viewer {
            login: data.viewer.login,
            rate_limit: data.rate_limit,
        })
    }

    async fn rest_core_remaining(&self) -> Result<(u32, u32)> {
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

    async fn search_prs_page(
        &self,
        query: &str,
        page_size: u32,
        after: Option<&str>,
    ) -> Result<SweepPage> {
        let mut vars = serde_json::Map::new();
        vars.insert("q".into(), query.into());
        vars.insert("size".into(), page_size.into());
        vars.insert(
            "after".into(),
            after.map_or(serde_json::Value::Null, Into::into),
        );

        let data: SearchQuery = self.graphql("search", SEARCH_QUERY, vars).await?;
        data.rate_limit.trace("sync:search");

        let mut prs = Vec::with_capacity(data.search.nodes.len());
        for node in data.search.nodes {
            prs.push(node.into_snapshot()?);
        }
        let next = data
            .search
            .page_info
            .has_next_page
            .then_some(data.search.page_info.end_cursor)
            .flatten();

        Ok(SweepPage {
            prs,
            next,
            total_count: data.search.issue_count,
            cost: data.rate_limit.cost,
            remaining: data.rate_limit.remaining,
        })
    }

    async fn fetch_pr(&self, owner: &str, name: &str, number: u64) -> Result<Option<PrSnapshot>> {
        let mut vars = serde_json::Map::new();
        vars.insert("owner".into(), owner.into());
        vars.insert("name".into(), name.into());
        vars.insert("number".into(), number.into());

        let data: FetchQuery = self
            .graphql(&format!("fetch_pr #{number}"), FETCH_PR_QUERY, vars)
            .await?;
        data.repository
            .and_then(|r| r.pull_request)
            .map(PrNode::into_snapshot)
            .transpose()
    }
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

#[derive(Debug, Deserialize)]
struct SearchQuery {
    search: SearchConn,
    #[serde(rename = "rateLimit")]
    rate_limit: RateLimit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConn {
    issue_count: u32,
    page_info: PageInfo,
    nodes: Vec<PrNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    end_cursor: Option<String>,
    has_next_page: bool,
}

/// A pull request as the sweep and single-PR fetch both see it, changed files
/// included.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrNode {
    number: u64,
    title: String,
    is_draft: bool,
    state: String,
    author: Option<Login>,
    author_association: String,
    head_ref_oid: String,
    updated_at: jiff::Timestamp,
    labels: LabelConn,
    milestone: Option<Milestone>,
    files: FilesConn,
}

#[derive(Debug, Deserialize)]
struct Login {
    login: String,
}

#[derive(Debug, Deserialize)]
struct LabelConn {
    nodes: Vec<Label>,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Milestone {
    title: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesConn {
    total_count: u32,
    nodes: Vec<PathNode>,
}

#[derive(Debug, Deserialize)]
struct PathNode {
    path: String,
}

impl PrNode {
    fn into_snapshot(self) -> Result<PrSnapshot> {
        let state = PrState::from_wire(&self.state)
            .with_context(|| format!("PR #{}: unknown state {:?}", self.number, self.state))?;
        let paths: Vec<String> = self.files.nodes.into_iter().map(|n| n.path).collect();
        let files_truncated = self.files.total_count > paths.len() as u32;
        Ok(PrSnapshot {
            number: self.number,
            title: self.title,
            // A deleted account shows as a null author; GitHub calls it "ghost".
            author: self.author.map_or_else(|| "ghost".to_string(), |a| a.login),
            author_association: self.author_association,
            head_sha: self.head_ref_oid,
            is_draft: self.is_draft,
            state,
            updated_at: self.updated_at,
            labels: self.labels.nodes.into_iter().map(|l| l.name).collect(),
            milestone: self.milestone.map(|m| m.title),
            files: Some(paths),
            files_truncated,
        })
    }
}

#[derive(Debug, Deserialize)]
struct FetchQuery {
    repository: Option<RepoPr>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPr {
    pull_request: Option<PrNode>,
}

// The two queries below share the same PR node selection (files included). If
// you change one selection, change the other; both are covered by the
// deserialization tests, which fail loudly if a field name drifts.
const SEARCH_QUERY: &str = r"
query($q: String!, $size: Int!, $after: String) {
  search(query: $q, type: ISSUE, first: $size, after: $after) {
    issueCount
    pageInfo { endCursor hasNextPage }
    nodes { ... on PullRequest {
      number title isDraft state
      author { login }
      authorAssociation
      headRefOid
      updatedAt
      labels(first: 30) { nodes { name } }
      milestone { title }
      files(first: 100) { totalCount nodes { path } }
    } }
  }
  rateLimit { limit cost remaining resetAt }
}
";

const FETCH_PR_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number title isDraft state
      author { login }
      authorAssociation
      headRefOid
      updatedAt
      labels(first: 30) { nodes { name } }
      milestone { title }
      files(first: 100) { totalCount nodes { path } }
    }
  }
  rateLimit { limit cost remaining resetAt }
}
";

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
    }

    #[test]
    fn a_response_missing_rate_limit_is_an_error() {
        let raw = r#"{"viewer": {"login": "ashb"}}"#;
        serde_json::from_str::<ViewerQuery>(raw).expect_err("rateLimit is required");
    }

    #[test]
    fn search_response_deserializes_with_files() {
        let raw = include_str!("../tests/fixtures/graphql/search.json");
        let data: SearchQuery = serde_json::from_str(raw).expect("captured search parses");

        assert_eq!(data.search.issue_count, 738);
        assert!(data.search.page_info.has_next_page);

        let first = data.search.nodes.into_iter().next().expect("a node");
        let snapshot = first.into_snapshot().expect("converts");
        assert_eq!(snapshot.number, 71196);
        assert!(snapshot.is_draft);
        assert_eq!(snapshot.state, PrState::Open);
        assert_eq!(snapshot.author, "rjgoyln");
        assert!(snapshot.labels.contains(&"area:task-sdk".to_string()));
        // Files arrive with the sweep now.
        let files = snapshot.files.expect("files populated");
        assert!(files.contains(&"task-sdk/src/airflow/sdk/api/client.py".to_string()));
        // The fixture node reports more files than it lists.
        assert!(snapshot.files_truncated);
    }

    #[test]
    fn a_null_author_becomes_ghost() {
        let node: PrNode = serde_json::from_str(
            r#"{
                "number": 1, "title": "t", "isDraft": false, "state": "OPEN",
                "author": null, "authorAssociation": "NONE",
                "headRefOid": "abc", "updatedAt": "2026-08-05T12:00:00Z",
                "labels": {"nodes": []}, "milestone": null,
                "files": {"totalCount": 0, "nodes": []}
            }"#,
        )
        .expect("parses");
        let snapshot = node.into_snapshot().unwrap();
        assert_eq!(snapshot.author, "ghost");
        assert!(!snapshot.files_truncated);
    }
}
