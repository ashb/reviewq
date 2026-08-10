//! `reviewq tui`: hand over to the terminal interface.
//!
//! Config isn't read here. The TUI is ledger-only for now, like every other
//! read command, so a missing or broken config mustn't stop it opening.

use std::process::ExitCode;

use anyhow::Result;

pub async fn run() -> Result<ExitCode> {
    reviewq_tui::run(reviewq_tui::Theme::default()).await?;
    Ok(ExitCode::SUCCESS)
}
