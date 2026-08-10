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

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use reviewq_app::config::Config;

pub use theme::{Mode, Theme};

/// Run the interface until the user quits.
///
/// Takes over the terminal — alternate screen, raw mode, mouse reporting — and
/// restores it on the way out, including when the body returns an error. Async
/// because forge work runs as tasks while the interface stays responsive.
///
/// `config` is the one the caller already loaded — or why it couldn't be, since a
/// broken config must not stop the queue being read. It is held for the session:
/// an interface that reloaded it per action could act on two different configs in
/// one sitting, and paid a file read and a parse per keystroke to do it.
pub async fn run(theme: Theme, config: Result<Config, String>) -> Result<()> {
    let config = Arc::new(config);
    let mut terminal = ratatui::init();
    // Not part of `ratatui::init`, so it is asked for and given back by hand.
    // Failing to enable it is not worth refusing to start over: the keyboard can
    // do everything the mouse can.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let outcome = match app::App::new(theme, Arc::clone(&config)) {
        Ok(mut app) => {
            let mut channel = app::Channel::new();
            app.run(&mut terminal, &mut channel, &app::Hooks::live(config))
                .await
        }
        Err(err) => Err(err),
    };
    // Before `restore`, so the sequence reaches the terminal while reviewq still
    // owns it — a shell left reporting mouse movement is a mess to get out of.
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    outcome
}
