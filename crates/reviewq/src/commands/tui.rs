//! `reviewq tui`: hand over to the terminal interface.
//!
//! The config is the one `dispatch` already loaded and validated, shared with the
//! interface for the session it runs.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Result;
use reviewq_app::config::Loaded;

pub async fn run(loaded: &Loaded) -> Result<ExitCode> {
    reviewq_tui::run(
        reviewq_tui::Theme::default(),
        Arc::new(loaded.config.clone()),
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}
