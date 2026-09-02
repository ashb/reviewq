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
use std::process::ExitStatus;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use reviewq_core::model::{ActivityKind, ActivityPayload, ActivitySource};
use reviewq_forge::Forge;
use reviewq_ledger::{Ledger, NewActivityEvent, RepoId, RepoKey};

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
    subject: Option<ReviewSubject>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewSubject {
    repo: RepoKey,
    repo_id: RepoId,
    number: u64,
}

/// The independently observable results of a child handoff and its history
/// write.
#[derive(Debug)]
pub struct HandoffOutcome {
    /// How the review child exited.
    pub status: ExitStatus,
    /// Why the review-started event could not be written, if it could not.
    pub history_error: Option<anyhow::Error>,
}

impl Handoff {
    /// The exact pull request this prepared handoff records and refreshes.
    pub fn target(&self) -> Result<(RepoKey, u64)> {
        let subject = self
            .subject
            .as_ref()
            .context("review handoff was not prepared against the ledger")?;
        Ok((subject.repo.clone(), subject.number))
    }

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

    /// Start the handoff without waiting for it to exit.
    pub fn spawn(&self) -> Result<std::process::Child> {
        self.command()
            .spawn()
            .with_context(|| format!("running {:?}", self.argv[0]))
    }

    /// Start the handoff, record that start, and wait for the child without
    /// allowing the history write to replace the child's exit status.
    pub fn run(&self, at: Timestamp) -> Result<HandoffOutcome> {
        let subject = self
            .subject
            .as_ref()
            .context("review handoff was not prepared against the ledger")?;
        self.run_with_record(|| {
            let ledger = crate::resolve::open()?;
            record_review_started_in(&ledger, subject, at)
        })
    }

    fn run_with_record(&self, record: impl FnOnce() -> Result<()>) -> Result<HandoffOutcome> {
        let mut child = self.spawn()?;
        let history_error = record().err();
        let status = child
            .wait()
            .with_context(|| format!("waiting for {:?}", self.argv[0]))?;
        Ok(HandoffOutcome {
            status,
            history_error,
        })
    }
}

fn record_review_started_in(ledger: &Ledger, subject: &ReviewSubject, at: Timestamp) -> Result<()> {
    let repo_id = ledger
        .repo_id(&subject.repo)?
        .with_context(|| format!("{} is no longer in the ledger", subject.repo.slug()))?;
    if repo_id != subject.repo_id {
        bail!("{} was replaced in the ledger", subject.repo.slug());
    }
    let show = ledger.show(repo_id, subject.number)?.with_context(|| {
        format!(
            "{} #{} is no longer in the ledger",
            subject.repo.slug(),
            subject.number
        )
    })?;
    ledger.record_activity(
        repo_id,
        subject.number,
        &NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            source: ActivitySource::Local,
            kind: ActivityKind::ReviewStarted,
            occurred_at: at,
            recorded_at: at,
            actor: None,
            head_sha: Some(show.pr.head_sha),
            external_id: None,
            permalink: None,
            payload: ActivityPayload::None,
        },
    )?;
    Ok(())
}

/// Resolve a handoff and make sure its PR has a durable subject for local
/// activity before the caller starts the child process.
pub async fn prepare_handoff_for(cfg: &Config, number: u64) -> Result<Handoff> {
    let ledger = crate::resolve::open()?;
    let repo = resolve_repo_in(cfg, &ledger, number)?;
    prepare_handoff_for_repo_in(cfg, &repo, number, &ledger).await
}

/// Prepare a handoff for a PR whose repository has already been resolved.
pub async fn prepare_handoff_for_repo(
    cfg: &Config,
    target: &RepoKey,
    number: u64,
) -> Result<Handoff> {
    let repo = cfg
        .repos()
        .find(|repo| repo.key() == *target)
        .with_context(|| format!("{} is not configured", target.slug()))?
        .clone();
    let ledger = crate::resolve::open()?;
    prepare_handoff_for_repo_in(cfg, &repo, number, &ledger).await
}

async fn prepare_handoff_for_repo_in(
    cfg: &Config,
    repo: &RepoRef,
    number: u64,
    ledger: &Ledger,
) -> Result<Handoff> {
    let forge = cfg
        .forge_for(&repo.host)
        .with_context(|| format!("no forge for {}, so #{number} has no URL", repo.host))?;
    prepare_handoff_with(cfg, forge.as_ref(), repo, number, ledger).await
}

async fn prepare_handoff_with(
    cfg: &Config,
    forge: &dyn Forge,
    repo: &RepoRef,
    number: u64,
    ledger: &Ledger,
) -> Result<Handoff> {
    let repo_key = repo.key();
    let existing_repo_id = ledger.repo_id(&repo_key)?;
    let known_repo_id = match existing_repo_id {
        Some(repo_id) if ledger.show(repo_id, number)?.is_some() => Some(repo_id),
        _ => None,
    };
    let repo_id = if let Some(repo_id) = known_repo_id {
        repo_id
    } else {
        let repo_id = match existing_repo_id {
            Some(repo_id) => repo_id,
            None => ledger.ensure_repo(&repo_key)?,
        };
        let fetched = forge
            .fetch_pr(&repo.owner, &repo.name, number)
            .await?
            .with_context(|| format!("{} has no pull request #{number}", repo.slug()))?;
        ledger.set_label_colours(
            repo_id,
            &fetched
                .labels
                .into_iter()
                .map(|label| (label.name, label.color))
                .collect::<Vec<_>>(),
        )?;
        ledger.upsert_pr(repo_id, &fetched.pr, None)?;
        repo_id
    };
    let mut handoff = handoff_with(cfg, forge, repo, number)?;
    handoff.subject = Some(ReviewSubject {
        repo: repo_key,
        repo_id,
        number,
    });
    Ok(handoff)
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

    Ok(Handoff {
        argv,
        token,
        cwd,
        subject: None,
    })
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
    let ledger = crate::resolve::open()?;
    resolve_repo_in(config, &ledger, number)
}

fn resolve_repo_in(config: &Config, ledger: &Ledger, number: u64) -> Result<RepoRef> {
    let repos: Vec<&RepoRef> = config.repos().collect();
    if let [repo] = repos.as_slice() {
        return Ok((*repo).clone());
    }

    let found = ledger.repos_with_pr(number)?;
    let configured: Vec<_> = repos
        .into_iter()
        .filter(|repo| found.contains(&repo.key()))
        .collect();
    match configured.as_slice() {
        [repo] => Ok((*repo).clone()),
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
    use crate::fake_forge::{FakeForge, pr};
    use reviewq_ledger::{ActivityScope, Ledger, RepoKey};
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

    fn multi_config_of(dir: &Path) -> Config {
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
            [[project]]
            repos = [
              { owner = "apache", name = "airflow" },
              { owner = "astronomer", name = "astro" },
            ]
            [[project.interest]]
            labels = ["x"]

            [handoff]
            review_command = ["sh", "-c", "exit 0"]
            "#,
        )
        .expect("write config");
        crate::config::load(Some(&path)).expect("loads").config
    }

    fn named_repo(owner: &str, name: &str) -> RepoRef {
        RepoRef {
            owner: owner.into(),
            name: name.into(),
            host: "github.com".into(),
            path: None,
        }
    }

    fn store_pr(ledger: &Ledger, repo: &RepoKey, number: u64) -> RepoId {
        let repo_id = ledger.ensure_repo(repo).expect("repo");
        ledger
            .upsert_pr(repo_id, &pr(number, "2026-08-20T09:00:00Z"), None)
            .expect("PR");
        repo_id
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
            subject: None,
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

    #[test]
    fn spawning_a_handoff_returns_while_the_child_is_still_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let started = dir.path().join("started");
        let release = dir.path().join("release");
        let handoff = Handoff {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "printf started > \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.01; done".into(),
                "reviewq-test".into(),
                started.display().to_string(),
                release.display().to_string(),
            ],
            token: None,
            cwd: None,
            subject: None,
        };

        let mut running = handoff.spawn().expect("child spawns");
        for _ in 0..200 {
            if started.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let reached_block = started.exists();
        let still_running = running.try_wait().unwrap().is_none();
        std::fs::write(&release, "go").unwrap();
        let status = running.wait().unwrap();

        assert!(reached_block, "child reached its blocking point");
        assert!(still_running, "spawn did not wait");
        assert!(status.success());
    }

    #[test]
    fn a_spawn_error_is_reported_before_a_running_handoff_exists() {
        let handoff = Handoff {
            argv: vec!["/definitely/not/a/review-command".into()],
            token: None,
            cwd: None,
            subject: None,
        };

        let error = handoff.spawn().expect_err("spawn fails");

        assert!(error.to_string().contains("running"), "{error:#}");
        assert!(
            error
                .to_string()
                .contains("/definitely/not/a/review-command"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn an_unknown_pr_is_stored_before_a_successful_handoff_is_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo = repo(None);
        let forge = forge().with_fetched_labels(&[("area:task-sdk", "00ff00")]);
        let at = "2026-08-21T10:00:00Z".parse().unwrap();

        let mut handoff = prepare_handoff_with(&config, &forge, &repo, 70135, &ledger)
            .await
            .expect("prepared");
        handoff.argv = vec!["sh".into(), "-c".into(), "exit 0".into()];
        let outcome = handoff
            .run_with_record(|| {
                record_review_started_in(&ledger, handoff.subject.as_ref().expect("subject"), at)
            })
            .expect("ran");

        let repo_id = ledger.repo_id(&repo.key()).unwrap().expect("repo");
        let show = ledger.show(repo_id, 70135).unwrap().expect("durable PR");
        let events = ledger
            .activity_page(
                ActivityScope::Pr {
                    repo_id,
                    number: 70135,
                },
                None,
                10,
            )
            .unwrap()
            .events;
        assert!(outcome.status.success());
        assert_eq!(
            show.tracked_reason, None,
            "reviewing did not start tracking it"
        );
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, ActivityKind::ReviewStarted);
        assert_eq!(events[0].head_sha.as_deref(), Some("sha70135"));
    }

    #[tokio::test]
    async fn an_unknown_pr_records_against_the_configured_repo_not_a_retained_namesake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let configured = repo(None);
        let retained = RepoKey {
            host: "github.com".into(),
            owner: "retired".into(),
            name: "old-repo".into(),
        };
        let retained_id = ledger.ensure_repo(&retained).expect("retained repo");
        let at = "2026-08-21T10:00:00Z".parse().unwrap();
        ledger
            .upsert_pr(retained_id, &pr(70135, "2026-08-20T09:00:00Z"), None)
            .expect("retained PR");

        let mut handoff = prepare_handoff_with(&config, &forge(), &configured, 70135, &ledger)
            .await
            .expect("prepared");
        handoff.argv = vec!["sh".into(), "-c".into(), "exit 0".into()];
        let outcome = handoff
            .run_with_record(|| {
                record_review_started_in(&ledger, handoff.subject.as_ref().expect("subject"), at)
            })
            .expect("ran");

        let configured_id = ledger
            .repo_id(&configured.key())
            .unwrap()
            .expect("configured repo");
        let configured_events = ledger
            .activity_page(
                ActivityScope::Pr {
                    repo_id: configured_id,
                    number: 70135,
                },
                None,
                10,
            )
            .unwrap()
            .events;
        let retained_events = ledger
            .activity_page(
                ActivityScope::Pr {
                    repo_id: retained_id,
                    number: 70135,
                },
                None,
                10,
            )
            .unwrap()
            .events;
        assert!(outcome.status.success());
        assert!(
            outcome.history_error.is_none(),
            "{:#?}",
            outcome.history_error
        );
        assert_eq!(configured_events.len(), 1);
        assert_eq!(configured_events[0].kind, ActivityKind::ReviewStarted);
        assert!(retained_events.is_empty());
    }

    #[tokio::test]
    async fn multi_config_one_match_ignores_a_retired_namesake_and_records_the_handoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = multi_config_of(dir.path());
        let ledger = Ledger::open_in_memory().expect("ledger");
        let at = "2026-08-21T10:00:00Z".parse().unwrap();
        let configured = named_repo("apache", "airflow");
        let configured_id = store_pr(&ledger, &configured.key(), 70135);
        let retired = named_repo("retired", "old-repo");
        let retired_id = store_pr(&ledger, &retired.key(), 70135);

        let resolved = resolve_repo_in(&config, &ledger, 70135).expect("one configured match");
        let handoff = prepare_handoff_with(&config, &forge(), &resolved, 70135, &ledger)
            .await
            .expect("prepared");
        assert_eq!(
            handoff.target().expect("post-review refresh target"),
            (configured.key(), 70135)
        );
        let outcome = handoff
            .run_with_record(|| {
                record_review_started_in(&ledger, handoff.subject.as_ref().expect("subject"), at)
            })
            .expect("ran");

        let configured_events = ledger
            .activity_page(
                ActivityScope::Pr {
                    repo_id: configured_id,
                    number: 70135,
                },
                None,
                10,
            )
            .unwrap()
            .events;
        let retired_events = ledger
            .activity_page(
                ActivityScope::Pr {
                    repo_id: retired_id,
                    number: 70135,
                },
                None,
                10,
            )
            .unwrap()
            .events;
        assert!(outcome.status.success());
        assert!(outcome.history_error.is_none());
        assert_eq!(resolved.key(), configured.key());
        assert_eq!(configured_events.len(), 1);
        assert_eq!(configured_events[0].kind, ActivityKind::ReviewStarted);
        assert!(retired_events.is_empty());
    }

    #[test]
    fn multi_config_two_configured_matches_remain_ambiguous() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = multi_config_of(dir.path());
        let ledger = Ledger::open_in_memory().expect("ledger");
        store_pr(&ledger, &named_repo("apache", "airflow").key(), 70135);
        store_pr(&ledger, &named_repo("astronomer", "astro").key(), 70135);
        store_pr(&ledger, &named_repo("retired", "old-repo").key(), 70135);

        let error = resolve_repo_in(&config, &ledger, 70135).expect_err("ambiguous");

        assert!(
            error
                .to_string()
                .contains("tracked in more than one configured repo"),
            "{error:#}"
        );
    }

    #[test]
    fn multi_config_retired_matches_follow_the_unknown_target_rule() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = multi_config_of(dir.path());
        let ledger = Ledger::open_in_memory().expect("ledger");
        store_pr(&ledger, &named_repo("retired", "old-repo").key(), 70135);

        let error = resolve_repo_in(&config, &ledger, 70135).expect_err("unknown target");

        assert!(
            error
                .to_string()
                .contains("isn't in the ledger yet and more than one repo is configured"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_known_pr_prepares_without_writing_and_history_failure_follows_spawn() {
        use diesel::{Connection as _, connection::SimpleConnection as _};

        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);
        let database = dir.path().join("reviewq.db");
        let ledger = Ledger::open(&database).expect("ledger");
        let repo = repo(None);
        let repo_id = ledger.ensure_repo(&repo.key()).expect("repo");
        let at = "2026-08-21T10:00:00Z".parse().unwrap();
        ledger
            .upsert_pr(repo_id, &pr(70135, "2026-08-20T09:00:00Z"), None)
            .expect("known PR");
        let mut connection =
            diesel::SqliteConnection::establish(database.to_str().expect("database path"))
                .expect("write guard");
        connection
            .batch_execute(
                "CREATE TRIGGER reject_repo_write BEFORE INSERT ON repos
                 BEGIN SELECT RAISE(FAIL, 'pre-spawn writes rejected'); END;
                 CREATE TRIGGER reject_pr_insert BEFORE INSERT ON prs
                 BEGIN SELECT RAISE(FAIL, 'pre-spawn writes rejected'); END;
                 CREATE TRIGGER reject_pr_update BEFORE UPDATE ON prs
                 BEGIN SELECT RAISE(FAIL, 'pre-spawn writes rejected'); END;
                 CREATE TRIGGER reject_history_write BEFORE INSERT ON activity_events
                 BEGIN SELECT RAISE(FAIL, 'history writes rejected'); END;",
            )
            .expect("write guards");
        drop(connection);
        let started = dir.path().join("started");

        let mut handoff = prepare_handoff_with(&config, &forge(), &repo, 70135, &ledger)
            .await
            .expect("known PR prepares without a write");
        handoff.argv = vec![
            "sh".into(),
            "-c".into(),
            "printf started > \"$1\"".into(),
            "reviewq-test".into(),
            started.display().to_string(),
        ];
        let outcome = handoff
            .run_with_record(|| {
                record_review_started_in(&ledger, handoff.subject.as_ref().expect("subject"), at)
            })
            .expect("child status retained");

        assert!(outcome.status.success());
        assert!(started.exists(), "the handoff child spawned");
        let error = outcome.history_error.expect("history failure");
        assert!(format!("{error:#}").contains("history writes rejected"));
    }

    #[tokio::test]
    async fn an_unknown_pr_that_the_forge_cannot_find_never_reaches_the_handoff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = config_of(dir.path(), None);
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo = repo(None);

        let error =
            prepare_handoff_with(&config, &forge().missing_pr(70135), &repo, 70135, &ledger)
                .await
                .expect_err("missing PR");

        assert!(
            error.to_string().contains("has no pull request #70135"),
            "{error:#}"
        );
        let repo_id = ledger
            .repo_id(&repo.key())
            .unwrap()
            .expect("repo identity retained");
        assert!(ledger.show(repo_id, 70135).unwrap().is_none());
        assert!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_unknown_pr_with_an_unspawnable_handoff_has_no_started_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = config_of(dir.path(), None);
        config.handoff.review_command = vec!["/definitely/not/a/review-command".into()];
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo = repo(None);
        let at = "2026-08-21T10:00:00Z".parse().unwrap();

        let handoff = prepare_handoff_with(&config, &forge(), &repo, 70135, &ledger)
            .await
            .expect("prepared");
        handoff
            .run_with_record(|| {
                record_review_started_in(&ledger, handoff.subject.as_ref().expect("subject"), at)
            })
            .expect_err("spawn fails");

        assert!(
            ledger
                .activity_page(ActivityScope::All, None, 10)
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn a_history_failure_does_not_mask_a_successful_child_exit() {
        let handoff = Handoff {
            argv: vec!["sh".into(), "-c".into(), "exit 0".into()],
            token: None,
            cwd: None,
            subject: None,
        };

        let outcome = handoff
            .run_with_record(|| bail!("history is read-only"))
            .expect("child status retained");

        assert!(outcome.status.success());
        assert_eq!(
            outcome.history_error.expect("history warning").to_string(),
            "history is read-only"
        );
    }

    #[test]
    fn a_history_failure_does_not_mask_a_nonzero_child_exit() {
        let handoff = Handoff {
            argv: vec!["sh".into(), "-c".into(), "exit 23".into()],
            token: None,
            cwd: None,
            subject: None,
        };

        let outcome = handoff
            .run_with_record(|| bail!("history is read-only"))
            .expect("child status retained");

        assert_eq!(outcome.status.code(), Some(23));
        assert_eq!(
            outcome.history_error.expect("history warning").to_string(),
            "history is read-only"
        );
    }
}
