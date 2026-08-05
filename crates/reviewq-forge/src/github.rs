//! The GitHub adapter: the one implementation of [`Forge`](crate::Forge) today.
//!
//! An octocrab wrapper that returns the plain data types in [`crate::types`];
//! no model or ledger types cross this boundary. The tier-1 sweep fetches each
//! PR's changed files in the same query, so there is no separate file round
//! trip and a PR arrives ready to classify.

use anyhow::{Context, Result};
use async_trait::async_trait;
use jiff::Timestamp;
use octocrab::Octocrab;
use reviewq_core::model::{Mention, PrSnapshot, PrState, ReviewRequest, ThreadState, Verdict};
use serde::Deserialize;

use crate::types::{PrDetail, RateLimit, SweepPage, Viewer};
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

    async fn fetch_pr_detail(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        login: &str,
    ) -> Result<Option<PrDetail>> {
        let mut vars = serde_json::Map::new();
        vars.insert("owner".into(), owner.into());
        vars.insert("name".into(), name.into());
        vars.insert("number".into(), number.into());

        let data: DetailQuery = self
            .graphql(&format!("fetch_detail #{number}"), DETAIL_QUERY, vars)
            .await?;
        data.rate_limit.trace("sync:detail");

        let cost = data.rate_limit.cost;
        let remaining = data.rate_limit.remaining;
        Ok(data
            .repository
            .and_then(|r| r.pull_request)
            .map(|pr| pr.into_detail(login, cost, remaining)))
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

#[derive(Debug, Deserialize)]
struct DetailQuery {
    repository: Option<DetailRepo>,
    #[serde(rename = "rateLimit")]
    rate_limit: RateLimit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailRepo {
    pull_request: Option<DetailPr>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailPr {
    number: u64,
    head_ref_oid: String,
    review_requests: NodeList<ReviewRequestNode>,
    reviews: NodeList<ReviewNode>,
    comments: NodeList<CommentNode>,
    commits: NodeList<CommitWrap>,
    review_threads: NodeList<ThreadNode>,
}

#[derive(Debug, Deserialize)]
struct NodeList<T> {
    nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewRequestNode {
    requested_reviewer: Option<Reviewer>,
}

/// A requested reviewer is a `User` or a `Team`; the inline fragments select the
/// discriminating field for each, and `__typename` says which was returned.
#[derive(Debug, Deserialize)]
struct Reviewer {
    #[serde(rename = "__typename")]
    typename: String,
    login: Option<String>,
    #[allow(dead_code)]
    slug: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    author: Option<Login>,
    state: String,
    submitted_at: Option<Timestamp>,
    commit: Option<Oid>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct Oid {
    oid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommentNode {
    author: Option<Login>,
    created_at: Timestamp,
    body: String,
}

#[derive(Debug, Deserialize)]
struct CommitWrap {
    commit: CommitNode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitNode {
    committed_date: Timestamp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadNode {
    id: String,
    is_resolved: bool,
    resolved_by: Option<Login>,
    comments: NodeList<CommentNode>,
}

fn author_login(author: &Option<Login>) -> Option<&str> {
    author.as_ref().map(|a| a.login.as_str())
}

/// Remove Markdown code — fenced blocks and inline spans — so a handle pasted
/// in a code sample or `@quoted` in backticks is not read as a live mention.
/// Backtick runs are matched by length (` ``` ` closes ` ``` `, `` ` `` closes
/// `` ` ``); an unterminated run drops the rest, which is the safe direction.
fn strip_code(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find('`') {
        out.push_str(&rest[..start]);
        let run = rest[start..].bytes().take_while(|&b| b == b'`').count();
        let delim = "`".repeat(run);
        let after = &rest[start + run..];
        match after.find(&delim) {
            Some(end) => rest = &after[end + run..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Whether `body` @mentions `login`, requiring a word boundary on each side so
/// `@ashbourne` and an email `x@ashb` do not count as mentions of `ashb`.
/// Code is stripped first (see [`strip_code`]).
fn mentions_login(body: &str, login: &str) -> bool {
    let needle = format!("@{}", login.to_ascii_lowercase());
    let hay = strip_code(body).to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(&needle) {
        let at = from + rel;
        let before_ok = at == 0
            || !hay[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after = at + needle.len();
        let after_ok = hay[after..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '/'));
        if before_ok && after_ok {
            return true;
        }
        from = after;
    }
    false
}

impl DetailPr {
    fn into_detail(self, login: &str, cost: u32, remaining: u32) -> PrDetail {
        let mut action_times: Vec<Timestamp> = Vec::new();
        let mut mentions: Vec<Mention> = Vec::new();

        // My latest parseable review sets last_reviewed_sha / verdict; every one
        // of my reviews counts toward last_action_at.
        let mut last: Option<(Timestamp, Verdict, Option<String>)> = None;
        for review in &self.reviews.nodes {
            let mine = author_login(&review.author) == Some(login);
            if mine && let Some(at) = review.submitted_at {
                action_times.push(at);
            }
            if let Some(other) =
                mention_from(&review.author, review.submitted_at, &review.body, login)
            {
                mentions.push(other);
            }
            if !mine {
                continue;
            }
            let Some(verdict) = Verdict::from_wire(&review.state) else {
                continue;
            };
            let Some(at) = review.submitted_at else {
                continue;
            };
            if last.as_ref().is_none_or(|(cur, ..)| at > *cur) {
                last = Some((at, verdict, review.commit.as_ref().map(|c| c.oid.clone())));
            }
        }
        let (review_at, last_verdict, last_reviewed_sha) = match last {
            Some((at, v, sha)) => (Some(at), Some(v), sha),
            None => (None, None, None),
        };

        for comment in &self.comments.nodes {
            if author_login(&comment.author) == Some(login) {
                action_times.push(comment.created_at);
            }
            if let Some(m) = mention_from(
                &comment.author,
                Some(comment.created_at),
                &comment.body,
                login,
            ) {
                mentions.push(m);
            }
        }

        let threads = self
            .review_threads
            .nodes
            .into_iter()
            .map(|t| t.into_thread(login, &mut action_times, &mut mentions))
            .collect();

        // Commits after my last review are the re-review's "new commits".
        let new_commits = review_at.map_or(0, |at| {
            self.commits
                .nodes
                .iter()
                .filter(|c| c.commit.committed_date > at)
                .count() as u32
        });

        let review_request = self.review_requests.nodes.iter().find_map(|r| {
            let reviewer = r.requested_reviewer.as_ref()?;
            (reviewer.typename == "User" && reviewer.login.as_deref() == Some(login))
                .then_some(ReviewRequest { team: None })
        });

        PrDetail {
            number: self.number,
            head_sha: self.head_ref_oid,
            last_reviewed_sha,
            last_verdict,
            last_action_at: action_times.into_iter().max(),
            threads,
            mentions,
            new_commits,
            review_request,
            cost,
            remaining,
        }
    }
}

/// A mention of `login` by someone else, if `body` names them and `at` is known.
fn mention_from(
    author: &Option<Login>,
    at: Option<Timestamp>,
    body: &str,
    login: &str,
) -> Option<Mention> {
    let by = author_login(author)?;
    if by == login || !mentions_login(body, login) {
        return None;
    }
    Some(Mention {
        by: by.to_string(),
        at: at?,
    })
}

impl ThreadNode {
    /// Fold this thread into a [`ThreadState`], contributing my comment times to
    /// `action_times` and any mentions of me to `mentions` along the way.
    fn into_thread(
        self,
        login: &str,
        action_times: &mut Vec<Timestamp>,
        mentions: &mut Vec<Mention>,
    ) -> ThreadState {
        let starter = self.comments.nodes.first();
        let i_own = starter
            .and_then(|c| author_login(&c.author))
            .is_some_and(|a| a == login);

        let mut my_last_comment_at: Option<Timestamp> = None;
        for comment in &self.comments.nodes {
            if author_login(&comment.author) == Some(login) {
                action_times.push(comment.created_at);
                my_last_comment_at = Some(
                    my_last_comment_at.map_or(comment.created_at, |m| m.max(comment.created_at)),
                );
            }
            if let Some(m) = mention_from(
                &comment.author,
                Some(comment.created_at),
                &comment.body,
                login,
            ) {
                mentions.push(m);
            }
        }

        let last = self.comments.nodes.iter().max_by_key(|c| c.created_at);
        ThreadState {
            thread_id: self.id,
            i_own,
            is_resolved: self.is_resolved,
            resolved_by: self.resolved_by.map(|r| r.login),
            last_comment_author: last.and_then(|c| author_login(&c.author).map(str::to_string)),
            last_comment_at: last.map(|c| c.created_at),
            my_last_comment_at,
        }
    }
}

const DETAIL_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      headRefOid
      reviewRequests(first: 20) {
        nodes { requestedReviewer {
          __typename
          ... on User { login }
          ... on Team { slug }
        } }
      }
      reviews(first: 100) {
        nodes { author { login } state submittedAt commit { oid } body }
      }
      comments(first: 100) {
        nodes { author { login } createdAt body }
      }
      commits(first: 100) {
        nodes { commit { committedDate } }
      }
      reviewThreads(first: 100) {
        nodes {
          id
          isResolved
          resolvedBy { login }
          comments(first: 100) { nodes { author { login } createdAt body } }
        }
      }
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
    fn detail_response_derives_state_from_my_point_of_view() {
        let raw = include_str!("../tests/fixtures/graphql/pr_detail.json");
        let data: DetailQuery = serde_json::from_str(raw).expect("captured detail parses");
        let pr = data.repository.unwrap().pull_request.unwrap();
        let detail = pr.into_detail("ashb", data.rate_limit.cost, data.rate_limit.remaining);

        assert_eq!(
            detail.last_reviewed_sha.as_deref(),
            Some("abc123f8901234567890123456789012345678ab"),
            "my latest submitted review sets the reviewed sha; the PENDING one is ignored"
        );
        assert_eq!(detail.last_verdict, Some(Verdict::Approved));
        // Two commits land after my 10:00 review.
        assert_eq!(detail.new_commits, 2);
        // A direct request to me fires; the team request does not.
        assert_eq!(detail.review_request, Some(ReviewRequest { team: None }));
        // uranusjr @mentioned me in a comment after I last acted.
        assert_eq!(detail.mentions.len(), 1);
        assert_eq!(detail.mentions[0].by, "uranusjr");
        // Last action is my most recent comment/review across everything.
        assert_eq!(
            detail.last_action_at,
            Some("2026-08-01T10:05:00Z".parse().unwrap())
        );

        let mine = detail
            .threads
            .iter()
            .find(|t| t.thread_id == "PRRT_mine")
            .unwrap();
        assert!(mine.i_own, "I started this thread");
        assert!(!mine.is_resolved);
        assert_eq!(mine.last_comment_author.as_deref(), Some("kaxil"));
        assert_eq!(
            mine.my_last_comment_at,
            Some("2026-08-01T10:05:00Z".parse().unwrap())
        );

        let theirs = detail
            .threads
            .iter()
            .find(|t| t.thread_id == "PRRT_theirs")
            .unwrap();
        assert!(!theirs.i_own, "uranusjr started this one");
        assert_eq!(theirs.resolved_by.as_deref(), Some("uranusjr"));
    }

    #[test]
    fn mention_matching_respects_word_boundaries() {
        assert!(mentions_login("ping @ashb please", "ashb"));
        assert!(mentions_login("@ashb", "ashb"));
        assert!(mentions_login("cc @AshB", "ashb"));
        assert!(!mentions_login("@ashbourne is someone else", "ashb"));
        assert!(!mentions_login("mail x@ashb.dev", "ashb"));
        assert!(!mentions_login("no handle here", "ashb"));
        // Handles inside code do not count.
        assert!(!mentions_login("see `@ashb` in the sample", "ashb"));
        assert!(!mentions_login("```\ncc @ashb\n```", "ashb"));
        // ...but a real mention alongside code still does.
        assert!(mentions_login("`code` then @ashb please", "ashb"));
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
