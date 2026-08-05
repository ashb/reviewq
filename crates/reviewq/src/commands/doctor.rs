use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_forge::{build, resolve_token};

use crate::{config, paths};

/// Report everything that has to be true before a sync can work, and exit
/// non-zero if any of it isn't.
pub async fn run(config_path: Option<&Path>) -> Result<ExitCode> {
    let loaded = config::load(config_path)?;
    if loaded.created {
        println!(
            "wrote a default config to {} — edit it before syncing",
            loaded.path.display()
        );
    }

    let mut problems = 0u32;

    row("config", &loaded.path.display().to_string());

    let db = paths::database_file()?;
    let db_note = if db.exists() {
        db.display().to_string()
    } else {
        format!("{} (not created yet)", db.display())
    };
    row("ledger", &db_note);

    let (_project, repo) = loaded.config.sole_repo()?;
    row("repo", &repo.slug());

    let host = loaded.config.forge_host_for(repo)?;
    let host_note = match &host.api_base {
        Some(api_base) => format!("{} ({api_base})", repo.host),
        None => repo.host.clone(),
    };
    row("forge", &host_note);

    let token = resolve_token(&host)?;
    row("token", &token.source.to_string());

    let forge = build(&host, &token.value)?;
    let viewer = forge.viewer().await?;
    viewer.rate_limit.trace("doctor:viewer");

    let configured = loaded.config.identity.login.trim();
    if viewer.login == configured {
        row(
            "viewer",
            &format!("{} {}", viewer.login, ok("matches identity.login")),
        );
    } else {
        problems += 1;
        row(
            "viewer",
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
        "graphql",
        &if rl.remaining < rl.limit / 10 {
            problems += 1;
            format!("{graphql} {}", warn("budget nearly exhausted"))
        } else {
            graphql
        },
    );

    match forge.rest_core_remaining().await {
        Ok((remaining, limit)) => row("rest", &format!("{remaining}/{limit} core requests")),
        Err(err) => {
            problems += 1;
            row("rest", &warn(&format!("rate limit unavailable: {err}")));
        }
    }

    if problems > 0 {
        eprintln!("\n{problems} problem(s) found");
        return Ok(ExitCode::FAILURE);
    }
    Ok(ExitCode::SUCCESS)
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
