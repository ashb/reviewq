//! Handing a PR to whatever actually reviews it.
//!
//! reviewq never reviews anything: it decides what deserves attention and then
//! execs `handoff.review_command`. This works out what to exec, shared because
//! both frontends do it and a `review` that passed different arguments depending
//! on where it was invoked would be a bug waiting to happen.
//!
//! Running it is the caller's job, because the two do it differently: the CLI
//! inherits its terminal as-is, while the TUI has to give the terminal back
//! first and take it over again afterwards.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::{Config, RepoRef};
use crate::paths;

/// A resolved handoff: the command to run, and what to run it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// The program and its arguments, `{number}` and `{url}` already
    /// substituted. Never empty — config load rejects that.
    pub argv: Vec<String>,
    /// A token to pass through as an environment variable, if one resolved.
    ///
    /// Saves the handoff command resolving credentials again for a forge reviewq
    /// has already authenticated against. `None` when that failed, in which case
    /// the command falls back to its own resolution rather than this stopping the
    /// review outright.
    pub token: Option<(String, String)>,
    /// The directory to run it in: the repo's local checkout, when config names
    /// one. `None` inherits reviewq's own, which is all that was ever possible
    /// before and is enough for a tool that works purely from a URL.
    ///
    /// It matters because a review tool is usually repo-shaped. A bare
    /// `{number}` only resolves against a checkout's remote, and wiff will not
    /// publish a review it mirrored from outside the repository it belongs to:
    /// "publishing a forge review pulled outside its repository is not
    /// supported from the review yet".
    pub cwd: Option<PathBuf>,
}

impl Handoff {
    /// The command, ready to run: program, arguments, token and working directory
    /// all applied.
    ///
    /// Assembled here rather than by each frontend so the two cannot drift on
    /// which of those they remember. Running it stays theirs, because that is
    /// where they genuinely differ — the CLI inherits its terminal, the TUI has
    /// to hand it over and take it back.
    pub fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.argv[0]);
        command.args(&self.argv[1..]);
        if let Some((var, value)) = &self.token {
            command.env(var, value);
        }
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        command
    }
}

/// Work out how to hand `number` off.
pub fn handoff_for(cfg: &Config, number: u64) -> Result<Handoff> {
    let repo = resolve_repo(cfg, number)?;

    // The URL comes from the forge connection, so a PR can be handed off by URL
    // from anywhere — a bare number only means something inside a checkout of
    // the right repo.
    let connected = cfg.forge_for(&repo.host).ok();
    let url = connected
        .as_ref()
        .map(|forge| forge.web_url(&repo.owner, &repo.name, number))
        .unwrap_or_default();
    // Only here is the token wanted, and only best-effort: the handoff command
    // does its own credential resolution if this comes back empty, so a locked
    // credential helper must not stop a review.
    let token = connected.as_ref().and_then(|forge| {
        forge
            .handoff_credentials()
            .inspect_err(|err| tracing::warn!(%err, "no token to forward to the review command"))
            .ok()
            .map(|(var, value)| (var.to_string(), value.to_string()))
    });

    let number = number.to_string();
    let argv: Vec<String> = cfg
        .handoff
        .review_command
        .iter()
        .map(|arg| arg.replace("{number}", &number).replace("{url}", &url))
        .collect();

    Ok(Handoff {
        argv,
        token,
        cwd: repo.path.clone(),
    })
}

/// Which configured repo `number` belongs to.
///
/// Trivial with one repo configured. With more than one, `review` — unlike
/// `show`/`done`/etc — can legitimately name a PR the ledger has never heard of,
/// so a bare number can't always be resolved by asking the ledger; this only
/// falls back to that when it is already tracked.
fn resolve_repo(config: &Config, number: u64) -> Result<RepoRef> {
    let repos: Vec<&RepoRef> = config.repos().collect();
    if let [repo] = repos.as_slice() {
        return Ok((*repo).clone());
    }

    let found = reviewq_ledger::repos_with_pr(&paths::database_file()?, number)?;
    match found.as_slice() {
        [key] => repos
            .into_iter()
            .find(|r| r.key() == *key)
            .cloned()
            .with_context(|| {
                format!(
                    "#{number} was last synced from {}/{}, which is no longer configured",
                    key.owner, key.name
                )
            }),
        [] => bail!(
            "#{number} isn't in the ledger yet and more than one repo is configured — \
             run `reviewq sync` first, or configure a single repo"
        ),
        _ => bail!("#{number} is tracked in more than one configured repo — not supported yet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;

    /// A config naming one repo, optionally with a checkout, and a review command
    /// that substitutes the number — so what comes back is the same whether or not
    /// a forge connection could be made in this environment.
    fn config_of(dir: &Path, checkout: Option<&Path>) -> Config {
        let path = config_file(dir, checkout);
        crate::config::load(Some(&path)).expect("loads").config
    }

    fn config_file(dir: &Path, checkout: Option<&Path>) -> PathBuf {
        let path = dir.join("config.toml");
        let repo = match checkout {
            Some(checkout) => format!(
                r#"{{ owner = "apache", name = "airflow", path = "{}" }}"#,
                checkout.display()
            ),
            None => r#"{ owner = "apache", name = "airflow" }"#.to_string(),
        };
        std::fs::write(
            &path,
            format!(
                r#"
                [identity]
                login = "ashb"
                [[project]]
                repos = [{repo}]
                [[project.interest]]
                labels = ["x"]

                [handoff]
                review_command = ["wiff", "forge", "pull", "{{number}}"]
                "#
            ),
        )
        .expect("write config");
        path
    }

    #[test]
    fn a_handoff_runs_in_the_repos_checkout_when_config_names_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = dir.path().join("airflow");
        std::fs::create_dir(&checkout).expect("checkout");
        let config = config_of(dir.path(), Some(&checkout));

        let handoff = handoff_for(&config, 70135).expect("handoff");

        assert_eq!(handoff.cwd.as_deref(), Some(checkout.as_path()));
        assert_eq!(handoff.argv.last().map(String::as_str), Some("70135"));
    }

    #[test]
    fn a_handoff_for_a_repo_with_no_checkout_inherits_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);

        let handoff = handoff_for(&config, 70135).expect("handoff");

        assert_eq!(handoff.cwd, None);
    }

    #[test]
    fn the_built_command_carries_the_directory_the_token_and_the_arguments() {
        let handoff = Handoff {
            argv: vec!["wiff".into(), "forge".into(), "pull".into(), "7".into()],
            token: Some(("GITHUB_TOKEN".into(), "secret".into())),
            cwd: Some(PathBuf::from("/tmp")),
        };

        let command = handoff.command();

        assert_eq!(command.get_program(), OsStr::new("wiff"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![OsStr::new("forge"), OsStr::new("pull"), OsStr::new("7")]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == OsStr::new("GITHUB_TOKEN")
                    && value == Some(OsStr::new("secret")))
        );
    }
}
