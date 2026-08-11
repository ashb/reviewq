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
mod svg;
mod ui;

pub mod keys;

pub mod theme;

use std::io::{self, Stdout};
use std::sync::Arc;

use anyhow::Result;
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use reviewq_app::config::Config;

pub use app::{Channel, Hooks, Message, PrHook};
pub use theme::{Mode, Theme};

/// Run the interface until the user quits.
///
/// Synchronous, and knows nothing about a runtime: the loop draws, reads a key
/// and acts, and everything that could block for an unbounded time is a hook the
/// caller supplies. Whoever wants forge work off the interface's thread arranges
/// that on their side of [`Hooks`] — which is why this crate no longer depends on
/// tokio at all.
///
/// `config` is the one the caller already loaded and validated, held for the
/// session: an interface that reloaded it per action could act on two different
/// versions of the file in one sitting, and paid a file read and a parse per
/// keystroke to do it.
pub fn run(theme: Theme, config: Arc<Config>, hooks: &Hooks) -> Result<()> {
    // The guard drops at the end of this function, restoring the terminal — when
    // the body returns an error as much as when it quits normally.
    let mut guard = TerminalGuard::enter()?;
    let mut app = app::App::new(theme, config)?;
    let mut channel = Channel::new();
    app.run(&mut guard.terminal, &mut channel, hooks)
}

/// Ownership of the terminal, given back when this drops.
///
/// `ratatui::init` installs a panic hook covering raw mode and the alternate
/// screen, but knows nothing about mouse reporting, which reviewq also turns on —
/// so a panic left the shell reporting every mouse movement, which is a real mess
/// to get out of. Setup and its undo live together here instead, and both the
/// `Drop` and the panic hook go through the same one.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        take_over_terminal()?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        install_panic_hook();
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Take the terminal over: raw mode, a fresh alternate screen, mouse reporting.
///
/// Also used to take it back after a review command has had it, so it installs
/// nothing that must happen only once.
pub fn take_over_terminal() -> Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    // Mouse reporting is a nicety, so failing to get it is no reason to refuse to
    // start: every gesture has a key.
    let _ = execute!(io::stdout(), EnableMouseCapture);
    Ok(())
}

/// Undo [`take_over_terminal`]. Best-effort and idempotent, so the guard's `Drop`
/// and the panic hook need not agree on which of them runs.
///
/// Mouse reporting goes first, while reviewq still owns the screen.
pub fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

/// Hand the terminal to a review command, keeping the alternate screen.
///
/// Leaving the alternate screen would drop the terminal back to the shell's
/// scrollback for however long the command takes to draw — seconds, for one that
/// resolves a token first — and reviewq's own frame, notice and all, would vanish
/// the instant the key was pressed. Staying put leaves the notice up until the
/// command paints over it. The cost is that a handoff command which only *prints*
/// writes onto that frame and has its output wiped when reviewq repaints; the
/// configured default is a full-screen reviewer, so this trades for the common
/// case.
///
/// The cursor stays hidden, or it would blink in the middle of reviewq's frame
/// next to the notice. Mouse reporting goes with raw mode: the child asks for
/// whatever it wants and turns that off again when it exits, which would otherwise
/// leave reviewq with no mouse for the rest of the session.
pub fn lend_terminal() {
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = disable_raw_mode();
}

/// Take the terminal back after a review command has had it.
///
/// The alternate screen is re-entered because a full-screen child leaves its own
/// on the way out, which drops us to the primary one; for a child that never took
/// it over this is a no-op. `Hide` again because the command may well have shown
/// the cursor and not put it away.
pub fn reclaim_terminal() {
    let _ = enable_raw_mode();
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        crossterm::cursor::Hide
    );
}

/// Chain a panic hook that hands the terminal back before the default one runs,
/// so a panic's message and backtrace land on the primary screen the user is
/// returned to rather than on an alternate screen about to be torn down.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}
