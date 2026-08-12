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
use reviewq_forge::Forge;

use crate::config::{Config, RepoRef};

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

/// The program that opens a URL in whatever the desktop uses for one.
///
/// Every platform spells its own differently and none of them is worth a
/// dependency: this is one argument and one process. It is the default
/// [`Handoff`](crate::config::Handoff) as well as what the interface's `o` key
/// runs, which is why it lives here rather than in a frontend.
#[cfg(target_os = "macos")]
pub const URL_OPENER: &str = "open";
/// The program that opens a URL, on Windows.
#[cfg(target_os = "windows")]
pub const URL_OPENER: &str = "explorer";
/// The program that opens a URL, everywhere else.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const URL_OPENER: &str = "xdg-open";

/// Work out how to hand `number` off.
///
/// This is where a handoff's requirements are actually enforced, rather than at
/// config load: a repo with no checkout, or one whose checkout has moved, is only
/// a problem for a review — `sync`, `list` and `show` never look at a working
/// tree, so they must not be refused over one.
pub fn handoff_for(cfg: &Config, number: u64) -> Result<Handoff> {
    let repo = resolve_repo(cfg, number)?;
    // Not `.ok()`: a host that resolves to no adapter used to leave `{url}` as an
    // empty string, so the configured default became `wiff forge pull ""` and the
    // review command reported reviewq's own config problem as a bad argument.
    let forge = cfg
        .forge_for(&repo.host)
        .with_context(|| format!("no forge for {}, so #{number} has no URL", repo.host))?;
    handoff_with(cfg, forge.as_ref(), &repo, number)
}

/// [`handoff_for`], given the repo and a forge already connected to its host.
///
/// Split out so a test can supply the forge. Building a real one is harmless, but
/// asking it for credentials is not: resolution runs whatever the host configures,
/// which may be a helper that blocks on an interactive unlock — `cargo test` must
/// not be able to make something prompt.
pub fn handoff_with(
    cfg: &Config,
    forge: &dyn Forge,
    repo: &RepoRef,
    number: u64,
) -> Result<Handoff> {
    let url = forge.web_url(&repo.owner, &repo.name, number);
    // The token is the one part that is best-effort: the handoff command does its
    // own credential resolution when this comes back empty, so a locked credential
    // helper must not stop a review.
    let token = forge
        .handoff_credentials()
        .inspect_err(|err| tracing::warn!(%err, "no token to forward to the review command"))
        .ok()
        .map(|(var, value)| (var.to_string(), value.to_string()));

    let cwd = checkout_for(repo)?;

    let number = number.to_string();
    let argv: Vec<String> = cfg
        .handoff
        .review_command
        .iter()
        .map(|arg| arg.replace("{number}", &number).replace("{url}", &url))
        .collect();

    Ok(Handoff { argv, token, cwd })
}

/// The directory to run the handoff in, checked here because here is where it
/// matters.
///
/// A repo naming no checkout is not an error — a review tool that works purely
/// from a URL needs none, and `doctor` is where that shortcoming is reported. A
/// repo naming one that isn't there is: the alternative is handing the command a
/// working directory that doesn't exist and letting it fail in its own words.
fn checkout_for(repo: &RepoRef) -> Result<Option<PathBuf>> {
    let Some(path) = repo.path.clone() else {
        return Ok(None);
    };
    if !path.is_dir() {
        bail!(
            "{}'s configured checkout {} is not a directory",
            repo.slug(),
            path.display()
        );
    }
    Ok(Some(path))
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

    let found = crate::resolve::open()?.repos_with_pr(number)?;
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
    use crate::fake_forge::FakeForge;
    use std::ffi::OsStr;
    use std::path::Path;

    /// The repo the test configs below name.
    fn repo(checkout: Option<&Path>) -> RepoRef {
        RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: checkout.map(Path::to_path_buf),
        }
    }

    /// A forge that answers without resolving anything, so no test can prompt.
    fn forge() -> FakeForge {
        FakeForge::new(vec![])
    }

    /// A config naming one repo, optionally with a checkout, and a review command
    /// that substitutes the number.
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

        let handoff =
            handoff_with(&config, &forge(), &repo(Some(&checkout)), 70135).expect("handoff");

        assert_eq!(handoff.cwd.as_deref(), Some(checkout.as_path()));
        assert_eq!(handoff.argv.last().map(String::as_str), Some("70135"));
    }

    #[test]
    fn a_handoff_refuses_a_checkout_that_is_not_there() {
        // Config load lets this through on purpose — `sync` and `list` don't care
        // — so the handoff is where an unmounted volume or a moved checkout has to
        // be caught, rather than handing the review command a directory that
        // isn't.
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("moved-away");
        let config = config_of(dir.path(), Some(&gone));

        let err = handoff_with(&config, &forge(), &repo(Some(&gone)), 70135)
            .expect_err("no such checkout");

        assert!(err.to_string().contains("not a directory"), "{err:#}");
        assert!(err.to_string().contains("apache/airflow"), "{err:#}");
    }

    #[test]
    fn a_handoff_for_a_repo_with_no_checkout_inherits_the_working_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);

        let handoff = handoff_with(&config, &forge(), &repo(None), 70135).expect("handoff");

        assert_eq!(handoff.cwd, None);
    }

    #[test]
    fn working_out_a_handoff_asks_the_forge_it_was_given_and_resolves_nothing() {
        // The forge is injected precisely so this test cannot reach a credential
        // helper: the token here is the fake's, and the config's `token_env` names
        // a variable nothing sets — if resolution were happening, it would fail.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            labels = ["x"]
            [handoff]
            review_command = ["wiff", "forge", "pull", "{url}"]
            [forge."github.com"]
            token_env = "REVIEWQ_TEST_ABSENT_TOKEN"
            "#,
        )
        .expect("write config");
        let config = crate::config::load(Some(&path)).expect("loads").config;

        let handoff = handoff_with(&config, &forge(), &repo(None), 70135).expect("handoff");

        assert_eq!(
            handoff.argv.last().map(String::as_str),
            Some("https://github.com/apache/airflow/pull/70135"),
            "the URL comes from the forge that was handed in"
        );
        assert_eq!(
            handoff.token,
            Some(("GITHUB_TOKEN".to_string(), "fake".to_string())),
            "and so does the token"
        );
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
