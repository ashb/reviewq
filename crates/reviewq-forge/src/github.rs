//! The GitHub adapter: the one implementation of [`Forge`](crate::Forge) today.
//!
//! An octocrab wrapper that returns the plain data types in [`crate::types`];
//! no model or ledger types cross this boundary. The tier-1 sweep fetches each
//! PR's changed files in the same query, so there is no separate file round
//! trip and a PR arrives ready to classify.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use jiff::Timestamp;
use octocrab::models::activity::Notification;
use octocrab::service::middleware::retry::RetryConfig;
use octocrab::{GraphqlError, GraphqlPathSegment, GraphqlResponse, Octocrab};
use reviewq_core::model::{
    ActivityKind, ActivityPayload, ActivityRelation, Mention, PrSnapshot, PrState, ReviewRequest,
    ReviewResult, ReviewerVerdict, Said, ThreadState, Verdict,
};
use serde::{Deserialize, Serialize};

use crate::host::GITHUB_TOKEN_ENV;
use crate::types::{
    ActivityRateLimit, FetchedPr, ForgeActivity, ForgeActivityPage, LabelColour, PrDetail,
    RateLimit, RateLimitUnit, SweepPage, Viewer,
};
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewCommentsRequest {
    owner: String,
    name: String,
    number: u64,
    page: u32,
}

#[async_trait]
trait ReviewCommentsTransport: Send + Sync {
    async fn fetch(
        &self,
        forge: &GithubForge,
        request: ReviewCommentsRequest,
    ) -> Result<ReviewCommentsPage>;
}

struct OctocrabReviewCommentsTransport;

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
    /// Activity calls expose every provider request to their caller, so this
    /// client has transport retries disabled.
    activity_client: OnceLock<Octocrab>,
    review_comments_transport: Arc<dyn ReviewCommentsTransport>,
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
            activity_client: OnceLock::new(),
            review_comments_transport: Arc::new(OctocrabReviewCommentsTransport),
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

    #[cfg(test)]
    fn with_review_comments_transport(
        mut self,
        transport: Arc<dyn ReviewCommentsTransport>,
    ) -> Self {
        self.review_comments_transport = transport;
        self
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

    fn activity_client(&self) -> Result<&Octocrab> {
        if let Some(client) = self.activity_client.get() {
            return Ok(client);
        }
        let mut builder = Octocrab::builder()
            .personal_token(self.token()?.value.clone())
            .add_retry_config(RetryConfig::None);
        if let Some(api_base) = &self.host.api_base {
            builder = builder
                .base_uri(api_base.as_str())
                .map_err(|err| unreachable(format!("invalid api_base {api_base:?}"), err))?;
        }
        let built = builder
            .build()
            .map_err(|err| unreachable("building the GitHub activity client", err))?;
        Ok(self.activity_client.get_or_init(|| built))
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

    async fn activity_graphql<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        query: &str,
        variables: serde_json::Map<String, serde_json::Value>,
    ) -> Result<T> {
        tracing::debug!(op, "graphql request");
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let response: GraphqlResponse<T> = self
            .activity_client()?
            .post("/graphql", Some(&payload))
            .await
            .map_err(|err| {
                classify(
                    &self.web_host,
                    format!("GitHub GraphQL request ({op})"),
                    err,
                )
            })?;
        activity_graphql_response(&self.web_host, op, response)
    }

    async fn fetch_graphql_activity(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        actor: &str,
        cursor: ActivityCursor,
    ) -> Result<ForgeActivityPage> {
        let mut vars = serde_json::Map::new();
        vars.insert("owner".into(), owner.into());
        vars.insert("name".into(), name.into());
        vars.insert("number".into(), number.into());
        cursor.add_variables(&mut vars);

        let data: ActivityQuery = self
            .activity_graphql(
                &format!("activity for {owner}/{name}#{number}"),
                ACTIVITY_QUERY,
                vars,
            )
            .await?;
        data.rate_limit.trace("sync:activity");
        data.into_activity_page_from(actor, cursor)
    }

    async fn fetch_review_comments_activity(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        actor: &str,
        mut cursor: ActivityCursor,
    ) -> Result<ForgeActivityPage> {
        let page = cursor
            .review_comments
            .scan
            .get_or_insert_with(ReviewCommentScan::new)
            .page;
        let data = self
            .review_comments_transport
            .fetch(
                self,
                ReviewCommentsRequest {
                    owner: owner.into(),
                    name: name.into(),
                    number,
                    page,
                },
            )
            .await?;
        cursor.review_comments.append_page(data.comments, actor);
        cursor.activity_page(Some(ActivityRateLimit {
            unit: RateLimitUnit::Requests,
            cost: data.cost,
            remaining: data.remaining,
        }))
    }
}

#[async_trait]
impl ReviewCommentsTransport for OctocrabReviewCommentsTransport {
    async fn fetch(
        &self,
        forge: &GithubForge,
        request: ReviewCommentsRequest,
    ) -> Result<ReviewCommentsPage> {
        let ReviewCommentsRequest {
            owner,
            name,
            number,
            page,
        } = request;
        let op = format!("activity review comments for {owner}/{name}#{number}");
        let route = format!(
            "/repos/{owner}/{name}/pulls/{number}/comments?sort=created&direction=desc&per_page={ACTIVITY_PAGE_SIZE}&page={page}"
        );
        tracing::debug!(op, "rest request");
        let client = forge.activity_client()?;
        let response = client._get(route).await.map_err(|err| {
            classify(
                &forge.web_host,
                "GitHub REST request (activity review comments)".into(),
                err,
            )
        })?;
        let remaining = response
            .headers()
            .get("x-ratelimit-remaining")
            .ok_or_else(|| ForgeError::Unreachable {
                doing: "reading GitHub REST activity rate limit".into(),
                source: "the GitHub response omitted x-ratelimit-remaining".into(),
            })?
            .to_str()
            .map_err(|err| unreachable("reading GitHub REST activity rate limit", err))?
            .parse()
            .map_err(|err| unreachable("parsing GitHub REST activity rate limit", err))?;
        let response = octocrab::map_github_error(response).await.map_err(|err| {
            classify(
                &forge.web_host,
                "GitHub REST request (activity review comments)".into(),
                err,
            )
        })?;
        let body = client
            .body_to_string(response)
            .await
            .map_err(|err| unreachable("reading GitHub REST activity comments", err))?;
        let comments = serde_json::from_str(&body)
            .map_err(|err| unreachable("parsing GitHub REST activity comments", err))?;
        Ok(ReviewCommentsPage {
            comments,
            cost: 1,
            remaining,
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

    fn initial_activity_rate_limit(&self) -> RateLimitUnit {
        RateLimitUnit::Points
    }

    async fn fetch_pr_activity(
        &self,
        owner: &str,
        name: &str,
        number: u64,
        actor: &str,
        cursor: Option<&str>,
    ) -> Result<ForgeActivityPage> {
        let cursor: Option<ActivityCursor> = cursor
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| unreachable("parsing a GitHub activity cursor", err))?;
        let mut cursor = cursor.unwrap_or_else(ActivityCursor::fresh);
        match cursor.next_step() {
            ActivityStep::Emit | ActivityStep::Finished => cursor.activity_page(None),
            ActivityStep::FetchGraphql => {
                self.fetch_graphql_activity(owner, name, number, actor, cursor)
                    .await
            }
            ActivityStep::FetchReviewComments => {
                self.fetch_review_comments_activity(owner, name, number, actor, cursor)
                    .await
            }
        }
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
    #[serde(default)]
    start_cursor: Option<String>,
    #[serde(default)]
    has_previous_page: bool,
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
    closed_at: Option<jiff::Timestamp>,
    merged_at: Option<jiff::Timestamp>,
    #[serde(default)]
    latest_lifecycle_event: Option<NodeList<LifecycleEvent>>,
    author: Option<Login>,
    author_association: String,
    head_ref_oid: String,
    base_ref_name: String,
    updated_at: jiff::Timestamp,
    /// When the PR was opened. Optional so a captured response from before the
    /// query asked for it still parses, which is what the fixtures are.
    #[serde(default)]
    created_at: Option<jiff::Timestamp>,
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
        let state_changed_at = transition_timestamp(
            state,
            self.closed_at,
            self.merged_at,
            self.latest_lifecycle_event
                .as_ref()
                .and_then(|events| events.nodes.last()),
        );
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
            created_at: self.created_at,
            state_changed_at,
            labels: self.labels.nodes.into_iter().map(|l| l.name).collect(),
            milestone: self.milestone.map(|m| m.title),
            files: Some(paths),
            files_truncated,
        })
    }
}

fn transition_timestamp(
    state: PrState,
    closed_at: Option<Timestamp>,
    merged_at: Option<Timestamp>,
    latest: Option<&LifecycleEvent>,
) -> Option<Timestamp> {
    let timeline_timestamp = latest.and_then(|event| match (state, event) {
        (PrState::Open, LifecycleEvent::Reopened { created_at })
        | (PrState::Closed, LifecycleEvent::Closed { created_at })
        | (PrState::Merged, LifecycleEvent::Merged { created_at }) => Some(*created_at),
        _ => None,
    });
    match state {
        PrState::Open => timeline_timestamp,
        PrState::Closed => timeline_timestamp.or(closed_at),
        PrState::Merged => timeline_timestamp.or(merged_at),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum LifecycleEvent {
    #[serde(rename = "ClosedEvent")]
    Closed {
        #[serde(rename = "createdAt")]
        created_at: Timestamp,
    },
    #[serde(rename = "ReopenedEvent")]
    Reopened {
        #[serde(rename = "createdAt")]
        created_at: Timestamp,
    },
    #[serde(rename = "MergedEvent")]
    Merged {
        #[serde(rename = "createdAt")]
        created_at: Timestamp,
    },
    #[serde(other)]
    Other,
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
struct ActivityQuery {
    repository: Option<ActivityRepo>,
    #[serde(rename = "rateLimit")]
    rate_limit: RateLimit,
}

impl ActivityQuery {
    fn into_activity_page_from(
        self,
        actor: &str,
        mut cursor: ActivityCursor,
    ) -> Result<ForgeActivityPage> {
        if let Some(repository) = self.repository
            && let Some(pull_request) = repository.pull_request
        {
            pull_request.append_activities(actor, &mut cursor)?;
        } else {
            cursor.finish_preserving_events();
        }
        cursor.activity_page(Some(ActivityRateLimit {
            unit: RateLimitUnit::Points,
            cost: self.rate_limit.cost,
            remaining: self.rate_limit.remaining,
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityRepo {
    pull_request: Option<ActivityPullRequest>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityPullRequest {
    url: Option<String>,
    reviews: Option<ActivityConnection<ActivityReview>>,
    comments: Option<ActivityConnection<ActivityComment>>,
    timeline_items: Option<ActivityConnection<TimelineItem>>,
}

impl ActivityPullRequest {
    fn append_activities(self, actor: &str, cursor: &mut ActivityCursor) -> Result<()> {
        append_connection(
            self.reviews,
            &mut cursor.reviews,
            "reviews",
            |review| review.occurred_at(),
            |review, activities| {
                review.append_for_viewer(actor, activities);
                Ok(())
            },
        )?;
        append_connection(
            self.comments,
            &mut cursor.comments,
            "comments",
            |comment| Some(comment.created_at),
            |comment, activities| {
                comment.append_for_viewer(actor, activities);
                Ok(())
            },
        )?;
        append_connection(
            self.timeline_items,
            &mut cursor.timeline,
            "timeline items",
            TimelineItem::occurred_at,
            |item, activities| {
                item.append(actor, self.url.as_deref(), activities);
                Ok(())
            },
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityConnection<T> {
    page_info: PageInfo,
    nodes: Vec<T>,
}

fn append_connection<T>(
    connection: Option<ActivityConnection<T>>,
    cursor: &mut ActivitySourceCursor,
    name: &str,
    occurred_at: impl Fn(&T) -> Option<Timestamp>,
    mut append: impl FnMut(T, &mut Vec<ForgeActivity>) -> Result<()>,
) -> Result<()> {
    if !cursor.needs_page() {
        return Ok(());
    }
    let Some(connection) = connection else {
        return Err(ForgeError::Unreachable {
            doing: format!("mapping GitHub activity: {name} were omitted"),
            source: "the GitHub response omitted a requested connection".into(),
        });
    };
    let mut watermark = None;
    for item in connection.nodes {
        if let Some(occurred_at) = occurred_at(&item)
            && watermark
                .as_ref()
                .is_none_or(|watermark| occurred_at < *watermark)
        {
            watermark = Some(occurred_at);
        }
        append(item, &mut cursor.events)?;
    }
    cursor.advance(connection.page_info, name, watermark)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityReview {
    id: String,
    author: Option<Login>,
    state: String,
    submitted_at: Option<Timestamp>,
    commit: Option<Oid>,
    url: String,
    #[serde(default)]
    body: String,
}

fn activity_relation(author: Option<&Login>, body: &str, viewer: &str) -> ActivityRelation {
    if author.is_some_and(|author| author.login.eq_ignore_ascii_case(viewer)) {
        ActivityRelation::Own
    } else if mentions_login(body, viewer) {
        ActivityRelation::Relevant
    } else {
        ActivityRelation::Context
    }
}

impl ActivityReview {
    fn occurred_at(&self) -> Option<Timestamp> {
        self.submitted_at
    }

    fn append_for_viewer(self, actor: &str, activities: &mut Vec<ForgeActivity>) {
        let Some(occurred_at) = self.submitted_at else {
            return;
        };
        let reviewed_sha = self.commit.as_ref().map(|commit| commit.oid.clone());
        let result = match self.state.as_str() {
            "APPROVED" => ReviewResult::Approved,
            "CHANGES_REQUESTED" => ReviewResult::ChangesRequested,
            "COMMENTED" => ReviewResult::Commented,
            _ => ReviewResult::Other(self.state),
        };
        activities.push(ForgeActivity {
            relation: activity_relation(self.author.as_ref(), &self.body, actor),
            kind: ActivityKind::ReviewSubmitted,
            occurred_at,
            actor: self.author.map(|author| author.login),
            head_sha: reviewed_sha.clone(),
            external_id: Some(self.id),
            permalink: Some(self.url),
            payload: ActivityPayload::ReviewSubmitted {
                result,
                reviewed_sha,
            },
        });
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityComment {
    id: String,
    author: Option<Login>,
    created_at: Timestamp,
    url: String,
    #[serde(default)]
    body: String,
}

impl ActivityComment {
    fn append_for_viewer(self, actor: &str, activities: &mut Vec<ForgeActivity>) {
        activities.push(ForgeActivity {
            relation: activity_relation(self.author.as_ref(), &self.body, actor),
            kind: ActivityKind::Commented,
            occurred_at: self.created_at,
            actor: self.author.map(|author| author.login),
            head_sha: None,
            external_id: Some(self.id),
            permalink: Some(self.url),
            payload: ActivityPayload::None,
        });
    }
}

#[derive(Debug, Deserialize)]
struct ReviewCommentsPage {
    comments: Vec<RestReviewComment>,
    cost: u32,
    remaining: u32,
}

#[derive(Debug, Deserialize)]
struct RestReviewComment {
    id: u64,
    in_reply_to_id: Option<u64>,
    user: Option<Login>,
    created_at: Timestamp,
    html_url: String,
    #[serde(default)]
    body: String,
}

impl RestReviewComment {
    fn append_for_viewer(self, actor: &str, activities: &mut Vec<ForgeActivity>) {
        activities.push(ForgeActivity {
            relation: activity_relation(self.user.as_ref(), &self.body, actor),
            kind: ActivityKind::ReviewThreadCommented,
            occurred_at: self.created_at,
            actor: self.user.map(|user| user.login),
            head_sha: None,
            external_id: Some(self.id.to_string()),
            permalink: Some(self.html_url),
            payload: ActivityPayload::ReviewThreadCommented {
                thread_id: Some(self.in_reply_to_id.unwrap_or(self.id).to_string()),
            },
        });
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
enum TimelineItem {
    #[serde(rename = "ClosedEvent")]
    Closed(ActivityTimelineEvent),
    #[serde(rename = "ReopenedEvent")]
    Reopened(ActivityTimelineEvent),
    #[serde(rename = "MergedEvent")]
    Merged(ActivityTimelineEvent),
    #[serde(other)]
    Other,
}

impl TimelineItem {
    fn occurred_at(&self) -> Option<Timestamp> {
        match self {
            Self::Closed(event) | Self::Reopened(event) | Self::Merged(event) => {
                Some(event.created_at)
            }
            Self::Other => None,
        }
    }

    fn append(
        self,
        viewer: &str,
        pull_request_url: Option<&str>,
        activities: &mut Vec<ForgeActivity>,
    ) {
        let (kind, from, to, event) = match self {
            Self::Closed(event) => (
                ActivityKind::PrClosed,
                PrState::Open,
                PrState::Closed,
                event,
            ),
            Self::Reopened(event) => (
                ActivityKind::PrReopened,
                PrState::Closed,
                PrState::Open,
                event,
            ),
            Self::Merged(event) => (
                ActivityKind::PrMerged,
                PrState::Open,
                PrState::Merged,
                event,
            ),
            Self::Other => return,
        };
        activities.push(ForgeActivity {
            kind,
            relation: activity_relation(event.actor.as_ref(), "", viewer),
            occurred_at: event.created_at,
            actor: event.actor.map(|actor| actor.login),
            head_sha: None,
            external_id: Some(event.id),
            permalink: pull_request_url.map(str::to_string),
            payload: ActivityPayload::StateChanged { from, to },
        });
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActivityTimelineEvent {
    id: String,
    actor: Option<Login>,
    created_at: Timestamp,
}

const ACTIVITY_PAGE_SIZE: usize = 100;

#[derive(Debug, Deserialize, Serialize)]
struct ActivityCursor {
    reviews: ActivitySourceCursor,
    comments: ActivitySourceCursor,
    timeline: ActivitySourceCursor,
    review_comments: ReviewCommentsCursor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityStep {
    Emit,
    FetchGraphql,
    FetchReviewComments,
    Finished,
}

impl Default for ActivityCursor {
    fn default() -> Self {
        Self {
            reviews: ActivitySourceCursor::default(),
            comments: ActivitySourceCursor::default(),
            timeline: ActivitySourceCursor::default(),
            review_comments: ReviewCommentsCursor {
                source: ActivitySourceCursor {
                    finished: true,
                    ..Default::default()
                },
                before: None,
                scan: None,
            },
        }
    }
}

impl ActivityCursor {
    fn fresh() -> Self {
        let mut cursor = Self::default();
        cursor.review_comments.source.finished = false;
        cursor
    }

    fn next_step(&self) -> ActivityStep {
        if self.can_emit() {
            ActivityStep::Emit
        } else if self.needs_initial_graphql_page() {
            ActivityStep::FetchGraphql
        } else if self.needs_initial_review_comments_page() {
            ActivityStep::FetchReviewComments
        } else if self.needs_graphql_page() {
            ActivityStep::FetchGraphql
        } else if self.needs_review_comments_page() {
            ActivityStep::FetchReviewComments
        } else {
            ActivityStep::Finished
        }
    }

    fn add_variables(&self, vars: &mut serde_json::Map<String, serde_json::Value>) {
        self.reviews
            .add_variables("reviewsBefore", "includeReviews", vars);
        self.comments
            .add_variables("commentsBefore", "includeComments", vars);
        self.timeline
            .add_variables("timelineBefore", "includeTimeline", vars);
    }

    fn needs_graphql_page(&self) -> bool {
        self.reviews.needs_page() || self.comments.needs_page() || self.timeline.needs_page()
    }

    fn needs_initial_graphql_page(&self) -> bool {
        [&self.reviews, &self.comments, &self.timeline]
            .into_iter()
            .any(|source| source.needs_initial_page())
    }

    fn needs_review_comments_page(&self) -> bool {
        self.review_comments.source.needs_page()
    }

    fn needs_initial_review_comments_page(&self) -> bool {
        self.review_comments.source.needs_initial_page()
    }

    fn is_finished(&self) -> bool {
        self.sources()
            .into_iter()
            .all(ActivitySourceCursor::is_finished)
    }

    #[cfg(test)]
    fn finish(&mut self) {
        for source in self.sources_mut() {
            source.finish();
        }
    }

    fn finish_preserving_events(&mut self) {
        for source in self.sources_mut() {
            source.before = None;
            source.finished = true;
            source.watermark = None;
        }
    }

    fn activity_page(
        &mut self,
        rate_limit: Option<ActivityRateLimit>,
    ) -> Result<ForgeActivityPage> {
        let mut activities = Vec::new();
        while activities.len() < ACTIVITY_PAGE_SIZE {
            let Some(source) = self.newest_source() else {
                break;
            };
            let occurred_at = self.source(source).events[0].occurred_at;
            if !self.sources().into_iter().all(|other| {
                other.finished
                    || !other.events.is_empty()
                    || other
                        .watermark
                        .as_ref()
                        .is_some_and(|watermark| occurred_at > *watermark)
            }) {
                break;
            }
            activities.push(self.source_mut(source).events.remove(0));
        }
        let next = (!self.is_finished())
            .then(|| serde_json::to_string(self))
            .transpose()
            .map_err(|err| unreachable("serializing a GitHub activity cursor", err))?;
        let page = ForgeActivityPage {
            activities,
            next,
            rate_limit,
            next_rate_limit: self.next_rate_limit(),
        };
        page.validate()?;
        Ok(page)
    }

    fn newest_source(&self) -> Option<usize> {
        self.sources()
            .into_iter()
            .enumerate()
            .filter(|(_, source)| !source.events.is_empty())
            .max_by(|(_, left), (_, right)| activity_order(&left.events[0], &right.events[0]))
            .map(|(index, _)| index)
    }

    fn can_emit(&self) -> bool {
        let Some(source) = self.newest_source() else {
            return false;
        };
        let occurred_at = self.source(source).events[0].occurred_at;
        self.sources().into_iter().all(|other| {
            other.finished
                || !other.events.is_empty()
                || other
                    .watermark
                    .as_ref()
                    .is_some_and(|watermark| occurred_at > *watermark)
        })
    }

    fn next_rate_limit(&self) -> Option<RateLimitUnit> {
        match self.next_step() {
            ActivityStep::FetchGraphql => Some(RateLimitUnit::Points),
            ActivityStep::FetchReviewComments => Some(RateLimitUnit::Requests),
            ActivityStep::Emit | ActivityStep::Finished => None,
        }
    }

    fn sources(&self) -> [&ActivitySourceCursor; 4] {
        [
            &self.reviews,
            &self.comments,
            &self.timeline,
            &self.review_comments.source,
        ]
    }

    fn sources_mut(&mut self) -> [&mut ActivitySourceCursor; 4] {
        [
            &mut self.reviews,
            &mut self.comments,
            &mut self.timeline,
            &mut self.review_comments.source,
        ]
    }

    fn source(&self, index: usize) -> &ActivitySourceCursor {
        self.sources()[index]
    }

    fn source_mut(&mut self, index: usize) -> &mut ActivitySourceCursor {
        match index {
            0 => &mut self.reviews,
            1 => &mut self.comments,
            2 => &mut self.timeline,
            3 => &mut self.review_comments.source,
            _ => unreachable!("activity source index came from newest_source"),
        }
    }
}

fn activity_order(left: &ForgeActivity, right: &ForgeActivity) -> std::cmp::Ordering {
    left.occurred_at
        .cmp(&right.occurred_at)
        .then_with(|| left.external_id.cmp(&right.external_id))
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ActivitySourceCursor {
    before: Option<String>,
    finished: bool,
    events: Vec<ForgeActivity>,
    watermark: Option<Timestamp>,
}

impl ActivitySourceCursor {
    fn needs_page(&self) -> bool {
        !self.finished && self.events.is_empty()
    }

    fn needs_initial_page(&self) -> bool {
        self.needs_page() && self.watermark.is_none()
    }

    fn is_finished(&self) -> bool {
        self.finished && self.events.is_empty()
    }

    #[cfg(test)]
    fn finish(&mut self) {
        self.before = None;
        self.finished = true;
        self.events.clear();
        self.watermark = None;
    }

    fn add_variables(
        &self,
        before_name: &str,
        include_name: &str,
        vars: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        vars.insert(
            before_name.into(),
            self.before
                .clone()
                .map_or(serde_json::Value::Null, Into::into),
        );
        vars.insert(include_name.into(), self.needs_page().into());
    }

    fn advance(
        &mut self,
        page_info: PageInfo,
        name: &str,
        watermark: Option<Timestamp>,
    ) -> Result<()> {
        self.watermark = watermark;
        if page_info.has_previous_page {
            self.before = page_info.start_cursor;
            if self.before.is_none() {
                return Err(ForgeError::Unreachable {
                    doing: format!("mapping GitHub activity: {name} have no next cursor"),
                    source: "the GitHub response was internally inconsistent".into(),
                });
            }
        } else {
            self.finished = true;
        }
        self.events
            .sort_by(|left, right| activity_order(right, left));
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ReviewCommentsCursor {
    source: ActivitySourceCursor,
    #[serde(default)]
    before: Option<ReviewCommentKey>,
    #[serde(default)]
    scan: Option<ReviewCommentScan>,
}

impl ReviewCommentsCursor {
    #[cfg(test)]
    fn append_scan(&mut self, comments: Vec<RestReviewComment>, actor: &str) {
        self.scan = Some(ReviewCommentScan::new());
        self.append_page(comments, actor);
    }

    fn append_page(&mut self, comments: Vec<RestReviewComment>, actor: &str) {
        let full_page = comments.len() == ACTIVITY_PAGE_SIZE;
        let fingerprint = ReviewCommentPageFingerprint::from(comments.as_slice());
        let repeated = self
            .scan
            .as_ref()
            .and_then(|scan| scan.fingerprint.as_ref())
            == Some(&fingerprint);
        let mut candidates = comments
            .into_iter()
            .filter(|comment| {
                self.before
                    .as_ref()
                    .is_none_or(|before| comment.key() < *before)
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(RestReviewComment::key);
        candidates.reverse();
        if let Some(last) = candidates.last() {
            self.before = Some(last.key());
            self.source.watermark = Some(last.created_at);
        }
        for comment in candidates {
            comment.append_for_viewer(actor, &mut self.source.events);
        }
        self.source
            .events
            .sort_by(|left, right| activity_order(right, left));

        let scan = self
            .scan
            .as_mut()
            .expect("the REST scan is initialized before a page is fetched");
        if !full_page {
            self.source.finished = true;
            self.scan = None;
        } else if repeated {
            scan.page += 1;
            scan.fingerprint = None;
        } else {
            scan.fingerprint = Some(fingerprint);
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ReviewCommentScan {
    page: u32,
    fingerprint: Option<ReviewCommentPageFingerprint>,
}

impl ReviewCommentScan {
    fn new() -> Self {
        Self {
            page: 1,
            fingerprint: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
struct ReviewCommentPageFingerprint {
    count: usize,
    first: Option<ReviewCommentKey>,
    last: Option<ReviewCommentKey>,
}

impl From<&[RestReviewComment]> for ReviewCommentPageFingerprint {
    fn from(comments: &[RestReviewComment]) -> Self {
        Self {
            count: comments.len(),
            first: comments.first().map(RestReviewComment::key),
            last: comments.last().map(RestReviewComment::key),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
struct ReviewCommentKey {
    created_at: Timestamp,
    id: String,
}

impl RestReviewComment {
    fn key(&self) -> ReviewCommentKey {
        ReviewCommentKey {
            created_at: self.created_at,
            id: self.id.to_string(),
        }
    }
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
      closedAt mergedAt
      latestLifecycleEvent: timelineItems(
        last: 1
        itemTypes: [CLOSED_EVENT, REOPENED_EVENT, MERGED_EVENT]
      ) {
        nodes {
          __typename
          ... on ClosedEvent { createdAt }
          ... on ReopenedEvent { createdAt }
          ... on MergedEvent { createdAt }
        }
      }
      author { login }
      authorAssociation
      headRefOid
      baseRefName
      updatedAt
      createdAt
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
      url
      number title isDraft state
      closedAt mergedAt
      latestLifecycleEvent: timelineItems(
        last: 1
        itemTypes: [CLOSED_EVENT, REOPENED_EVENT, MERGED_EVENT]
      ) {
        nodes {
          __typename
          ... on ClosedEvent { createdAt }
          ... on ReopenedEvent { createdAt }
          ... on MergedEvent { createdAt }
        }
      }
      author { login }
      authorAssociation
      headRefOid
      baseRefName
      updatedAt
      createdAt
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

fn activity_graphql_response<T>(host: &str, op: &str, response: GraphqlResponse<T>) -> Result<T> {
    match response {
        GraphqlResponse::Ok(ok) => Ok(ok.data),
        GraphqlResponse::Err(err) => {
            let rendered = render_graphql_errors(&err.errors);
            if rendered.contains("Bad credentials") {
                return Err(ForgeError::Rejected {
                    host: host.to_string(),
                    source: rendered.into(),
                });
            }
            Err(ForgeError::Unreachable {
                doing: format!("GitHub GraphQL request ({op})"),
                source: rendered.into(),
            })
        }
    }
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
    /// `OPEN`, `CLOSED` or `MERGED`. Asked for here as well as in the sweep,
    /// because a refresh of one PR is the whole of what some runs do — and a
    /// PR closed since the last sweep is exactly the kind of thing somebody
    /// presses `r` to find out about.
    state: String,
    closed_at: Option<Timestamp>,
    merged_at: Option<Timestamp>,
    #[serde(default)]
    latest_lifecycle_event: Option<NodeList<LifecycleEvent>>,
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
    id: Option<String>,
    url: Option<String>,
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

/// Record everybody `body` pulls into the discussion, bar me.
fn invite(invited: &mut Vec<String>, body: &str, login: &str) {
    for handle in handles_in(body) {
        if !handle.eq_ignore_ascii_case(login) && !invited.contains(&handle) {
            invited.push(handle);
        }
    }
}

/// `body` without the lines somebody else wrote.
///
/// A markdown blockquote is a quotation: the words in it are not the words of
/// whoever posted the comment. That matters for reading who *I* pulled into a
/// discussion — quoting somebody else's "hey @otheruser, what do you think" is
/// not me asking @otheruser anything, and treating it as one would wait on a
/// reply I never asked for.
fn strip_quotes(body: &str) -> String {
    body.lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every login `body` @mentions, in the order they appear.
///
/// The inverse of [`mentions_login`], with the same boundary rules and the same
/// blindness to code — plus quotes, since this is only ever asked of something I
/// wrote and I am only answerable for my own words. A team (`@org/team`) is
/// dropped: knowing whether somebody is in one takes membership this never
/// fetches, so a team mention invites nobody rather than everybody.
fn handles_in(body: &str) -> Vec<String> {
    let text = strip_quotes(&strip_code(body));
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    for (at, _) in text.match_indices('@') {
        let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        if !before_ok {
            continue;
        }
        let handle: String = text[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        // A `/` after the handle makes it a team, which invites nobody.
        let is_team = text[at + 1 + handle.len()..].starts_with('/');
        if handle.is_empty() || is_team {
            continue;
        }
        if !out.contains(&handle) {
            out.push(handle);
        }
    }
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
        let state = PrState::from_wire(&self.state).unwrap_or(PrState::Open);
        let state_changed_at = transition_timestamp(
            state,
            self.closed_at,
            self.merged_at,
            self.latest_lifecycle_event
                .as_ref()
                .and_then(|events| events.nodes.last()),
        );
        let mut activities = Vec::new();
        let mut action_times: Vec<Timestamp> = Vec::new();
        let mut mentions: Vec<Mention> = Vec::new();
        // Two halves of "somebody is waiting on my answer": what other people
        // have said here, and who I asked to say it.
        let mut said: Vec<Said> = Vec::new();
        let mut invited: Vec<String> = Vec::new();

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
            let mine = author_login(&review.author)
                .is_some_and(|author| author.eq_ignore_ascii_case(login));
            if let Some(at) = review.submitted_at {
                if mine {
                    action_times.push(at);
                }
                if let Some(id) = &review.id {
                    let reviewed_sha = review.commit.as_ref().map(|c| c.oid.clone());
                    let result = match review.state.as_str() {
                        "APPROVED" => ReviewResult::Approved,
                        "CHANGES_REQUESTED" => ReviewResult::ChangesRequested,
                        "COMMENTED" => ReviewResult::Commented,
                        _ => ReviewResult::Other(review.state.clone()),
                    };
                    activities.push(ForgeActivity {
                        kind: ActivityKind::ReviewSubmitted,
                        relation: activity_relation(review.author.as_ref(), &review.body, login),
                        occurred_at: at,
                        actor: review.author.as_ref().map(|a| a.login.clone()),
                        head_sha: reviewed_sha.clone(),
                        external_id: Some(id.clone()),
                        permalink: review.url.clone(),
                        payload: ActivityPayload::ReviewSubmitted {
                            result,
                            reviewed_sha,
                        },
                    });
                }
            }
            match (mine, author_login(&review.author), review.submitted_at) {
                (true, _, _) => invite(&mut invited, &review.body, login),
                (false, Some(by), Some(at)) => said.push(Said {
                    by: by.to_string(),
                    at,
                    review: Verdict::from_wire(&review.state),
                }),
                (false, _, _) => {}
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
                invite(&mut invited, &comment.body, login);
            } else if let Some(by) = author_login(&comment.author) {
                said.push(Said {
                    by: by.to_string(),
                    at: comment.created_at,
                    review: None,
                });
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
            activities,
            number: self.number,
            // An unknown spelling is treated as still open: the states that
            // silence a PR are the ones worth being sure about, and guessing
            // `Closed` from a value we don't recognise would hide it.
            state,
            state_changed_at,
            head_sha: self.head_ref_oid,
            body: self.body,
            last_reviewed_sha,
            last_verdict,
            last_action_at: action_times.into_iter().max(),
            threads,
            reviewers,
            mentions,
            said,
            invited,
            new_commits,
            review_request,
            cost,
            remaining,
        }
    }
}

/// A mention of `login`, if `body` names them and `at` is known.
fn mention_from(
    author: &Option<Login>,
    at: Option<Timestamp>,
    body: &str,
    login: &str,
) -> Option<Mention> {
    let by = author_login(author)?;
    if !mentions_login(body, login) {
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
      state
      closedAt
      mergedAt
      latestLifecycleEvent: timelineItems(
        last: 1
        itemTypes: [CLOSED_EVENT, REOPENED_EVENT, MERGED_EVENT]
      ) {
        nodes {
          __typename
          ... on ClosedEvent { createdAt }
          ... on ReopenedEvent { createdAt }
          ... on MergedEvent { createdAt }
        }
      }
      headRefOid
      body
      reviewRequests(first: 20) {
        nodes { requestedReviewer {
          __typename
          ... on User { login }
          ... on Team { slug }
        } }
      }
      reviews(last: 100) {
        nodes { id url author { login } state submittedAt commit { oid } body }
      }
      comments(last: 100) {
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

const ACTIVITY_QUERY: &str = r"
query(
  $owner: String!
  $name: String!
  $number: Int!
  $reviewsBefore: String
  $commentsBefore: String
  $timelineBefore: String
  $includeReviews: Boolean!
  $includeComments: Boolean!
  $includeTimeline: Boolean!
) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      url
      reviews(last: 100, before: $reviewsBefore) @include(if: $includeReviews) {
        pageInfo { endCursor hasNextPage startCursor hasPreviousPage }
        nodes { id author { login } state submittedAt commit { oid } url body }
      }
      comments(last: 100, before: $commentsBefore) @include(if: $includeComments) {
        pageInfo { endCursor hasNextPage startCursor hasPreviousPage }
        nodes { id author { login } createdAt url body }
      }
      timelineItems(
        last: 100
        before: $timelineBefore
        itemTypes: [CLOSED_EVENT, REOPENED_EVENT, MERGED_EVENT]
      ) @include(if: $includeTimeline) {
        pageInfo { endCursor hasNextPage startCursor hasPreviousPage }
        nodes {
          __typename
          ... on ClosedEvent { id actor { login } createdAt }
          ... on ReopenedEvent { id actor { login } createdAt }
          ... on MergedEvent { id actor { login } createdAt }
        }
      }
    }
  }
  rateLimit { limit cost remaining resetAt }
}
";

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn activity_cursor_uses_graphql_variable_names() {
        let mut variables = serde_json::Map::new();

        ActivityCursor::fresh().add_variables(&mut variables);

        assert_eq!(
            variables,
            serde_json::json!({
                "reviewsBefore": null,
                "includeReviews": true,
                "commentsBefore": null,
                "includeComments": true,
                "timelineBefore": null,
                "includeTimeline": true,
            })
            .as_object()
            .unwrap()
            .clone()
        );
    }

    #[test]
    fn activity_response_without_a_pull_request_url_still_deserializes() {
        let response =
            serde_json::from_value::<GraphqlResponse<ActivityQuery>>(serde_json::json!({
                "data": {
                    "repository": {
                        "pullRequest": {
                            "reviews": {
                                "pageInfo": {
                                    "endCursor": null,
                                    "hasNextPage": false,
                                    "startCursor": null,
                                    "hasPreviousPage": false
                                },
                                "nodes": []
                            },
                            "comments": {
                                "pageInfo": {
                                    "endCursor": null,
                                    "hasNextPage": false,
                                    "startCursor": null,
                                    "hasPreviousPage": false
                                },
                                "nodes": []
                            },
                            "timelineItems": {
                                "pageInfo": {
                                    "endCursor": null,
                                    "hasNextPage": false,
                                    "startCursor": null,
                                    "hasPreviousPage": false
                                },
                                "nodes": []
                            }
                        }
                    },
                    "rateLimit": {
                        "limit": 5000,
                        "cost": 1,
                        "remaining": 4989,
                        "resetAt": "2026-08-25T14:15:02Z"
                    }
                }
            }))
            .expect("GitHub activity response");

        assert!(matches!(response, GraphqlResponse::Ok(_)));
    }

    enum ScriptedReviewCommentsResponse {
        Page { ids: Vec<u64>, remaining: u32 },
        Failure(String),
    }

    struct ScriptedReviewCommentsTransport {
        responses: Mutex<VecDeque<ScriptedReviewCommentsResponse>>,
        requests: Mutex<Vec<ReviewCommentsRequest>>,
    }

    impl ScriptedReviewCommentsTransport {
        fn new(responses: Vec<ScriptedReviewCommentsResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ReviewCommentsRequest> {
            self.requests.lock().expect("lock requests").clone()
        }
    }

    #[async_trait]
    impl ReviewCommentsTransport for ScriptedReviewCommentsTransport {
        async fn fetch(
            &self,
            _forge: &GithubForge,
            request: ReviewCommentsRequest,
        ) -> Result<ReviewCommentsPage> {
            self.requests.lock().expect("lock requests").push(request);
            match self
                .responses
                .lock()
                .expect("lock responses")
                .pop_front()
                .expect("a scripted review-comments response")
            {
                ScriptedReviewCommentsResponse::Page { ids, remaining } => Ok(ReviewCommentsPage {
                    comments: ids.into_iter().map(review_comment).collect(),
                    cost: 1,
                    remaining,
                }),
                ScriptedReviewCommentsResponse::Failure(message) => Err(ForgeError::Unreachable {
                    doing: "scripted review-comments request".into(),
                    source: message.into(),
                }),
            }
        }
    }

    fn review_comments_response(
        ids: impl IntoIterator<Item = u64>,
        remaining: u32,
    ) -> ScriptedReviewCommentsResponse {
        ScriptedReviewCommentsResponse::Page {
            ids: ids.into_iter().collect(),
            remaining,
        }
    }

    fn failed_response(message: &str) -> ScriptedReviewCommentsResponse {
        ScriptedReviewCommentsResponse::Failure(message.into())
    }

    fn rest_only_cursor() -> ActivityCursor {
        let mut cursor = ActivityCursor::fresh();
        cursor.reviews.finish();
        cursor.comments.finish();
        cursor.timeline.finish();
        cursor
    }

    fn forge_for(transport: Arc<ScriptedReviewCommentsTransport>) -> GithubForge {
        GithubForge::with_token(
            &ForgeHost::default(),
            "github.example",
            token("secret-token"),
        )
        .with_review_comments_transport(transport)
    }

    fn review_comments_request(page: u32) -> ReviewCommentsRequest {
        ReviewCommentsRequest {
            owner: "apache".into(),
            name: "airflow".into(),
            number: 17,
            page,
        }
    }

    async fn fetch_all_rest_activity(
        forge: &GithubForge,
        transport: &ScriptedReviewCommentsTransport,
    ) -> Vec<ForgeActivityPage> {
        let mut cursor =
            Some(serde_json::to_string(&rest_only_cursor()).expect("serialize cursor"));
        let mut pages = Vec::new();
        loop {
            let requests_before = transport.requests().len();
            let page = forge
                .fetch_pr_activity("apache", "airflow", 17, "ashb", cursor.as_deref())
                .await
                .expect("fetch REST activity page");
            let request_count = transport.requests().len() - requests_before;
            assert_eq!(
                request_count,
                usize::from(page.rate_limit.is_some()),
                "a Forge page performs at most one provider request"
            );
            cursor = page.next.clone();
            let finished = cursor.is_none();
            pages.push(page);
            if finished {
                break;
            }
        }
        pages
    }

    fn activity(id: usize, source: ActivityKind) -> ForgeActivity {
        ForgeActivity {
            kind: source,
            relation: ActivityRelation::Own,
            occurred_at: format!("2026-08-05T12:{:02}:{:02}Z", (id / 60) % 60, id % 60)
                .parse()
                .expect("timestamp"),
            actor: Some("ashb".into()),
            head_sha: None,
            external_id: Some(format!("event-{id:03}")),
            permalink: Some(format!("https://github.example/events/{id}")),
            payload: ActivityPayload::None,
        }
    }

    fn review_comment(id: u64) -> RestReviewComment {
        RestReviewComment {
            id,
            in_reply_to_id: None,
            user: Some(Login {
                login: "ashb".into(),
            }),
            created_at: format!(
                "2026-08-{:02}T{:02}:{:02}:00Z",
                1 + id / (24 * 60),
                (id / 60) % 24,
                id % 60
            )
            .parse()
            .expect("timestamp"),
            body: String::new(),
            html_url: format!("https://github.example/pull/17#discussion_r{id}"),
        }
    }

    fn request_budget(cost: u32, remaining: u32) -> Option<ActivityRateLimit> {
        Some(ActivityRateLimit {
            unit: RateLimitUnit::Requests,
            cost,
            remaining,
        })
    }

    #[tokio::test]
    async fn a_forge_activity_page_makes_one_review_comment_request() {
        let transport = Arc::new(ScriptedReviewCommentsTransport::new(vec![
            review_comments_response((0..100).rev(), 4999),
        ]));
        let forge = forge_for(Arc::clone(&transport));
        let cursor = serde_json::to_string(&rest_only_cursor()).expect("serialize cursor");

        let page = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", Some(&cursor))
            .await
            .expect("fetch one forge activity page");

        assert_eq!(transport.requests(), vec![review_comments_request(1)]);
        assert_eq!(page.rate_limit, request_budget(1, 4999));
        assert_eq!(page.next_rate_limit, Some(RateLimitUnit::Requests));
        assert!(page.next.is_some(), "the REST scan is resumable");
    }

    #[tokio::test]
    async fn review_comment_transport_scans_301_comments_linearly() {
        let transport = Arc::new(ScriptedReviewCommentsTransport::new(vec![
            review_comments_response((201..=300).rev(), 4999),
            review_comments_response((201..=300).rev(), 4998),
            review_comments_response((101..=200).rev(), 4997),
            review_comments_response((101..=200).rev(), 4996),
            review_comments_response((1..=100).rev(), 4995),
            review_comments_response((1..=100).rev(), 4994),
            review_comments_response([0], 4993),
        ]));
        let forge = forge_for(Arc::clone(&transport));

        let pages = fetch_all_rest_activity(&forge, &transport).await;

        assert_eq!(
            pages[0].activities.len(),
            100,
            "stop-at-known can stop early"
        );
        assert_eq!(
            transport.requests(),
            vec![
                review_comments_request(1),
                review_comments_request(1),
                review_comments_request(2),
                review_comments_request(2),
                review_comments_request(3),
                review_comments_request(3),
                review_comments_request(4),
            ]
        );
        assert_eq!(
            pages
                .iter()
                .flat_map(|page| &page.activities)
                .map(|activity| activity.external_id.as_deref().expect("stable ID"))
                .collect::<Vec<_>>(),
            (0..=300).rev().map(|id| id.to_string()).collect::<Vec<_>>()
        );
        assert!(
            pages
                .iter()
                .filter_map(|page| page.next.as_ref())
                .all(|cursor| cursor.len() < 2_000),
            "the resumable REST scan stays bounded"
        );
        for pair in pages.windows(2) {
            assert_ne!(
                pair[0].next, pair[1].next,
                "each request advances its cursor"
            );
        }
        assert_eq!(
            pages
                .iter()
                .filter_map(|page| page.rate_limit)
                .collect::<Vec<_>>(),
            (4993..=4999)
                .rev()
                .map(|remaining| ActivityRateLimit {
                    unit: RateLimitUnit::Requests,
                    cost: 1,
                    remaining,
                })
                .collect::<Vec<_>>()
        );
        assert!(
            pages[..pages.len() - 1]
                .iter()
                .all(|page| page.next_rate_limit == Some(RateLimitUnit::Requests))
        );
        assert_eq!(pages.last().expect("last page").next_rate_limit, None);
    }

    #[tokio::test]
    async fn a_failed_review_comment_page_retries_the_same_checkpoint() {
        let transport = Arc::new(ScriptedReviewCommentsTransport::new(vec![
            review_comments_response((0..100).rev(), 4999),
            failed_response("temporary failure"),
            review_comments_response((0..100).rev(), 4997),
        ]));
        let forge = forge_for(Arc::clone(&transport));
        let initial = serde_json::to_string(&rest_only_cursor()).expect("serialize cursor");
        let first = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", Some(&initial))
            .await
            .expect("first REST page");
        let checkpoint = first.next.expect("scan checkpoint");

        forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", Some(&checkpoint))
            .await
            .expect_err("the scripted transport failure is returned");
        let retried = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", Some(&checkpoint))
            .await
            .expect("retry the same REST page");

        assert_eq!(
            transport.requests(),
            vec![
                review_comments_request(1),
                review_comments_request(1),
                review_comments_request(1),
            ]
        );
        assert_eq!(retried.rate_limit, request_budget(1, 4997));
        assert_eq!(retried.next_rate_limit, Some(RateLimitUnit::Requests));
        assert_ne!(retried.next.as_deref(), Some(checkpoint.as_str()));
    }

    #[tokio::test]
    async fn review_comment_transport_survives_page_boundary_insertions_and_deletions() {
        let first_page = (150..250).rev().collect::<Vec<_>>();
        let changed_first_page = (149..250).rev().filter(|id| *id != 200).collect::<Vec<_>>();
        let changed_second_page = (51..=150).rev().collect::<Vec<_>>();
        let transport = Arc::new(ScriptedReviewCommentsTransport::new(vec![
            review_comments_response(first_page, 4999),
            review_comments_response(changed_first_page.clone(), 4998),
            review_comments_response(changed_first_page, 4997),
            review_comments_response(changed_second_page.clone(), 4996),
            review_comments_response(changed_second_page, 4995),
            review_comments_response((0..=50).rev(), 4994),
        ]));
        let forge = forge_for(Arc::clone(&transport));

        let pages = fetch_all_rest_activity(&forge, &transport).await;

        assert_eq!(
            transport.requests(),
            vec![
                review_comments_request(1),
                review_comments_request(1),
                review_comments_request(1),
                review_comments_request(2),
                review_comments_request(2),
                review_comments_request(3),
            ]
        );
        assert_eq!(
            pages
                .iter()
                .flat_map(|page| &page.activities)
                .map(|activity| activity.external_id.as_deref().expect("stable ID"))
                .collect::<Vec<_>>(),
            (0..250).rev().map(|id| id.to_string()).collect::<Vec<_>>()
        );
    }

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
    fn a_quoted_mention_is_not_an_invitation() {
        // Replying to somebody who tagged a third party does not make that
        // third party's answer mine to wait for — the tag is in their words.
        let body = "> hey @otheruser, what do you think\n\nI don't know about \
                    them but I would keep it.";

        assert!(handles_in(body).is_empty(), "{:?}", handles_in(body));
    }

    #[test]
    fn an_invitation_i_wrote_myself_counts() {
        assert_eq!(
            handles_in("@o-nikolas does this match the executor?"),
            ["o-nikolas"]
        );
        // Quoted above, asked below: the ask is mine.
        assert_eq!(
            handles_in("> not sure about @someone-else\n\nfair — @potiuk, thoughts?"),
            ["potiuk"]
        );
    }

    #[test]
    fn a_handle_in_code_or_a_team_invites_nobody() {
        // Code is somebody's example, and a team needs membership this never
        // fetches — inviting everybody in it would be the wrong guess.
        assert!(handles_in("run `git log @potiuk` for that").is_empty());
        assert!(handles_in("cc @apache/airflow-committers").is_empty());
        // An email address is not a mention either.
        assert!(handles_in("mail ash@astronomer.io about it").is_empty());
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

    #[test]
    fn activity_graphql_errors_reach_the_forge_error_message() {
        let response: GraphqlResponse<ActivityQuery> = serde_json::from_value(serde_json::json!({
            "errors": [{
                "message": "Variable $includeReviews of required type Boolean! was not provided."
            }]
        }))
        .expect("GraphQL error response");

        let error =
            activity_graphql_response("github.com", "activity for apache/airflow#72034", response)
                .expect_err("GraphQL errors fail the activity request");

        assert_eq!(
            error.to_string(),
            "GitHub GraphQL request (activity for apache/airflow#72034): Variable $includeReviews of required type Boolean! was not provided"
        );
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
        // Opened long before it was last touched, which is the pair worth
        // keeping apart — and neither is when this ledger first saw it.
        assert_eq!(
            snapshot.created_at,
            Some("2026-07-22T09:14:00Z".parse().unwrap())
        );
        assert_eq!(snapshot.updated_at, "2026-08-05T18:04:01Z".parse().unwrap());
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
    fn pr_nodes_map_the_authoritative_lifecycle_timestamp() {
        let snapshot = |state: &str,
                        closed_at: Option<&str>,
                        merged_at: Option<&str>,
                        lifecycle_event: serde_json::Value| {
            serde_json::from_value::<PrNode>(serde_json::json!({
                "number": 1,
                "title": "lifecycle",
                "isDraft": false,
                "state": state,
                "author": {"login": "octocat"},
                "authorAssociation": "MEMBER",
                "headRefOid": "abc",
                "baseRefName": "main",
                "updatedAt": "2026-08-05T16:00:00Z",
                "createdAt": "2026-08-01T10:00:00Z",
                "closedAt": closed_at,
                "mergedAt": merged_at,
                "latestLifecycleEvent": {
                    "nodes": if lifecycle_event.is_null() { vec![] } else { vec![lifecycle_event] }
                },
                "labels": {"nodes": []},
                "milestone": null,
                "files": {"totalCount": 0, "nodes": []}
            }))
            .unwrap()
            .into_snapshot()
            .unwrap()
        };

        assert_eq!(
            snapshot(
                "CLOSED",
                Some("2026-08-05T14:00:00Z"),
                None,
                serde_json::Value::Null,
            )
            .state_changed_at,
            Some("2026-08-05T14:00:00Z".parse().unwrap())
        );
        assert_eq!(
            snapshot(
                "MERGED",
                Some("2026-08-05T14:00:00Z"),
                Some("2026-08-05T15:00:00Z"),
                serde_json::Value::Null,
            )
            .state_changed_at,
            Some("2026-08-05T15:00:00Z".parse().unwrap())
        );
        assert_eq!(
            snapshot(
                "OPEN",
                Some("2026-08-05T14:00:00Z"),
                None,
                serde_json::json!({
                    "__typename": "ReopenedEvent",
                    "createdAt": "2026-08-05T16:00:00Z",
                }),
            )
            .state_changed_at,
            Some("2026-08-05T16:00:00Z".parse().unwrap())
        );
        assert_eq!(
            snapshot("OPEN", None, None, serde_json::Value::Null).state_changed_at,
            None,
            "a PR that has always been open has no lifecycle transition"
        );
        assert_eq!(
            snapshot(
                "OPEN",
                Some("2026-08-05T14:00:00Z"),
                None,
                serde_json::json!({
                    "__typename": "ClosedEvent",
                    "createdAt": "2026-08-05T14:00:00Z",
                }),
            )
            .state_changed_at,
            None,
            "an event that disagrees with the current state is not authoritative"
        );
    }

    #[test]
    fn detail_records_all_submitted_reviews_without_changing_my_review_metadata() {
        let mut raw: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/graphql/pr_detail.json")).unwrap();
        raw["repository"]["pullRequest"]["reviews"]["nodes"] = serde_json::json!([
            {"id":"my-review","url":"https://example.test/review/1","author":{"login":"ashb"},"state":"COMMENTED","submittedAt":"2026-08-05T10:00:00Z","commit":{"oid":"head"},"body":""},
            {"id":"pending","author":{"login":"ashb"},"state":"PENDING","submittedAt":null,"commit":null,"body":""},
            {"id":"other","author":{"login":"other"},"state":"APPROVED","submittedAt":"2026-08-05T11:00:00Z","commit":null,"body":""}
        ]);
        let data: DetailQuery = serde_json::from_value(raw).unwrap();
        let detail = data
            .repository
            .unwrap()
            .pull_request
            .unwrap()
            .into_detail("ashb", 1, 4999);
        assert_eq!(detail.activities.len(), 2);
        assert_eq!(detail.activities[0].relation, ActivityRelation::Own);
        assert_eq!(detail.activities[1].relation, ActivityRelation::Context);
        assert_eq!(detail.last_reviewed_sha.as_deref(), Some("head"));
        let event = &detail.activities[0];
        assert_eq!(event.external_id.as_deref(), Some("my-review"));
        assert_eq!(
            event.permalink.as_deref(),
            Some("https://example.test/review/1")
        );
        assert_eq!(event.occurred_at, "2026-08-05T10:00:00Z".parse().unwrap());
        assert_eq!(
            event.payload,
            ActivityPayload::ReviewSubmitted {
                result: ReviewResult::Commented,
                reviewed_sha: Some("head".into())
            }
        );
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
        // The state comes back with the detail, which is the only way a refresh
        // of one PR can learn it has been closed since the last sweep.
        assert_eq!(detail.state, PrState::Open);

        // Everybody else's comments and reviews, and mine left out of them.
        assert!(
            detail.said.iter().all(|said| said.by != "ashb"),
            "{:?}",
            detail.said
        );
        assert!(
            detail.said.iter().any(|said| said.by == "uranusjr"),
            "the comment that mentioned me is also somebody speaking: {:?}",
            detail.said
        );
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
    fn detail_response_maps_the_authoritative_lifecycle_timestamp() {
        let detail = |state: &str, closed_at: Option<&str>, lifecycle_event: serde_json::Value| {
            let mut raw: serde_json::Value =
                serde_json::from_str(include_str!("../tests/fixtures/graphql/pr_detail.json"))
                    .unwrap();
            let pull_request = raw
                .pointer_mut("/repository/pullRequest")
                .unwrap()
                .as_object_mut()
                .unwrap();
            pull_request.insert("state".into(), serde_json::json!(state));
            pull_request.insert("closedAt".into(), serde_json::json!(closed_at));
            pull_request.insert("mergedAt".into(), serde_json::Value::Null);
            pull_request.insert(
                "latestLifecycleEvent".into(),
                serde_json::json!({"nodes": [lifecycle_event]}),
            );
            let data: DetailQuery = serde_json::from_value(raw).unwrap();
            data.repository.unwrap().pull_request.unwrap().into_detail(
                "ashb",
                data.rate_limit.cost,
                data.rate_limit.remaining,
            )
        };

        assert_eq!(
            detail(
                "CLOSED",
                Some("2026-08-05T14:00:00Z"),
                serde_json::json!({
                    "__typename": "ClosedEvent",
                    "createdAt": "2026-08-05T14:00:00Z",
                })
            )
            .state_changed_at,
            Some("2026-08-05T14:00:00Z".parse().unwrap())
        );
        assert_eq!(
            detail(
                "OPEN",
                Some("2026-08-05T14:00:00Z"),
                serde_json::json!({
                    "__typename": "ReopenedEvent",
                    "createdAt": "2026-08-05T16:00:00Z",
                })
            )
            .state_changed_at,
            Some("2026-08-05T16:00:00Z".parse().unwrap())
        );
    }

    #[test]
    fn independently_paged_activity_emits_the_newest_safe_prefix() {
        let raw = include_str!("../tests/fixtures/graphql/pr_activity.json");
        let [root_first, root_second]: [ActivityQuery; 2] =
            serde_json::from_str(raw).expect("captured activity pages parse");

        let first = root_first
            .into_activity_page_from("ashb", ActivityCursor::default())
            .expect("first page maps");
        let cursor = serde_json::from_str(first.next.as_deref().expect("a second page"))
            .expect("the GitHub cursor stays private to this adapter");
        let second = root_second
            .into_activity_page_from("ashb", cursor)
            .expect("second page maps");

        assert_eq!(
            first.rate_limit,
            Some(ActivityRateLimit {
                unit: RateLimitUnit::Points,
                cost: 8,
                remaining: 4992,
            })
        );
        assert_eq!(first.next_rate_limit, Some(RateLimitUnit::Points));
        assert!(first.next.is_some(), "the first response has another page");
        assert_eq!(
            second.rate_limit,
            Some(ActivityRateLimit {
                unit: RateLimitUnit::Points,
                cost: 4,
                remaining: 4988,
            })
        );
        assert_eq!(second.next_rate_limit, None);
        assert!(second.next.is_none());
        assert_eq!(
            first
                .activities
                .iter()
                .map(|activity| activity.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("review-commented"),
                Some("review-unknown"),
                Some("review-changes"),
                Some("review-approved"),
                Some("review-other-user"),
                Some("issue-comment-10"),
                Some("issue-comment-other-user"),
                Some("closed-event-9"),
            ]
        );
        assert_eq!(
            second
                .activities
                .iter()
                .map(|activity| activity.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("issue-comment-8"),
                Some("reopened-event-7"),
                Some("merged-event-6"),
            ]
        );
        assert_eq!(
            first.activities[0],
            ForgeActivity {
                kind: ActivityKind::ReviewSubmitted,
                relation: ActivityRelation::Own,
                occurred_at: "2026-08-05T11:40:00Z".parse().expect("timestamp"),
                actor: Some("ashb".into()),
                head_sha: Some("sha-commented".into()),
                external_id: Some("review-commented".into()),
                permalink: Some("https://github.example/reviews/commented".into()),
                payload: ActivityPayload::ReviewSubmitted {
                    result: ReviewResult::Commented,
                    reviewed_sha: Some("sha-commented".into()),
                },
            }
        );
        assert_eq!(
            first.activities[1].payload,
            ActivityPayload::ReviewSubmitted {
                result: ReviewResult::Other("NEEDS_SECURITY_SIGNOFF".into()),
                reviewed_sha: Some("sha-unknown".into()),
            }
        );
        assert_eq!(
            first.activities[2].payload,
            ActivityPayload::ReviewSubmitted {
                result: ReviewResult::ChangesRequested,
                reviewed_sha: Some("sha-changes".into()),
            }
        );
        assert_eq!(
            first.activities[3].payload,
            ActivityPayload::ReviewSubmitted {
                result: ReviewResult::Approved,
                reviewed_sha: Some("sha-approved".into()),
            }
        );
        assert_eq!(first.activities[4].relation, ActivityRelation::Relevant);
        assert_eq!(first.activities[6].relation, ActivityRelation::Context);
        assert_eq!(first.activities[7].relation, ActivityRelation::Context);
        assert_eq!(first.activities[7].actor.as_deref(), Some("closer"));
        assert_eq!(
            first.activities[7].permalink.as_deref(),
            Some("https://github.example/pull/17")
        );
        assert_eq!(second.activities[1].actor.as_deref(), Some("reopener"));
        assert_eq!(second.activities[2].actor.as_deref(), Some("merger"));
        assert_eq!(first.activities[5].actor.as_deref(), Some("ashb"));
        assert_eq!(
            first.activities[5].permalink.as_deref(),
            Some("https://github.example/comments/10")
        );
        assert_eq!(
            second.activities[2].permalink.as_deref(),
            Some("https://github.example/pull/17")
        );
    }

    #[tokio::test]
    async fn a_finished_cursor_drains_more_than_one_buffered_output_page() {
        let mut cursor = ActivityCursor::default();
        cursor.finish();
        cursor.reviews.events = (0..130)
            .rev()
            .map(|id| activity(id, ActivityKind::Commented))
            .collect();
        cursor.timeline.events = (130..230)
            .rev()
            .map(|id| activity(id, ActivityKind::PrClosed))
            .collect();

        let first = cursor
            .activity_page(request_budget(8, 4992))
            .expect("first output page");
        let forge = GithubForge::with_token(
            &ForgeHost::default(),
            "github.example",
            Token {
                value: "unused".into(),
                source: crate::TokenSource::Override,
            },
        );
        let second = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", first.next.as_deref())
            .await
            .expect("second output page");
        let third = forge
            .fetch_pr_activity("apache", "airflow", 17, "ashb", second.next.as_deref())
            .await
            .expect("last output page");

        assert_eq!(first.activities.len(), 100);
        assert_eq!(second.activities.len(), 100);
        assert_eq!(third.activities.len(), 30);
        assert_eq!(third.next, None);
        assert_eq!(
            first
                .activities
                .iter()
                .chain(&second.activities)
                .chain(&third.activities)
                .map(|event| event.external_id.as_deref().expect("stable ID"))
                .collect::<Vec<_>>(),
            (0..230)
                .rev()
                .map(|id| format!("event-{id:03}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_unavailable_pull_request_does_not_discard_buffered_activity() {
        for repository in [None, Some(ActivityRepo { pull_request: None })] {
            let mut cursor = ActivityCursor::default();
            cursor.finish();
            cursor.comments.events = (0..110)
                .rev()
                .map(|id| activity(id, ActivityKind::Commented))
                .collect();
            let query = ActivityQuery {
                repository,
                rate_limit: RateLimit {
                    limit: 5000,
                    cost: 3,
                    remaining: 4997,
                    reset_at: "2026-08-05T13:00:00Z".parse().expect("timestamp"),
                },
            };

            let first = query
                .into_activity_page_from("ashb", cursor)
                .expect("buffered activity maps");
            let mut cursor: ActivityCursor = serde_json::from_str(
                first
                    .next
                    .as_deref()
                    .expect("a second buffered output page"),
            )
            .expect("checkpoint resumes");
            let second = cursor.activity_page(None).expect("last output page");

            assert_eq!(first.activities.len(), 100);
            assert_eq!(second.activities.len(), 10);
            assert_eq!(second.next, None);
        }
    }

    #[test]
    fn ordered_review_comments_join_the_resumed_activity_merge_without_gaps() {
        let raw = include_str!("../tests/fixtures/graphql/pr_activity.json");
        let [root_first, root_second]: [ActivityQuery; 2] =
            serde_json::from_str(raw).expect("captured activity pages parse");
        let raw = include_str!("../tests/fixtures/rest/pr_activity_review_comments.json");
        let review_comments = serde_json::from_str(raw).expect("captured review comments parse");

        let root_page = root_first
            .into_activity_page_from("ashb", ActivityCursor::fresh())
            .expect("root page maps");
        assert!(
            root_page.activities.is_empty(),
            "REST lookahead is not known yet"
        );
        assert_eq!(
            root_page.rate_limit,
            Some(ActivityRateLimit {
                unit: RateLimitUnit::Points,
                cost: 8,
                remaining: 4992,
            })
        );
        assert_eq!(root_page.next_rate_limit, Some(RateLimitUnit::Requests));
        let mut cursor: ActivityCursor =
            serde_json::from_str(root_page.next.as_deref().expect("REST page"))
                .expect("checkpoint resumes");
        cursor.review_comments.append_scan(review_comments, "ashb");
        let first = cursor
            .activity_page(request_budget(1, 4991))
            .expect("safe prefix maps");
        assert_eq!(first.next_rate_limit, Some(RateLimitUnit::Points));
        let cursor = serde_json::from_str(first.next.as_deref().expect("timeline page"))
            .expect("checkpoint resumes");
        let last = root_second
            .into_activity_page_from("ashb", cursor)
            .expect("timeline page maps");

        assert_eq!(
            first
                .activities
                .iter()
                .chain(&last.activities)
                .map(|activity| activity.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("22"),
                Some("review-commented"),
                Some("review-unknown"),
                Some("review-changes"),
                Some("review-approved"),
                Some("review-other-user"),
                Some("21"),
                Some("issue-comment-10"),
                Some("20"),
                Some("issue-comment-other-user"),
                Some("closed-event-9"),
                Some("issue-comment-8"),
                Some("reopened-event-7"),
                Some("merged-event-6"),
            ]
        );
        assert_eq!(
            first
                .activities
                .iter()
                .chain(&last.activities)
                .filter_map(|activity| {
                    matches!(
                        activity.kind,
                        ActivityKind::PrClosed | ActivityKind::PrReopened | ActivityKind::PrMerged
                    )
                    .then_some((activity.kind, activity.payload.clone()))
                })
                .collect::<Vec<_>>(),
            vec![
                (
                    ActivityKind::PrClosed,
                    ActivityPayload::StateChanged {
                        from: PrState::Open,
                        to: PrState::Closed,
                    },
                ),
                (
                    ActivityKind::PrReopened,
                    ActivityPayload::StateChanged {
                        from: PrState::Closed,
                        to: PrState::Open,
                    },
                ),
                (
                    ActivityKind::PrMerged,
                    ActivityPayload::StateChanged {
                        from: PrState::Open,
                        to: PrState::Merged,
                    },
                ),
            ]
        );
        assert_eq!(last.next, None);
    }

    #[test]
    fn rest_review_comments_keep_the_root_thread_id() {
        let raw = include_str!("../tests/fixtures/rest/pr_activity_review_comments.json");
        let comments = serde_json::from_str(raw).expect("captured review comments parse");
        let mut cursor = ActivityCursor::fresh();
        cursor.reviews.finish();
        cursor.comments.finish();
        cursor.timeline.finish();

        cursor.review_comments.append_scan(comments, "ashb");
        let page = cursor
            .activity_page(request_budget(1, 4991))
            .expect("page maps");

        assert_eq!(page.next, None);
        assert_eq!(
            page.activities
                .iter()
                .map(|activity| activity.external_id.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("22"), Some("21"), Some("20")]
        );
        assert_eq!(
            page.activities
                .iter()
                .map(|activity| activity.permalink.as_deref())
                .collect::<Vec<_>>(),
            vec![
                Some("https://github.example/pull/17#discussion_r22"),
                Some("https://github.example/pull/17#discussion_r21"),
                Some("https://github.example/pull/17#discussion_r20"),
            ]
        );
        assert_eq!(
            page.activities
                .iter()
                .map(|activity| &activity.payload)
                .collect::<Vec<_>>(),
            vec![
                &ActivityPayload::ReviewThreadCommented {
                    thread_id: Some("21".into()),
                },
                &ActivityPayload::ReviewThreadCommented {
                    thread_id: Some("21".into()),
                },
                &ActivityPayload::ReviewThreadCommented {
                    thread_id: Some("19".into()),
                },
            ]
        );
    }

    #[test]
    fn activity_relations_preserve_other_actors_and_ignore_quoted_mentions() {
        for (author, body, expected) in [
            (Some("AshB"), "", ActivityRelation::Own),
            (
                Some("other"),
                "please @AshB review",
                ActivityRelation::Relevant,
            ),
            (Some("other"), "see `@ashb`", ActivityRelation::Context),
            (None, "", ActivityRelation::Context),
        ] {
            let author = author.map(|login| Login {
                login: login.into(),
            });
            assert_eq!(activity_relation(author.as_ref(), body, "ashb"), expected);
        }
    }

    #[test]
    fn legacy_buffered_other_actor_events_default_to_context() {
        let mut cursor = ActivityCursor::default();
        cursor.finish();
        let mut event = activity(1, ActivityKind::Commented);
        event.actor = Some("other".into());
        cursor.comments.events.push(event);
        let mut raw = serde_json::to_value(cursor).unwrap();
        raw["comments"]["events"][0]
            .as_object_mut()
            .unwrap()
            .remove("relation");
        let mut cursor: ActivityCursor = serde_json::from_value(raw).unwrap();
        let page = cursor.activity_page(None).unwrap();
        assert_eq!(page.activities[0].actor.as_deref(), Some("other"));
        assert_eq!(page.activities[0].relation, ActivityRelation::Context);
    }

    #[test]
    fn paged_review_comments_retain_context_and_mentions() {
        let mut cursor = rest_only_cursor();
        let comments = (100..200)
            .rev()
            .map(|id| {
                let mut comment = review_comment(id);
                comment.user = Some(Login {
                    login: "other".into(),
                });
                if id == 199 {
                    comment.body = "ping @ashb".into();
                }
                comment
            })
            .collect();
        cursor.review_comments.append_scan(comments, "ashb");
        let first = cursor.activity_page(None).unwrap();
        assert_eq!(first.activities.len(), 100);
        assert_eq!(first.activities[0].relation, ActivityRelation::Relevant);
        assert!(
            first.activities[1..]
                .iter()
                .all(|event| event.relation == ActivityRelation::Context)
        );
        let mut cursor: ActivityCursor =
            serde_json::from_str(first.next.as_deref().unwrap()).unwrap();
        cursor
            .review_comments
            .append_scan(vec![review_comment(99)], "ashb");
        let second = cursor.activity_page(None).unwrap();
        assert_eq!(second.activities.len(), 1);
        assert_eq!(second.activities[0].relation, ActivityRelation::Own);
        assert_eq!(second.next, None);
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
    fn my_own_mention_is_returned_as_a_mention() {
        let author = Some(Login {
            login: "ashb".into(),
        });
        let at = "2026-08-30T12:29:21Z".parse().unwrap();

        assert_eq!(
            mention_from(&author, Some(at), "@ashb take another look", "ashb"),
            Some(Mention {
                by: "ashb".into(),
                at,
            })
        );
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
