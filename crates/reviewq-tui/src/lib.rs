//! reviewq's terminal interface.
//!
//! The CLI answers "what should I look at" one question at a time. This answers
//! it continuously: the queue on the left, everything known about the selected
//! PR on the right, and every action a keystroke away.
//!
//! Like the CLI, it reads the ledger and nothing else — the queue is whatever
//! the last sync computed, never something fetched behind your back. Syncing is
//! an explicit keystroke, and how stale the ledger is stays on screen so the
//! choice is informed.

mod app;
mod ui;

pub mod keys;

pub mod theme;

use anyhow::Result;

pub use theme::{Mode, Theme};

/// Run the interface until the user quits.
///
/// Takes over the terminal — alternate screen, raw mode — and restores it on
/// the way out, including when the body returns an error. Async because forge
/// work runs as tasks while the interface stays responsive.
pub async fn run(theme: Theme) -> Result<()> {
    let mut terminal = ratatui::init();
    let outcome = match app::App::new(theme) {
        Ok(mut app) => {
            let mut channel = app::Channel::with_input_reader();
            app.run(&mut terminal, &mut channel, &app::Hooks::live())
                .await
        }
        Err(err) => Err(err),
    };
    ratatui::restore();
    outcome
}
