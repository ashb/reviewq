//! Which host a repo lives on, which adapter speaks to it, and where its token
//! comes from.
//!
//! Keyed by host so a self-hosted instance is a config entry rather than a code
//! change, and so per-host tokens fall out for free. Only the GitHub provider
//! has an adapter today; [`resolve_host`] is where an unsupported provider is
//! turned away, and the seam a second one plugs into.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The public GitHub host, used when `[repo]` names no other.
pub const DEFAULT_HOST: &str = "github.com";

/// reviewq's own token override, tried before any host-configured source.
const OVERRIDE_ENV: &str = "REVIEWQ_GITHUB_TOKEN";

/// A gh CLI convention honoured as a fallback for the GitHub provider.
const GH_TOKEN_ENV: &str = "GH_TOKEN";

/// One host's row: which adapter family speaks to it, its API root, and the
/// variables its token comes from. Every field is optional so a user entry can
/// override one value of a built-in without restating the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeHost {
    /// The provider family whose adapter speaks to this host, e.g. `github`.
    pub provider: Option<String>,
    /// The API root URL, for an instance that is not the provider's public one
    /// (a GitHub Enterprise host, say).
    pub api_base: Option<String>,
    /// The environment variable holding the token.
    pub token_env: Option<String>,
    /// The environment variable naming a file that holds the token.
    pub token_file_env: Option<String>,
}

impl ForgeHost {
    /// Take each field from `self` where set, else from `base`. A `None` field
    /// inherits, so a user entry replaces a built-in value but cannot clear one.
    fn overlay_onto(&self, base: &ForgeHost) -> ForgeHost {
        ForgeHost {
            provider: self.provider.clone().or_else(|| base.provider.clone()),
            api_base: self.api_base.clone().or_else(|| base.api_base.clone()),
            token_env: self.token_env.clone().or_else(|| base.token_env.clone()),
            token_file_env: self
                .token_file_env
                .clone()
                .or_else(|| base.token_file_env.clone()),
        }
    }
}

/// The `[forge]` table, keyed by host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ForgeTable(BTreeMap<String, ForgeHost>);

impl ForgeTable {
    fn from_entries(entries: impl IntoIterator<Item = (String, ForgeHost)>) -> Self {
        ForgeTable(
            entries
                .into_iter()
                .map(|(host, row)| (host.to_ascii_lowercase(), row))
                .collect(),
        )
    }

    /// Resolve `host`, overlaying any user entry onto the built-in default.
    /// `None` when the host is neither built in nor configured.
    pub fn host(&self, host: &str) -> Option<ForgeHost> {
        let host = host.to_ascii_lowercase();
        match (self.0.get(&host), BUILT_IN_HOSTS.get(&host)) {
            (Some(user), Some(default)) => Some(user.overlay_onto(default)),
            (Some(user), None) => Some(user.clone()),
            (None, default) => default.cloned(),
        }
    }
}

impl<'de> Deserialize<'de> for ForgeTable {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entries = BTreeMap::<String, ForgeHost>::deserialize(deserializer)?;
        Ok(ForgeTable::from_entries(entries))
    }
}

/// Providers reviewq has an adapter for. A host naming anything else resolves
/// but is turned away by [`resolve_host`], so the failure is one clear message
/// rather than a later mystery.
const SUPPORTED_PROVIDERS: &[&str] = &["github"];

/// Resolve `host` in `table` to settings with a supported provider.
///
/// This is the single place forge-support is decided: it errors if the host is
/// neither built in nor configured, if it names no provider, or if it names one
/// with no adapter yet.
pub fn resolve_host(table: &ForgeTable, host: &str) -> Result<ForgeHost> {
    let resolved = table.host(host).with_context(|| {
        format!("unknown forge host {host:?}; add a [forge.\"{host}\"] entry naming its provider")
    })?;
    match resolved.provider.as_deref() {
        Some(provider) if SUPPORTED_PROVIDERS.contains(&provider) => Ok(resolved),
        Some(other) => {
            bail!("forge host {host:?} names provider {other:?}, which has no adapter yet")
        }
        None => bail!("forge host {host:?} has no provider"),
    }
}

/// Built-in row for public GitHub. A self-hosted instance is added by the user;
/// this is only what works with no config at all.
static BUILT_IN_HOSTS: LazyLock<BTreeMap<String, ForgeHost>> = LazyLock::new(|| {
    BTreeMap::from([(
        DEFAULT_HOST.to_string(),
        ForgeHost {
            provider: Some("github".to_string()),
            api_base: None,
            token_env: Some("GITHUB_TOKEN".to_string()),
            token_file_env: Some("GITHUB_TOKEN_FILE".to_string()),
        },
    )])
});

/// Where a resolved token came from, so `doctor` can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenSource {
    /// reviewq's own `$REVIEWQ_GITHUB_TOKEN` override.
    Override,
    /// A file named by the host's `token_file_env` variable.
    HostFile(String),
    /// The host's `token_env` variable.
    HostEnv(String),
    /// The gh CLI convention `$GH_TOKEN`.
    GhTokenEnv,
    /// Shelling out to `gh auth token`.
    GhCli,
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Override => write!(f, "${OVERRIDE_ENV}"),
            Self::HostFile(var) => write!(f, "file named by ${var}"),
            Self::HostEnv(var) => write!(f, "${var}"),
            Self::GhTokenEnv => write!(f, "${GH_TOKEN_ENV}"),
            Self::GhCli => write!(f, "gh auth token"),
        }
    }
}

/// A token and the source it was resolved from.
#[derive(Debug, Clone)]
pub struct Token {
    /// The token value.
    pub value: String,
    /// Where it came from.
    pub source: TokenSource,
}

/// Resolve the token for `host` against the real environment, falling back to
/// `gh auth token` for the GitHub provider.
pub fn resolve_token(host: &ForgeHost) -> Result<Token> {
    let env = |name: &str| std::env::var(name).ok();
    if let Some(token) = resolve_from_env(host, &env)? {
        return Ok(token);
    }
    if is_github(host)
        && let Some(value) = gh_auth_token()?
    {
        return Ok(Token {
            value,
            source: TokenSource::GhCli,
        });
    }
    bail!(
        "no token found for {host}: set ${OVERRIDE_ENV}{}, or make `gh auth token` work",
        host.token_env
            .as_deref()
            .map(|v| format!(" / ${v}"))
            .unwrap_or_default(),
        host = host.provider.as_deref().unwrap_or("this host"),
    )
}

/// The environment half of resolution, with the variable lookup injected so it
/// is testable without touching the process environment. `Ok(None)` means
/// nothing was configured and the caller may try `gh`; a source that is present
/// but empty (or an unreadable token file) is an error, never a fall-through.
fn resolve_from_env(
    host: &ForgeHost,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Token>> {
    if let Some(value) = env(OVERRIDE_ENV) {
        return non_empty(&value, || format!("${OVERRIDE_ENV} is set but empty")).map(|value| {
            Some(Token {
                value,
                source: TokenSource::Override,
            })
        });
    }

    if let Some(var) = &host.token_file_env
        && let Some(path) = env(var)
    {
        let path = path.trim();
        if path.is_empty() {
            bail!("the token file path in ${var} is empty");
        }
        let value = read_token_file(Path::new(path))?;
        return Ok(Some(Token {
            value,
            source: TokenSource::HostFile(var.clone()),
        }));
    }

    if let Some(var) = &host.token_env
        && let Some(value) = env(var)
    {
        return non_empty(&value, || format!("${var} is set but empty")).map(|value| {
            Some(Token {
                value,
                source: TokenSource::HostEnv(var.clone()),
            })
        });
    }

    if is_github(host)
        && let Some(value) = env(GH_TOKEN_ENV)
    {
        return non_empty(&value, || format!("${GH_TOKEN_ENV} is set but empty")).map(|value| {
            Some(Token {
                value,
                source: TokenSource::GhTokenEnv,
            })
        });
    }

    Ok(None)
}

fn is_github(host: &ForgeHost) -> bool {
    host.provider.as_deref() == Some("github")
}

fn non_empty(value: &str, describe: impl FnOnce() -> String) -> Result<String> {
    let token = value.trim();
    if token.is_empty() {
        bail!(describe());
    }
    Ok(token.to_string())
}

fn read_token_file(path: &Path) -> Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("reading token from {}", path.display()))?;
    let token = contents.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", path.display());
    }
    Ok(token.to_string())
}

fn gh_auth_token() -> Result<Option<String>> {
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output();
    let output = match output {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let token = String::from_utf8(output.stdout)?.trim().to_string();
    Ok((!token.is_empty()).then_some(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<const N: usize>(
        pairs: [(&'static str, &'static str); N],
    ) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<&str, &str> = pairs.into_iter().collect();
        move |name: &str| map.get(name).map(|v| v.to_string())
    }

    fn github_host() -> ForgeHost {
        ForgeTable::default()
            .host(DEFAULT_HOST)
            .expect("built-in github.com")
    }

    #[test]
    fn built_in_github_needs_no_config() {
        let host = github_host();
        assert_eq!(host.provider.as_deref(), Some("github"));
        assert_eq!(host.token_env.as_deref(), Some("GITHUB_TOKEN"));
        assert!(host.api_base.is_none());
    }

    #[test]
    fn a_user_entry_overlays_the_built_in_without_clearing_it() {
        let table: ForgeTable = toml::from_str(
            r#"
            ["github.com"]
            api_base = "https://ghe.example.com/api/v3"
            "#,
        )
        .expect("parses");

        let host = table.host("github.com").expect("known host");
        assert_eq!(
            host.api_base.as_deref(),
            Some("https://ghe.example.com/api/v3")
        );
        // Provider and token vars are inherited from the built-in.
        assert_eq!(host.provider.as_deref(), Some("github"));
        assert_eq!(host.token_env.as_deref(), Some("GITHUB_TOKEN"));
    }

    #[test]
    fn host_lookup_is_case_insensitive() {
        let table = ForgeTable::default();
        assert!(table.host("GitHub.com").is_some());
    }

    #[test]
    fn an_unknown_host_resolves_to_nothing() {
        let table = ForgeTable::default();
        assert!(table.host("git.example.org").is_none());
    }

    #[test]
    fn a_configured_unknown_host_is_usable() {
        let table: ForgeTable = toml::from_str(
            r#"
            ["git.example.org"]
            provider = "forgejo"
            token_env = "EXAMPLE_TOKEN"
            "#,
        )
        .expect("parses");

        let host = table.host("git.example.org").expect("configured host");
        assert_eq!(host.provider.as_deref(), Some("forgejo"));
    }

    #[test]
    fn resolve_host_returns_the_built_in_github() {
        let host = resolve_host(&ForgeTable::default(), DEFAULT_HOST).expect("github resolves");
        assert_eq!(host.provider.as_deref(), Some("github"));
    }

    #[test]
    fn resolve_host_rejects_an_unknown_host() {
        let err = resolve_host(&ForgeTable::default(), "git.example.org").unwrap_err();
        assert!(err.to_string().contains("unknown forge host"));
    }

    #[test]
    fn resolve_host_rejects_a_provider_without_an_adapter() {
        let table: ForgeTable = toml::from_str(
            r#"
            ["git.example.org"]
            provider = "gitlab"
            "#,
        )
        .expect("parses");

        let err = resolve_host(&table, "git.example.org").unwrap_err();
        assert!(err.to_string().contains("no adapter yet"));
    }

    #[test]
    fn override_env_wins_over_everything() {
        let host = github_host();
        let env = env_of([
            (OVERRIDE_ENV, "from-override"),
            ("GITHUB_TOKEN", "from-github"),
        ]);
        let token = resolve_from_env(&host, &env).unwrap().unwrap();
        assert_eq!(token.value, "from-override");
        assert_eq!(token.source, TokenSource::Override);
    }

    #[test]
    fn host_token_env_is_used_when_no_override() {
        let host = github_host();
        let env = env_of([("GITHUB_TOKEN", "from-github")]);
        let token = resolve_from_env(&host, &env).unwrap().unwrap();
        assert_eq!(token.value, "from-github");
        assert_eq!(token.source, TokenSource::HostEnv("GITHUB_TOKEN".into()));
    }

    #[test]
    fn gh_token_is_a_github_fallback() {
        let host = github_host();
        let env = env_of([("GH_TOKEN", "from-gh")]);
        let token = resolve_from_env(&host, &env).unwrap().unwrap();
        assert_eq!(token.value, "from-gh");
        assert_eq!(token.source, TokenSource::GhTokenEnv);
    }

    #[test]
    fn gh_token_is_not_used_for_other_providers() {
        let host = ForgeHost {
            provider: Some("forgejo".into()),
            token_env: Some("FORGEJO_TOKEN".into()),
            ..Default::default()
        };
        let env = env_of([("GH_TOKEN", "from-gh")]);
        assert!(resolve_from_env(&host, &env).unwrap().is_none());
    }

    #[test]
    fn a_configured_token_env_takes_precedence_over_gh_token() {
        let host = ForgeHost {
            provider: Some("github".into()),
            token_env: Some("CI_TOKEN".into()),
            ..Default::default()
        };
        let env = env_of([("CI_TOKEN", "from-ci"), ("GH_TOKEN", "from-gh")]);
        let token = resolve_from_env(&host, &env).unwrap().unwrap();
        assert_eq!(token.value, "from-ci");
    }

    #[test]
    fn an_empty_configured_source_is_an_error_not_a_fallthrough() {
        let host = github_host();
        let env = env_of([(OVERRIDE_ENV, "   ")]);
        let err = resolve_from_env(&host, &env).unwrap_err();
        assert!(err.to_string().contains("set but empty"));
    }

    #[test]
    fn nothing_configured_yields_none() {
        let host = github_host();
        let env = env_of([]);
        assert!(resolve_from_env(&host, &env).unwrap().is_none());
    }

    #[test]
    fn a_token_file_is_read_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "  file-token\n").unwrap();

        let host = ForgeHost {
            provider: Some("github".into()),
            token_file_env: Some("TOKEN_FILE".into()),
            ..Default::default()
        };
        let path_str = path.to_string_lossy().into_owned();
        let env = |name: &str| (name == "TOKEN_FILE").then(|| path_str.clone());

        let token = resolve_from_env(&host, &env).unwrap().unwrap();
        assert_eq!(token.value, "file-token");
        assert_eq!(token.source, TokenSource::HostFile("TOKEN_FILE".into()));
    }

    #[test]
    fn a_token_file_env_pointing_nowhere_is_an_error() {
        let host = ForgeHost {
            provider: Some("github".into()),
            token_file_env: Some("TOKEN_FILE".into()),
            ..Default::default()
        };
        let env = env_of([("TOKEN_FILE", "/nonexistent/reviewq-token")]);
        assert!(resolve_from_env(&host, &env).is_err());
    }
}
