use std::process::ExitCode;

use anyhow::Result;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_forge::{build, resolve_token};
use reviewq_ledger::Ledger;

use reviewq_app::config::{self, Loaded};
use reviewq_app::paths;
use reviewq_app::sync::{CURSOR_KEY, TRUNCATED_KEY};

/// Report everything that has to be true before a sync can work, and exit
/// non-zero if any of it isn't.
pub async fn run(loaded: &Loaded) -> Result<ExitCode> {
    let mut problems = 0u32;

    row("config", &loaded.path.display().to_string());

    let db = paths::database_file()?;
    // `Ledger::open` would create an empty file, so a ledger that isn't there
    // yet is reported as a note, never opened.
    let ledger = db.exists().then(|| Ledger::open(&db)).transpose()?;
    let db_note = if db.exists() {
        db.display().to_string()
    } else {
        format!("{} (not created yet)", db.display())
    };
    row("ledger", &db_note);
    row("handoff", &handoff_note(&loaded.config, &mut problems));

    for repo in loaded.config.repos() {
        row("repo", &repo.slug());
        row("  checkout", &checkout_note(repo, &mut problems));

        row(
            "  last sync",
            &last_sync_note(ledger.as_ref(), &repo.key(), &mut problems)?,
        );

        let host = loaded.config.forge_host_for(&repo.host)?;
        let host_note = match &host.api_base {
            Some(api_base) => format!("{} ({api_base})", repo.host),
            None => repo.host.clone(),
        };
        row("  forge", &host_note);

        let token = resolve_token(&host)?;
        row("  token", &token.source.to_string());

        // Handed the token this step just resolved, rather than letting the
        // adapter resolve its own: `doctor` reports on that step, and a second
        // resolution would mean a second credential-helper prompt for one run.
        let forge = build(&host, &repo.host, Some(token))?;
        let viewer = forge.viewer().await?;
        viewer.rate_limit.trace("doctor:viewer");

        let configured = loaded.config.identity.login.trim();
        if viewer.login == configured {
            row(
                "  viewer",
                &format!("{} {}", viewer.login, ok("matches identity.login")),
            );
        } else {
            problems += 1;
            row(
                "  viewer",
                &format!(
                    "{} {}",
                    viewer.login,
                    warn(&format!("but identity.login is {configured}"))
                ),
            );
        }

        let rl = &viewer.rate_limit;
        let graphql = format!(
            "{}/{} points, resets {}",
            rl.remaining, rl.limit, rl.reset_at
        );
        row(
            "  graphql",
            &if rl.remaining < rl.limit / 10 {
                problems += 1;
                format!("{graphql} {}", warn("budget nearly exhausted"))
            } else {
                graphql
            },
        );

        match forge.rest_core_remaining().await {
            Ok((remaining, limit)) => row("  rest", &format!("{remaining}/{limit} core requests")),
            Err(err) => {
                problems += 1;
                row("  rest", &warn(&format!("rate limit unavailable: {err}")));
            }
        }
    }

    if problems > 0 {
        eprintln!("\n{problems} problem(s) found");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
}

/// When the last sweep completed and whether it hit the search cap — a capped
/// sweep means some PRs in that window were silently missed, so it counts as
/// a problem rather than just a note. `None` when the ledger doesn't exist
/// yet — nothing has ever synced.
fn last_sync_note(
    ledger: Option<&Ledger>,
    repo: &reviewq_ledger::RepoKey,
    problems: &mut u32,
) -> Result<String> {
    let Some(ledger) = ledger else {
        return Ok("never".to_string());
    };
    let repo_id = ledger.ensure_repo(repo)?;
    let Some(at) = ledger.get_meta(repo_id, CURSOR_KEY)? else {
        return Ok("never".to_string());
    };
    if ledger.get_meta(repo_id, TRUNCATED_KEY)?.as_deref() == Some("1") {
        *problems += 1;
        Ok(format!(
            "{at} {}",
            warn("last sweep hit the search cap — some PRs were missed")
        ))
    } else {
        Ok(at)
    }
}

/// The review command, and whether it can actually name a PR.
///
/// A `{number}` with no `{url}` only works run from inside a checkout of the
/// right repo — the handoff tool has nothing else to resolve the number against,
/// and reports something like "the repository has no remotes". reviewq itself
/// runs from anywhere, and its TUI usually runs from wherever you happened to
/// open it, so this is a trap worth naming rather than leaving to be discovered
/// when a review fails.
///
/// A config written before `{url}` existed still says `{number}`, which is the
/// common way to end up here: the default changed, existing files didn't.
fn handoff_note(config: &config::Config, problems: &mut u32) -> String {
    let argv = &config.handoff.review_command;
    let shown = argv.join(" ");
    let mentions = |token: &str| argv.iter().any(|arg| arg.contains(token));
    // Naming a checkout is the other way a bare number resolves: the handoff runs
    // in that directory, so the tool can read the number against its remote.
    let all_have_checkouts = config.repos().all(|repo| repo.path.is_some());

    if mentions("{number}") && !mentions("{url}") && !all_have_checkouts {
        *problems += 1;
        format!(
            "{shown} {}",
            warn(
                "substitutes {number} but not {url} — that only resolves inside a \
                 checkout, and not every repo sets `path`"
            )
        )
    } else {
        shown
    }
}

/// Where a review of this repo runs.
///
/// Nothing in a sync reads a working tree, so this is not required to load — but
/// the handoff runs in it, and a review tool that publishes back needs it: wiff
/// will not publish a review it mirrored by URL from outside the repository, and
/// says so only *after* you have written the review. That's a bad moment to find
/// out, so a repo without one counts as a problem here rather than being noted in
/// passing.
fn checkout_note(repo: &config::RepoRef, problems: &mut u32) -> String {
    match &repo.path {
        Some(path) => path.display().to_string(),
        None => {
            *problems += 1;
            warn(
                "none — set `path` to the local checkout, or a review cannot be \
                 published back to the PR",
            )
        }
    }
}

fn row(label: &str, value: &str) {
    println!(
        "{:<10} {value}",
        label.if_supports_color(Stdout, |l| l.dimmed())
    );
}

fn ok(text: &str) -> String {
    format!("{}", text.if_supports_color(Stdout, |t| t.green()))
}

fn warn(text: &str) -> String {
    format!("{}", text.if_supports_color(Stdout, |t| t.yellow()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(review_command: &[&str]) -> config::Config {
        let mut config: config::Config =
            toml::from_str(config::DEFAULT_CONFIG).expect("default config parses");
        config.handoff.review_command = review_command.iter().map(|s| (*s).to_string()).collect();
        config
    }

    #[test]
    fn a_url_handoff_is_reported_without_comment() {
        let mut problems = 0;
        let note = handoff_note(
            &config_with(&["wiff", "forge", "pull", "{url}"]),
            &mut problems,
        );
        assert_eq!(note, "wiff forge pull {url}");
        assert_eq!(problems, 0);
    }

    #[test]
    fn a_number_only_handoff_is_flagged() {
        // The trap: it works from inside the right checkout and nowhere else, and
        // a config written before `{url}` existed still looks like this.
        let mut problems = 0;
        let note = handoff_note(
            &config_with(&["wiff", "forge", "pull", "{number}"]),
            &mut problems,
        );
        assert!(note.contains("only resolves inside a checkout"), "{note}");
        assert_eq!(
            problems, 1,
            "it should count against a clean bill of health"
        );
    }

    #[test]
    fn a_handoff_using_both_is_fine() {
        let mut problems = 0;
        let note = handoff_note(
            &config_with(&["review", "--id", "{number}", "--url", "{url}"]),
            &mut problems,
        );
        assert!(!note.contains("only resolves"), "{note}");
        assert_eq!(problems, 0);
    }

    #[test]
    fn a_number_only_handoff_is_fine_once_every_repo_names_a_checkout() {
        // The handoff runs in the checkout, so the tool has a remote to read the
        // number against — which is the whole reason `path` exists.
        let mut config = config_with(&["wiff", "forge", "pull", "{number}"]);
        for project in &mut config.projects {
            for repo in &mut project.repos {
                repo.path = Some(std::path::PathBuf::from("/somewhere/airflow"));
            }
        }
        let mut problems = 0;

        let note = handoff_note(&config, &mut problems);

        assert_eq!(note, "wiff forge pull {number}");
        assert_eq!(problems, 0);
    }

    #[test]
    fn a_repo_with_no_checkout_is_counted_as_a_problem() {
        let repo = config::RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: None,
        };
        let mut problems = 0;

        let note = checkout_note(&repo, &mut problems);

        assert!(note.contains("set `path`"), "{note}");
        assert_eq!(
            problems, 1,
            "it should count against a clean bill of health"
        );
    }

    #[test]
    fn a_repo_with_a_checkout_reports_the_path_and_is_no_problem() {
        let repo = config::RepoRef {
            owner: "apache".into(),
            name: "airflow".into(),
            host: "github.com".into(),
            path: Some(std::path::PathBuf::from("/home/ash/code/airflow")),
        };
        let mut problems = 0;

        assert_eq!(
            checkout_note(&repo, &mut problems),
            "/home/ash/code/airflow"
        );
        assert_eq!(problems, 0);
    }

    #[test]
    fn a_handoff_naming_neither_is_left_alone() {
        // Someone's wrapper script may resolve the PR itself; not our business.
        let mut problems = 0;
        let note = handoff_note(&config_with(&["my-review-script"]), &mut problems);
        assert_eq!(note, "my-review-script");
        assert_eq!(problems, 0);
    }
}
