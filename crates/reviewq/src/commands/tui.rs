//! `reviewq tui`: hand over to the terminal interface.
//!
//! Config is read here, once, and handed over for the session — but a failure to
//! read it is passed along rather than raised. The queue itself is ledger-only,
//! like every other read command, so a missing or broken config must not stop the
//! interface opening; what it stops is the actions that reach the forge, each of
//! which says so when pressed.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

pub async fn run(config_path: Option<&Path>) -> Result<ExitCode> {
    let config = reviewq_app::config::load(config_path)
        .map(|loaded| loaded.config)
        .map_err(|err| format!("{err:#}"));
    reviewq_tui::run(reviewq_tui::Theme::default(), config).await?;
    Ok(ExitCode::SUCCESS)
}
