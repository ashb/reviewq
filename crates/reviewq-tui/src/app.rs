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

use anyhow::{Context, Result};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui::layout::{Position, Rect};
use reviewq_app::config::Config;
use reviewq_app::peek::Peeked;
use reviewq_app::sync::{Refreshed, RepoSummary};
use reviewq_core::model::{MyState, PrSnapshot, PrState};
use reviewq_forge::ForgeError;
use std::collections::BTreeMap;

use reviewq_ledger::{
    AttentionRow, Ledger, LedgerError, Located, PrShow, QueueItem, RepoKey, TrackedPr,
};
use std::sync::mpsc;

/// A minimal valid config naming the fixture's repo.
///
/// Parsed rather than built field by field, so it goes through the same
/// deserialisation and validation a real one does.
pub(crate) fn fixture_config() -> HeldConfig {
    Arc::new(
        toml::from_str(
            r#"
            [identity]
            login = "ashb"
            [[project]]
            repos = [{ owner = "apache", name = "airflow" }]
            show_labels = ["area:", "backport"]
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
            return "this ledger was written by a newer reviewq — upgrade to read it".to_string();
        }
        Some(LedgerError::Busy { .. }) => {
            return format!("#{number} is waiting on another reviewq's write — try again");
        }
        _ => {}
    }
    match err.downcast_ref::<ForgeError>() {
        Some(ForgeError::Rejected { host, .. }) => {
            format!("{host} rejected the token — check `reviewq doctor`")
        }
        Some(ForgeError::BudgetSpent { host }) => {
            format!("{host}'s API budget is spent — it refills on the hour")
        }
        Some(ForgeError::NoToken(_)) => {
            format!("no token for #{number}'s host — see `reviewq doctor`")
        }
        _ => format!("#{number} refresh failed: {err:#}"),
    }
}

/// What to put in the header when a sync finished.
///
/// The header has one line, and a per-repo breakdown is what `reviewq sync`
/// prints — so this totals them and names the repo only when there was one, the
/// case where naming it costs nothing and says which.
fn synced_note(summaries: &[RepoSummary]) -> String {
    let new: u64 = summaries.iter().map(|s| s.stats.new).sum();
    let queued: u64 = summaries.iter().map(|s| s.stats.queued).sum();
    let what = match summaries {
        [] => "synced".to_string(),
        [one] => format!("synced {}", one.repo),
        many => format!("synced {} repos", many.len()),
    };
    format!("{what} — {new} new, {queued} on the queue")
}

/// The fixture's config, as the tests have always called it.
#[cfg(test)]
pub(crate) fn test_config() -> HeldConfig {
    fixture_config()
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
use crate::svg;
use crate::theme::{Rgb, Theme};
use crate::ui;

/// Everything on screen, plus the ledger handle it was read from.
pub struct App {
    /// The palette, resolved once at startup.
    pub theme: Theme,
    /// The rows on the left, most-urgent first, spanning every repo the ledger
    /// knows — which of the three lists they are is [`listing`](Self::listing).
    pub queue: Vec<Located<Row>>,
    /// Which of the three lists the rows are.
    pub listing: Listing,
    /// How much is on the lists that are not showing. Held rather than counted
    /// at render time: it takes two ledger reads, and a list you are not looking
    /// at only changes when something you did changed it.
    pub elsewhere: Counts,
    /// Each repo's label colours, by repo id. Read once per reload rather than
    /// per row: a repo's palette is small, and every row on screen wants it.
    pub label_colours: std::collections::HashMap<i64, BTreeMap<String, String>>,
    /// Index into [`queue`](Self::queue) of the highlighted row. Always a valid
    /// index when the queue is non-empty; meaningless when it's empty.
    pub selected: usize,
    /// Full detail for the selected PR, re-read whenever the selection moves.
    /// `None` when the queue is empty, or when the row somehow has no stored
    /// detail.
    pub detail: Option<PrShow>,
    /// A PR being looked at that is not on the queue. While one is, it is what
    /// the detail pane shows and the panes are read-only; Esc puts it away.
    pub peek: Option<Peeked>,
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
    /// And for a PR to look at, which may have to be fetched to show at all.
    pending_peek: Option<u64>,
    /// A screen to save once it has been drawn. Held rather than done on the
    /// keypress because what is wanted is the frame *without* the note saying it
    /// was saved — which is the frame drawn a moment later.
    pending_svg: bool,
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
    /// Whether a full sync is in flight. One at a time: two sweeps of the same
    /// repos would spend the rate-limit budget twice to reach the same ledger,
    /// and the second would race the first for the cursor.
    pub syncing: bool,
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
    /// Begin a full sync of every configured repo. Must not block, for the same
    /// reason [`refresh`](Self::refresh) must not: this is minutes of work on a
    /// large repo, and the queue stays usable throughout — what a sync writes,
    /// the interface reads on the next reload.
    ///
    /// Expected to report progress as [`Message::SyncNote`] and to finish with
    /// exactly one [`Message::Synced`], which is what lets another be started.
    pub sync: Box<dyn Fn(mpsc::Sender<Message>) + Send + Sync>,
    /// Tell the forge a PR's notifications are read. Fire-and-forget: `done` has
    /// already been recorded locally by the time this runs, and nothing waits on
    /// it, so a failure is logged and no more.
    pub mark_read: Box<dyn Fn(u64) + Send + Sync>,
    /// Fetch a PR the ledger has never seen and start tracking it. Blocks, like
    /// the handoff, because the interface has nothing to show until it returns
    /// and the notice explains the wait.
    pub fetch: Box<dyn Fn(u64) -> Result<()> + Send + Sync>,
    /// Read a PR for display without tracking it — from the ledger where it is
    /// stored, from the forge where it isn't. Blocks for the same reason
    /// [`fetch`](Self::fetch) does, and may not have to touch the network at all.
    pub peek: Box<dyn Fn(u64) -> Result<Peeked> + Send + Sync>,
    /// Put a drawn screen somewhere, returning where it went so the header can
    /// say. Given the finished SVG rather than the buffer: composing it is this
    /// crate's business and writing a file is not, which also lets a test read
    /// what would have been saved without touching a disk.
    pub save_screen: Box<dyn Fn(String) -> Result<String> + Send + Sync>,
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

/// Why a PR you asked for isn't on the queue.
///
/// Stored and tracked are different things: a sweep stores every PR it sees, and
/// tracks only the ones a rule matched or that name you. There are usually far
/// more of the former, and they are the ones worth offering to track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unqueued {
    /// The ledger has never seen it, so only the forge can say anything.
    Unknown,
    /// A sweep stored it and no rule matched — the commonest case by far.
    Untracked,
    /// Tracked, but merged or closed: it left the queue and no listing brings it
    /// back.
    Archived(PrState),
    /// Muted: off the queue because you put it there, and the only one of these
    /// you can undo from here.
    Muted,
    /// Snoozed until a moment that has not arrived.
    Snoozed(Timestamp),
    /// Tracked and open, and simply wants nothing right now.
    Quiet,
}

impl Unqueued {
    /// Whether tracking it is worth offering. An already-tracked PR is off the
    /// queue for a reason tracking it again would not touch.
    pub fn trackable(self) -> bool {
        matches!(self, Self::Unknown | Self::Untracked)
    }

    /// Whether unmuting it is worth offering — which is to say, whether it is
    /// muted.
    ///
    /// Muting is the one way off the queue you choose, and it takes the PR out
    /// of the list you would have selected it in to change your mind. So the
    /// undo lives where you land when you ask for it by number.
    pub fn unmutable(self) -> bool {
        matches!(self, Self::Muted)
    }

    /// The one line explaining why it isn't on the queue.
    pub fn reason(self) -> String {
        match self {
            Self::Unknown => "Not in your ledger.".to_string(),
            Self::Untracked => "Stored by a sweep, but no rule matched it.".to_string(),
            Self::Archived(state) => format!(
                "It {} — so it left the queue.",
                match state {
                    PrState::Merged => "merged",
                    _ => "was closed",
                }
            ),
            Self::Muted => "Muted, so nothing on it reaches the queue.".to_string(),
            Self::Snoozed(until) => {
                format!("Snoozed until {}.", reviewq_app::present::stamp(until))
            }
            Self::Quiet => "Tracked, and wants nothing right now.".to_string(),
        }
    }
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
    /// Confirming an `untrack`, which drops the reason a PR is watched at all
    /// — and, unlike a `done`, nothing but tracking it again brings it back.
    ConfirmUntrack {
        /// The PR it would stop watching.
        number: u64,
    },
    /// Picking a snooze duration from presets.
    SnoozePresets {
        /// The PR it would snooze.
        number: u64,
    },
    /// A PR asked for by number that isn't on the queue — offering what can be
    /// done with it rather than refusing.
    ///
    /// A number you typed is a number you meant. Showing it always works and
    /// changes nothing, so that is the offer every case gets; tracking it is
    /// offered on top where it would mean something.
    Unqueued {
        /// The PR asked for.
        number: u64,
        /// Why it isn't on the queue, which is what the offer explains.
        why: Unqueued,
    },
    /// Fetching a PR the ledger had never seen, to track it.
    Fetching {
        /// The PR being fetched.
        number: u64,
    },
    /// Reading a PR for display, which may mean fetching it first.
    Peeking {
        /// The PR being read.
        number: u64,
        /// It is not in the ledger, so this is a round trip to the forge rather
        /// than a read that will be over before the notice is seen. The wait is
        /// the reason the notice exists, so it should say which one it is.
        from_forge: bool,
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

    /// A sync in flight got somewhere worth saying. Advisory: a sync that sent
    /// none of these would still finish, and the interface would simply have
    /// said "syncing…" throughout.
    SyncNote {
        /// What to put in the header, already phrased for a person.
        note: String,
    },

    /// A sync finished, for better or worse. One per [`Hooks::sync`] call,
    /// whatever happened, since it is what puts the interface back in a state
    /// where another can be started.
    Synced {
        /// Each repo's numbers, in the order they were synced, or the error
        /// that stopped the run. Everything committed before a failure stays
        /// committed — the summaries of repos that finished are lost with it,
        /// not their work.
        outcome: Result<Vec<RepoSummary>>,
    },
}

/// How many rows of context to keep between the selection and an edge before the
/// list starts scrolling. Vim calls this `scrolloff`; three is its common value
/// and enough to see what's coming without the list moving under every keypress.
const SCROLLOFF: usize = 3;

/// Re-wrap a ledger read as rows, keeping each one's repo.
fn located<T: Into<Row>>(items: Vec<Located<T>>) -> Vec<Located<Row>> {
    items
        .into_iter()
        .map(|found| Located {
            repo: found.repo,
            repo_id: found.repo_id,
            item: found.item.into(),
        })
        .collect()
}

/// One row of whichever list is on screen.
///
/// The three listings differ in one thing: a queued PR has a reason at the top
/// of its pile and a waiting one has none — that *is* what waiting means. So the
/// reason is the only optional part, and everything else a row shows is the same
/// for all three.
#[derive(Debug, Clone)]
pub struct Row {
    /// The stored snapshot.
    pub pr: PrSnapshot,
    /// The rendered `tracked_reason` — why reviewq is watching it at all.
    pub tracked_reason: String,
    /// The reason it wants attention, when it wants any.
    pub top: Option<AttentionRow>,
    /// My history on it, for the mark in front of the row.
    pub my_state: MyState,
    /// Sunk to the bottom by `reviewq defer`.
    pub deferred: bool,
}

impl Row {
    /// What the row says it is here for: the reason at the top of its pile, or —
    /// for one that wants nothing — the rule that has reviewq watching it.
    pub fn top_text(&self) -> String {
        match &self.top {
            Some(top) => top.reason.to_string(),
            None => self.tracked_reason.clone(),
        }
    }
}

impl From<QueueItem> for Row {
    fn from(item: QueueItem) -> Self {
        Self {
            pr: item.pr,
            tracked_reason: item.tracked_reason,
            top: Some(item.top),
            my_state: item.my_state,
            deferred: item.deferred,
        }
    }
}

impl From<TrackedPr> for Row {
    fn from(item: TrackedPr) -> Self {
        Self {
            pr: item.pr,
            tracked_reason: item.tracked_reason,
            top: None,
            my_state: item.my_state,
            deferred: false,
        }
    }
}

/// How much is on the lists other than the one showing.
///
/// A list nobody can see the size of may as well not exist: the point of
/// counting them where the keys are is that `W` and `M` are worth pressing —
/// or, when both are zero, that there is nothing behind them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Tracked, open, wanting nothing: waiting on somebody else.
    pub waiting: usize,
    /// Silenced by hand.
    pub muted: usize,
}

/// Which list the left pane is showing.
///
/// Two views of the same rows: a muted PR keeps its reasons — the mute is a
/// statement about what you want shown, not about the PR — so what you silenced
/// can be listed with the reason it would have been there for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Listing {
    /// Everything that wants attention and isn't muted.
    #[default]
    Queue,
    /// Tracked, open, and wanting nothing: seen, and waiting on somebody else.
    /// Where a PR goes when you review it — the ball is in the author's court
    /// until they push or reply, and then it is back on the queue by itself.
    Waiting,
    /// Only what is muted.
    Muted,
}

impl Listing {
    /// What the pane is called while it shows this.
    pub fn title(self) -> &'static str {
        match self {
            Self::Queue => "Queue",
            Self::Waiting => "Waiting",
            Self::Muted => "Muted",
        }
    }

    /// What to say when it holds nothing.
    pub fn empty(self) -> &'static str {
        match self {
            Self::Queue => "Nothing on the queue.",
            Self::Waiting => "Nothing waiting on anyone else.",
            Self::Muted => "Nothing muted.",
        }
    }

    /// Toggle to `listing`, or back to the queue if that is already what is
    /// showing — so the key that opened a list is the key that closes it.
    pub fn toggled(self, listing: Self) -> Self {
        if self == listing {
            Self::Queue
        } else {
            listing
        }
    }
}

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
            listing: Listing::default(),
            elsewhere: Counts::default(),
            label_colours: std::collections::HashMap::new(),
            selected: 0,
            detail: None,
            peek: None,
            repo_count: 0,
            focus: Focus::default(),
            detail_scroll: 0,
            overlay: Overlay::None,
            queue_scroll: 0,
            pending_review: None,
            pending_fetch: None,
            pending_peek: None,
            pending_svg: false,
            pending_mark_read: None,
            repaint: false,
            dirty: true,
            help_max_scroll: 0,
            status: None,
            refreshing: BTreeSet::new(),
            syncing: false,
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
        self.queue = self.rows_for(self.listing)?;
        self.selected = held
            .and_then(|(repo, number)| {
                self.queue
                    .iter()
                    .position(|i| i.repo == repo && i.item.pr.number == number)
            })
            .unwrap_or(self.selected)
            .min(self.queue.len().saturating_sub(1));
        self.label_colours = self
            .ledger
            .repos()?
            .into_iter()
            .map(|(repo_id, _)| Ok((repo_id, self.ledger.label_colours(repo_id)?)))
            .collect::<Result<_>>()?;
        self.elsewhere = Counts {
            waiting: self.ledger.waiting_all()?.len(),
            muted: self.ledger.muted_all()?.len(),
        };
        // A list that has emptied under the keyboard takes it back. Submitting a
        // review clears the reason the PR was there for, so the refresh after a
        // handoff can empty the queue while the description pane holds the
        // focus — and a description pane with nothing in it is a pane where the
        // movement keys do nothing and `Tab` looks broken, because the only
        // thing it changes is a border on two empty panes.
        if self.queue.is_empty() {
            self.focus = Focus::Queue;
        }
        self.load_detail()
    }

    /// Show one of the other lists, for a caller arranging a screen rather than
    /// pressing a key.
    pub(crate) fn show_muted(&mut self) {
        self.show(Listing::Muted);
    }

    /// Show what is waiting on somebody else.
    pub(crate) fn show_waiting(&mut self) {
        self.show(Listing::Waiting);
    }

    fn show(&mut self, listing: Listing) {
        self.listing = listing;
        self.reload().expect("reading the list");
    }

    /// The rows one of the two listings holds, straight from the ledger.
    fn rows_for(&self, listing: Listing) -> Result<Vec<Located<Row>>> {
        let rows = match listing {
            Listing::Queue => located(self.ledger.queue_all()?),
            Listing::Muted => located(self.ledger.muted_all()?),
            Listing::Waiting => located(self.ledger.waiting_all()?),
        };
        Ok(rows)
    }

    /// Swap the list for `listing`, or back to the queue if it is already up.
    ///
    /// Back to the top rather than keeping the row: no PR is on two of these at
    /// once, so a kept index would land on an unrelated one.
    fn toggle_listing(&mut self, listing: Listing) -> Result<()> {
        self.listing = self.listing.toggled(listing);
        self.selected = 0;
        self.queue_scroll = 0;
        self.detail_scroll = 0;
        self.status = None;
        self.reload()
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
    pub fn current(&self) -> Option<&Located<Row>> {
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

    /// The labels a row should show, in the order the PR carries them, each with
    /// the colour its own repo paints it.
    ///
    /// Filtered by the project the repo belongs to: a PR in a busy repo carries
    /// a dozen labels and a row has space for two, so a project names the few it
    /// steers by. A repo no project claims shows none — there is nobody to have
    /// said which.
    pub(crate) fn labels_for(&self, row: &Located<Row>) -> Vec<(String, Option<Rgb>)> {
        let Some(project) = self.config.projects.iter().find(|project| {
            project
                .repos
                .iter()
                .any(|configured| configured.key() == row.repo)
        }) else {
            return Vec::new();
        };
        let colours = self.label_colours.get(&row.repo_id);
        row.item
            .pr
            .labels
            .iter()
            .filter(|label| project.shows_label(label))
            .map(|label| {
                let colour = colours
                    .and_then(|colours| colours.get(label))
                    .and_then(|hex| crate::theme::from_hex(hex));
                (label.clone(), colour)
            })
            .collect()
    }

    /// The glyphs to mark queue rows with, as configured.
    pub(crate) fn marks(&self) -> &reviewq_app::config::Marks {
        &self.config.output.marks
    }

    /// The glyphs that label a fact about the PR, as configured.
    pub(crate) fn icons(&self) -> &reviewq_app::config::Icons {
        &self.config.output.icons
    }

    /// Record how far the key reference can usefully scroll, and hold it there.
    /// Called by the renderer, which knows both how many rows it has and how
    /// many fit.
    /// How far the reference can scroll, as the last render measured it.
    #[cfg(test)]
    pub(crate) fn help_max_scroll(&self) -> u16 {
        self.help_max_scroll
    }

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
                let drawn = terminal.draw(|frame| ui::draw(frame, self))?;
                // The frame that was just composed, whatever the backend did
                // with it afterwards — so this works against a test terminal
                // exactly as it does against a real one.
                if std::mem::take(&mut self.pending_svg) {
                    let picture = svg::render(drawn.buffer, &self.theme, &self.config.output.svg);
                    self.status = Some(match (hooks.save_screen)(picture) {
                        Ok(where_it_went) => format!("screen saved to {where_it_went}"),
                        Err(err) => format!("the screen could not be saved: {err:#}"),
                    });
                    self.dirty = true;
                }
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
            if let Some(number) = self.pending_peek.take() {
                self.show_anyway(number, hooks);
                continue;
            }
            // Nothing waits on this one, so it needs no draw of its own.
            if let Some(number) = self.pending_mark_read.take() {
                (hooks.mark_read)(number);
            }

            // Results from tasks first, and without waiting: they may be already
            // queued, and a keystroke should not have to arrive to reveal them.
            self.drain(channel);
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
        // Before anything else claims the keyboard: the screen worth saving is
        // often one an overlay is covering, and a modal that swallowed the key
        // would be the one thing that could never be photographed.
        if keys::action_for(key) == Some(Action::SaveSvg) {
            self.pending_svg = true;
            return Ok(());
        }
        if self.overlay != Overlay::None {
            return self.on_overlay_key(key);
        }
        if self.peek.is_some() {
            return self.on_peek_key(key, channel, hooks);
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
            Update::Passed(Action::SyncAll) => {
                self.start_sync(channel, hooks);
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
            Update::Passed(Action::Untrack) => {
                if let Some(number) = self.selected_number() {
                    self.overlay = Overlay::ConfirmUntrack { number };
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
                | Action::Back
                | Action::Help
                | Action::Jump
                | Action::SwitchPane
                | Action::ToggleTheme
                | Action::SaveSvg
                | Action::ShowMuted
                | Action::ShowWaiting
                | Action::Down
                | Action::Up
                | Action::PageDown
                | Action::PageUp
                | Action::First
                | Action::Last,
            ) => unreachable!("update handles these and returns Handled"),
        }
    }

    /// Handle a keystroke while a PR is being looked at.
    ///
    /// A peeked PR may be merged, closed, or not tracked at all, so nothing that
    /// writes is offered for it: `done` on a PR with no ledger row has nothing to
    /// write to, and acting on the row *behind* the view would be worse. The keys
    /// that only read are all here — scroll it, open it, copy its URL — and Esc
    /// puts it away.
    fn on_peek_key(&mut self, key: KeyEvent, channel: &Channel, hooks: &Hooks) -> Result<()> {
        let page = self.page() as isize;
        let Some(action) = keys::action_for(key) else {
            return Ok(());
        };
        match action {
            Action::Quit => {
                self.quit = true;
                Ok(())
            }
            // The nearest thing to leave is the PR being shown, so Esc puts that
            // away rather than reaching past it to the list behind.
            Action::Back => {
                self.stop_peeking();
                Ok(())
            }
            Action::Help => {
                self.overlay = Overlay::Help { scroll: 0 };
                Ok(())
            }
            Action::ToggleTheme => {
                self.theme = self.theme.toggled();
                Ok(())
            }
            // `dispatch` takes this one before anything else gets the keyboard,
            // so it does not arrive here — but a shown PR is worth photographing
            // too, and refusing it as a write would be wrong.
            Action::SaveSvg => {
                self.pending_svg = true;
                Ok(())
            }
            // Always the description: the queue is still drawn, but it is not
            // what you are reading, and moving its selection under a view of
            // something else would be a change you cannot see.
            Action::Down => self.scroll_peek(1),
            Action::Up => self.scroll_peek(-1),
            Action::PageDown => self.scroll_peek(page),
            Action::PageUp => self.scroll_peek(-page),
            Action::First => self.scroll_peek(isize::MIN),
            Action::Last => self.scroll_peek(isize::MAX),
            Action::OpenInBrowser => {
                let Some(peek) = &self.peek else {
                    return Ok(());
                };
                let (repo, number) = (peek.repo.clone(), peek.show.pr.number);
                self.status = Some(match (hooks.open_url)(&repo, number) {
                    Ok(()) => format!("#{number} opened"),
                    Err(err) => format!("#{number} could not be opened: {err:#}"),
                });
                Ok(())
            }
            Action::CopyUrl => {
                let Some(peek) = &self.peek else {
                    return Ok(());
                };
                let (repo, number) = (peek.repo.clone(), peek.show.pr.number);
                self.status = Some(match (hooks.copy_url)(&repo, number) {
                    Ok(()) => format!("#{number}'s URL copied"),
                    Err(err) => format!("#{number}'s URL could not be copied: {err:#}"),
                });
                Ok(())
            }
            // Swapping lists is reading, not writing, so it works from here —
            // and the shown PR stays shown, since the list is behind it.
            Action::ShowMuted => self.toggle_listing(Listing::Muted),
            Action::ShowWaiting => self.toggle_listing(Listing::Waiting),
            // A sync is about every repo rather than about this PR, so being in
            // a read-only view of one is no reason to refuse it.
            Action::SyncAll => {
                self.start_sync(channel, hooks);
                Ok(())
            }
            // Not a refusal: `Tab` asks for the other pane, and while a PR is
            // on show the other pane is the queue behind it — so the way there
            // is to put this away, which is what the key is reaching for. It
            // refused before, and a key that does nothing reads as a freeze.
            Action::SwitchPane => {
                self.stop_peeking();
                Ok(())
            }
            Action::Jump
            | Action::RefreshSelected
            | Action::Review
            | Action::Done
            | Action::Snooze
            | Action::ToggleMute
            | Action::ToggleDefer
            | Action::Untrack => {
                let number = self.peek.as_ref().map_or(0, |peek| peek.show.pr.number);
                self.status = Some(format!(
                    "#{number} is only being shown — Esc returns to the queue"
                ));
                Ok(())
            }
        }
    }

    /// Where the reference lands after moving `delta` rows from `scroll`.
    ///
    /// Held to what the renderer measured, so neither the keys nor the wheel can
    /// push it past the last row into blank space — the two must agree, and the
    /// measurement is the only thing that knows how much of it fits.
    fn scrolled_help(&self, scroll: u16, delta: isize) -> u16 {
        let target = (scroll as isize).saturating_add(delta).max(0);
        u16::try_from(target)
            .unwrap_or(u16::MAX)
            .min(self.help_max_scroll)
    }

    /// Scroll the peeked description, saturating at both ends.
    fn scroll_peek(&mut self, delta: isize) -> Result<()> {
        let target = (self.detail_scroll as isize).saturating_add(delta);
        self.detail_scroll = target.max(0).try_into().unwrap_or(u16::MAX);
        self.clamp_detail_scroll();
        Ok(())
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
    pub(crate) fn on_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        // An overlay owns the keyboard, and the mouse with it: the rows are still
        // drawn underneath, so a click on one you cannot see would act on
        // whatever the modal is covering. The reference is the exception — it is
        // the one overlay that can outgrow the screen, and a panel you can scroll
        // with the keys should scroll with the wheel wherever the pointer is.
        if let Overlay::Help { scroll } = self.overlay {
            let rows = match mouse.kind {
                MouseEventKind::ScrollUp => -WHEEL_ROWS,
                MouseEventKind::ScrollDown => WHEEL_ROWS,
                _ => return Ok(()),
            };
            self.overlay = Overlay::Help {
                scroll: self.scrolled_help(scroll, rows),
            };
            return Ok(());
        }
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

        // A peeked PR owns the screen the way the keyboard sees it, so the wheel
        // scrolls what is being read wherever the pointer is. A click on the
        // queue is the other thing somebody does to leave: those rows are drawn
        // and clickable, and pointing at one is not ambiguous — it says put this
        // away and take me to that. Ignoring it, as this did, leaves a reader
        // clicking a row that never lights up.
        if self.peek.is_some() {
            return match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_peek(-WHEEL_ROWS),
                MouseEventKind::ScrollDown => self.scroll_peek(WHEEL_ROWS),
                MouseEventKind::Down(MouseButton::Left) if pane == Focus::Queue => {
                    self.stop_peeking();
                    self.select_row_at(mouse.row)
                }
                _ => Ok(()),
            };
        }

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
                let page = isize::try_from(self.page()).unwrap_or(isize::MAX);
                let moved = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => Some(self.scrolled_help(scroll, 1)),
                    KeyCode::Up | KeyCode::Char('k') => Some(self.scrolled_help(scroll, -1)),
                    KeyCode::PageDown => Some(self.scrolled_help(scroll, page)),
                    KeyCode::PageUp => Some(self.scrolled_help(scroll, -page)),
                    KeyCode::Home | KeyCode::Char('g') => Some(0),
                    KeyCode::End | KeyCode::Char('G') => Some(self.help_max_scroll),
                    _ => None,
                };
                self.overlay = match moved {
                    Some(scroll) => Overlay::Help { scroll },
                    None => Overlay::None,
                };
                Ok(())
            }
            Overlay::Unqueued { number, why } => {
                // Both are held for the loop, so the notice is drawn before
                // either blocks on the forge — same reason as the handoff.
                match key.code {
                    KeyCode::Char('s') | KeyCode::Enter => {
                        self.overlay = Overlay::Peeking {
                            number,
                            // Everything but `Unknown` is stored, and a stored
                            // PR is read from the ledger without a network in
                            // sight.
                            from_forge: why == Unqueued::Unknown,
                        };
                        self.pending_peek = Some(number);
                    }
                    KeyCode::Char('t') if why.trackable() => {
                        self.overlay = Overlay::Fetching { number };
                        self.pending_fetch = Some(number);
                    }
                    KeyCode::Char('u') if why.unmutable() => {
                        self.overlay = Overlay::None;
                        self.unmute(number)?;
                    }
                    _ => self.overlay = Overlay::None,
                }
                Ok(())
            }
            // Nothing to accept: the loop replaces these when the work returns.
            Overlay::Fetching { .. } | Overlay::Peeking { .. } => Ok(()),
            Overlay::ConfirmDone { number } => {
                let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
                self.overlay = Overlay::None;
                if confirmed {
                    self.mark_done(number)?;
                }
                Ok(())
            }
            Overlay::ConfirmUntrack { number } => {
                let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Enter);
                self.overlay = Overlay::None;
                if confirmed {
                    self.untrack(number)?;
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

    /// Stop watching the selected PR, and say what undoes it.
    ///
    /// It leaves every list at once — the queue, waiting and muted all ask for a
    /// tracked reason — so the row under the cursor goes with it and the reload
    /// picks whatever is now in its place.
    fn untrack(&mut self, number: u64) -> Result<()> {
        let Some(repo_id) = self.selected_repo_id() else {
            return Ok(());
        };
        self.status = Some(
            if reviewq_app::actions::untrack(&self.ledger, repo_id, number)? {
                format!("#{number} untracked — `reviewq track {number}` puts it back")
            } else {
                format!("#{number} is not in the ledger")
            },
        );
        self.reload()
    }

    /// Select the PR `text` names, or offer what else can be done with it.
    ///
    /// A number that isn't on the queue is never a refusal: showing a PR is
    /// read-only and always possible, whatever state it is in and whether or not
    /// the ledger has ever seen it. Why it isn't queued still decides what the
    /// offer says, and whether tracking it would mean anything.
    fn jump_to(&mut self, text: &str) -> Result<()> {
        let number = self
            .pr_number_in(text)
            .with_context(|| format!("{text:?} is not a PR number"))?;

        if let Some(index) = self.row_of(number) {
            self.status = None;
            return self.move_to(index);
        }

        // It may be on one of the lists you are not looking at, which is a place
        // to be taken rather than a reason to be refused — asking for a PR by
        // number says nothing about which view you happened to have up.
        for elsewhere in [Listing::Queue, Listing::Waiting, Listing::Muted] {
            if elsewhere == self.listing {
                continue;
            }
            let Some(index) = self
                .rows_for(elsewhere)?
                .iter()
                .position(|item| item.item.pr.number == number)
            else {
                continue;
            };
            self.listing = elsewhere;
            self.reload()?;
            self.status = Some(match elsewhere {
                Listing::Queue => format!("#{number} is on the queue"),
                Listing::Waiting => format!("#{number} is waiting on someone else"),
                Listing::Muted => format!("#{number} is muted — `m` unmutes it"),
            });
            return self.select(index);
        }

        self.overlay = Overlay::Unqueued {
            number,
            why: self.why_unqueued(number, Timestamp::now())?,
        };
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

    /// Why a PR that isn't on the queue isn't, as far as the ledger knows.
    ///
    /// `now` decides only whether a snooze has lapsed: one that has is not a
    /// reason for anything, and saying so would send you looking for a
    /// suppression that expired.
    fn why_unqueued(&self, number: u64, now: Timestamp) -> Result<Unqueued> {
        for (repo_id, _) in self.ledger.repos()? {
            let Some(show) = self.ledger.show(repo_id, number)? else {
                continue;
            };
            // In the order that decides what to say: what you did to it outranks
            // what became of it, because it is the part you can change your mind
            // about — and "wants nothing right now" is a poor answer to "where
            // is the PR I muted?".
            return Ok(if show.tracked_reason.is_none() {
                Unqueued::Untracked
            } else if show.my_state.muted {
                Unqueued::Muted
            } else if let Some(until) = show.my_state.snoozed_until.filter(|&until| now < until) {
                Unqueued::Snoozed(until)
            } else if !show.pr.state.is_open() {
                Unqueued::Archived(show.pr.state)
            } else {
                Unqueued::Quiet
            });
        }
        Ok(Unqueued::Unknown)
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

    /// Unmute a PR that isn't on the queue, found by number.
    ///
    /// Unlike [`toggle_mute`](Self::toggle_mute) this takes a number rather than
    /// the selection, because a muted PR is precisely one the selection cannot
    /// reach.
    fn unmute(&mut self, number: u64) -> Result<()> {
        let Some((repo_id, _)) = self.ledger.repos()?.into_iter().find(|(repo_id, _)| {
            self.ledger
                .show(*repo_id, number)
                .is_ok_and(|show| show.is_some())
        }) else {
            return Ok(());
        };
        reviewq_app::actions::set_muted(&self.ledger, repo_id, number, false)?;
        self.status = Some(format!("#{number} unmuted — back on the queue"));
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
            format!("#{number} unmuted — back on the queue")
        } else {
            format!("#{number} muted — `M` lists what you have silenced")
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
            // One list deep, Esc is the way back out of it — quitting from there
            // would throw away the queue as well as the list, and the key that
            // opened the list is not the one a hand reaches for to close it.
            Action::Back if self.listing != Listing::Queue => {
                handled(self.toggle_listing(Listing::Queue))
            }
            Action::Back => {
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
            Action::SaveSvg => {
                self.pending_svg = true;
                Ok(Update::Handled)
            }
            Action::ShowMuted => handled(self.toggle_listing(Listing::Muted)),
            Action::ShowWaiting => handled(self.toggle_listing(Listing::Waiting)),
            Action::Down => handled(self.scroll(1)),
            Action::Up => handled(self.scroll(-1)),
            Action::PageDown => handled(self.scroll(page)),
            Action::PageUp => handled(self.scroll(-page)),
            Action::First => handled(self.scroll_to_start()),
            Action::Last => handled(self.scroll_to_end()),
            // Not this layer's: performing these needs the hooks, the channel or
            // the ledger. Handed back rather than silently ignored.
            Action::RefreshSelected
            | Action::SyncAll
            | Action::Untrack
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

    /// Read `number` for display, and show it if it could be read.
    fn show_anyway(&mut self, number: u64, hooks: &Hooks) {
        let outcome = (hooks.peek)(number);
        self.overlay = Overlay::None;
        self.dirty = true;
        match outcome {
            Ok(peeked) => {
                // The description is the point of looking, so the pane it scrolls
                // in gets the keyboard and starts at the top.
                self.focus = Focus::Detail;
                self.detail_scroll = 0;
                self.status = Some(format!("showing #{number} — Esc returns to the queue"));
                self.peek = Some(peeked);
            }
            Err(err) => self.status = Some(format!("#{number} could not be shown: {err:#}")),
        }
    }

    /// Put a peeked PR away, back to the queue as it was.
    fn stop_peeking(&mut self) {
        self.peek = None;
        self.focus = Focus::Queue;
        self.detail_scroll = 0;
        self.status = None;
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

    /// Take in everything finished work has reported, without waiting for any
    /// of it: a message may be queued already, and a keystroke should not have
    /// to arrive to reveal it.
    fn drain(&mut self, channel: &Channel) {
        while let Ok(message) = channel.rx.try_recv() {
            match message {
                Message::Refreshed { number, outcome } => self.on_refreshed(number, outcome),
                // Only while a sync is running: a note that outlived the run it
                // came from would leave the header describing work that has
                // already been summarised.
                Message::SyncNote { note } if self.syncing => self.status = Some(note),
                Message::SyncNote { .. } => {}
                Message::Synced { outcome } => self.on_synced(outcome),
            }
            // Work landing is a change like any other, and the one that arrives
            // without anybody pressing a key.
            self.dirty = true;
        }
    }

    /// Start a full sync, unless one is already running.
    ///
    /// Nothing waits for it: the queue you are reading is the ledger's last
    /// committed state, and the sweep writes through a connection of its own —
    /// so the rows only change when [`on_synced`](Self::on_synced) reloads them.
    fn start_sync(&mut self, channel: &Channel, hooks: &Hooks) {
        if self.syncing {
            self.status = Some("already syncing".into());
            return;
        }
        self.syncing = true;
        self.status = Some("syncing…".into());
        (hooks.sync)(channel.tx.clone());
    }

    /// Take in a finished sync: report what it did, and re-read the ledger it
    /// wrote to.
    ///
    /// A failure is the status line rather than the end of the session, as a
    /// refresh's is — and what a failed run committed before it stopped is
    /// still there to be read, so it reloads either way.
    fn on_synced(&mut self, outcome: Result<Vec<RepoSummary>>) {
        self.syncing = false;
        self.status = Some(match outcome {
            Ok(summaries) => synced_note(&summaries),
            Err(err) => format!("sync failed: {err:#}"),
        });
        if let Err(err) = self.reload() {
            self.status = Some(format!("reload failed: {err:#}"));
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
            // What the PR *is* outranks what it wants: a PR that turns out to
            // have been closed or merged since the last sweep wants nothing by
            // definition, and reporting that as "wants nothing" says the fetch
            // found nothing new when it found the only thing that mattered.
            Ok(Refreshed::Updated {
                state: PrState::Closed,
                ..
            }) => format!("#{number} is closed on the forge"),
            Ok(Refreshed::Updated {
                state: PrState::Merged,
                ..
            }) => format!("#{number} is merged"),
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
        if index == self.selected {
            return Ok(());
        }
        self.select(index)
    }

    /// Put the selection on `index` and read what it points at, whether or not
    /// it is where the selection already was — which is what a jump that has
    /// just swapped lists needs, since row 0 of the new one is a different PR.
    fn select(&mut self, index: usize) -> Result<()> {
        if self.queue.is_empty() {
            return Ok(());
        }
        self.selected = index.min(self.queue.len() - 1);
        self.keep_selection_visible();
        // A new PR means a new description: keeping the old offset would open
        // it halfway down.
        self.detail_scroll = 0;
        self.load_detail()
    }

    /// Where `number` sits in the list on screen, if it is on it.
    fn row_of(&self, number: u64) -> Option<usize> {
        self.queue
            .iter()
            .position(|item| item.item.pr.number == number)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use jiff::Timestamp;
    use reviewq_core::model::{Attention, AttentionReason, MyState, PrSnapshot, PrState};
    use reviewq_ledger::TrackedReason;

    pub(super) fn ts(s: &str) -> Timestamp {
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
            created_at: None,
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
                Some(TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
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
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        };
        ledger
            .upsert_pr(
                repo_id,
                &pr,
                Some(TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
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
                state: PrState::Open,
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
    fn a_rejected_token_is_told_apart_from_the_forge_being_unreachable() {
        // The point of the forge's errors being typed: one of these needs a new
        // token and the other needs nothing but time.
        let mut app = app();
        app.deliver(
            70135,
            Err(ForgeError::Rejected {
                host: "github.com".into(),
                source: "Bad credentials".into(),
            }
            .into()),
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("rejected the token"), "{status}");

        app.deliver(
            70135,
            Err(ForgeError::BudgetSpent {
                host: "github.com".into(),
            }
            .into()),
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("budget is spent"), "{status}");
        assert!(status.contains("refills"), "{status}");
    }

    #[test]
    fn any_other_failure_is_quoted_as_it_arrived() {
        let mut app = app();
        app.deliver(70135, Err(anyhow::anyhow!("something nobody typed")));

        let status = app.status.clone().expect("a status");
        assert!(status.contains("something nobody typed"), "{status}");
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
                    Some(TrackedReason::Interest {
                        rule: "label x".into(),
                        after_merge: false,
                    }),
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
    use super::tests::{add_queued, app, fixture, pr_snapshot, seed, ts, two_queued};
    use super::*;
    use anyhow::bail;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use reviewq_core::model::MyState;
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
        /// Screens handed to the save hook — the SVG itself, not a path, since
        /// nothing here writes one.
        saved: Arc<Mutex<Vec<String>>>,
        refreshed: Seen,
        /// How many full syncs were asked for — a sync names nothing, so a
        /// count is all there is to record.
        synced: Arc<Mutex<usize>>,
        marked: Seen,
        reviewed: Seen,
        fetched: Seen,
        peeked: Seen,
        opened: Seen,
        copied: Seen,
    }

    /// One repo's sync outcome, with only the two counters the header reports
    /// set — the rest is what `reviewq sync` prints and this never reads.
    fn summary(repo: &str, new: u64, queued: u64) -> RepoSummary {
        RepoSummary {
            repo: repo.to_string(),
            stats: reviewq_app::sync::Stats {
                new,
                queued,
                ..Default::default()
            },
            tracked: 0,
            total: 0,
            truncated: false,
        }
    }

    /// What the peek hook hands back: a PR that exists on the forge and nowhere
    /// else, which is the case the real one has to fetch.
    fn scratch_peek(number: u64) -> Peeked {
        Peeked {
            repo: RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            },
            show: PrShow {
                pr: pr_snapshot(number),
                body: Some("a description".into()),
                tracked_reason: None,
                after_merge: false,
                my_state: MyState::default(),
                threads: vec![],
                reviewers: vec![],
                attention: vec![],
            },
            scratch: true,
        }
    }

    /// Hooks over `script`. `answer` is what a refresh reports back, if anything;
    /// `review_fails` makes the handoff error.
    fn fake_hooks(
        script: Vec<Event>,
        answer: Option<Refreshed>,
        review_fails: bool,
    ) -> (Hooks, Recorded) {
        let recorded = Recorded {
            saved: Arc::new(Mutex::new(Vec::new())),
            refreshed: Arc::new(Mutex::new(Vec::new())),
            synced: Arc::new(Mutex::new(0)),
            marked: Arc::new(Mutex::new(Vec::new())),
            reviewed: Arc::new(Mutex::new(Vec::new())),
            fetched: Arc::new(Mutex::new(Vec::new())),
            peeked: Arc::new(Mutex::new(Vec::new())),
            opened: Arc::new(Mutex::new(Vec::new())),
            copied: Arc::new(Mutex::new(Vec::new())),
        };
        let saved = Arc::clone(&recorded.saved);
        let refreshed = Arc::clone(&recorded.refreshed);
        let synced = Arc::clone(&recorded.synced);
        let marked = Arc::clone(&recorded.marked);
        let reviewed = Arc::clone(&recorded.reviewed);
        let fetched = Arc::clone(&recorded.fetched);
        let peeked = Arc::clone(&recorded.peeked);
        let opened = Arc::clone(&recorded.opened);
        let copied = Arc::clone(&recorded.copied);
        let hooks = Hooks {
            fetch: Box::new(move |number| {
                fetched.lock().expect("lock").push(number);
                Ok(())
            }),
            peek: Box::new(move |number| {
                peeked.lock().expect("lock").push(number);
                Ok(scratch_peek(number))
            }),
            save_screen: Box::new(move |picture| {
                saved.lock().expect("lock").push(picture);
                Ok("/tmp/reviewq.svg".to_string())
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
            // Started and never heard from again, which is what lets a test
            // see the interface's in-flight state at all.
            sync: Box::new(move |_tx| *synced.lock().expect("lock") += 1),
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
                state: PrState::Open,
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
    fn u_asks_first_and_a_confirmation_stops_watching_the_pr_for_good() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![], None, false);
        feed(&mut app, &hooks, &[KeyCode::Char('u')]);
        assert_eq!(app.overlay, Overlay::ConfirmUntrack { number: 70135 });
        assert_eq!(app.queue.len(), 1, "asking is not doing");

        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![press('u'), press('y'), press('q')], None, false);
        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 untracked"), "{status}");
        assert!(
            status.contains("track 70135"),
            "and says what puts it back: {status}"
        );
        assert!(app.queue.is_empty());
        // Not merely off the queue: off every list, which is what `done` and
        // `mute` deliberately are not.
        app.listing = Listing::Waiting;
        app.reload().expect("reload");
        assert!(app.queue.is_empty());
        app.listing = Listing::Muted;
        app.reload().expect("reload");
        assert!(app.queue.is_empty());
    }

    #[test]
    fn declining_an_untrack_leaves_the_pr_watched() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![press('u'), press('n'), press('q')], None, false);

        drive(&mut app, &hooks);

        assert_eq!(app.overlay, Overlay::None);
        assert!(app.status.is_none());
        assert_eq!(app.queue.len(), 1);
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

        let status = app.status.clone().expect("a status");
        assert!(status.starts_with("#70135 muted"), "{status}");
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
    fn reviewing_the_last_pr_does_not_strand_you_on_an_empty_detail() {
        // What actually happens after a review: submitting one clears the reason
        // the PR was there for, so the refresh that follows the handoff drops it
        // off the queue — and if it was the only one, the queue empties under
        // you. The keyboard must not be left pointing at a pane with nothing in
        // it and nothing to move.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reviewq.db");
        let ledger = Ledger::open(&path).expect("ledger");
        let repo_id = seed(&ledger);
        add_queued(&ledger, repo_id, 70135);
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        assert_eq!(app.queue.len(), 1);

        let write_path = path.clone();
        let hooks = Hooks {
            next_event: scripted(vec![
                special(KeyCode::Tab),
                special(KeyCode::Enter),
                press('q'),
            ]),
            // The real refresh: a submitted review leaves the PR wanting
            // nothing, so its attention goes.
            refresh: Box::new(move |number, tx| {
                let other = Ledger::open(&write_path).expect("second connection");
                let repo_id = other.repos().expect("repos")[0].0;
                other.clear_attention(repo_id, number).expect("cleared");
                let _ = tx.send(Message::Refreshed {
                    number,
                    outcome: Ok(Refreshed::Updated {
                        state: PrState::Open,
                        repo: "apache/airflow".into(),
                        queued: false,
                        cost: 1,
                        remaining: 4900,
                    }),
                });
            }),
            fetch: Box::new(|_| Ok(())),
            peek: Box::new(|number| Ok(scratch_peek(number))),
            save_screen: Box::new(|_| Ok(String::new())),
            sync: Box::new(|_| {}),
            mark_read: Box::new(|_| {}),
            review: Box::new(|_| Ok(())),
            open_url: Box::new(|_, _| Ok(())),
            copy_url: Box::new(|_, _| Ok(())),
        };

        drive(&mut app, &hooks);

        assert!(app.queue.is_empty(), "the PR left the queue, as it should");
        assert_eq!(
            app.focus,
            Focus::Queue,
            "so the keyboard belongs back on the list, not on a blank description"
        );
    }

    #[test]
    fn a_reviewed_pr_is_findable_under_w_rather_than_gone() {
        // Where a PR goes when you review it: off the queue, because the ball is
        // in the author's court — and onto the list of what you are waiting on,
        // which is the part that used to exist only on the CLI.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reviewq.db");
        let ledger = Ledger::open(&path).expect("ledger");
        let repo_id = seed(&ledger);
        add_queued(&ledger, repo_id, 70135);
        add_queued(&ledger, repo_id, 70201);
        // The shape a submitted review leaves behind: the reason it was queued
        // for has gone, the review itself is on the record.
        ledger.clear_attention(repo_id, 70135).expect("reviewed");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70201],
            "the queue is what still wants something"
        );

        feed(&mut app, &hooks, &[KeyCode::Char('W')]);

        assert_eq!(app.listing, Listing::Waiting);
        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70135],
            "and it is here, not gone"
        );
        assert!(
            app.queue[0].item.top.is_none(),
            "with no reason, which is what waiting means"
        );

        // The same key puts it away again.
        feed(&mut app, &hooks, &[KeyCode::Char('W')]);
        assert_eq!(app.listing, Listing::Queue);
    }

    #[test]
    fn a_waiting_row_says_why_it_is_watched_since_it_wants_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ledger = Ledger::open(&dir.path().join("reviewq.db")).expect("ledger");
        let repo_id = seed(&ledger);
        add_queued(&ledger, repo_id, 70135);
        ledger.clear_attention(repo_id, 70135).expect("reviewed");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        app.listing = Listing::Waiting;
        app.reload().expect("reload");

        assert_eq!(app.queue[0].item.top_text(), "involved: manual");
    }

    #[test]
    fn shift_m_swaps_between_the_queue_and_what_is_muted() {
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        reviewq_app::actions::set_muted(&ledger, repo_id, 70201, true).expect("mute");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        // The queue has the one that is not muted, and says nothing of the other.
        assert_eq!(app.listing, Listing::Queue);
        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70135]
        );

        feed(&mut app, &hooks, &[KeyCode::Char('M')]);

        assert_eq!(app.listing, Listing::Muted);
        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70201],
            "and the muted list has the other, with its reason intact"
        );
        assert_eq!(app.selected, 0, "starting at the top of a different list");

        feed(&mut app, &hooks, &[KeyCode::Char('M')]);
        assert_eq!(app.listing, Listing::Queue);
    }

    #[test]
    fn shift_s_starts_one_sync_and_the_queue_stays_usable_while_it_runs() {
        // The point of the key: a sweep is minutes of work on a large repo, so
        // it happens behind the queue rather than in place of it.
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, recorded) = fake_hooks(vec![], None, false);

        feed(&mut app, &hooks, &[KeyCode::Char('S')]);

        assert_eq!(*recorded.synced.lock().expect("lock"), 1);
        assert!(app.syncing, "and the interface knows one is in flight");
        assert_eq!(app.status.as_deref(), Some("syncing…"));

        // Moving about still works, and the rows are the ones the ledger last
        // committed — a sweep in flight has changed nothing yet.
        feed(&mut app, &hooks, &[KeyCode::Char('j')]);
        assert_eq!(app.selected, 1);
        assert_eq!(app.queue.len(), 2);

        // A second press is not a second sweep: it would spend the budget twice
        // to reach the same ledger.
        feed(&mut app, &hooks, &[KeyCode::Char('S')]);
        assert_eq!(*recorded.synced.lock().expect("lock"), 1);
        assert_eq!(app.status.as_deref(), Some("already syncing"));
    }

    #[test]
    fn a_finished_sync_reports_what_it_did_and_re_reads_the_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reviewq.db");
        let ledger = Ledger::open(&path).expect("ledger");
        let repo_id = seed(&ledger);
        add_queued(&ledger, repo_id, 70201);
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        assert_eq!(app.queue.len(), 2);
        let (hooks, _) = fake_hooks(vec![], None, false);
        feed(&mut app, &hooks, &[KeyCode::Char('S')]);

        // What the sync task would have written, through a connection of its
        // own — which is how the real one reaches the ledger this app is reading.
        let other = Ledger::open(&path).expect("second connection");
        other.clear_attention(repo_id, 70201).expect("cleared");

        app.on_synced(Ok(vec![summary("apache/airflow", 3, 1)]));

        assert!(!app.syncing);
        assert_eq!(
            app.status.as_deref(),
            Some("synced apache/airflow — 3 new, 1 on the queue")
        );
        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70135],
            "the reload picked up what the sync committed"
        );
    }

    #[test]
    fn a_failed_sync_says_so_and_keeps_the_queue() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);
        feed(&mut app, &hooks, &[KeyCode::Char('S')]);

        app.on_synced(Err(anyhow::anyhow!("github.com rejected the token")));

        assert!(!app.syncing, "and another can be started");
        assert_eq!(
            app.status.as_deref(),
            Some("sync failed: github.com rejected the token")
        );
        assert_eq!(app.queue.len(), 2, "a failure discards nothing");
    }

    #[test]
    fn a_syncs_progress_reaches_the_header_only_while_it_is_running() {
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);
        let channel = Channel::new();

        channel
            .tx
            .send(Message::SyncNote {
                note: "syncing — updated 50/300".into(),
            })
            .expect("send");
        app.drain(&channel);
        assert_eq!(
            app.status, None,
            "nothing is syncing, so this is a note about work already summarised"
        );

        feed(&mut app, &hooks, &[KeyCode::Char('S')]);
        channel
            .tx
            .send(Message::SyncNote {
                note: "syncing — updated 50/300".into(),
            })
            .expect("send");
        app.drain(&channel);
        assert_eq!(app.status.as_deref(), Some("syncing — updated 50/300"));
    }

    #[test]
    fn esc_leaves_a_list_before_it_leaves_the_interface() {
        // Esc means "out of this" everywhere else in the interface — an overlay,
        // a shown PR — so a list it quit out of would be the one place it threw
        // away more than what you were looking at.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        reviewq_app::actions::set_muted(&ledger, repo_id, 70201, true).expect("mute");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        for list in [KeyCode::Char('M'), KeyCode::Char('W')] {
            feed(&mut app, &hooks, &[list, KeyCode::Esc]);
            assert_eq!(app.listing, Listing::Queue, "{list:?} then Esc");
            assert!(!app.quit, "{list:?} then Esc must not quit");
        }

        // On the queue there is nothing left to leave, so it still does.
        feed(&mut app, &hooks, &[KeyCode::Esc]);
        assert!(app.quit);
    }

    #[test]
    fn unmuting_from_the_muted_list_puts_the_pr_back_at_once() {
        // The reasons were never erased, so there is nothing to wait for.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        reviewq_app::actions::set_muted(&ledger, repo_id, 70201, true).expect("mute");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(&mut app, &hooks, &[KeyCode::Char('M'), KeyCode::Char('m')]);

        assert!(app.queue.is_empty(), "nothing muted any more");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("unmuted"), "{status}");

        feed(&mut app, &hooks, &[KeyCode::Char('M')]);
        assert_eq!(
            app.queue
                .iter()
                .map(|item| item.item.pr.number)
                .collect::<Vec<_>>(),
            vec![70135, 70201],
            "and it is back on the queue with the rest"
        );
    }

    #[test]
    fn going_to_a_muted_pr_from_the_queue_takes_you_to_it() {
        // Asking for a PR by number says nothing about which list you had up, so
        // being on the wrong one is not a reason to refuse.
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

        assert_eq!(app.listing, Listing::Muted, "it switched to find it");
        assert_eq!(app.selected_number(), Some(70201));
        assert_eq!(app.overlay, Overlay::None, "no offer was needed");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("muted"), "{status}");
    }

    #[test]
    fn a_muted_pr_with_no_reasons_is_explained_and_unmuted_from_the_offer() {
        // Muted and quiet: it is on neither list — the muted list is built from
        // the reasons a mute hides, and this one has none — so being asked for by
        // number is the only way to reach it, and the offer is where the undo has
        // to live.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let mut quiet = pr_snapshot(70999);
        quiet.labels.clear();
        ledger
            .upsert_pr(
                repo_id,
                &quiet,
                Some(reviewq_ledger::TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
                ts("2026-08-11T12:00:00Z"),
            )
            .expect("tracked, no detail");
        reviewq_app::actions::set_muted(&ledger, repo_id, 70999, true).expect("mute");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('7'),
                KeyCode::Char('0'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Enter,
            ],
        );

        assert_eq!(
            app.overlay,
            Overlay::Unqueued {
                number: 70999,
                why: Unqueued::Muted
            },
            "it says what you did to it, not that it wants nothing"
        );

        feed(&mut app, &hooks, &[KeyCode::Char('u')]);

        assert_eq!(app.overlay, Overlay::None);
        let repo_id = app.ledger.repos().expect("repos")[0].0;
        assert!(!app.ledger.my_state(repo_id, 70999).expect("state").muted);
    }

    #[test]
    fn a_snoozed_pr_says_when_it_comes_back() {
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let until = ts("2099-01-01T00:00:00Z");
        reviewq_app::actions::snooze(&ledger, repo_id, 70201, until).expect("snooze");
        let app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        let why = app
            .why_unqueued(70201, ts("2026-08-12T09:00:00Z"))
            .expect("why");

        assert_eq!(why, Unqueued::Snoozed(until));
        assert!(why.reason().contains("2099-01-01"), "{}", why.reason());
        assert!(!why.unmutable(), "a snooze is not undone from here");
    }

    #[test]
    fn a_snooze_that_has_lapsed_is_not_a_reason_for_anything() {
        // It is off the queue until the next sync recomputes it, which is
        // "quiet" — pointing at a suppression that has expired would send you
        // looking for something to undo that is already undone.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        reviewq_app::actions::snooze(&ledger, repo_id, 70201, ts("2026-08-12T08:00:00Z"))
            .expect("snooze");
        let app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        assert_eq!(
            app.why_unqueued(70201, ts("2026-08-12T09:00:00Z"))
                .expect("why"),
            Unqueued::Quiet
        );
    }

    #[test]
    fn going_to_a_stored_but_untracked_pr_offers_to_track_it() {
        // The common case by far — a sweep stores every PR it sees and tracks only
        // what a rule matched, so most stored PRs are untracked. This used to say
        // "tracked but not on the queue — try `list --all`", which lists tracked
        // PRs only, so the advice could not have worked.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let mut swept = pr_snapshot(70999);
        swept.labels.clear();
        ledger
            .upsert_pr(repo_id, &swept, None, ts("2026-08-11T12:00:00Z"))
            .expect("stored, untracked");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('7'),
                KeyCode::Char('0'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Enter,
            ],
        );

        assert_eq!(
            app.overlay,
            Overlay::Unqueued {
                number: 70999,
                why: Unqueued::Untracked
            }
        );
    }

    #[test]
    fn going_to_a_merged_pr_says_it_left_the_queue() {
        // Tracking it again would change nothing — it is off the queue because it
        // merged — so the only thing offered is to show it.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let mut merged = pr_snapshot(70777);
        merged.state = PrState::Merged;
        ledger
            .upsert_pr(
                repo_id,
                &merged,
                Some(reviewq_ledger::TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
                ts("2026-08-11T12:00:00Z"),
            )
            .expect("stored");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut app,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('7'),
                KeyCode::Char('0'),
                KeyCode::Char('7'),
                KeyCode::Char('7'),
                KeyCode::Char('7'),
                KeyCode::Enter,
            ],
        );

        assert_eq!(
            app.overlay,
            Overlay::Unqueued {
                number: 70777,
                why: Unqueued::Archived(PrState::Merged)
            }
        );
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

        assert_eq!(
            app.overlay,
            Overlay::Unqueued {
                number: 9,
                why: Unqueued::Unknown
            }
        );
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
                KeyCode::Char('t'),
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
                press('t'),
                press('q'),
            ]),
            fetch: Box::new(move |number| {
                seen.lock().expect("lock").push(number);
                let other = Ledger::open(&write_path).expect("second connection");
                add_queued(&other, repo_id, number);
                Ok(())
            }),
            peek: Box::new(|number| Ok(scratch_peek(number))),
            save_screen: Box::new(|_| Ok(String::new())),
            refresh: Box::new(|_, _| {}),
            sync: Box::new(|_| {}),
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

    /// Go to #9 — which nothing knows about — and take the offer to show it.
    fn showing_nine(extra: &[Event]) -> (App, Recorded) {
        let mut app = app();
        let mut script = vec![press(':'), press('9'), special(KeyCode::Enter), press('s')];
        script.extend_from_slice(extra);
        script.push(press('q'));
        let (hooks, recorded) = fake_hooks(script, None, false);
        drive(&mut app, &hooks);
        (app, recorded)
    }

    #[test]
    fn showing_an_unqueued_pr_displays_it_without_tracking_it() {
        // The whole point of the offer: reading a PR that is merged, closed or
        // unknown must not mean committing to keep it.
        let (app, recorded) = showing_nine(&[]);

        assert_eq!(*recorded.peeked.lock().expect("lock"), vec![9]);
        assert!(
            recorded.fetched.lock().expect("lock").is_empty(),
            "showing it is not tracking it"
        );
        let peek = app.peek.as_ref().expect("it is on show");
        assert_eq!(peek.show.pr.number, 9);
        assert_eq!(
            app.focus,
            Focus::Detail,
            "the keys should scroll what you asked to read"
        );
        assert_eq!(
            app.current().map(|item| item.item.pr.number),
            Some(70135),
            "and the queue's selection is where it was"
        );
    }

    #[test]
    fn escape_puts_a_shown_pr_away_rather_than_quitting() {
        let (app, _) = showing_nine(&[special(KeyCode::Esc)]);

        assert!(app.peek.is_none());
        assert_eq!(app.focus, Focus::Queue);
        assert!(app.status.is_none(), "and says nothing about it afterwards");
    }

    #[test]
    fn the_wait_for_a_pr_says_which_wait_it_is() {
        // A PR the ledger has never seen costs a round trip; one it has is read
        // and drawn before anybody reads the notice. The notice exists for the
        // first, so it should not claim the second.
        let mut unknown = app();
        let (hooks, _) = fake_hooks(vec![], None, false);

        feed(
            &mut unknown,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('9'),
                KeyCode::Enter,
                KeyCode::Char('s'),
            ],
        );
        assert_eq!(
            unknown.overlay,
            Overlay::Peeking {
                number: 9,
                from_forge: true
            },
            "#9 is unknown, so it has to be fetched"
        );

        // A stored one is a local read, and saying "fetching" of it would be a
        // lie. Merged, because `:` finds a tracked PR on whichever list it is
        // on and switches to it — the offer to show only comes up for a PR no
        // list has, and merged is the commonest of those.
        let ledger = two_queued();
        let repo_id = ledger.repos().expect("repos")[0].0;
        let mut merged = pr_snapshot(70999);
        merged.state = reviewq_core::model::PrState::Merged;
        ledger
            .upsert_pr(repo_id, &merged, None, ts("2026-08-11T12:00:00Z"))
            .expect("stored");
        let mut stored = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");
        feed(
            &mut stored,
            &hooks,
            &[
                KeyCode::Char(':'),
                KeyCode::Char('7'),
                KeyCode::Char('0'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Char('9'),
                KeyCode::Enter,
                KeyCode::Char('s'),
            ],
        );
        assert_eq!(
            stored.overlay,
            Overlay::Peeking {
                number: 70999,
                from_forge: false
            }
        );
    }

    #[test]
    fn tab_leaves_a_shown_pr_rather_than_doing_nothing() {
        // Reported as "tab doesn't switch panels any more": `:`, `s`, and then
        // a key that refused with a status line, which reads as a stuck screen
        // rather than as a mode.
        let (app, _) = showing_nine(&[special(KeyCode::Tab)]);

        assert!(app.peek.is_none(), "it put the PR away");
        assert_eq!(app.focus, Focus::Queue, "and gave the keys to the queue");
    }

    #[test]
    fn clicking_a_queue_row_leaves_a_shown_pr_for_that_row() {
        // The other half of the same report: the rows are drawn and clickable
        // the whole time a PR is on show, and clicking one did nothing at all.
        let mut app = App::with_ledger(Theme::default(), two_queued(), test_config()).expect("app");
        let (hooks, _) = fake_hooks(
            vec![
                press(':'),
                press('9'),
                special(KeyCode::Enter),
                press('s'),
                click(QUEUE_COLUMN, FIRST_ROW + 1),
                press('q'),
            ],
            None,
            false,
        );

        drive(&mut app, &hooks);

        assert!(app.peek.is_none(), "the click put it away");
        assert_eq!(
            app.current().map(|item| item.item.pr.number),
            Some(70201),
            "and selected the row that was clicked"
        );
        assert_eq!(app.focus, Focus::Queue);
    }

    #[test]
    fn a_key_that_writes_is_refused_while_a_pr_is_only_being_shown() {
        // A shown PR may have no ledger row at all, so `done` has nothing to
        // write to — and writing to the row *behind* the view would be worse.
        let (app, _) = showing_nine(&[press('d')]);

        assert_eq!(app.overlay, Overlay::None, "no confirmation was opened");
        let status = app.status.clone().expect("a status");
        assert!(status.contains("only being shown"), "{status}");
        assert!(app.peek.is_some(), "and it is still on show");
    }

    #[test]
    fn opening_a_shown_pr_opens_that_one_and_not_the_selected_row() {
        let (_, recorded) = showing_nine(&[press('o')]);

        assert_eq!(*recorded.opened.lock().expect("lock"), vec![9]);
    }

    #[test]
    fn a_pr_that_cannot_be_read_says_so_and_leaves_the_queue_alone() {
        let mut app = app();
        let (hooks, _) = fake_hooks(
            vec![
                press(':'),
                press('9'),
                special(KeyCode::Enter),
                press('s'),
                press('q'),
            ],
            None,
            false,
        );
        let hooks = Hooks {
            peek: Box::new(|_| bail!("no such pull request")),
            ..hooks
        };

        drive(&mut app, &hooks);

        assert!(app.peek.is_none());
        let status = app.status.clone().expect("a status");
        assert!(status.contains("could not be shown"), "{status}");
        assert!(!app.queue.is_empty(), "the queue survives");
    }

    #[test]
    fn the_screen_is_saved_as_it_was_before_the_note_saying_so() {
        // The point of holding it for the loop: a screenshot with "screen saved
        // to …" across its header is a picture of the act, not of the queue.
        let mut app = app();
        let (hooks, recorded) = fake_hooks(vec![special(KeyCode::F(12)), press('q')], None, false);

        drive(&mut app, &hooks);

        let saved = recorded.saved.lock().expect("lock");
        assert_eq!(saved.len(), 1);
        assert!(saved[0].starts_with("<svg"), "{}", &saved[0][..40]);
        assert!(
            !saved[0].contains("screen saved"),
            "the note must not be in the picture of what it describes"
        );
        assert!(
            saved[0].contains("70135"),
            "and the queue that was on screen is"
        );
        let status = app.status.clone().expect("a status");
        assert!(status.contains("/tmp/reviewq.svg"), "{status}");
    }

    #[test]
    fn the_screen_can_be_saved_from_under_an_overlay() {
        // The frames worth keeping are often the ones a modal is covering, and a
        // modal owns the keyboard — so this key is taken before it can be
        // swallowed.
        let mut app = app();
        let (hooks, recorded) = fake_hooks(
            // The first `q` dismisses the reference — which is itself the proof
            // that F12 did not, since a key the overlay had seen would have.
            vec![press('?'), special(KeyCode::F(12)), press('q'), press('q')],
            None,
            false,
        );

        drive(&mut app, &hooks);

        let saved = recorded.saved.lock().expect("lock");
        assert_eq!(saved.len(), 1);
        assert!(saved[0].contains("Reference"), "the overlay is in the shot");
    }

    #[test]
    fn a_save_that_fails_says_so_and_carries_on() {
        let mut app = app();
        let (hooks, _) = fake_hooks(vec![special(KeyCode::F(12)), press('q')], None, false);
        let hooks = Hooks {
            save_screen: Box::new(|_| bail!("read-only filesystem")),
            ..hooks
        };

        drive(&mut app, &hooks);

        let status = app.status.clone().expect("a status");
        assert!(status.contains("could not be saved"), "{status}");
        assert!(status.contains("read-only"), "{status}");
        assert!(!app.queue.is_empty(), "the queue survives");
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
                state: PrState::Open,
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
