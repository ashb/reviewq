use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reviewq_core::rules::{ConditionInput, Interest, RuleInput};
use serde::{Deserialize, Serialize};

use reviewq_forge::{
    DEFAULT_HOST, Forge, ForgeHost, ForgeTable, build, resolve_host, resolve_token,
};

/// Written verbatim to the config path on first run. Kept as a literal (rather
/// than serialised from `Config::default()`) so the comments survive.
pub const DEFAULT_CONFIG: &str = include_str!("config.default.toml");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub identity: Identity,
    /// One or more projects, each bundling its repos with the interest rules
    /// that apply to them. `[[project]]` in TOML.
    #[serde(rename = "project", default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub bots: Bots,
    #[serde(default)]
    pub handoff: Handoff,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub output: Output,
    /// Global default for which relationships make a PR involve me; a project
    /// may override it.
    #[serde(default)]
    pub involvement: Involvement,
    /// Per-host forge settings, keyed by host. Overlaid onto built-in defaults,
    /// so an empty table still resolves the public GitHub host. Plural since
    /// the table itself may hold settings for several hosts; kept as `forge`
    /// on the TOML side (`[forge."host"]`) so the file format and this
    /// internal name aren't tied together.
    #[serde(default, rename = "forge")]
    pub forges: ForgeTable,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Identity {
    /// My GitHub login. Reasons are computed relative to this account.
    pub login: String,
}

/// A group of repos that share a set of interest rules. Conventions differ per
/// project, so rules are duplicated across projects rather than shared.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    /// Optional label, used in reason strings and diagnostics.
    #[serde(default)]
    pub name: Option<String>,
    /// The repos in this project.
    pub repos: Vec<RepoRef>,
    /// Interest rules, matched against every repo in the project. `A PR is
    /// interesting if ANY rule matches.` `[[project.interest]]` in TOML.
    #[serde(default)]
    pub interest: Vec<InterestRule>,
    /// Relationships that involve me in this project's PRs. Overrides
    /// `[involvement].reasons` when set; inherits it when omitted.
    #[serde(default)]
    pub involvement: Option<Vec<String>>,
    /// Keep surfacing a PR after it merges, so post-merge activity (a reply, a
    /// mention) can flag something that shipped broken. Off by default — most
    /// people want the queue to end at merge.
    #[serde(default)]
    pub include_merged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RepoRef {
    pub owner: String,
    pub name: String,
    /// The host the repo lives on; selects its `[forge]` entry. Defaults to
    /// public GitHub.
    #[serde(default = "default_host")]
    pub host: String,
}

impl RepoRef {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// This repo's identity as the ledger knows it.
    pub fn key(&self) -> reviewq_ledger::RepoKey {
        reviewq_ledger::RepoKey {
            host: self.host.clone(),
            owner: self.owner.clone(),
            name: self.name.clone(),
        }
    }
}

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

/// One interest rule. Today exactly one dimension may be set; the loader
/// rejects a rule that sets more than one, so the future "A and B" conjunction
/// is a config change, not a redesign.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct InterestRule {
    /// Optional label; when set it becomes the rule's reason string.
    pub name: Option<String>,
    pub labels: Vec<String>,
    pub paths: Vec<String>,
    pub author_associations: Vec<String>,
    pub milestones: Vec<String>,
}

impl InterestRule {
    /// Convert to a core rule input, enforcing the one-dimension-per-rule gate.
    fn to_input(&self) -> Result<RuleInput> {
        let mut conditions = Vec::new();
        if !self.labels.is_empty() {
            conditions.push(ConditionInput::Labels(self.labels.clone()));
        }
        if !self.paths.is_empty() {
            conditions.push(ConditionInput::Paths(self.paths.clone()));
        }
        if !self.author_associations.is_empty() {
            conditions.push(ConditionInput::Authors(self.author_associations.clone()));
        }
        if !self.milestones.is_empty() {
            conditions.push(ConditionInput::Milestones(self.milestones.clone()));
        }
        match conditions.len() {
            1 => Ok(RuleInput {
                name: self.name.clone(),
                conditions,
            }),
            0 => bail!(
                "interest rule{} sets no condition (needs one of labels/paths/\
                 author_associations/milestones)",
                self.name
                    .as_deref()
                    .map(|n| format!(" {n:?}"))
                    .unwrap_or_default(),
            ),
            n => bail!(
                "interest rule{} sets {n} conditions; combining conditions in one rule \
                 isn't supported yet — split them into separate rules",
                self.name
                    .as_deref()
                    .map(|n| format!(" {n:?}"))
                    .unwrap_or_default(),
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Bots {
    pub logins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Handoff {
    /// Argv for `reviewq review N`; `{number}` and `{url}` (the PR's full web
    /// URL) are substituted in each element. Prefer `{url}` where the tool
    /// supports it — a bare number only works run from inside a checkout of
    /// the right repo, since that's the only way to infer which one.
    pub review_command: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Sync {
    /// How far back the first-ever sync reaches.
    pub bootstrap_days: u32,
    /// Overlap subtracted from the stored cursor, to absorb clock skew and
    /// GitHub's search index lag.
    pub overlap_minutes: u32,
    /// GraphQL page size for the tier-1 sweep.
    pub page_size: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Output {
    /// Underline a hyperlinked PR title in `show`'s human output. Some
    /// terminals (Ghostty, for one) give no hover indication that a
    /// terminal hyperlink exists at all unless a modifier is already held,
    /// so without this the link is invisible until you know to try it.
    pub underline_links: bool,
}

impl Default for Bots {
    fn default() -> Self {
        Self {
            logins: ["boring-cyborg[bot]", "github-actions[bot]", "codecov[bot]"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Default for Handoff {
    fn default() -> Self {
        Self {
            review_command: ["wiff", "forge", "pull", "{url}"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            bootstrap_days: 14,
            overlap_minutes: 5,
            page_size: 50,
        }
    }
}

impl Default for Output {
    fn default() -> Self {
        Self {
            underline_links: true,
        }
    }
}

/// Which relationships to me make a PR "involved". Each maps to a GitHub search
/// qualifier run against the repo, so involvement is found the same way as
/// interest — no notifications API.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Involvement {
    /// The relationships to search for: `review_requested`, `mention`,
    /// `assign`, `author`, `comment`.
    pub reasons: Vec<String>,
}

impl Default for Involvement {
    fn default() -> Self {
        // The "a human pulled me in and I couldn't otherwise know" signals.
        // `author`/`comment` are deliberately out of the default — the M3
        // attention state machine handles those cases more precisely — but
        // remain available to anyone who lists them.
        Self {
            reasons: ["review_requested", "mention", "assign"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

/// Outcome of resolving the config, so callers can tell the user that a config
/// was just created for them.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
    pub created: bool,
}

/// Load the config, writing the documented default if nothing is there yet.
///
/// An explicit `--config` path is never created: asking for a specific file and
/// silently getting a fresh default would hide a typo.
pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
    match explicit {
        Some(path) => load_from(path, false),
        None => load_from(&crate::paths::config_file()?, true),
    }
}

fn load_from(path: &Path, create_if_missing: bool) -> Result<Loaded> {
    let mut created = false;
    if !path.exists() {
        if !create_if_missing {
            bail!("config not found: {}", path.display());
        }
        let dir = path
            .parent()
            .with_context(|| format!("config path has no parent: {}", path.display()))?;
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating config dir {}", dir.display()))?;
        std::fs::write(path, DEFAULT_CONFIG)
            .with_context(|| format!("writing default config to {}", path.display()))?;
        created = true;
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config: Config =
        toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
    config.validate(path)?;

    Ok(Loaded {
        config,
        path: path.to_path_buf(),
        created,
    })
}

impl Config {
    fn validate(&self, path: &Path) -> Result<()> {
        let at = || path.display();

        if self.identity.login.trim().is_empty() {
            bail!(
                "identity.login is empty in {} — set it to your GitHub login",
                at()
            );
        }
        if self.handoff.review_command.is_empty() {
            bail!("handoff.review_command is empty in {}", at());
        }

        let repos: Vec<&RepoRef> = self.repos().collect();
        if repos.is_empty() {
            bail!(
                "no repos configured in {} — add a [[project]] with a repo",
                at()
            );
        }
        let mut seen = std::collections::HashSet::new();
        for repo in &repos {
            if repo.owner.trim().is_empty() || repo.name.trim().is_empty() {
                bail!("a repo is missing owner or name in {}", at());
            }
            if repo.host.trim().is_empty() {
                bail!("repo {} has an empty host in {}", repo.slug(), at());
            }
            if !seen.insert(repo.slug()) {
                bail!(
                    "repo {} appears in more than one project in {}",
                    repo.slug(),
                    at()
                );
            }
            resolve_host(&self.forges, &repo.host).with_context(|| {
                format!("resolving the forge host for {} in {}", repo.slug(), at())
            })?;
        }

        // Compile every project's rules so glob errors and the
        // one-condition-per-rule gate surface at load, not mid-sync.
        for project in &self.projects {
            self.interest_for(project)
                .with_context(|| format!("in project {} in {}", project.label(), at()))?;
        }
        Ok(())
    }

    /// Every repo across every project.
    pub fn repos(&self) -> impl Iterator<Item = &RepoRef> {
        self.projects.iter().flat_map(|p| p.repos.iter())
    }

    /// The resolved forge settings for `host`, with a supported provider.
    pub fn forge_host_for(&self, host: &str) -> Result<ForgeHost> {
        resolve_host(&self.forges, host)
    }

    /// Resolve `host`'s settings, resolve a token for it, and build a connected
    /// [`Forge`] — the sequence every command that talks to the forge repeats.
    /// Takes a bare host rather than a [`RepoRef`], since credential
    /// resolution only ever depends on which host a repo lives on, never its
    /// owner/name — callers with a `RepoRef` in hand pass `&repo.host`;
    /// callers that only resolved a repo's identity from the ledger (no
    /// config `RepoRef` at all) pass that instead.
    ///
    /// `doctor` is the one command that calls the pieces directly instead of
    /// this: it reports on each step (host, token) as it succeeds, so
    /// collapsing them here would cost it that granularity.
    pub fn forge_for(&self, host: &str) -> Result<Box<dyn Forge>> {
        let resolved = self.forge_host_for(host)?;
        let token = resolve_token(&resolved)?;
        build(&resolved, host, &token.value)
    }

    /// The relationships that involve me in `project`: its own override if set,
    /// else the global default.
    pub fn involving_reasons<'a>(&'a self, project: &'a Project) -> &'a [String] {
        project
            .involvement
            .as_deref()
            .unwrap_or(&self.involvement.reasons)
    }

    /// Compile a project's interest rules into the pure evaluator.
    pub fn interest_for(&self, project: &Project) -> Result<Interest> {
        let inputs = project
            .interest
            .iter()
            .map(InterestRule::to_input)
            .collect::<Result<Vec<_>>>()?;
        Interest::compile(inputs).context("compiling interest globs")
    }
}

impl Project {
    /// A name for messages: the configured name, else the first repo's slug.
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.repos
                .first()
                .map(RepoRef::slug)
                .unwrap_or_else(|| "<empty>".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid config: one project, one repo, one rule.
    fn minimal(extra: &str) -> String {
        format!(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            name = "airflow"
            repos = [{{ owner = "apache", name = "airflow" }}]
            [[project.interest]]
            labels = ["area:task-sdk"]
            {extra}
            "#
        )
    }

    #[test]
    fn default_config_parses_and_validates() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config
            .validate(Path::new("default"))
            .expect("default config validates");
        let repo = config.repos().next().expect("one repo");
        assert_eq!(repo.slug(), "apache/airflow");
    }

    #[test]
    fn omitted_sections_fall_back_to_defaults() {
        let config: Config = toml::from_str(&minimal("")).expect("minimal config parses");
        config.validate(Path::new("cfg")).expect("validates");

        assert_eq!(config.sync.bootstrap_days, 14);
        assert_eq!(config.handoff.review_command[0], "wiff");
        assert!(config.bots.logins.contains(&"codecov[bot]".to_string()));
        assert!(config.output.underline_links);

        let repo = config.repos().next().unwrap();
        assert_eq!(repo.host, "github.com");
        let host = config
            .forge_host_for(&repo.host)
            .expect("built-in github host");
        assert_eq!(host.provider.as_deref(), Some("github"));
    }

    #[test]
    fn nested_interest_rules_parse() {
        let config: Config = toml::from_str(&minimal(
            r#"
            [[project.interest]]
            name = "serialization"
            paths = ["airflow-core/src/airflow/serialization/**"]
            [[project.interest]]
            author_associations = ["FIRST_TIME_CONTRIBUTOR"]
            "#,
        ))
        .expect("parses");

        let project = &config.projects[0];
        assert_eq!(project.interest.len(), 3);
        config.interest_for(project).expect("compiles");
    }

    #[test]
    fn involving_reasons_default_then_project_override() {
        let config: Config = toml::from_str(&minimal("")).expect("parses");
        let project = &config.projects[0];
        // Global default: the lean human-pulled-me-in set, no `subscribed`.
        assert_eq!(
            config.involving_reasons(project),
            ["review_requested", "mention", "assign"]
        );

        let overridden: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            involvement = ["mention"]
            [[project.interest]]
            labels = ["area:task-sdk"]
            "#,
        )
        .expect("parses");
        let project = &overridden.projects[0];
        assert_eq!(overridden.involving_reasons(project), ["mention"]);
    }

    #[test]
    fn a_rule_with_two_dimensions_is_rejected_for_now() {
        let config: Config = toml::from_str(&minimal(
            r#"
            [[project.interest]]
            author_associations = ["FIRST_TIME_CONTRIBUTOR"]
            paths = ["task-sdk/**"]
            "#,
        ))
        .expect("parses");

        let err = config.validate(Path::new("cfg")).unwrap_err();
        assert!(
            format!("{err:#}").contains("isn't supported yet"),
            "{err:#}"
        );
    }

    #[test]
    fn a_rule_with_no_condition_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            name = "empty"
            "#,
        )
        .expect("parses");

        let err = config.validate(Path::new("cfg")).unwrap_err();
        assert!(format!("{err:#}").contains("sets no condition"), "{err:#}");
    }

    #[test]
    fn more_than_one_repo_validates() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [
              { owner = "apache", name = "airflow" },
              { owner = "astronomer", name = "astro" },
            ]
            [[project.interest]]
            labels = ["area:task-sdk"]
            "#,
        )
        .expect("parses");

        config.validate(Path::new("cfg")).expect("validates");
        assert_eq!(
            config.repos().map(RepoRef::slug).collect::<Vec<_>>(),
            ["apache/airflow", "astronomer/astro"]
        );
    }

    #[test]
    fn no_repo_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            "#,
        )
        .expect("parses");

        let err = config.validate(Path::new("cfg")).unwrap_err();
        assert!(err.to_string().contains("no repos configured"), "{err:#}");
    }

    #[test]
    fn a_repo_on_an_unknown_host_without_a_forge_entry_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "acme", name = "widgets", host = "git.acme.example" }]
            [[project.interest]]
            labels = ["x"]
            "#,
        )
        .expect("config parses");

        let err = config.validate(Path::new("cfg.toml")).unwrap_err();
        assert!(err.to_string().contains("resolving the forge host"));
    }

    #[test]
    fn a_self_hosted_github_enterprise_host_resolves() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "acme", name = "widgets", host = "github.acme.example" }]
            [[project.interest]]
            labels = ["x"]

            [forge."github.acme.example"]
            provider = "github"
            api_base = "https://github.acme.example/api/v3"
            "#,
        )
        .expect("config parses");

        config.validate(Path::new("cfg.toml")).expect("validates");
        let repo = config.repos().next().unwrap();
        let host = config.forge_host_for(&repo.host).expect("configured host");
        assert_eq!(
            host.api_base.as_deref(),
            Some("https://github.acme.example/api/v3")
        );
    }

    #[test]
    fn empty_login_is_rejected() {
        let config: Config =
            toml::from_str(&minimal("").replace(r#"login = "ashb""#, r#"login = "  ""#))
                .expect("config parses");

        let err = config.validate(Path::new("cfg.toml")).unwrap_err();
        assert!(err.to_string().contains("identity.login is empty"));
    }

    #[test]
    fn first_run_writes_the_documented_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/config.toml");

        let loaded = load_from(&path, true).expect("creates and loads");
        assert!(loaded.created);
        assert_eq!(
            std::fs::read_to_string(&path).expect("written"),
            DEFAULT_CONFIG
        );

        let reloaded = load_from(&path, false).expect("loads without creating");
        assert!(!reloaded.created);
    }

    #[test]
    fn an_explicitly_named_missing_config_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.toml");

        let err = load_from(&path, false).unwrap_err();
        assert!(err.to_string().contains("config not found"));
        assert!(
            !path.exists(),
            "must not create the file it complained about"
        );
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        toml::from_str::<Config>(&minimal(
            r#"
            [future]
            thing = 1
            "#,
        ))
        .expect("forward-compatible");
    }
}
