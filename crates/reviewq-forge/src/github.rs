//! The GitHub adapter: the one implementation of [`Forge`](crate::Forge) today.
//!
//! An octocrab wrapper that returns the plain data types in [`crate::types`];
//! no model or ledger types cross this boundary. The tier-1 sweep fetches each
//! PR's changed files in the same query, so there is no separate file round
//! trip and a PR arrives ready to classify.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use async_trait::async_trait;
use jiff::Timestamp;
use octocrab::models::activity::Notification;
use octocrab::{GraphqlError, GraphqlPathSegment, GraphqlResponse, Octocrab};
use reviewq_core::model::{
    Mention, PrSnapshot, PrState, ReviewRequest, ReviewerVerdict, ThreadState, Verdict,
};
use serde::Deserialize;

use crate::host::GITHUB_TOKEN_ENV;
use crate::types::{FetchedPr, LabelColour, PrDetail, RateLimit, SweepPage, Viewer};
use crate::{Forge, ForgeError, ForgeHost, Result, Token, resolve_token};

/// Classify what octocrab reported.
///
/// The three that change what a reader should do are told apart here, once, rather
/// than every call site guessing: credentials the forge refused, a spent budget,
/// and everything else.
fn classify(host: &str, doing: String, err: octocrab::Error) -> ForgeError {
    let message = err.to_string();
    let rejected = message.contains("401")
        || message.contains("Bad credentials")
        || message.contains("Unauthorized");
    let budget = message.contains("rate limit") || message.contains("API rate limit exceeded");
    if rejected {
        ForgeError::Rejected {
            host: host.to_string(),
            source: Box::new(err),
        }
    } else if budget {
        ForgeError::BudgetSpent {
            host: host.to_string(),
        }
    } else {
        ForgeError::Unreachable {
            doing,
            source: Box::new(err),
        }
    }
}

/// Anything that isn't the forge's fault: a bad `api_base`, a state we don't know.
fn unreachable(
    doing: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> ForgeError {
    ForgeError::Unreachable {
        doing: doing.into(),
        source: Box::new(source),
    }
}

/// A GitHub connection bound to one host.
///
/// Constructing one costs nothing and needs no credential: the token and the API
/// client behind it are resolved on first use and remembered. Resolution can run
/// a subprocess — a configured `token_command`, `gh auth token` — and that
/// subprocess may prompt, so it must not happen for a caller that only wanted
/// [`web_url`](Forge::web_url).
pub struct GithubForge {
    /// The host's resolved settings, kept because the token and the client are
    /// derived from them on demand rather than up front.
    host: ForgeHost,
    /// The host's own hostname (`github.com`, or a GitHub Enterprise host),
    /// doubling as its web root — kept alongside the settings so
    /// [`web_url`](Forge::web_url) needs no extra argument from the caller.
    web_host: String,
    /// The env var external GitHub tooling expects the token under: the host's
    /// own `token_env` if configured, else GitHub's own convention. Known from
    /// config alone, so naming it never triggers a resolution.
    token_env: String,
    /// The token, resolved at most once — see [`token`](Self::token).
    token: OnceLock<Token>,
    /// The API client, built from that token on the same terms.
    client: OnceLock<Octocrab>,
}

impl GithubForge {
    /// An adapter for `host` that will resolve its own token when it first needs
    /// one.
    pub fn new(host: &ForgeHost, host_name: &str) -> Self {
        Self {
            host: host.clone(),
            web_host: host_name.to_string(),
            token_env: host
                .token_env
                .clone()
                .unwrap_or_else(|| GITHUB_TOKEN_ENV.to_string()),
            token: OnceLock::new(),
            client: OnceLock::new(),
        }
    }

    /// An adapter for `host` using a token already in hand.
    ///
    /// For a caller that resolved one itself and wants to report on it —
    /// `doctor`, which prints where the token came from as its own step — so that
    /// reporting doesn't cost a second resolution, and a second prompt.
    pub fn with_token(host: &ForgeHost, host_name: &str, token: Token) -> Self {
        let forge = Self::new(host, host_name);
        let _ = forge.token.set(token);
        forge
    }

    /// The token, resolving it once on first use.
    ///
    /// `OnceLock` rather than a lock held across the resolution: two threads
    /// racing here would resolve twice and one result is dropped, which is
    /// cheaper than serialising every authenticated call behind a mutex.
    fn token(&self) -> Result<&Token> {
        if let Some(token) = self.token.get() {
            return Ok(token);
        }
        let resolved = resolve_token(&self.host)?;
        Ok(self.token.get_or_init(|| resolved))
    }

    /// The API client, built once from the token.
    fn client(&self) -> Result<&Octocrab> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let mut builder = Octocrab::builder().personal_token(self.token()?.value.clone());
        if let Some(api_base) = &self.host.api_base {
            builder = builder
                .base_uri(api_base.as_str())
                .map_err(|err| unreachable(format!("invalid api_base {api_base:?}"), err))?;
        }
        let built = builder
            .build()
            .map_err(|err| unreachable("building the GitHub client", err))?;
        Ok(self.client.get_or_init(|| built))
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
        self.client()?.graphql(&payload).await.map_err(|err| {
            classify(
                &self.web_host,
                format!("GitHub GraphQL request ({op})"),
                err,
            )
        })
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
        let limits = self.client()?.ratelimit().get().await.map_err(|err| {
            classify(
                &self.web_host,
                "fetching the REST rate limit".to_string(),
                err,
            )
        })?;
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

    async fn fetch_pr(&self, owner: &str, name: &str, number: u64) -> Result<Option<FetchedPr>> {
        let mut vars = serde_json::Map::new();
        vars.insert("owner".into(), owner.into());
        vars.insert("name".into(), name.into());
        vars.insert("number".into(), number.into());

        let data: FetchQuery = self
            .graphql(&format!("fetch_pr #{number}"), FETCH_PR_QUERY, vars)
            .await?;
        data.repository
            .and_then(|r| r.pull_request)
            .map(|node| {
                let labels = node.label_colours().collect();
                Ok(FetchedPr {
                    pr: node.into_snapshot()?,
                    labels,
                })
            })
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

        // Not `self.graphql`: a PR that no longer exists comes back as a
        // GraphQL error, which that helper collapses into one opaque failure —
        // aborting a sync over a single unreachable PR. Posting directly keeps
        // the error list so [`pull_request_is_gone`] can recognise that one
        // case and let every other error through unchanged.
        let op = format!("fetch_detail #{number}");
        let payload = serde_json::json!({ "query": DETAIL_QUERY, "variables": vars });
        tracing::debug!(op, "graphql request");
        let response: GraphqlResponse<DetailQuery> = self
            .client()?
            .post("/graphql", Some(&payload))
            .await
            .map_err(|err| {
                classify(
                    &self.web_host,
                    format!("GitHub GraphQL request ({op})"),
                    err,
                )
            })?;

        let data = match response {
            GraphqlResponse::Ok(ok) => ok.data,
            GraphqlResponse::Err(err) => {
                if pull_request_is_gone(&err.errors) {
                    tracing::warn!(
                        owner,
                        name,
                        number,
                        "PR could not be resolved on the forge — deleted, or never a \
                         pull request; treating it as gone"
                    );
                    return Ok(None);
                }
                // A GraphQL error list, not a transport failure — the forge
                // answered and refused. Rejected credentials come back this way
                // too, so the message is checked before settling on unreachable.
                let rendered = render_graphql_errors(&err.errors);
                if rendered.contains("Bad credentials") {
                    return Err(ForgeError::Rejected {
                        host: self.web_host.clone(),
                        source: rendered.into(),
                    });
                }
                return Err(ForgeError::Unreachable {
                    doing: format!("GitHub GraphQL request ({op}): {rendered}"),
                    source: "the forge returned errors".into(),
                });
            }
        };
        data.rate_limit.trace("sync:detail");

        let cost = data.rate_limit.cost;
        let remaining = data.rate_limit.remaining;
        Ok(data
            .repository
            .and_then(|r| r.pull_request)
            .map(|pr| pr.into_detail(login, cost, remaining)))
    }

    async fn fetch_labels(&self, owner: &str, name: &str) -> Result<Vec<LabelColour>> {
        let mut labels = Vec::new();
        let mut after: Option<String> = None;
        // Paginated because a big project has hundreds: apache/airflow alone
        // carries a `provider:*` label per provider.
        loop {
            let mut vars = serde_json::Map::new();
            vars.insert("owner".into(), owner.into());
            vars.insert("name".into(), name.into());
            vars.insert("after".into(), after.clone().into());

            let data: LabelsQuery = self
                .graphql(&format!("labels for {owner}/{name}"), LABELS_QUERY, vars)
                .await?;
            let Some(page) = data.repository.map(|repo| repo.labels) else {
                return Ok(labels);
            };
            labels.extend(page.nodes.iter().filter_map(|label| {
                Some(LabelColour {
                    name: label.name.clone(),
                    color: label.color.clone()?,
                })
            }));
            match page
                .page_info
                .has_next_page
                .then_some(page.page_info.end_cursor)
            {
                Some(Some(cursor)) => after = Some(cursor),
                _ => return Ok(labels),
            }
        }
    }

    async fn mark_pr_notifications_read(&self, owner: &str, name: &str, number: u64) -> Result<()> {
        let client = self.client()?;
        // Deliberately not `.all(true)` — that opts into *every* notification
        // for the repo, read ones included ("If set, show notifications
        // marked as read", per octocrab's docs), which on a busy repo means
        // paginating the whole read backlog on every single `reviewq done`.
        // The default is already unread-only, matching what `done` needs.
        let first_page = client
            .activity()
            .notifications()
            .list_for_repo(owner, name)
            .per_page(50)
            .send()
            .await
            .map_err(|err| {
                classify(
                    &self.web_host,
                    format!("listing notifications for {owner}/{name}"),
                    err,
                )
            })?;
        let notifications: Vec<Notification> =
            client.all_pages(first_page).await.map_err(|err| {
                classify(
                    &self.web_host,
                    format!("paginating notifications for {owner}/{name}"),
                    err,
                )
            })?;

        // The subject URL is the PR's REST API URL (".../pulls/{number}"); it's
        // the only field that names which PR a notification belongs to.
        let suffix = format!("/pulls/{number}");
        for n in notifications {
            let is_this_pr = n
                .subject
                .url
                .as_ref()
                .is_some_and(|url| url.as_str().ends_with(&suffix));
            if is_this_pr {
                client
                    .activity()
                    .notifications()
                    .mark_as_read(n.id)
                    .await
                    .map_err(|err| {
                        classify(
                            &self.web_host,
                            format!("marking notification {} read", n.id),
                            err,
                        )
                    })?;
            }
        }
        Ok(())
    }

    fn web_url(&self, owner: &str, name: &str, number: u64) -> String {
        format!("https://{}/{owner}/{name}/pull/{number}", self.web_host)
    }

    fn handoff_credentials(&self) -> Result<(&str, &str)> {
        Ok((&self.token_env, self.token()?.value.as_str()))
    }
}

impl GithubForge {
    /// Read `owner`, `name` and the number out of the path of a pull-request
    /// URL on this provider — the inverse of [`Forge::web_url`], and kept beside
    /// it so the pair cannot drift.
    ///
    /// Associated rather than a method: parsing a URL needs no connection and no
    /// token, so an interface can do it before deciding whether to fetch
    /// anything.
    ///
    /// `path` is everything after the host. GitHub's shape is
    /// `/owner/name/pull/N`, and the number is read as its leading digits
    /// because a URL copied from a browser rarely ends there — `/files`, `?w=1`
    /// and a `#issuecomment-…` permalink all arrive stuck to it.
    pub fn parse_web_path(path: &str) -> Option<(String, String, u64)> {
        let (repo, tail) = path.trim_start_matches('/').split_once("/pull/")?;
        let (owner, name) = repo.split_once('/')?;
        if owner.is_empty() || name.is_empty() {
            return None;
        }
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        Some((owner.to_string(), name.to_string(), digits.parse().ok()?))
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
    base_ref_name: String,
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
    /// Six hex digits, no `#`. Absent from the tier-2 query, which has no use
    /// for it.
    #[serde(default)]
    color: Option<String>,
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
    /// The labels this PR carries, with the colours the repo paints them.
    ///
    /// Taken before the snapshot swallows the node, since the snapshot keeps
    /// only the names.
    fn label_colours(&self) -> impl Iterator<Item = LabelColour> + '_ {
        self.labels.nodes.iter().filter_map(|label| {
            Some(LabelColour {
                name: label.name.clone(),
                color: label.color.clone()?,
            })
        })
    }

    fn into_snapshot(self) -> Result<PrSnapshot> {
        let state = PrState::from_wire(&self.state).ok_or_else(|| ForgeError::Unreachable {
            doing: format!("PR #{}: unknown state {:?}", self.number, self.state),
            source: "the forge reported a pull-request state this build does not know".into(),
        })?;
        let paths: Vec<String> = self.files.nodes.into_iter().map(|n| n.path).collect();
        let files_truncated = self.files.total_count > paths.len() as u32;
        Ok(PrSnapshot {
            number: self.number,
            title: self.title,
            // A deleted account shows as a null author; GitHub calls it "ghost".
            author: self.author.map_or_else(|| "ghost".to_string(), |a| a.login),
            author_association: self.author_association,
            head_sha: self.head_ref_oid,
            base_ref: self.base_ref_name,
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
struct LabelsQuery {
    repository: Option<RepoLabels>,
}

#[derive(Debug, Deserialize)]
struct RepoLabels {
    labels: LabelPage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelPage {
    page_info: PageInfo,
    nodes: Vec<Label>,
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
      baseRefName
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
      baseRefName
      updatedAt
      labels(first: 30) { nodes { name color } }
      milestone { title }
      files(first: 100) { totalCount nodes { path } }
    }
  }
  rateLimit { limit cost remaining resetAt }
}
";

/// Join GraphQL error messages into one line for a [`ForgeError`].
///
/// Written here rather than reusing octocrab's own `Display`, which isn't
/// publicly re-exported and which pads the message with source locations and a
/// backtrace note that say nothing useful about a failed sync.
fn render_graphql_errors(errors: &[GraphqlError]) -> String {
    if errors.is_empty() {
        return "no error detail".to_string();
    }
    errors
        .iter()
        .map(|error| error.message.trim_end_matches('.'))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether `errors` say only that the pull request asked for doesn't exist.
///
/// GitHub answers a request for a PR that has been deleted — or a number that
/// was never a PR — with `pullRequest: null` *and* a `NOT_FOUND` error, so the
/// response is an error rather than the empty success the null alone would be.
///
/// It's recognised structurally rather than by that `NOT_FOUND` type, because
/// octocrab's [`GraphqlError`] follows the GraphQL spec and drops GitHub's
/// non-standard `type` field: what's left is the path the error is attached to
/// and its message. Both are required to match, so an error about some other
/// field can't be mistaken for this.
///
/// Requires *every* error to be that one thing. A response mixing an
/// unreachable PR with a rate-limit or permission error is a real failure, and
/// swallowing it would turn a broken token into a silently short sync.
fn pull_request_is_gone(errors: &[GraphqlError]) -> bool {
    !errors.is_empty()
        && errors.iter().all(|error| {
            let about_the_pull_request = error.path.as_deref().is_some_and(|path| {
                path.iter().any(|segment| {
                    matches!(segment, GraphqlPathSegment::Path(field) if field == "pullRequest")
                })
            });
            about_the_pull_request
                && error
                    .message
                    .starts_with("Could not resolve to a PullRequest")
        })
}

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
    /// The PR's description. GraphQL types it non-null, but an empty
    /// description is the common case, so it's defaulted rather than required.
    #[serde(default)]
    body: String,
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
        // Everyone's latest parseable review, not just mine — purely
        // informational, so a dismissed or superseded-by-a-plain-comment
        // review is dropped the same way `last`/`last_verdict` already treats
        // mine: whichever review has the newest `submittedAt` wins, dismissal
        // notwithstanding.
        let mut reviewer_latest: BTreeMap<String, (Timestamp, Verdict)> = BTreeMap::new();
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
            if let Some(reviewer) = author_login(&review.author)
                && let Some(verdict) = Verdict::from_wire(&review.state)
                && let Some(at) = review.submitted_at
            {
                reviewer_latest
                    .entry(reviewer.to_string())
                    .and_modify(|(cur_at, cur_verdict)| {
                        if at > *cur_at {
                            *cur_at = at;
                            *cur_verdict = verdict;
                        }
                    })
                    .or_insert((at, verdict));
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
        let reviewers = reviewer_latest
            .into_iter()
            .map(|(login, (at, verdict))| ReviewerVerdict { login, verdict, at })
            .collect();

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
            body: self.body,
            last_reviewed_sha,
            last_verdict,
            last_action_at: action_times.into_iter().max(),
            threads,
            reviewers,
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

const LABELS_QUERY: &str = r"
query($owner: String!, $name: String!, $after: String) {
  repository(owner: $owner, name: $name) {
    labels(first: 100, after: $after) {
      pageInfo { endCursor hasNextPage }
      nodes { name color }
    }
  }
  rateLimit { limit cost remaining resetAt }
}
";

const DETAIL_QUERY: &str = r"
query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      number
      headRefOid
      body
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

    /// A GraphQL error as GitHub sends it, minus the `type` octocrab discards.
    fn graphql_error(message: &str, path: &[&str]) -> GraphqlError {
        GraphqlError {
            message: message.to_string(),
            locations: None,
            path: Some(
                path.iter()
                    .map(|segment| GraphqlPathSegment::Path((*segment).to_string()))
                    .collect(),
            ),
            extensions: None,
        }
    }

    /// Verbatim from a real sync against a PR that had gone.
    fn pr_not_found() -> GraphqlError {
        graphql_error(
            "Could not resolve to a PullRequest with the number of 70787.",
            &["repository", "pullRequest"],
        )
    }

    #[test]
    fn a_missing_pull_request_is_recognised_as_gone() {
        assert!(pull_request_is_gone(&[pr_not_found()]));
    }

    #[test]
    fn an_empty_error_list_is_not_a_missing_pull_request() {
        // Nothing went wrong is not the same as the PR being gone, and must not
        // silently turn into an empty result.
        assert!(!pull_request_is_gone(&[]));
    }

    #[test]
    fn an_unrelated_failure_is_never_treated_as_gone() {
        for error in [
            // A real problem with the request, not with the PR.
            graphql_error("API rate limit exceeded for user ID 1.", &["repository"]),
            graphql_error(
                "Resource not accessible by integration",
                &["repository", "pullRequest"],
            ),
            // The repo is missing, which is a config or permissions problem —
            // the message is about a Repository, and the path stops short.
            graphql_error(
                "Could not resolve to a Repository with the name 'apache/nope'.",
                &["repository"],
            ),
        ] {
            assert!(!pull_request_is_gone(&[error]));
        }
    }

    #[test]
    fn a_missing_pull_request_alongside_a_real_failure_still_fails() {
        // Swallowing this would turn an expired token into a quietly short
        // sync, which is far worse than an error.
        let errors = vec![
            pr_not_found(),
            graphql_error("API rate limit exceeded for user ID 1.", &["repository"]),
        ];
        assert!(!pull_request_is_gone(&errors));
    }

    #[test]
    fn graphql_errors_render_as_one_line_per_cause() {
        assert_eq!(
            render_graphql_errors(&[pr_not_found()]),
            "Could not resolve to a PullRequest with the number of 70787"
        );
        assert_eq!(
            render_graphql_errors(&[
                pr_not_found(),
                graphql_error("Something else.", &["repository"]),
            ]),
            "Could not resolve to a PullRequest with the number of 70787; Something else"
        );
        assert_eq!(render_graphql_errors(&[]), "no error detail");
    }

    #[tokio::test]
    async fn a_pull_request_path_round_trips_with_web_url() {
        // The pair has to agree, so the parse is tested against what `web_url`
        // actually builds rather than against a URL written out by hand.
        let host = ForgeHost {
            provider: Some("github".to_string()),
            ..Default::default()
        };
        let forge = GithubForge::new(&host, "github.com");
        let url = forge.web_url("apache", "airflow", 70135);
        let path = url.strip_prefix("https://github.com").expect("host");

        assert_eq!(
            GithubForge::parse_web_path(path),
            Some(("apache".to_string(), "airflow".to_string(), 70135))
        );
    }

    #[test]
    fn a_pull_request_path_tolerates_what_a_browser_adds() {
        // A URL copied from a browser rarely ends at the number.
        for tail in [
            "",
            "/",
            "/files",
            "/commits/abc123",
            "?w=1",
            "#issuecomment-2851",
        ] {
            let path = format!("/apache/airflow/pull/70135{tail}");
            assert_eq!(
                GithubForge::parse_web_path(&path),
                Some(("apache".to_string(), "airflow".to_string(), 70135)),
                "{path}"
            );
        }
    }

    #[test]
    fn a_path_that_is_not_a_pull_request_is_refused() {
        for path in [
            "/apache/airflow",
            "/apache/airflow/issues/70135",
            "/apache/airflow/pull/notanumber",
            "/pull/70135",
            "",
        ] {
            assert_eq!(GithubForge::parse_web_path(path), None, "{path}");
        }
    }

    #[tokio::test]
    async fn web_url_matches_githubs_pull_layout() {
        let host = ForgeHost {
            provider: Some("github".to_string()),
            ..Default::default()
        };
        let forge = GithubForge::new(&host, "github.com");
        assert_eq!(
            forge.web_url("apache", "airflow", 12345),
            "https://github.com/apache/airflow/pull/12345"
        );
    }

    #[tokio::test]
    async fn web_url_uses_the_enterprise_hosts_own_hostname() {
        let host = ForgeHost {
            provider: Some("github".to_string()),
            api_base: Some("https://github.acme.example/api/v3".to_string()),
            ..Default::default()
        };
        let forge = GithubForge::new(&host, "github.acme.example");
        assert_eq!(
            forge.web_url("acme", "widgets", 7),
            "https://github.acme.example/acme/widgets/pull/7"
        );
    }

    /// A token as if already resolved, for the adapters a test presets rather
    /// than letting resolve from this machine's environment.
    fn token(value: &str) -> Token {
        Token {
            value: value.to_string(),
            source: crate::TokenSource::Override,
        }
    }

    #[tokio::test]
    async fn handoff_credentials_default_to_githubs_own_convention() {
        let host = ForgeHost {
            provider: Some("github".to_string()),
            ..Default::default()
        };
        let forge = GithubForge::with_token(&host, "github.com", token("secret-token"));
        assert_eq!(
            forge
                .handoff_credentials()
                .expect("a preset token needs no resolution"),
            ("GITHUB_TOKEN", "secret-token")
        );
    }

    #[tokio::test]
    async fn handoff_credentials_use_the_hosts_configured_token_env() {
        let host = ForgeHost {
            provider: Some("github".to_string()),
            token_env: Some("ACME_GH_TOKEN".to_string()),
            ..Default::default()
        };
        let forge = GithubForge::with_token(&host, "github.acme.example", token("secret-token"));
        assert_eq!(
            forge
                .handoff_credentials()
                .expect("a preset token needs no resolution"),
            ("ACME_GH_TOKEN", "secret-token")
        );
    }

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
        assert_eq!(snapshot.base_ref, "main");
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
        let reviewers: std::collections::BTreeMap<_, _> = detail
            .reviewers
            .iter()
            .map(|r| (r.login.as_str(), r.verdict))
            .collect();
        assert_eq!(detail.reviewers.len(), 3, "ashb, kaxil, uranusjr");
        assert_eq!(reviewers.get("ashb"), Some(&Verdict::Approved));
        assert_eq!(reviewers.get("kaxil"), Some(&Verdict::Approved));
        // uranusjr requested changes, then later left a plain comment review —
        // the same "latest submitted review wins" rule already applied to my
        // own last_verdict above.
        assert_eq!(reviewers.get("uranusjr"), Some(&Verdict::Commented));
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
                "headRefOid": "abc", "baseRefName": "main",
                "updatedAt": "2026-08-05T12:00:00Z",
                "labels": {"nodes": []}, "milestone": null,
                "files": {"totalCount": 0, "nodes": []}
            }"#,
        )
        .expect("parses");
        let snapshot = node.into_snapshot().unwrap();
        assert_eq!(snapshot.author, "ghost");
        assert!(!snapshot.files_truncated);
    }

    #[test]
    fn the_target_branch_comes_through_the_sweep_selection() {
        // Both queries select `baseRefName`; this pins that the node reads it and
        // carries it into the snapshot, rather than quietly defaulting to empty.
        let node: PrNode = serde_json::from_str(
            r#"{
                "number": 7, "title": "backport", "isDraft": false, "state": "OPEN",
                "author": {"login": "potiuk"}, "authorAssociation": "MEMBER",
                "headRefOid": "abc", "baseRefName": "v3-1-test",
                "updatedAt": "2026-08-05T12:00:00Z",
                "labels": {"nodes": []}, "milestone": null,
                "files": {"totalCount": 0, "nodes": []}
            }"#,
        )
        .expect("parses");

        assert_eq!(node.into_snapshot().unwrap().base_ref, "v3-1-test");
    }

    /// A PR selection that omitted `baseRefName` must not silently parse: the
    /// field is required on the node precisely so a query and its reader cannot
    /// drift apart unnoticed.
    #[test]
    fn a_pr_node_without_a_target_branch_is_refused() {
        let err = serde_json::from_str::<PrNode>(
            r#"{
                "number": 7, "title": "t", "isDraft": false, "state": "OPEN",
                "author": null, "authorAssociation": "NONE",
                "headRefOid": "abc", "updatedAt": "2026-08-05T12:00:00Z",
                "labels": {"nodes": []}, "milestone": null,
                "files": {"totalCount": 0, "nodes": []}
            }"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("baseRefName"), "{err}");
    }
}
