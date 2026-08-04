use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use reviewq_forge::{DEFAULT_HOST, ForgeHost, ForgeTable, resolve_host};

/// Written verbatim to the config path on first run. Kept as a literal (rather
/// than serialised from `Config::default()`) so the comments survive.
pub const DEFAULT_CONFIG: &str = include_str!("config.default.toml");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub identity: Identity,
    pub repo: Repo,
    #[serde(default)]
    pub interest: Interest,
    #[serde(default)]
    pub bots: Bots,
    #[serde(default)]
    pub handoff: Handoff,
    #[serde(default)]
    pub sync: Sync,
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Repo {
    pub owner: String,
    pub name: String,
    /// The host the repo lives on; selects its `[forge]` entry. Defaults to
    /// public GitHub.
    #[serde(default = "default_host")]
    pub host: String,
}

fn default_host() -> String {
    DEFAULT_HOST.to_string()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Interest {
    pub labels: Vec<String>,
    pub paths: Vec<String>,
    pub author_associations: Vec<String>,
    pub milestones: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Bots {
    pub logins: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Handoff {
    /// Argv for `reviewq review N`; `{number}` is substituted in each element.
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
            review_command: ["wiff", "forge", "pull", "{number}"]
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
        if self.identity.login.trim().is_empty() {
            bail!(
                "identity.login is empty in {} — set it to your GitHub login",
                path.display()
            );
        }
        if self.repo.owner.trim().is_empty() || self.repo.name.trim().is_empty() {
            bail!(
                "repo.owner and repo.name must both be set in {}",
                path.display()
            );
        }
        if self.handoff.review_command.is_empty() {
            bail!("handoff.review_command is empty in {}", path.display());
        }
        if self.repo.host.trim().is_empty() {
            bail!("repo.host is empty in {}", path.display());
        }
        self.forge_host().with_context(|| {
            format!(
                "resolving the forge host for {} in {}",
                self.slug(),
                path.display()
            )
        })?;
        Ok(())
    }

    pub fn slug(&self) -> String {
        format!("{}/{}", self.repo.owner, self.repo.name)
    }

    /// The resolved forge settings for the configured repo host, with a
    /// supported provider. Errors if the host is neither built in nor
    /// configured, or names a provider without an adapter.
    pub fn forge_host(&self) -> Result<ForgeHost> {
        resolve_host(&self.forges, &self.repo.host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses_and_validates() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config parses");
        config
            .validate(Path::new("default"))
            .expect("default config validates");
        assert_eq!(config.slug(), "apache/airflow");
    }

    #[test]
    fn omitted_sections_fall_back_to_defaults() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [repo]
            owner = "apache"
            name = "airflow"
            "#,
        )
        .expect("minimal config parses");

        assert_eq!(config.sync.bootstrap_days, 14);
        assert_eq!(config.handoff.review_command[0], "wiff");
        assert!(config.bots.logins.contains(&"codecov[bot]".to_string()));
        assert!(config.interest.labels.is_empty());
        assert_eq!(config.repo.host, "github.com");
        let host = config.forge_host().expect("built-in github host");
        assert_eq!(host.provider.as_deref(), Some("github"));
    }

    #[test]
    fn a_repo_on_an_unknown_host_without_a_forge_entry_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [repo]
            owner = "acme"
            name = "widgets"
            host = "git.acme.example"
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
            [repo]
            owner = "acme"
            name = "widgets"
            host = "github.acme.example"

            [forge."github.acme.example"]
            provider = "github"
            api_base = "https://github.acme.example/api/v3"
            "#,
        )
        .expect("config parses");

        config.validate(Path::new("cfg.toml")).expect("validates");
        let host = config.forge_host().expect("configured host");
        assert_eq!(
            host.api_base.as_deref(),
            Some("https://github.acme.example/api/v3")
        );
    }

    #[test]
    fn a_host_with_an_unsupported_provider_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [repo]
            owner = "acme"
            name = "widgets"
            host = "git.acme.example"

            [forge."git.acme.example"]
            provider = "gitlab"
            "#,
        )
        .expect("config parses");

        let err = config.forge_host().unwrap_err();
        assert!(err.to_string().contains("no adapter yet"));
    }

    #[test]
    fn empty_login_is_rejected() {
        let config: Config = toml::from_str(
            r#"
            [identity]
            login = "  "
            [repo]
            owner = "apache"
            name = "airflow"
            "#,
        )
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
        toml::from_str::<Config>(
            r#"
            [identity]
            login = "ashb"
            [repo]
            owner = "apache"
            name = "airflow"
            [future]
            thing = 1
            "#,
        )
        .expect("forward-compatible");
    }
}
