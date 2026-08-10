//! What's on screen and what the keys do.
//!
//! Ledger reads stay synchronous: they're sub-millisecond against local SQLite,
//! so making them async would buy nothing and cost a shared `Connection` (which
//! is `Send` but not `Sync`) held across await points.
//!
//! Anything touching the network does not. A forge round trip is unbounded — a
//! slow response must never stop the queue scrolling or `q` working — so it runs
//! as a task and reports back. Its own [`Ledger`] handle writes while this one
//! reads, which is what the ledger's WAL mode is for.
//!
//! The loop reads input itself, and takes finished work off a channel between
//! keystrokes. It does *not* use a thread for input: a background reader would
//! still be sitting in `event::read` while a review command had the terminal,
//! stealing that program's keystrokes and swallowing the terminal's reply to the
//! cursor-position query `Terminal::clear` makes — which surfaces as "the cursor
//! position could not be read".
//!
//! Both input and the side effects are handed in as [`Hooks`], so a test can
//! drive the real loop from a script with no terminal and no forge.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Position, Rect};
use reviewq_app::config::Config;
use reviewq_app::sync::Refreshed;
use reviewq_ledger::{Ledger, LedgerError, Located, PrShow, QueueItem, RepoKey};
use std::sync::mpsc;

#[cfg(test)]
/// A minimal valid config naming the fixture's repo.
///
/// Parsed rather than built field by field, so it goes through the same
/// deserialisation and validation a real one does.
pub(crate) fn test_config() -> HeldConfig {
    Arc::new(
        toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            [[project.interest]]
            labels = ["area:async"]
            "#,
        )
        .expect("test config parses"),
    )
}

/// What to put in the header when refreshing a PR failed.
///
/// The two cases a reader can act on are named as themselves rather than left in
/// the message: a ledger from a newer build needs reviewq upgraded, and a busy one
/// needs nothing but patience. Everything else is reported as it arrives, since
/// guessing at its shape would be worse than quoting it.
fn failure_note(number: u64, err: &anyhow::Error) -> String {
    match err.downcast_ref::<LedgerError>() {
        Some(LedgerError::FromTheFuture) => {
            "this ledger was written by a newer reviewq — upgrade to read it".to_string()
        }
        Some(LedgerError::Busy { .. }) => {
            format!("#{number} is waiting on another reviewq's write — try again")
        }
        _ => format!("#{number} refresh failed: {err:#}"),
    }
}

/// Refuse, in a test build, the paths that reach what the developer actually uses.
///
/// The real ledger is opened — and created — by `App::new`, and the live hooks
/// reach it, the forge, and the configured review command. A test has no business
/// in any of that, so arriving here is a bug in the test: it fails loudly instead
/// of quietly working against real data. Compiled away entirely in a release
/// build.
#[cfg_attr(not(test), expect(unused_variables))]
fn forbid_in_tests(what: &str) {
    #[cfg(test)]
    panic!("a test reached {what}");
}

/// Rows the wheel scrolls the detail pane per notch.
///
/// More than one, because a PR description is long and a terminal sends one event
/// per notch. The queue moves a single row instead: each step there reloads the
/// selected PR's detail, so three at a time would read two of them for nothing.
const WHEEL_ROWS: isize = 3;

use crate::keys::{self, Action};
use crate::theme::Theme;
use crate::ui;

/// Everything on screen, plus the ledger handle it was read from.
pub struct App {
    /// The palette, resolved once at startup.
    pub theme: Theme,
    /// The queue, most-urgent first, spanning every repo the ledger knows.
    pub queue: Vec<Located<QueueItem>>,
    /// Index into [`queue`](Self::queue) of the highlighted row. Always a valid
    /// index when the queue is non-empty; meaningless when it's empty.
    pub selected: usize,
    /// Full detail for the selected PR, re-read whenever the selection moves.
    /// `None` when the queue is empty, or when the row somehow has no stored
    /// detail.
    pub detail: Option<PrShow>,
    /// How many repos the ledger knows about. More than one, and rows carry
    /// `owner/name#N` rather than a bare `#N` — matching what `list` does.
    pub repo_count: usize,
    /// Which pane the movement keys act on.
    pub focus: Focus,
    /// First visible line of the detail pane. A PR description easily outruns
    /// the pane, so it scrolls independently of the queue's selection.
    pub detail_scroll: u16,
    /// What is covering the panes, if anything. While something is, it owns the
    /// keyboard.
    pub overlay: Overlay,
    /// A one-line note in the header: what is happening, or what just did.
    pub status: Option<String>,
    /// First visible row of the queue.
    ///
    /// Held rather than derived from the selection: deriving it is what made
    /// moving up off the last row scroll the whole list, because the selection
    /// was pinned to whichever edge it had reached.
    pub queue_scroll: usize,
    /// A PR whose handoff has been asked for but not yet run, so the loop can
    /// draw the notice first.
    pending_review: Option<u64>,
    /// Likewise for a PR to fetch, which is also a network call worth announcing
    /// before it blocks.
    pending_fetch: Option<u64>,
    /// A PR whose forge notifications should be marked read, recorded once the
    /// local `done` is committed. Fire-and-forget, so the loop needs no draw
    /// first — it is held only so that acting on an overlay's keys needs no
    /// hooks.
    pending_mark_read: Option<u64>,
    /// The screen is not to be trusted — something else had the terminal, or the
    /// window resized — so the next draw must repaint every cell rather than only
    /// what changed.
    repaint: bool,
    /// Something changed, so the screen no longer matches the state.
    ///
    /// Held rather than assumed: the input poll wakes on a timer whether or not
    /// anything happened, and an interface that redrew on each of those wakeups
    /// re-parsed the selected PR's description as markdown ten times a second
    /// while sitting idle.
    dirty: bool,
    /// How far the key reference can scroll before its last row is on screen.
    /// Written by the renderer, which is the only thing that knows how much of it
    /// fits.
    help_max_scroll: u16,
    /// PRs with a refresh in flight. Keyed by number so pressing `r` twice on
    /// one PR doesn't fetch it twice, while two different PRs can refresh at
    /// once.
    pub refreshing: BTreeSet<u64>,
    /// How many rows the focused pane last displayed, so a paging key moves by a
    /// screenful rather than a guessed constant. Written by the renderer, which
    /// is the only thing that knows the laid-out height; 1 until the first draw.
    page: usize,
    /// Lines the detail pane last needed, so scrolling can stop at the end
    /// rather than running off into blank space. Also written by the renderer.
    detail_lines: usize,
    /// The session's config. Read here only to resolve a pasted URL — which
    /// host's layout to read it with.
    config: HeldConfig,
    /// Where the queue's rows and the detail's text last landed on screen, so a
    /// click can be turned back into the row under the pointer. Written by the
    /// renderer, which is the only thing that knows where the layout put them;
    /// empty until the first draw, which makes every hit test miss.
    queue_area: Rect,
    detail_area: Rect,
    ledger: Ledger,
    quit: bool,
}

/// Both ends of the loop's channel.
///
/// The loop needs the receiver to wait on and the sender to hand to a task, so
/// they travel together rather than as two arguments that must match.
pub struct Channel {
    /// Cloned into each task so it can report back.
    pub tx: mpsc::Sender<Message>,
    /// What the loop waits on.
    pub rx: mpsc::Receiver<Message>,
}

impl Default for Channel {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel {
    /// A fresh channel. Only tasks send on it — input is read by the loop
    /// itself, so that nothing is holding stdin when a review command wants it.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

/// A side effect that needs a PR's full identity — its repo as well as its
/// number — because it has to reach the forge the PR lives on.
pub type PrHook = Box<dyn Fn(&RepoKey, u64) -> Result<()> + Send + Sync>;

/// The session's config: loaded and validated once by the caller, shared with
/// every hook that needs it.
///
/// `Arc` because the closures that reach the forge run on the blocking pool and
/// so must own what they capture.
pub type HeldConfig = Arc<Config>;

/// The side effects the loop performs, as functions it is given.
///
/// Injected rather than called directly so the loop can be driven in a test
/// without a forge, a token or a config — the same reason wiff hands its own
/// loop a struct of hooks. It also keeps `App` unaware of how a refresh is
/// actually run.
pub struct Hooks {
    /// Wait briefly for a terminal event, returning `None` if none arrived.
    ///
    /// Injected for the same reason the rest are: it lets a test drive the real
    /// loop from a script. It also means input is read here, by the loop, and
    /// never by a thread of its own — a background reader would still be sitting
    /// in `read` while a review command had the terminal, stealing that program's
    /// keystrokes and swallowing the terminal's reply to the cursor-position
    /// query `Terminal::clear` makes.
    pub next_event: Box<dyn Fn() -> Result<Option<Event>> + Send + Sync>,
    /// Begin refreshing a PR. Must not block: whatever it starts is expected to
    /// report back as [`Message::Refreshed`] eventually, or never.
    pub refresh: Box<dyn Fn(u64, mpsc::Sender<Message>) + Send + Sync>,
    /// Tell the forge a PR's notifications are read. Fire-and-forget: `done` has
    /// already been recorded locally by the time this runs, and nothing waits on
    /// it, so a failure is logged and no more.
    pub mark_read: Box<dyn Fn(u64) + Send + Sync>,
    /// Fetch a PR the ledger has never seen and start tracking it. Blocks, like
    /// the handoff, because the interface has nothing to show until it returns
    /// and the notice explains the wait.
    pub fetch: Box<dyn Fn(u64) -> Result<()> + Send + Sync>,
    /// Show a PR's page in whatever the desktop opens URLs with.
    ///
    /// Takes the PR rather than a URL because working out the URL is itself
    /// config work — which host, whose layout — and that belongs on this side of
    /// the seam with every other config touch, not in `App`.
    pub open_url: PrHook,
    /// Put a PR's URL on the clipboard, resolved the same way.
    pub copy_url: PrHook,
    /// Hand a PR to the configured review command, giving the terminal back for
    /// its duration and taking it over again afterwards.
    ///
    /// Blocks on purpose — the reviewer is *in* that program, and a queue
    /// redrawing underneath it would be nonsense. It's a hook partly so a test
    /// can stand in for it, and partly because suspending is the one thing the
    /// loop cannot do generically: it belongs to the real backend, not to
    /// `TestBackend`.
    pub review: Box<dyn Fn(u64) -> Result<()> + Send + Sync>,
}

/// Whether [`App::update`] dealt with an action, or handed it back.
///
/// The state machine owns everything it can do with nothing but itself. The rest
/// — a forge round trip, a handoff, a clipboard — needs what only the loop has,
/// so it comes back out. Before this they were separated by a comment and an arm
/// returning `Ok(())`, which made forgetting the second half a silent no-op: the
/// key would appear in the reference and do nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum Update {
    /// Done; the state has moved on.
    Handled,
    /// Not mine — perform it where the hooks are.
    Passed(Action),
}

/// What is covering the panes.
///
/// Several actions need a follow-up keystroke — a confirmation, a duration — so
/// this is a small state machine rather than a flag. Whichever variant is up owns
/// the keyboard, which is why the bindings table does not describe their keys:
/// each overlay says on screen what it accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Overlay {
    /// The panes have the keyboard.
    #[default]
    None,
    /// Handing the terminal over to the review command.
    ///
    /// Drawn before the handoff rather than after: the command may take a moment
    /// to appear — resolving a token, starting up — and an unexplained pause with
    /// reviewq's own frame still on screen reads as a hang.
    Launching {
        /// The PR being handed off.
        number: u64,
    },
    /// The key reference, scrolled to this row. It outgrew a short terminal as
    /// soon as there were actions worth listing, so it scrolls rather than
    /// silently truncating.
    Help {
        /// First visible row of the reference.
        scroll: u16,
    },
    /// Confirming a `done`, which is the one action worth a second thought: it
    /// is the most-pressed, and clears reasons that only a sync brings back.
    ConfirmDone {
        /// The PR it would mark done.
        number: u64,
    },
    /// Picking a snooze duration from presets.
    SnoozePresets {
        /// The PR it would snooze.
        number: u64,
    },
    /// A PR asked for by number that the ledger has never seen — offering to
    /// fetch it rather than refusing.
    ///
    /// A number you typed is a number you meant, and it not being here is more
    /// often "my sweep hasn't reached it" than a mistake. Offering the fetch
    /// turns a dead end into the thing you were going to do next anyway.
    OfferFetch {
        /// The PR to fetch.
        number: u64,
    },
    /// Fetching a PR the ledger had never seen.
    Fetching {
        /// The PR being fetched.
        number: u64,
    },
    /// Typing a PR number to jump to.
    JumpPrompt {
        /// What has been typed so far.
        input: String,
        /// Why the last attempt didn't land, shown under the field.
        error: Option<String>,
    },
    /// Typing a snooze duration, for one the presets don't cover.
    SnoozePrompt {
        /// The PR it would snooze.
        number: u64,
        /// What has been typed so far.
        input: String,
        /// Why the last attempt was rejected, shown under the field.
        error: Option<String>,
    },
}

/// The snooze presets, in the order they're listed and the key that picks each.
pub(crate) const SNOOZE_PRESETS: &[(char, &str, &str)] = &[
    ('1', "1d", "tomorrow"),
    ('3', "3d", "3 days"),
    ('7', "1w", "a week"),
    ('4', "4w", "4 weeks"),
];

/// Something the loop should wake up for.
///
/// Input and finished work share one channel so the loop is a single `recv`:
/// both are just reasons to update state and redraw.
pub enum Message {
    /// A refresh task finished, for better or worse.
    Refreshed {
        /// The PR it was refreshing.
        number: u64,
        /// What came back.
        outcome: Result<Refreshed>,
    },
}

/// How many rows of context to keep between the selection and an edge before the
/// list starts scrolling. Vim calls this `scrolloff`; three is its common value
/// and enough to see what's coming without the list moving under every keypress.
const SCROLLOFF: usize = 3;

/// Which pane has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Movement keys change the selected PR.
    #[default]
    Queue,
    /// Movement keys scroll the description.
    Detail,
}

impl App {
    /// Open the ledger and load the queue.
    pub fn new(theme: Theme, config: HeldConfig) -> Result<Self> {
        forbid_in_tests("App::new, which opens the real ledger — use with_ledger");
        let path = reviewq_app::paths::database_file()?;
        let ledger = Ledger::open(&path)
            .with_context(|| format!("opening the ledger at {}", path.display()))?;
        Self::with_ledger(theme, ledger, config)
    }

    /// Build over an already-open ledger, so a test can render against a
    /// fixture rather than whatever this machine happens to have synced.
    pub(crate) fn with_ledger(theme: Theme, ledger: Ledger, config: HeldConfig) -> Result<Self> {
        let mut app = Self {
            theme,
            queue: Vec::new(),
            selected: 0,
            detail: None,
            repo_count: 0,
            focus: Focus::default(),
            detail_scroll: 0,
            overlay: Overlay::None,
            queue_scroll: 0,
            pending_review: None,
            pending_fetch: None,
            pending_mark_read: None,
            repaint: false,
            dirty: true,
            help_max_scroll: 0,
            status: None,
            refreshing: BTreeSet::new(),
            page: 1,
            detail_lines: 0,
            queue_area: Rect::ZERO,
            detail_area: Rect::ZERO,
            config,
            ledger,
            quit: false,
        };
        app.reload()?;
        Ok(app)
    }

    /// Re-read the queue from the ledger, keeping the selection on the same PR
    /// where it still exists.
    ///
    /// Identity is `(repo, number)` rather than the row index, because an action
    /// that writes to the ledger can drop a PR off the queue or change its
    /// urgency, and holding the index would silently move the selection to an
    /// unrelated PR. Only the initial load calls this so far, where there is no
    /// selection to keep — it is written this way for the actions that will.
    fn reload(&mut self) -> Result<()> {
        let held = self
            .current()
            .map(|item| (item.repo.clone(), item.item.pr.number));
        self.repo_count = self.ledger.repos()?.len();
        self.queue = self.ledger.queue_all()?;
        self.selected = held
            .and_then(|(repo, number)| {
                self.queue
                    .iter()
                    .position(|i| i.repo == repo && i.item.pr.number == number)
            })
            .unwrap_or(self.selected)
            .min(self.queue.len().saturating_sub(1));
        self.load_detail()
    }

    /// Read the selected PR's full detail.
    ///
    /// The row carries its own `repo_id`, so moving the selection stays a read.
    fn load_detail(&mut self) -> Result<()> {
        self.detail = match self.current() {
            None => None,
            Some(item) => self.ledger.show(item.repo_id, item.item.pr.number)?,
        };
        Ok(())
    }

    /// The highlighted queue row, or `None` when the queue is empty.
    pub fn current(&self) -> Option<&Located<QueueItem>> {
        self.queue.get(self.selected)
    }

    /// Record how many rows the queue pane can show. Called by the renderer
    /// once the layout is decided. Never zero, so a paging key on a terminal
    /// too short to show anything still moves by one row instead of stalling.
    pub(crate) fn set_page(&mut self, rows: usize) {
        self.page = rows.max(1);
        // A resize changes what "near the edge" means, so the window may need
        // moving even though the selection hasn't.
        self.keep_selection_visible();
    }

    /// Move the queue's window only when the selection gets near an edge.
    ///
    /// Within [`SCROLLOFF`] rows of the top or bottom the list scrolls to keep
    /// that much context ahead; anywhere else in the window the selection moves
    /// alone. Deriving the window from the selection instead — pinning it to
    /// whichever edge it had reached — meant that from the last row, moving up
    /// scrolled the whole list at the same time, which reads as two things
    /// happening for one keypress.
    ///
    /// The margin shrinks on a pane too short to honour it, and the ends of the
    /// list win over it: there is nothing above row zero to keep in view.
    fn keep_selection_visible(&mut self) {
        let page = self.page;
        let margin = SCROLLOFF.min(page.saturating_sub(1) / 2);
        let top = self.queue_scroll;

        if self.selected < top + margin {
            self.queue_scroll = self.selected.saturating_sub(margin);
        } else if self.selected + margin >= top + page {
            self.queue_scroll = (self.selected + margin + 1).saturating_sub(page);
        }
        // Never past the end: a short queue, or one that shrank under an action,
        // would otherwise leave the window looking at nothing.
        self.queue_scroll = self.queue_scroll.min(self.queue.len().saturating_sub(page));
    }

    /// Rows a paging key moves by — whatever the last render measured.
    pub(crate) fn page(&self) -> usize {
        self.page
    }

    /// Record how far the key reference can usefully scroll, and hold it there.
    /// Called by the renderer, which knows both how many rows it has and how
    /// many fit.
    pub(crate) fn set_help_max_scroll(&mut self, max: u16) {
        self.help_max_scroll = max;
        if let Overlay::Help { scroll } = self.overlay {
            self.overlay = Overlay::Help {
                scroll: scroll.min(max),
            };
        }
    }

    /// Record how many lines the detail pane's content came to, so scrolling
    /// can stop at the last line instead of running into empty space.
    pub(crate) fn set_detail_lines(&mut self, lines: usize) {
        self.detail_lines = lines;
        self.clamp_detail_scroll();
    }

    /// Record where the two panes' contents were drawn, so a mouse position can
    /// be resolved to the row under it. The areas inside the borders, not the
    /// panes: a click on a border is on no row.
    pub(crate) fn set_pane_areas(&mut self, queue: Rect, detail: Rect) {
        self.queue_area = queue;
        self.detail_area = detail;
    }

    /// Hold the detail scroll within its content. Re-applied after a render
    /// because the content's length isn't known until then — a narrower pane
    /// wraps to more lines, a shorter description to fewer.
    fn clamp_detail_scroll(&mut self) {
        let last = self
            .detail_lines
            .saturating_sub(self.page)
            .try_into()
            .unwrap_or(u16::MAX);
        self.detail_scroll = self.detail_scroll.min(last);
    }

    /// Draw, wait for something to happen, repeat, until the user quits.
    ///
    /// Generic over the backend and handed its channel and side effects rather
    /// than creating them, so a test can drive the real loop against a
    /// `TestBackend` with a scripted sequence of messages and no network.
    pub fn run<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        channel: &mut Channel,
        hooks: &Hooks,
    ) -> Result<()>
    where
        B: Backend,
        // ratatui 0.30 gives each backend its own error type; `anyhow` needs it
        // to be a sendable error before `?` will take it.
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        // The opening frame: nothing has happened yet, but there is a queue to
        // show.
        self.dirty = true;
        while !self.quit {
            // Something else had the terminal, so ratatui's record of what is on
            // screen is stale: clear it, or its diffing renderer will leave
            // whatever the other program drew.
            if std::mem::take(&mut self.repaint) {
                terminal.clear()?;
                self.dirty = true;
            }
            // Only when something changed. The poll below returns every
            // `POLL_INTERVAL` whether or not anything arrived, and drawing on each
            // of those meant re-laying out the queue and re-parsing the selected
            // PR's description as markdown ten times a second, for as long as the
            // interface was open.
            if std::mem::take(&mut self.dirty) {
                terminal.draw(|frame| ui::draw(frame, self))?;
            }

            // A handoff runs here rather than in `dispatch`, because the draw
            // above is what puts "launching" on screen before the terminal is
            // given away — 1Password may prompt, and an unexplained pause with
            // reviewq's last frame still showing is alarming.
            if let Some(number) = self.pending_review.take() {
                self.hand_off(number, channel, hooks);
                continue;
            }
            if let Some(number) = self.pending_fetch.take() {
                self.fetch_unknown(number, hooks);
                continue;
            }
            // Nothing waits on this one, so it needs no draw of its own.
            if let Some(number) = self.pending_mark_read.take() {
                (hooks.mark_read)(number);
            }

            // Results from tasks first, and without waiting: they may be already
            // queued, and a keystroke should not have to arrive to reveal them.
            while let Ok(message) = channel.rx.try_recv() {
                let Message::Refreshed { number, outcome } = message;
                self.on_refreshed(number, outcome);
                // A refresh landing is a change like any other, and the one that
                // arrives without anybody pressing a key.
                self.dirty = true;
            }
            if self.quit {
                break;
            }

            match (hooks.next_event)()? {
                // Windows reports press and release; acting on both double-fires.
                Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    self.dispatch(key, channel, hooks)?;
                    // Every key is treated as having changed something. A few
                    // haven't — an unbound one, or `k` on the top row — and
                    // redrawing an identical frame costs a diff that finds
                    // nothing, which is far cheaper than working out per key
                    // whether it mattered and being wrong once.
                    self.dirty = true;
                }
                Some(Event::Mouse(mouse)) => {
                    self.on_mouse(mouse)?;
                    self.dirty = true;
                }
                // A resize invalidates the whole layout; ratatui's own record of
                // the screen is sized, so this needs a clear as well as a draw.
                Some(Event::Resize(_, _)) => self.repaint = true,
                // Nothing arrived, so there is nothing new to show.
                Some(_) | None => {}
            }
        }
        Ok(())
    }

    /// Route a keystroke.
    ///
    /// An overlay that is up gets it raw, because what a key means there is the
    /// overlay's business — `3` picks a duration, `y` confirms — and none of that
    /// belongs in a table of global bindings. Otherwise it resolves to an
    /// [`Action`]: the ones with side effects are performed here, where the hooks
    /// and the ledger are, and the rest go to [`update`](Self::update).
    fn dispatch(&mut self, key: KeyEvent, channel: &Channel, hooks: &Hooks) -> Result<()> {
        if self.overlay != Overlay::None {
            return self.on_overlay_key(key);
        }
        let Some(action) = keys::action_for(key) else {
            return Ok(());
        };
        // Whatever `update` doesn't own comes back here to be performed. Matched
        // exhaustively and with no catch-all: an action added to the table and
        // forgotten in both places fails to compile, where before it would have
        // been advertised in the key reference and quietly done nothing.
        match self.update(action)? {
            Update::Handled => Ok(()),
            Update::Passed(Action::RefreshSelected) => {
                if let Some(number) = self.refresh_target() {
                    (hooks.refresh)(number, channel.tx.clone());
                }
                Ok(())
            }
            Update::Passed(Action::Review) => {
                if let Some(number) = self.selected_number() {
                    // Held for the loop to perform after one more draw, so the
                    // notice is up before the terminal is handed over.
                    self.overlay = Overlay::Launching { number };
                    self.pending_review = Some(number);
                }
                Ok(())
            }
            Update::Passed(Action::Done) => {
                if let Some(number) = self.selected_number() {
                    self.overlay = Overlay::ConfirmDone { number };
                }
                Ok(())
            }
            Update::Passed(Action::Snooze) => {
                if let Some(number) = self.selected_number() {
                    self.overlay = Overlay::SnoozePresets { number };
                }
                Ok(())
            }
            Update::Passed(Action::OpenInBrowser) => self.open_selected(hooks),
            Update::Passed(Action::CopyUrl) => self.copy_selected_url(hooks),
            Update::Passed(Action::ToggleMute) => self.toggle_mute(),
            Update::Passed(Action::ToggleDefer) => self.toggle_defer(),
            // Everything else `update` handles itself, and says so.
            Update::Passed(
                Action::Quit
                | Action::Help
                | Action::Jump
                | Action::SwitchPane
                | Action::ToggleTheme
                | Action::Down
                | Action::Up
                | Action::PageDown
                | Action::PageUp
                | Action::First
                | Action::Last,
            ) => unreachable!("update handles these and returns Handled"),
        }
    }

    /// The selected PR's number, or `None` on an empty queue.
    fn selected_number(&self) -> Option<u64> {
        self.current().map(|item| item.item.pr.number)
    }

    /// The `repo_id` the selected PR belongs to, as the queue read reported it.
    fn selected_repo_id(&self) -> Option<i64> {
        self.current().map(|item| item.repo_id)
    }

    /// Open the selected PR in a browser, reporting either way in the header.
    ///
    /// A failure is a status line rather than an error out of the loop: the
    /// browser not opening is no reason for the queue to stop.
    fn open_selected(&mut self, hooks: &Hooks) -> Result<()> {
        let Some((repo, number)) = self.selected_pr() else {
            return Ok(());
        };
        self.status = Some(match (hooks.open_url)(&repo, number) {
            Ok(()) => format!("#{number} opened"),
            Err(err) => format!("#{number} could not be opened: {err:#}"),
        });
        Ok(())
    }

    /// Put the selected PR's URL on the clipboard, reporting either way.
    fn copy_selected_url(&mut self, hooks: &Hooks) -> Result<()> {
        let Some((repo, number)) = self.selected_pr() else {
            return Ok(());
        };
        self.status = Some(match (hooks.copy_url)(&repo, number) {
            Ok(()) => format!("#{number}'s URL copied"),
            Err(err) => format!("#{number}'s URL could not be copied: {err:#}"),
        });
        Ok(())
    }

    /// The selected PR's repo and number — the identity a hook needs to reach the
    /// forge for it.
    fn selected_pr(&self) -> Option<(RepoKey, u64)> {
        self.current()
            .map(|item| (item.repo.clone(), item.item.pr.number))
    }

    /// Handle a mouse event: a click selects the row under the pointer, the wheel
    /// scrolls whichever pane it is over.
    ///
    /// The pane under the pointer is what acts, and takes focus with it. A wheel
    /// over the detail that scrolled the queue instead — because the queue
    /// happened to have focus — would be worse than doing nothing.
    fn on_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        // An overlay owns the keyboard, and the mouse with it: the rows are still
        // drawn underneath, so a click on one you cannot see would act on
        // whatever the modal is covering.
        if !matches!(self.overlay, Overlay::None) {
            return Ok(());
        }
        let at = Position::new(mouse.column, mouse.row);
        let pane = if self.queue_area.contains(at) {
            Focus::Queue
        } else if self.detail_area.contains(at) {
            Focus::Detail
        } else {
            return Ok(());
        };

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.focus = pane;
                if pane == Focus::Queue {
                    self.select_row_at(mouse.row)?;
                }
                Ok(())
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                self.focus = pane;
                let rows = match pane {
                    Focus::Queue => 1,
                    Focus::Detail => WHEEL_ROWS,
                };
                self.scroll(if mouse.kind == MouseEventKind::ScrollUp {
                    -rows
                } else {
                    rows
                })
            }
            // Drags, releases, middle and right buttons: nothing here wants them.
            _ => Ok(()),
        }
    }

    /// Select the queue row drawn at screen row `row`.
    ///
    /// Screen rows map one-to-one onto queue entries — the list is a line per PR,
    /// never wrapped — so the entry is the window's first plus how far down the
    /// pane the click landed. Below the last PR is empty space, and a click there
    /// is ignored rather than jumping to the end of the queue.
    fn select_row_at(&mut self, row: u16) -> Result<()> {
        let offset = row.saturating_sub(self.queue_area.y) as usize;
        let index = self.queue_scroll.saturating_add(offset);
        if index >= self.queue.len() {
            return Ok(());
        }
        self.move_to(index)
    }

    /// Handle a key while an overlay owns the keyboard.
    pub(crate) fn on_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let escape = matches!(key.code, KeyCode::Esc);
        match self.overlay.clone() {
            // Nothing to accept: the loop replaces it the moment the handoff
            // returns, so a keystroke here is one the review command will get.
            Overlay::None | Overlay::Launching { .. } => Ok(()),
            // Movement scrolls the reference; anything else dismisses it, so it
            // still needs nothing remembered to get out of.
            Overlay::Help { scroll } => {
                let page = self.page().try_into().unwrap_or(u16::MAX);
                let moved = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => Some(scroll.saturating_add(1)),
                    KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
                    KeyCode::PageDown => Some(scroll.saturating_add(page)),
                    KeyCode::PageUp => Some(scroll.saturating_sub(page)),
                    KeyCode::Home | KeyCode::Char('g') => Some(0),
                    KeyCode::End | KeyCode::Char('G') => Some(u16::MAX),
                    _ => None,
                };
                self.overlay = match moved {
                    Some(to) => Overlay::Help {
                        scroll: to.min(self.help_max_scroll),
                    },
                    None => Overlay::None,
                };
                Ok(())
            }
            Overlay::OfferFetch { number } => {
                let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
                if confirmed {
                    // Held for the loop, so the notice is drawn before the fetch
                    // blocks on the forge — same reason as the handoff.
                    self.overlay = Overlay::Fetching { number };
                    self.pending_fetch = Some(number);
                } else {
                    self.overlay = Overlay::None;
                }
                Ok(())
            }
            // Nothing to accept: the loop replaces it when the fetch returns.
            Overlay::Fetching { .. } => Ok(()),
            Overlay::ConfirmDone { number } => {
                let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
                self.overlay = Overlay::None;
                if confirmed {
                    self.mark_done(number)?;
                }
                Ok(())
            }
            Overlay::SnoozePresets { number } => {
                if escape {
                    self.overlay = Overlay::None;
                    return Ok(());
                }
                if matches!(key.code, KeyCode::Char('o')) {
                    self.overlay = Overlay::SnoozePrompt {
                        number,
                        input: String::new(),
                        error: None,
                    };
                    return Ok(());
                }
                let picked = SNOOZE_PRESETS
                    .iter()
                    .find(|(k, _, _)| KeyCode::Char(*k) == key.code);
                if let Some((_, duration, _)) = picked {
                    self.overlay = Overlay::None;
                    self.apply_snooze(number, duration)?;
                }
                // Anything else is ignored rather than dismissing: a mistyped key
                // shouldn't silently abandon what you were doing.
                Ok(())
            }
            Overlay::JumpPrompt { mut input, error } => {
                match key.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Enter => match self.jump_to(&input) {
                        // `jump_to` may have put up its own overlay — the offer to
                        // fetch an unknown PR — so only close the prompt if it
                        // left the field.
                        Ok(()) => {
                            if matches!(self.overlay, Overlay::JumpPrompt { .. }) {
                                self.overlay = Overlay::None;
                            }
                        }
                        // Back to the field with the reason, keeping what was
                        // typed: the number may be right and just not here.
                        Err(err) => {
                            self.overlay = Overlay::JumpPrompt {
                                input,
                                error: Some(format!("{err:#}")),
                            };
                        }
                    },
                    KeyCode::Backspace => {
                        input.pop();
                        self.overlay = Overlay::JumpPrompt { input, error };
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        self.overlay = Overlay::JumpPrompt { input, error: None };
                    }
                    _ => {}
                }
                Ok(())
            }
            Overlay::SnoozePrompt {
                number,
                mut input,
                error,
            } => {
                match key.code {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        if let Err(err) = self.apply_snooze(number, &input) {
                            // Back to the prompt with the reason, rather than
                            // losing what was typed.
                            self.overlay = Overlay::SnoozePrompt {
                                number,
                                input,
                                error: Some(format!("{err:#}")),
                            };
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.overlay = Overlay::SnoozePrompt {
                            number,
                            input,
                            error,
                        };
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        self.overlay = Overlay::SnoozePrompt {
                            number,
                            input,
                            error: None,
                        };
                    }
                    _ => {}
                }
                Ok(())
            }
        }
    }

    /// Record the PR done, then let the forge know in the background.
    fn mark_done(&mut self, number: u64) -> Result<()> {
        let Some(repo_id) = self.selected_repo_id() else {
            return Ok(());
        };
        let head = match &self.detail {
            Some(show) => show.pr.head_sha.clone(),
            None => return Ok(()),
        };
        reviewq_app::actions::done(&self.ledger, repo_id, number, &head)?;
        // Held for the loop, which performs it after the local record — the PR is
        // done whether or not the forge can be reached. Deferring it is also what
        // keeps the overlay's keys free of hooks entirely.
        self.pending_mark_read = Some(number);
        self.status = Some(format!("#{number} done at {}", short(&head)));
        self.reload()
    }

    /// Select the PR `text` names, if it's on the queue.
    ///
    /// The distinction between "not on the queue" and "never heard of it" is
    /// worth drawing: the first means the PR is tracked and simply wants nothing
    /// — muted, snoozed, or waiting on someone else — and `list --all` will show
    /// it. A flat "not found" would send you looking for the wrong problem.
    fn jump_to(&mut self, text: &str) -> Result<()> {
        let number = self
            .pr_number_in(text)
            .with_context(|| format!("{text:?} is not a PR number"))?;

        if let Some(index) = self
            .queue
            .iter()
            .position(|item| item.item.pr.number == number)
        {
            self.status = None;
            return self.move_to(index);
        }

        if self.is_stored(number)? {
            bail!("#{number} is tracked but not on the queue — try `list --all`");
        }
        // Not a refusal: offer to go and get it.
        self.overlay = Overlay::OfferFetch { number };
        Ok(())
    }

    /// The PR number `text` names — a bare number, `#number`, or a pasted URL.
    ///
    /// A URL is handed to the forge, which knows its own layout; only the plain
    /// forms are read here. `None` when it is neither — a typo, or a URL on a host
    /// nothing configured knows.
    fn pr_number_in(&self, text: &str) -> Option<u64> {
        if let Some(number) = bare_pr_number(text) {
            return Some(number);
        }
        reviewq_forge::parse_pull_request_url(&self.config.forges, text)
            .ok()?
            .map(|pr| pr.number)
    }

    /// Whether any repo the ledger knows has this PR stored at all.
    fn is_stored(&self, number: u64) -> Result<bool> {
        for (repo_id, _) in self.ledger.repos()? {
            if self.ledger.show(repo_id, number)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Snooze the PR for a duration in the CLI's syntax.
    fn apply_snooze(&mut self, number: u64, duration: &str) -> Result<()> {
        let until = reviewq_app::actions::snooze_until(Timestamp::now(), duration)?;
        let Some(repo_id) = self.selected_repo_id() else {
            return Ok(());
        };
        let until = reviewq_app::actions::snooze(&self.ledger, repo_id, number, until)?;
        self.status = Some(format!("#{number} snoozed until {until}"));
        self.reload()
    }

    /// Mute the selected PR, or unmute an already-muted one.
    fn toggle_mute(&mut self) -> Result<()> {
        let (Some(number), Some(repo_id)) = (self.selected_number(), self.selected_repo_id())
        else {
            return Ok(());
        };
        let muted = self.detail.as_ref().is_some_and(|show| show.my_state.muted);
        reviewq_app::actions::set_muted(&self.ledger, repo_id, number, !muted)?;
        self.status = Some(if muted {
            format!("#{number} unmuted — its reasons return on the next sync")
        } else {
            format!("#{number} muted")
        });
        self.reload()
    }

    /// Sink the selected PR to the bottom of the queue, or restore it.
    fn toggle_defer(&mut self) -> Result<()> {
        let (Some(number), Some(repo_id)) = (self.selected_number(), self.selected_repo_id())
        else {
            return Ok(());
        };
        let deferred = self
            .detail
            .as_ref()
            .is_some_and(|show| show.my_state.deferred_at.is_some());
        reviewq_app::actions::set_deferred(&self.ledger, repo_id, number, !deferred)?;
        self.status = Some(if deferred {
            format!("#{number} undeferred")
        } else {
            format!("#{number} deferred to the bottom")
        });
        self.reload()
    }

    /// Apply a state change. No I/O beyond the ledger reads a moved selection
    /// needs, so a test can call it with any action and inspect the result.
    pub(crate) fn update(&mut self, action: Action) -> Result<Update> {
        let page = self.page() as isize;
        let handled = |result: Result<()>| result.map(|()| Update::Handled);
        match action {
            Action::Quit => {
                self.quit = true;
                Ok(Update::Handled)
            }
            Action::Help => {
                self.overlay = Overlay::Help { scroll: 0 };
                Ok(Update::Handled)
            }
            Action::Jump => {
                self.overlay = Overlay::JumpPrompt {
                    input: String::new(),
                    error: None,
                };
                Ok(Update::Handled)
            }
            Action::SwitchPane => {
                self.toggle_focus();
                Ok(Update::Handled)
            }
            Action::ToggleTheme => {
                // Nothing but the palette: every colour comes from the theme, so
                // swapping it is the whole change, and the next draw carries it.
                self.theme = self.theme.toggled();
                Ok(Update::Handled)
            }
            Action::Down => handled(self.scroll(1)),
            Action::Up => handled(self.scroll(-1)),
            Action::PageDown => handled(self.scroll(page)),
            Action::PageUp => handled(self.scroll(-page)),
            Action::First => handled(self.scroll_to_start()),
            Action::Last => handled(self.scroll_to_end()),
            // Not this layer's: performing these needs the hooks, the channel or
            // the ledger. Handed back rather than silently ignored.
            Action::RefreshSelected
            | Action::Review
            | Action::Done
            | Action::Snooze
            | Action::OpenInBrowser
            | Action::CopyUrl
            | Action::ToggleMute
            | Action::ToggleDefer => Ok(Update::Passed(action)),
        }
    }

    /// Fetch a PR the ledger has never seen, then select it if it landed on the
    /// queue.
    fn fetch_unknown(&mut self, number: u64, hooks: &Hooks) {
        let outcome = (hooks.fetch)(number);
        self.overlay = Overlay::None;
        self.dirty = true;
        match outcome {
            Ok(()) => {
                if let Err(err) = self.reload() {
                    self.status = Some(format!("reload failed: {err:#}"));
                    return;
                }
                // Land on it if it reached the queue; say so if it didn't, since
                // a PR nothing wants from you is a real outcome rather than a
                // failure.
                match self
                    .queue
                    .iter()
                    .position(|item| item.item.pr.number == number)
                {
                    Some(index) => {
                        self.status = Some(format!("#{number} tracked"));
                        if let Err(err) = self.move_to(index) {
                            self.status = Some(format!("#{number} tracked, but: {err:#}"));
                        }
                    }
                    None => {
                        self.status =
                            Some(format!("#{number} tracked — it wants nothing right now"));
                    }
                }
            }
            Err(err) => self.status = Some(format!("#{number} could not be fetched: {err:#}")),
        }
    }

    /// Hand `number` to the review command, then take the terminal back.
    fn hand_off(&mut self, number: u64, channel: &Channel, hooks: &Hooks) {
        let outcome = (hooks.review)(number);
        self.overlay = Overlay::None;
        self.dirty = true;
        // Whatever ran had the terminal, so nothing on screen can be trusted.
        self.repaint = true;
        match outcome {
            Ok(()) => {
                self.status = Some(format!("#{number} handed off"));
                // A review is the likeliest thing to have changed the PR, so
                // fetch it rather than making you press `r`.
                if self.refreshing.insert(number) {
                    (hooks.refresh)(number, channel.tx.clone());
                }
            }
            Err(err) => self.status = Some(format!("#{number} review failed: {err:#}")),
        }
    }

    /// The PR a refresh should fetch, marking it in flight — `None` when nothing
    /// is selected, or when this PR is already being fetched.
    fn refresh_target(&mut self) -> Option<u64> {
        let number = self.current().map(|item| item.item.pr.number)?;
        self.refreshing.insert(number).then_some(number)
    }

    /// Take in a finished refresh: report it, and re-read what it changed.
    ///
    /// A failure becomes the status line rather than ending the session — a bad
    /// token or a dropped connection should not discard the queue you were
    /// reading.
    fn on_refreshed(&mut self, number: u64, outcome: Result<Refreshed>) {
        self.refreshing.remove(&number);
        self.status = Some(match outcome {
            Ok(Refreshed::Updated { queued, .. }) => format!(
                "#{number} refreshed — {}",
                if queued {
                    "wants attention"
                } else {
                    "wants nothing"
                }
            ),
            Ok(Refreshed::Gone) => format!("#{number} no longer exists on the forge"),
            Ok(Refreshed::Untracked) => format!("#{number} is not in the ledger"),
            Err(err) => failure_note(number, &err),
        });
        // It may have changed what is on the queue, and changes the selected
        // PR's detail either way.
        if let Err(err) = self.reload() {
            self.status = Some(format!("reload failed: {err:#}"));
        }
    }

    /// Swap which pane the movement keys drive. Focusing the queue leaves the
    /// description where it was scrolled to, so tabbing away and back doesn't
    /// lose your place in a long one.
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Queue => Focus::Detail,
            Focus::Detail => Focus::Queue,
        };
    }

    /// Move `delta` rows in whichever pane has focus.
    fn scroll(&mut self, delta: isize) -> Result<()> {
        match self.focus {
            Focus::Queue => self.move_by(delta),
            Focus::Detail => {
                let target = self.detail_scroll as isize + delta;
                self.detail_scroll = target.max(0).try_into().unwrap_or(u16::MAX);
                self.clamp_detail_scroll();
                Ok(())
            }
        }
    }

    fn scroll_to_start(&mut self) -> Result<()> {
        match self.focus {
            Focus::Queue => self.move_to(0),
            Focus::Detail => {
                self.detail_scroll = 0;
                Ok(())
            }
        }
    }

    fn scroll_to_end(&mut self) -> Result<()> {
        match self.focus {
            Focus::Queue => self.move_to(self.queue.len().saturating_sub(1)),
            Focus::Detail => {
                self.detail_scroll = u16::MAX;
                self.clamp_detail_scroll();
                Ok(())
            }
        }
    }

    /// Move the selection `delta` rows, clamping at both ends. Clamping rather
    /// than wrapping: the queue is ordered by urgency, so falling off the top
    /// into the least urgent PR would be a surprise.
    fn move_by(&mut self, delta: isize) -> Result<()> {
        if self.queue.is_empty() {
            return Ok(());
        }
        let last = self.queue.len() - 1;
        let target = (self.selected as isize + delta).clamp(0, last as isize) as usize;
        self.move_to(target)
    }

    /// Take in a refresh result without a runtime, for tests.
    #[cfg(test)]
    fn deliver(&mut self, number: u64, outcome: Result<Refreshed>) {
        self.on_refreshed(number, outcome);
    }

    fn move_to(&mut self, index: usize) -> Result<()> {
        if self.queue.is_empty() || index == self.selected {
            return Ok(());
        }
        self.selected = index.min(self.queue.len() - 1);
        self.keep_selection_visible();
        // A new PR means a new description: keeping the old offset would open
        // it halfway down.
        self.detail_scroll = 0;
        self.load_detail()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use jiff::Timestamp;
    use reviewq_core::model::{Attention, AttentionReason, MyState, PrSnapshot, PrState};
    use reviewq_ledger::TrackedReason;

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("timestamp")
    }

    /// A PR snapshot with `number`, for a test building its own queue.
    pub(super) fn pr_snapshot(number: u64) -> PrSnapshot {
        PrSnapshot {
            number,
            title: format!("PR {number}"),
            author: "potiuk".into(),
            author_association: "MEMBER".into(),
            head_sha: "abc1234".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-11T09:00:00Z"),
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    /// Add a queued PR to an existing ledger, as a fetch-and-track would.
    pub(super) fn add_queued(ledger: &Ledger, repo_id: i64, number: u64) {
        let now = ts("2026-08-11T12:00:00Z");
        ledger
            .upsert_pr(
                repo_id,
                &pr_snapshot(number),
                Some(TrackedReason::Involved("manual".into())),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                number,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason: AttentionReason::NeedsFirstLook { rule: "x".into() },
                    since: ts("2026-08-11T08:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail")
            .expect_applied();
    }

    /// Two queued PRs, so a test can move between them.
    pub(super) fn two_queued() -> Ledger {
        let ledger = fixture();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let now = ts("2026-08-11T12:00:00Z");
        ledger
            .upsert_pr(
                repo_id,
                &pr_snapshot(70201),
                Some(TrackedReason::Interest("label x".into())),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                70201,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason: AttentionReason::NeedsFirstLook { rule: "x".into() },
                    since: ts("2026-08-11T08:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail")
            .expect_applied();
        ledger
    }

    /// One queued PR, enough to have something selected.
    pub(super) fn fixture() -> Ledger {
        let ledger = Ledger::open_in_memory().expect("ledger");
        seed(&ledger);
        ledger
    }

    /// Put the fixture's contents into an already-open ledger, returning the
    /// repo's id. Separate from [`fixture`] so a test that needs a second
    /// connection can seed a file-backed one.
    pub(super) fn seed(ledger: &Ledger) -> i64 {
        let repo = reviewq_ledger::RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        };
        let repo_id = ledger.ensure_repo(&repo).expect("repo");
        let now = ts("2026-08-11T12:00:00Z");
        let pr = PrSnapshot {
            number: 70135,
            title: "Add deferrable mode".into(),
            author: "potiuk".into(),
            author_association: "MEMBER".into(),
            head_sha: "abc1234".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-11T09:00:00Z"),
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        };
        ledger
            .upsert_pr(
                repo_id,
                &pr,
                Some(TrackedReason::Interest("label x".into())),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                70135,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason: AttentionReason::Mention { by: "kaxil".into() },
                    since: ts("2026-08-11T09:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail")
            .expect_applied();
        repo_id
    }

    pub(super) fn app() -> App {
        App::with_ledger(Theme::default(), fixture(), test_config()).expect("app")
    }

    #[test]
    fn a_refresh_is_only_started_once_per_pr() {
        let mut app = app();
        assert_eq!(app.refresh_target(), Some(70135), "first press starts one");
        assert_eq!(
            app.refresh_target(),
            None,
            "a second press on the same PR must not fetch it twice"
        );
        assert_eq!(app.refreshing.len(), 1);
    }

    #[test]
    fn nothing_selected_means_nothing_to_refresh() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut empty = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        assert_eq!(empty.refresh_target(), None);
        assert!(empty.refreshing.is_empty());
    }

    #[test]
    fn a_finished_refresh_clears_its_in_flight_mark_and_reports() {
        let mut app = app();
        assert_eq!(app.refresh_target(), Some(70135));

        app.deliver(
            70135,
            Ok(Refreshed::Updated {
                repo: "apache/airflow".into(),
                queued: true,
                cost: 1,
                remaining: 4999,
            }),
        );

        assert!(app.refreshing.is_empty());
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 refreshed"), "{status}");
        assert!(status.contains("wants attention"), "{status}");
    }

    #[test]
    fn a_failed_refresh_reports_without_ending_the_session() {
        let mut app = app();
        assert_eq!(app.refresh_target(), Some(70135));

        app.deliver(70135, Err(anyhow::anyhow!("bad credentials")));

        assert!(app.refreshing.is_empty(), "a failure still clears the mark");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("refresh failed"), "{status}");
        assert!(status.contains("bad credentials"), "{status}");
        // The queue survives: a bad token must not discard what you were reading.
        assert!(!app.quit);
        assert_eq!(app.queue.len(), 1);
        assert!(app.detail.is_some());
    }

    #[test]
    fn a_ledger_from_a_newer_reviewq_says_what_to_do_about_it() {
        // The point of the ledger's errors being typed: this one needs the binary
        // upgraded, and "refresh failed: running ledger migrations: …" does not
        // say so.
        let mut app = app();
        app.deliver(70135, Err(LedgerError::FromTheFuture.into()));

        let status = app.status.clone().expect("a status");
        assert!(status.contains("newer reviewq"), "{status}");
        assert!(status.contains("upgrade"), "{status}");
    }

    #[test]
    fn a_busy_ledger_says_to_try_again_rather_than_quoting_sqlite() {
        let mut app = app();
        let busy = LedgerError::Busy {
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(5),
                Some("database is locked".to_string()),
            ),
        };
        app.deliver(70135, Err(busy.into()));

        let status = app.status.clone().expect("a status");
        assert!(status.contains("try again"), "{status}");
        assert!(!status.contains("SQLITE"), "{status}");
    }

    #[test]
    fn any_other_failure_is_quoted_as_it_arrived() {
        let mut app = app();
        app.deliver(70135, Err(anyhow::anyhow!("bad credentials")));

        let status = app.status.clone().expect("a status");
        assert!(status.contains("bad credentials"), "{status}");
    }

    #[test]
    fn a_pr_the_forge_lost_is_reported_as_such() {
        let mut app = app();
        assert_eq!(app.refresh_target(), Some(70135));
        app.deliver(70135, Ok(Refreshed::Gone));
        let status = app.status.clone().expect("a status");
        assert!(status.contains("no longer exists"), "{status}");
    }
}

/// Driving the real loop: scripted messages in, a `TestBackend` to draw on, and
/// a fake refresh hook. No terminal, no forge, no sleeps — the same shape wiff
/// uses, which is what having the channel and the hooks handed in buys.
#[cfg(test)]
mod scroll_tests {
    use super::tests::pr_snapshot;
    use super::*;
    use reviewq_core::model::{Attention, AttentionReason, MyState};
    use reviewq_ledger::{RepoKey, TrackedReason};

    /// A queue of `count` PRs, numbered from 1.
    fn queue_of(count: u64) -> App {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
            .expect("repo");
        let now: Timestamp = "2026-08-11T12:00:00Z".parse().unwrap();
        for number in 1..=count {
            ledger
                .upsert_pr(
                    repo_id,
                    &pr_snapshot(number),
                    Some(TrackedReason::Interest("label x".into())),
                    now,
                )
                .expect("upsert");
            ledger
                .commit_detail(
                    repo_id,
                    number,
                    &MyState::default(),
                    &[],
                    &[],
                    &[Attention {
                        reason: AttentionReason::NeedsFirstLook { rule: "x".into() },
                        // Older sorts first, so the numbering and the order agree.
                        since: Timestamp::from_second(number as i64).expect("since"),
                    }],
                    None,
                    now,
                )
                .expect("detail")
                .expect_applied();
        }
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        // A ten-row window, as a render would report.
        app.set_page(10);
        app
    }

    /// Apply a movement action, asserting `update` owned it — none of these needs
    /// a hook, which is the point of the split.
    fn moved(app: &mut App, action: Action) {
        assert_eq!(app.update(action).expect("update"), Update::Handled);
    }

    fn go_to(app: &mut App, index: usize) {
        while app.selected < index {
            moved(app, Action::Down);
        }
    }

    #[test]
    fn the_window_holds_still_while_the_selection_moves_inside_it() {
        let mut app = queue_of(30);
        // Down to row 6, which is still SCROLLOFF away from the bottom of a
        // ten-row window, so nothing should have scrolled.
        go_to(&mut app, 6);
        assert_eq!(app.queue_scroll, 0, "the window should not have moved yet");
        assert_eq!(app.selected, 6);
    }

    #[test]
    fn it_starts_scrolling_three_rows_before_the_bottom() {
        let mut app = queue_of(30);
        go_to(&mut app, 7);
        assert_eq!(
            app.queue_scroll, 1,
            "row 7 in a ten-row window is within 3 of the bottom, so it scrolls one"
        );
        go_to(&mut app, 8);
        assert_eq!(app.queue_scroll, 2);
    }

    #[test]
    fn moving_up_off_the_last_row_does_not_drag_the_window() {
        // The reported bug: from the very last row, `up` moved the selection and
        // scrolled at the same time, so two things happened for one keypress.
        let mut app = queue_of(30);
        go_to(&mut app, 29);
        let bottom = app.queue_scroll;
        assert_eq!(bottom, 20, "the last row sits at the window's bottom");

        moved(&mut app, Action::Up);
        assert_eq!(app.selected, 28);
        assert_eq!(
            app.queue_scroll, bottom,
            "the selection moved within the window, so the window stayed put"
        );

        // It keeps still until the selection reaches the margin, which with the
        // window at rows 20..=29 is row 23.
        for _ in 0..5 {
            moved(&mut app, Action::Up);
        }
        assert_eq!(app.selected, 23);
        assert_eq!(app.queue_scroll, bottom, "still 3 rows clear of the top");

        moved(&mut app, Action::Up);
        assert_eq!(app.selected, 22);
        assert_eq!(
            app.queue_scroll, 19,
            "inside the margin now, so the window follows a row at a time"
        );
    }

    #[test]
    fn the_ends_of_the_list_win_over_the_margin() {
        let mut app = queue_of(30);
        // There is nothing above row zero to keep in view.
        go_to(&mut app, 2);
        moved(&mut app, Action::First);
        assert_eq!((app.selected, app.queue_scroll), (0, 0));

        // Nor below the last row: the window stops with it at the bottom rather
        // than scrolling into empty space.
        moved(&mut app, Action::Last);
        assert_eq!(app.selected, 29);
        assert_eq!(app.queue_scroll, 20);
    }

    #[test]
    fn a_queue_shorter_than_the_window_never_scrolls() {
        let mut app = queue_of(4);
        moved(&mut app, Action::Last);
        assert_eq!(app.selected, 3);
        assert_eq!(
            app.queue_scroll, 0,
            "it all fits, so there is nothing to scroll"
        );
    }

    #[test]
    fn a_pane_too_short_for_the_margin_still_moves() {
        let mut app = queue_of(30);
        app.set_page(3);
        go_to(&mut app, 10);
        assert_eq!(app.selected, 10);
        assert!(
            app.queue_scroll >= 8,
            "the selection has to stay visible even with no room for a margin, was {}",
            app.queue_scroll
        );
        assert!(app.queue_scroll <= 10);
    }
}

/// The PR number `text` names: a bare number, or one with a `#` in front.
///
/// A URL is *not* handled here — which repo layout a URL uses is the forge's
/// knowledge, not the interface's, so [`App::pr_number_in`] asks the forge.
fn bare_pr_number(text: &str) -> Option<u64> {
    let text = text.trim();
    text.strip_prefix('#').unwrap_or(text).parse().ok()
}

/// A head SHA at GitHub's own abbreviation length, so it can be pasted into
/// `git show`.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// The loop and the actions, driven through the real `run` with a scripted input
/// source and fake side effects. No terminal, no forge, no sleeps.
#[cfg(test)]
mod loop_tests {
    use super::tests::{add_queued, app, fixture, seed, two_queued};
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// PRs a hook was asked about, shared with the test that reads them.
    type Seen = Arc<Mutex<Vec<u64>>>;

    fn press(code: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::NONE))
    }

    fn special(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// An input source that plays `script`, then fails.
    ///
    /// Failing rather than reporting nothing is what keeps a script that forgot
    /// to quit from hanging the test: the loop's body is synchronous between
    /// events, so an input source that always says "nothing yet" spins forever
    /// and never yields to the timeout in [`drive`].
    fn scripted(script: Vec<Event>) -> Box<dyn Fn() -> Result<Option<Event>> + Send + Sync> {
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        Box::new(move || match queue.lock().expect("lock").pop_front() {
            Some(event) => Ok(Some(event)),
            None => bail!("the input script ran dry without quitting"),
        })
    }

    /// What the hooks were asked to do.
    struct Recorded {
        refreshed: Seen,
        marked: Seen,
        reviewed: Seen,
        fetched: Seen,
        opened: Seen,
        copied: Seen,
    }

    /// Hooks over `script`. `answer` is what a refresh reports back, if anything;
    /// `review_fails` makes the handoff error.
    fn fake_hooks(
        script: Vec<Event>,
        answer: Option<Refreshed>,
        review_fails: bool,
    ) -> (Hooks, Recorded) {
        let recorded = Recorded {
            refreshed: Arc::new(Mutex::new(Vec::new())),
            marked: Arc::new(Mutex::new(Vec::new())),
            reviewed: Arc::new(Mutex::new(Vec::new())),
            fetched: Arc::new(Mutex::new(Vec::new())),
            opened: Arc::new(Mutex::new(Vec::new())),
            copied: Arc::new(Mutex::new(Vec::new())),
        };
        let refreshed = Arc::clone(&recorded.refreshed);
        let marked = Arc::clone(&recorded.marked);
        let reviewed = Arc::clone(&recorded.reviewed);
        let fetched = Arc::clone(&recorded.fetched);
        let opened = Arc::clone(&recorded.opened);
        let copied = Arc::clone(&recorded.copied);
        let hooks = Hooks {
            fetch: Box::new(move |number| {
                fetched.lock().expect("lock").push(number);
                Ok(())
            }),
            next_event: scripted(script),
            refresh: Box::new(move |number, tx| {
                refreshed.lock().expect("lock").push(number);
                if let Some(answer) = answer.clone() {
                    let _ = tx.send(Message::Refreshed {
                        number,
                        outcome: Ok(answer),
                    });
                }
            }),
            mark_read: Box::new(move |number| marked.lock().expect("lock").push(number)),
            review: Box::new(move |number| {
                reviewed.lock().expect("lock").push(number);
                if review_fails {
                    bail!("wiff not found");
                }
                Ok(())
            }),
            open_url: Box::new(move |_repo, number| {
                opened.lock().expect("lock").push(number);
                Ok(())
            }),
            copy_url: Box::new(move |_repo, number| {
                copied.lock().expect("lock").push(number);
                Ok(())
            }),
        };
        (hooks, recorded)
    }

    /// A `TestBackend` that counts how many times it was drawn to.
    ///
    /// The point of the dirty flag is what *doesn't* happen on an idle wakeup, and
    /// nothing else here can see that.
    struct CountingBackend {
        inner: TestBackend,
        draws: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Backend for CountingBackend {
        type Error = std::convert::Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.draws.set(self.draws.get() + 1);
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> Result<ratatui::layout::Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(
            &mut self,
            clear_type: ratatui::backend::ClearType,
        ) -> Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<ratatui::layout::Size, Self::Error> {
            self.inner.size()
        }

        fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), Self::Error> {
            self.inner.flush()
        }
    }

    /// Run the loop against a counting backend, returning how many draws it did.
    fn draws_while(app: &mut App, hooks: &Hooks) -> usize {
        let draws = std::rc::Rc::new(std::cell::Cell::new(0));
        let backend = CountingBackend {
            inner: TestBackend::new(100, 20),
            draws: std::rc::Rc::clone(&draws),
        };
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut channel = Channel::new();
        app.run(&mut terminal, &mut channel, hooks).expect("loop");
        draws.get()
    }

    #[test]
    fn an_idle_wakeup_does_not_redraw() {
        // The input poll returns on a timer whether or not anything arrived. Every
        // one of those used to cost a full layout and a markdown parse of the
        // selected PR's description.
        let mut app = app();
        let script = vec![None, None, None, None, Some(press('q'))];
        let queue = Arc::new(Mutex::new(VecDeque::from(script)));
        let (hooks, _) = fake_hooks(vec![], None, false);
        let hooks = Hooks {
            next_event: Box::new(move || Ok(queue.lock().expect("lock").pop_front().flatten())),
            ..hooks
        };

        let draws = draws_while(&mut app, &hooks);

        assert_eq!(
            draws, 1,
            "the opening frame, and nothing for four idle wakeups"
        );
    }

    #[test]
    fn a_keystroke_redraws() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![press('j'), press('q')], None, false);

        let draws = draws_while(&mut app, &hooks);

        assert_eq!(
            draws, 2,
            "the opening frame and one for the moved selection"
        );
    }

    /// Run the loop to completion, or fail if it doesn't finish.
    ///
    /// No timeout: the loop's body is synchronous, so nothing could interrupt it
    /// anyway. A script that forgets to quit is caught by [`scripted`], which
    /// errors when it runs dry instead of returning "nothing yet" forever.
    fn drive(app: &mut App, hooks: &Hooks) {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        let mut channel = Channel::new();
        app.run(&mut terminal, &mut channel, hooks).expect("loop");
    }

    /// Hand keys straight to the dispatcher, for a sequence meant to finish with
    /// an overlay still up and inspectable — in a prompt, `q` types a letter
    /// rather than quitting, so such a script would never end the loop.
    fn feed(app: &mut App, hooks: &Hooks, codes: &[KeyCode]) {
        let channel = Channel::new();
        for code in codes {
            app.dispatch(KeyEvent::new(*code, KeyModifiers::NONE), &channel, hooks)
                .expect("dispatch");
        }
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area();
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_quit_key_ends_the_loop_after_one_frame() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('q')], None, false);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        let mut channel = Channel::new();

        app.run(&mut terminal, &mut channel, &hooks).expect("loop");

        assert!(app.quit);
        // It drew before waiting, so the opening frame is on screen.
        assert!(screen(&terminal).contains("on the queue"));
    }

    #[test]
    fn a_refresh_result_reaches_the_state_through_the_loop() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(
            // `r` starts it; the answer lands on the channel and is taken in
            // before the trailing `q` is read.
            vec![press('r'), press('q')],
            Some(Refreshed::Updated {
                repo: "apache/airflow".into(),
                queued: false,
                cost: 1,
                remaining: 4999,
            }),
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(*recorded.refreshed.lock().expect("lock"), vec![70135]);
        assert!(app.refreshing.is_empty(), "the result cleared the mark");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 refreshed"), "{status}");
    }

    #[test]
    fn the_help_overlay_toggles_through_the_loop() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![press('?'), press(' '), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(app.overlay, Overlay::None, "opened and closed again");
        assert!(app.quit);
    }

    #[test]
    fn d_asks_first_and_a_confirmation_marks_it_done() {
        // `d` alone only raises the question. Fed directly, because with the
        // confirmation up a `q` is taken as "cancel" rather than "quit".
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![], None, false);
        feed(&mut app, &hooks, &[KeyCode::Char('d')]);
        assert_eq!(app.overlay, Overlay::ConfirmDone { number: 70135 });
        assert!(app.status.is_none(), "asking is not doing");
        assert_eq!(app.queue.len(), 1);
        assert!(recorded.marked.lock().expect("lock").is_empty());

        // `d` then `y` does it.
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let (hooks, recorded) = fake_hooks(vec![press('d'), press('y'), press('q')], None, false);
        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 done"), "{status}");
        assert!(app.queue.is_empty(), "a mention is cleared by done");
        // And the forge was told, after the local record rather than before.
        assert_eq!(*recorded.marked.lock().expect("lock"), vec![70135]);
    }

    #[test]
    fn declining_the_confirmation_changes_nothing() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![press('d'), press('n'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(app.overlay, Overlay::None, "the question is dismissed");
        assert!(app.status.is_none(), "and nothing happened");
        assert_eq!(app.queue.len(), 1);
        assert!(recorded.marked.lock().expect("lock").is_empty());
    }

    #[test]
    fn z_then_a_preset_snoozes_it_off_the_queue() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('z'), press('3'), press('q')], None, false);

        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 snoozed until"), "{status}");
        assert!(app.queue.is_empty(), "snoozing takes it off the queue");
    }

    #[test]
    fn a_typed_duration_reaches_the_same_place() {
        let mut app = app();
        // `o` escapes the presets to the prompt; then type `2d` and confirm.
        let (hooks, _) = fake_hooks(
            vec![
                press('z'),
                press('o'),
                press('2'),
                press('d'),
                special(KeyCode::Enter),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("snoozed until"), "{status}");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn a_bad_duration_says_why_and_keeps_what_was_typed() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char('z'),
                KeyCode::Char('o'),
                KeyCode::Char('s'),
                KeyCode::Char('o'),
                KeyCode::Char('o'),
                KeyCode::Char('n'),
                KeyCode::Enter,
            ],
        );

        match &app.overlay {
            Overlay::SnoozePrompt { input, error, .. } => {
                assert_eq!(input, "soon", "what was typed survives the rejection");
                let error = error.clone().expect("a reason");
                assert!(error.contains("invalid duration"), "{error}");
            }
            other => panic!("should still be prompting, was {other:?}"),
        }
        assert_eq!(app.queue.len(), 1, "nothing was snoozed");
    }

    #[test]
    fn backspace_edits_the_typed_duration() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char('z'),
                KeyCode::Char('o'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Backspace,
                KeyCode::Char('d'),
            ],
        );

        match &app.overlay {
            Overlay::SnoozePrompt { input, .. } => assert_eq!(input, "9d"),
            other => panic!("should still be prompting, was {other:?}"),
        }
    }

    #[test]
    fn m_mutes_and_takes_it_off_the_queue() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('m'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(app.status.as_deref(), Some("#70135 muted"));
        assert!(app.queue.is_empty(), "muting clears what it holds");
        // Which leaves nothing selected to unmute — the honest consequence of
        // acting from a queue view, and why `list --all` exists.
        assert!(app.current().is_none());
    }

    #[test]
    fn f_defers_without_hiding_it_and_again_restores_it() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('f'), press('q')], None, false);
        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("deferred to the bottom"), "{status}");
        assert_eq!(app.queue.len(), 1, "deferred is sunk, not hidden");
        assert!(app.queue[0].item.deferred);

        let mut again = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![press('f'), press('f'), press('q')], None, false);
        drive(&mut again, &hooks);
        let status = again.status.clone().expect("a status");
        assert!(status.contains("undeferred"), "{status}");
        assert!(!again.queue[0].item.deferred);
    }

    #[test]
    fn an_action_on_an_empty_queue_does_nothing() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, recorded) = fake_hooks(
            vec![
                special(KeyCode::Enter),
                press('d'),
                press('z'),
                press('m'),
                press('f'),
                press('r'),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.overlay, Overlay::None, "nothing to ask about");
        assert!(app.status.is_none());
        assert!(recorded.marked.lock().expect("lock").is_empty());
        assert!(recorded.reviewed.lock().expect("lock").is_empty());
        assert!(recorded.refreshed.lock().expect("lock").is_empty());
    }

    #[test]
    fn a_bare_pr_reference_is_read_from_a_number_or_a_hash() {
        // A URL is the forge's to parse — see `GithubForge::parse_web_path` —
        // because its shape belongs to the provider, not to this interface.
        assert_eq!(bare_pr_number("70135"), Some(70135));
        assert_eq!(bare_pr_number("#70135"), Some(70135));
        assert_eq!(bare_pr_number("  70135 "), Some(70135));
        assert_eq!(bare_pr_number(""), None);
        assert_eq!(bare_pr_number("soon"), None);
        assert_eq!(bare_pr_number("#nope"), None);
        assert_eq!(
            bare_pr_number("https://github.com/apache/airflow/pull/70135"),
            None,
            "not this function's job"
        );
    }

    #[test]
    fn colon_then_a_number_selects_that_pr() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        assert_eq!(app.selected, 0);
        let (hooks, _) = fake_hooks(
            vec![
                press(':'),
                press('7'),
                press('0'),
                press('2'),
                press('0'),
                press('1'),
                special(KeyCode::Enter),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.overlay, Overlay::None, "the prompt closed");
        assert_eq!(
            app.current().map(|item| item.item.pr.number),
            Some(70201),
            "the selection moved to the PR named"
        );
    }

    #[test]
    fn going_to_a_pr_that_is_tracked_but_not_queued_says_which() {
        // Worth distinguishing: the number is right, the PR is simply quiet, and
        // `list --all` will show it. "Not found" would send you looking for the
        // wrong problem.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        reviewq_app::actions::set_muted(&ledger, repo_id, 70201, true).expect("mute");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('7'),
                KeyCode::Char('0'),
                KeyCode::Char('2'),
                KeyCode::Char('0'),
                KeyCode::Char('1'),
                KeyCode::Enter,
            ],
        );

        match &app.overlay {
            Overlay::JumpPrompt { input, error } => {
                assert_eq!(input, "70201", "what was typed survives");
                let error = error.clone().expect("a reason");
                assert!(error.contains("not on the queue"), "{error}");
                assert!(error.contains("list --all"), "{error}");
            }
            other => panic!("should still be prompting, was {other:?}"),
        }
    }

    #[test]
    fn going_to_a_pr_the_ledger_never_saw_offers_to_fetch_it() {
        // Not a refusal: a number you typed is a number you meant, and it being
        // absent is more often "the sweep hasn't reached it" than a mistake.
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[KeyCode::Char(':'), KeyCode::Char('9'), KeyCode::Enter],
        );

        assert_eq!(app.overlay, Overlay::OfferFetch { number: 9 });
        assert!(
            recorded.fetched.lock().expect("lock").is_empty(),
            "asking is not fetching"
        );
    }

    #[test]
    fn declining_the_fetch_offer_leaves_everything_alone() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('9'),
                KeyCode::Enter,
                KeyCode::Char('n'),
            ],
        );

        assert_eq!(app.overlay, Overlay::None);
        assert!(recorded.fetched.lock().expect("lock").is_empty());
        assert!(app.status.is_none());
    }

    #[test]
    fn accepting_the_fetch_offer_defers_it_to_the_loop_behind_a_notice() {
        // The same ordering the handoff needs: the notice has to be drawn before
        // the network call blocks, so `dispatch` only records the intent.
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('9'),
                KeyCode::Enter,
                KeyCode::Char('y'),
            ],
        );

        assert_eq!(app.overlay, Overlay::Fetching { number: 9 });
        assert_eq!(app.pending_fetch, Some(9));
        assert!(
            recorded.fetched.lock().expect("lock").is_empty(),
            "the loop performs it, after drawing"
        );
    }

    #[test]
    fn a_fetch_that_lands_on_the_queue_selects_it() {
        // The PR has to be unknown to the ledger when the jump asks for it, and
        // there by the time the fetch returns — so the ledger is a file, and the
        // fake fetch writes over its own connection, which is what the real
        // `track_one` does.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reviewq.db");
        let ledger = Ledger::open(&path).expect("ledger");
        let repo_id = seed(&ledger);
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        let added = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&added);
        let write_path = path.clone();
        let hooks = Hooks {
            next_event: scripted(vec![
                press(':'),
                press('7'),
                press('0'),
                press('2'),
                press('0'),
                press('1'),
                special(KeyCode::Enter),
                press('y'),
                press('q'),
            ]),
            fetch: Box::new(move |number| {
                seen.lock().expect("lock").push(number);
                let other = Ledger::open(&write_path).expect("second connection");
                add_queued(&other, repo_id, number);
                Ok(())
            }),
            refresh: Box::new(|_, _| {}),
            mark_read: Box::new(|_| {}),
            review: Box::new(|_| Ok(())),
            open_url: Box::new(|_, _| Ok(())),
            copy_url: Box::new(|_, _| Ok(())),
        };

        drive(&mut app, &hooks);

        assert_eq!(*added.lock().expect("lock"), vec![70201]);
        assert_eq!(
            app.current().map(|item| item.item.pr.number),
            Some(70201),
            "it landed on the queue, so the selection went to it"
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70201 tracked"), "{status}");
    }

    #[test]
    fn a_non_number_in_the_go_to_field_is_refused_without_losing_it() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('a'),
                KeyCode::Char('b'),
                KeyCode::Enter,
            ],
        );

        match &app.overlay {
            Overlay::JumpPrompt { input, error } => {
                assert_eq!(input, "ab");
                assert!(
                    error
                        .as_deref()
                        .expect("a reason")
                        .contains("not a PR number")
                );
            }
            other => panic!("should still be prompting, was {other:?}"),
        }
    }

    #[test]
    fn enter_shows_the_notice_before_handing_over_and_refreshes_after() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(
            vec![special(KeyCode::Enter), press('q')],
            Some(Refreshed::Updated {
                repo: "apache/airflow".into(),
                queued: false,
                cost: 1,
                remaining: 4999,
            }),
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(*recorded.reviewed.lock().expect("lock"), vec![70135]);
        assert_eq!(
            *recorded.refreshed.lock().expect("lock"),
            vec![70135],
            "a review is the likeliest thing to have changed it, so it's fetched"
        );
        assert_eq!(app.overlay, Overlay::None, "the notice is taken down again");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 refreshed"), "{status}");
    }

    #[test]
    fn the_notice_is_up_before_the_handoff_runs() {
        // The order that matters: the review command may sit on a credential
        // prompt, so the notice has to be on screen before it is invoked. The
        // loop draws between the keypress and the handoff, which is why
        // `dispatch` only records the intent.
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        feed(&mut app, &hooks, &[KeyCode::Enter]);

        assert_eq!(app.overlay, Overlay::Launching { number: 70135 });
        assert_eq!(app.pending_review, Some(70135));
        assert!(
            recorded.reviewed.lock().expect("lock").is_empty(),
            "nothing has been handed off yet — the loop does that after drawing"
        );
    }

    #[test]
    fn a_review_command_that_fails_says_so_and_keeps_the_queue() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![special(KeyCode::Enter), press('q')], None, true);

        drive(&mut app, &hooks);

        assert_eq!(*recorded.reviewed.lock().expect("lock"), vec![70135]);
        assert!(
            recorded.refreshed.lock().expect("lock").is_empty(),
            "a failed handoff has nothing to refresh"
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("review failed"), "{status}");
        assert!(status.contains("wiff not found"), "{status}");
        assert_eq!(app.queue.len(), 1, "the queue survives a failed handoff");
        assert!(app.repaint || app.quit, "a handoff forces a full repaint");
    }

    #[test]
    fn a_pasted_url_is_read_against_the_session_config() {
        // Reading a URL means knowing which provider its host is, which the held
        // config answers — no load, and so no file read per keystroke.
        let app = app();

        assert_eq!(
            app.pr_number_in("https://github.com/apache/airflow/pull/70135"),
            Some(70135)
        );
        assert_eq!(
            app.pr_number_in("https://example.invalid/apache/airflow/pull/70135"),
            None,
            "a host nothing configured knows resolves to nothing"
        );
        assert_eq!(app.pr_number_in("70135"), Some(70135));
    }

    #[test]
    fn t_adapts_the_palette_for_the_other_background_and_back() {
        // A terminal that flips light and dark on a schedule leaves a running
        // reviewq adapted for the background it no longer has.
        let mut app = app();
        assert_eq!(app.theme.mode, crate::theme::Mode::Dark);
        let dark_text = app.theme.text;
        let (hooks, _) = fake_hooks(vec![press('t'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(app.theme.mode, crate::theme::Mode::Light);
        assert_ne!(
            app.theme.text, dark_text,
            "the palette has to actually change, not just its label"
        );

        let (back, _) = fake_hooks(vec![press('t'), press('q')], None, false);
        app.quit = false;
        drive(&mut app, &back);

        assert_eq!(app.theme.mode, crate::theme::Mode::Dark);
        assert_eq!(app.theme.text, dark_text);
    }

    #[test]
    fn o_opens_the_selected_pr() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![press('o'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(*recorded.opened.lock().expect("lock"), vec![70135]);
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 opened"), "{status}");
    }

    #[test]
    fn c_and_y_are_the_same_copy() {
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![press('c'), press('y'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(
            *recorded.copied.lock().expect("lock"),
            vec![70135, 70135],
            "`y` reaches the same action as `c`, not a different one"
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("URL copied"), "{status}");
    }

    #[test]
    fn a_url_that_cannot_be_resolved_says_so_and_keeps_the_queue() {
        // What fails in practice is the config the hook needs to know the host's
        // layout. It must land in the header, not take the interface down.
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('o'), press('q')], None, false);
        let hooks = Hooks {
            open_url: Box::new(|_, _| bail!("no config")),
            ..hooks
        };

        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("could not be opened"), "{status}");
        assert!(status.contains("no config"), "{status}");
        assert!(!app.queue.is_empty(), "the queue survives");
    }

    #[test]
    fn open_and_copy_do_nothing_on_an_empty_queue() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        app.open_selected(&hooks).expect("open");
        app.copy_selected_url(&hooks).expect("copy");

        assert!(recorded.opened.lock().expect("lock").is_empty());
        assert!(recorded.copied.lock().expect("lock").is_empty());
        assert_eq!(app.status, None, "nothing was selected, so nothing to say");
    }

    /// The two panes' contents, as the 100x20 terminal [`drive`] uses lays them
    /// out: header, then the panes between the borders, then the footer. Named
    /// rather than written into each test, because a column of `47` explains
    /// nothing about which pane it is in.
    const QUEUE_COLUMN: u16 = 4;
    const DETAIL_COLUMN: u16 = 60;
    const FIRST_ROW: u16 = 2;

    fn click(column: u16, row: u16) -> Event {
        mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_click_selects_the_queue_row_under_the_pointer() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        assert_eq!(app.selected, 0);
        let (hooks, _) = fake_hooks(
            vec![click(QUEUE_COLUMN, FIRST_ROW + 1), press('q')],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.selected, 1, "the second row was clicked");
        assert_eq!(
            app.detail.as_ref().map(|show| show.pr.number),
            app.current().map(|item| item.item.pr.number),
            "the detail follows a click as it does a key"
        );
        assert_eq!(app.focus, Focus::Queue);
    }

    #[test]
    fn a_click_past_the_last_row_leaves_the_selection_alone() {
        // Empty space below a two-item queue: a click there means nothing, and
        // must not be read as "the last row".
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![click(QUEUE_COLUMN, FIRST_ROW + 9), press('q')],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.selected, 0);
    }

    #[test]
    fn a_click_in_the_detail_pane_focuses_it_without_moving_the_selection() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![click(DETAIL_COLUMN, FIRST_ROW + 1), press('q')],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.focus, Focus::Detail);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn the_wheel_moves_the_queue_a_row_at_a_time() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![
                mouse(MouseEventKind::ScrollDown, QUEUE_COLUMN, FIRST_ROW),
                mouse(MouseEventKind::ScrollDown, QUEUE_COLUMN, FIRST_ROW),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(
            app.selected, 1,
            "one notch moved one row; the second clamped at the last of two"
        );
    }

    #[test]
    fn the_wheel_goes_back_up_the_queue_too() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![
                mouse(MouseEventKind::ScrollDown, QUEUE_COLUMN, FIRST_ROW),
                mouse(MouseEventKind::ScrollUp, QUEUE_COLUMN, FIRST_ROW),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.selected, 0, "down then up is back where it started");
    }

    #[test]
    fn the_wheel_over_the_detail_takes_focus_from_the_queue() {
        // Whichever pane is under the pointer is the one that scrolls, however the
        // keyboard's focus happens to be set.
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        assert_eq!(app.focus, Focus::Queue);
        let (hooks, _) = fake_hooks(
            vec![
                mouse(MouseEventKind::ScrollDown, DETAIL_COLUMN, FIRST_ROW),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.focus, Focus::Detail);
        assert_eq!(app.selected, 0, "the queue did not move");
    }

    #[test]
    fn the_mouse_is_ignored_while_an_overlay_is_up() {
        // The rows are still drawn under a modal, so a click landing on one you
        // cannot see would act on whatever it happens to cover.
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![
                press('?'),
                click(QUEUE_COLUMN, FIRST_ROW + 1),
                press('q'),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.selected, 0, "the click did not reach the queue");
    }

    #[test]
    fn a_click_outside_both_panes_does_nothing() {
        // The header and footer are not clickable, and neither are the borders.
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![click(QUEUE_COLUMN, 0), click(QUEUE_COLUMN, 19), press('q')],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert_eq!(app.selected, 0);
        assert_eq!(app.focus, Focus::Queue);
    }
}
