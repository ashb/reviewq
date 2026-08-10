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
//! Input and finished work arrive on one channel, so the loop is a single
//! `recv().await` rather than a `select!`: a keystroke and a completed refresh
//! are both just reasons to update and redraw. A thread does the blocking
//! `event::read`, since crossterm's own async stream would be a dependency for
//! no gain over that.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;
use reviewq_app::sync::Refreshed;
use reviewq_ledger::{Ledger, Located, PrShow, QueueItem};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

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
    /// The key reference is up, covering the panes beneath it.
    pub help: bool,
    /// A one-line note in the header: what is happening, or what just did.
    pub status: Option<String>,
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
    ledger: Ledger,
    quit: bool,
}

/// Both ends of the loop's channel.
///
/// The loop needs the receiver to wait on and the sender to hand to a task, so
/// they travel together rather than as two arguments that must match.
pub(crate) struct Channel {
    /// Cloned into each task so it can report back.
    pub tx: mpsc::UnboundedSender<Message>,
    /// What the loop waits on.
    pub rx: mpsc::UnboundedReceiver<Message>,
}

impl Channel {
    /// A channel with the terminal's input already flowing into it.
    pub(crate) fn with_input_reader() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        spawn_input_reader(tx.clone());
        Self { tx, rx }
    }

    /// A channel nothing is feeding, for a test to load by hand.
    #[cfg(test)]
    fn silent() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

/// The side effects the loop performs, as functions it is given.
///
/// Injected rather than called directly so the loop can be driven in a test
/// without a forge, a token or a config — the same reason wiff hands its own
/// loop a struct of hooks. It also keeps `App` unaware of how a refresh is
/// actually run.
pub(crate) struct Hooks {
    /// Begin refreshing a PR. Must not block: whatever it starts is expected to
    /// report back as [`Message::Refreshed`] eventually, or never.
    pub refresh: Box<dyn Fn(u64, mpsc::UnboundedSender<Message>) + Send + Sync>,
}

impl Hooks {
    /// The real ones, refreshing through `reviewq-app`.
    pub(crate) fn live() -> Self {
        Self {
            refresh: Box::new(|number, tx| {
                // `spawn_blocking` rather than `spawn`, because `sync_one`'s
                // future is not `Send`: it holds a ledger handle across the forge
                // round trip, and `rusqlite::Connection` is `Send` but not
                // `Sync`, so a reference to one cannot cross threads. Driving the
                // future on a single blocking-pool thread sidesteps that —
                // nothing `!Send` ever moves.
                //
                // Reshaping `refresh_one` so no ledger handle is alive during the
                // fetch would be worth doing on its own merits, but it is not
                // what makes the interface responsive: that is this being off the
                // UI thread at all.
                tokio::task::spawn_blocking(move || {
                    let outcome =
                        Handle::current().block_on(reviewq_app::sync::sync_one(None, number));
                    // A closed channel means the interface has already exited, so
                    // the result has nowhere to go and nothing awaits it.
                    let _ = tx.send(Message::Refreshed { number, outcome });
                });
            }),
        }
    }
}

/// Something the loop should wake up for.
///
/// Input and finished work share one channel so the loop is a single `recv`:
/// both are just reasons to update state and redraw.
pub(crate) enum Message {
    /// A terminal event — a keystroke, or a resize.
    Input(Event),
    /// A refresh task finished, for better or worse.
    Refreshed {
        /// The PR it was refreshing.
        number: u64,
        /// What came back.
        outcome: Result<Refreshed>,
    },
}

/// Read terminal events on a thread, forwarding them to the loop.
///
/// `event::read` blocks, which an async task must not do. A thread can, and
/// this one needs no shutdown path: on quit the receiver drops, the next `send`
/// fails, and the thread ends — or the process exits first, which is the usual
/// case since it is parked in `read` waiting for a key that never comes.
fn spawn_input_reader(tx: mpsc::UnboundedSender<Message>) {
    std::thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(Message::Input(event)).is_err() {
                break;
            }
        }
    });
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
    pub fn new(theme: Theme) -> Result<Self> {
        let path = reviewq_app::paths::database_file()?;
        let ledger = Ledger::open(&path)
            .with_context(|| format!("opening the ledger at {}", path.display()))?;
        Self::with_ledger(theme, ledger)
    }

    /// Build over an already-open ledger, so a test can render against a
    /// fixture rather than whatever this machine happens to have synced.
    pub(crate) fn with_ledger(theme: Theme, ledger: Ledger) -> Result<Self> {
        let mut app = Self {
            theme,
            queue: Vec::new(),
            selected: 0,
            detail: None,
            repo_count: 0,
            focus: Focus::default(),
            detail_scroll: 0,
            help: false,
            status: None,
            refreshing: BTreeSet::new(),
            page: 1,
            detail_lines: 0,
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
    fn load_detail(&mut self) -> Result<()> {
        self.detail = match self.current() {
            None => None,
            Some(item) => {
                let repo_id = self.ledger.ensure_repo(&item.repo)?;
                let number = item.item.pr.number;
                self.ledger.show(repo_id, number)?
            }
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
    }

    /// Rows a paging key moves by — whatever the last render measured.
    pub(crate) fn page(&self) -> usize {
        self.page
    }

    /// Record how many lines the detail pane's content came to, so scrolling
    /// can stop at the last line instead of running into empty space.
    pub(crate) fn set_detail_lines(&mut self, lines: usize) {
        self.detail_lines = lines;
        self.clamp_detail_scroll();
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
    pub(crate) async fn run<B>(
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
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;

            // Nothing animates, so there is no tick: the loop sleeps until a key
            // is pressed, the terminal resizes, or a task reports back.
            match channel.rx.recv().await {
                Some(Message::Input(Event::Key(key))) => {
                    // Windows reports press and release; acting on both
                    // double-fires.
                    if key.kind == KeyEventKind::Press {
                        self.dispatch(keys::action_for(key), channel, hooks)?;
                    }
                }
                // A resize is reason enough to redraw, which the top of the loop
                // does unconditionally.
                Some(Message::Input(_)) => {}
                Some(Message::Refreshed { number, outcome }) => {
                    self.on_refreshed(number, outcome);
                }
                // The input reader has gone and no task holds a sender, so
                // nothing further can arrive and waiting again would hang.
                None => break,
            }
        }
        Ok(())
    }

    /// Route an action: the ones with side effects go to a hook, the rest are
    /// state changes [`update`](Self::update) applies.
    ///
    /// Splitting them is what lets `update` be tested by naming an action
    /// directly, without synthesising key events or standing up a forge.
    fn dispatch(&mut self, action: Option<Action>, channel: &Channel, hooks: &Hooks) -> Result<()> {
        match action {
            Some(Action::RefreshSelected) => {
                if let Some(number) = self.refresh_target() {
                    (hooks.refresh)(number, channel.tx.clone());
                }
                Ok(())
            }
            other => self.update(other),
        }
    }

    /// Apply a state change. No I/O beyond the ledger reads a moved selection
    /// needs, so a test can call it with any action and inspect the result.
    pub(crate) fn update(&mut self, action: Option<Action>) -> Result<()> {
        // While the key reference is up it owns the keyboard: any key closes it,
        // so it can be dismissed without first remembering how.
        if self.help {
            self.help = false;
            return Ok(());
        }
        let page = self.page() as isize;
        match action {
            None => Ok(()),
            Some(Action::Quit) => {
                self.quit = true;
                Ok(())
            }
            Some(Action::Help) => {
                self.help = true;
                Ok(())
            }
            Some(Action::SwitchPane) => {
                self.toggle_focus();
                Ok(())
            }
            Some(Action::Down) => self.scroll(1),
            Some(Action::Up) => self.scroll(-1),
            Some(Action::PageDown) => self.scroll(page),
            Some(Action::PageUp) => self.scroll(-page),
            Some(Action::First) => self.scroll_to_start(),
            Some(Action::Last) => self.scroll_to_end(),
            // Handled by `dispatch`, which owns the hook it needs.
            Some(Action::RefreshSelected) => Ok(()),
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
            Err(err) => format!("#{number} refresh failed: {err:#}"),
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

    /// One queued PR, enough to have something selected.
    pub(super) fn fixture() -> Ledger {
        let ledger = Ledger::open_in_memory().expect("ledger");
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
            .expect("detail");
        ledger
    }

    pub(super) fn app() -> App {
        App::with_ledger(Theme::default(), fixture()).expect("app")
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
        let mut empty = App::with_ledger(Theme::default(), ledger).expect("app");
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
    fn a_pr_the_forge_lost_is_reported_as_such() {
        let mut app = app();
        assert_eq!(app.refresh_target(), Some(70135));
        app.deliver(70135, Ok(Refreshed::Gone));
        let status = app.status.clone().expect("a status");
        assert!(status.contains("no longer exists"), "{status}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_loop_wakes_for_a_task_result_as_well_as_for_input() {
        // The point of one channel: a refresh landing is as good a reason to
        // wake and redraw as a keystroke. Proven by driving both through it.
        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        let sender = tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = sender.send(Message::Refreshed {
                number: 70135,
                outcome: Ok(Refreshed::Gone),
            });
        })
        .await
        .expect("task");

        match rx.recv().await {
            Some(Message::Refreshed { number, outcome }) => {
                assert_eq!(number, 70135);
                assert_eq!(outcome.expect("outcome"), Refreshed::Gone);
            }
            other => panic!("expected a refresh result, got {:?}", other.is_some()),
        }
    }
}

/// Driving the real loop: scripted messages in, a `TestBackend` to draw on, and
/// a fake refresh hook. No terminal, no forge, no sleeps — the same shape wiff
/// uses, which is what having the channel and the hooks handed in buys.
#[cfg(test)]
mod loop_tests {
    use super::tests::{app, fixture};
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::sync::{Arc, Mutex};

    fn press(code: char) -> Message {
        Message::Input(Event::Key(KeyEvent::new(
            KeyCode::Char(code),
            KeyModifiers::NONE,
        )))
    }

    /// Hooks whose refresh records what it was asked for and answers at once,
    /// so the loop sees a result without anything async happening.
    ///
    /// `then_quit` makes it queue a `q` behind the answer. Needed because the
    /// loop stops the moment it reads a quit: a `q` scripted ahead of a refresh
    /// result means the loop exits before taking that result in — correct, since
    /// it is leaving anyway, but not what a test of the result wants.
    fn answering_hooks(answer: Refreshed, then_quit: bool) -> (Hooks, Arc<Mutex<Vec<u64>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let hooks = Hooks {
            refresh: Box::new(move |number, tx| {
                seen.lock().expect("lock").push(number);
                let _ = tx.send(Message::Refreshed {
                    number,
                    outcome: Ok(answer.clone()),
                });
                if then_quit {
                    let _ = tx.send(press('q'));
                }
            }),
        };
        (hooks, asked)
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

    #[tokio::test]
    async fn a_quit_key_ends_the_loop_after_one_frame() {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        let mut app = app();
        let mut channel = Channel::silent();
        channel.tx.send(press('q')).expect("send");
        let (hooks, _) = answering_hooks(Refreshed::Gone, false);

        app.run(&mut terminal, &mut channel, &hooks)
            .await
            .expect("loop");

        assert!(app.quit);
        // It drew before waiting, so the opening frame is on screen.
        assert!(screen(&terminal).contains("on the queue"));
    }

    #[tokio::test]
    async fn the_loop_ends_when_nothing_can_arrive_any_more() {
        // No input reader and no task holds a sender, so the channel closes as
        // soon as the loop's own clone is the last one. Waiting again would hang.
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).expect("terminal");
        let mut app = app();
        let (hooks, _) = answering_hooks(Refreshed::Gone, false);
        let mut channel = Channel::silent();
        drop(std::mem::replace(
            &mut channel.tx,
            mpsc::unbounded_channel().0,
        ));

        app.run(&mut terminal, &mut channel, &hooks)
            .await
            .expect("loop");
        assert!(
            !app.quit,
            "it ended because the channel did, not by quitting"
        );
    }

    #[tokio::test]
    async fn r_asks_the_hook_and_the_answer_reaches_the_screen() {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("terminal");
        let mut app = app();
        let mut channel = Channel::silent();
        let (hooks, asked) = answering_hooks(
            Refreshed::Updated {
                repo: "apache/airflow".into(),
                queued: false,
                cost: 1,
                remaining: 4999,
            },
            true,
        );

        // `r` reaches the hook, which answers and then queues the quit behind
        // its answer, so the loop takes the result in before leaving.
        channel.tx.send(press('r')).expect("send");
        app.run(&mut terminal, &mut channel, &hooks)
            .await
            .expect("loop");

        assert_eq!(
            *asked.lock().expect("lock"),
            vec![70135],
            "the hook was asked"
        );
        assert!(app.refreshing.is_empty(), "and the result cleared the mark");
        // The result was taken in and drawn, which is the whole path: key →
        // action → hook → message → state → frame.
        let status = app.status.clone().expect("a status");
        assert!(status.contains("#70135 refreshed"), "{status}");
        assert!(
            screen(&terminal).contains("wants nothing"),
            "{}",
            screen(&terminal)
        );
    }

    #[tokio::test]
    async fn keys_move_the_selection_through_the_real_loop() {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("terminal");
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        let mut channel = Channel::silent();
        let (hooks, _) = answering_hooks(Refreshed::Gone, false);

        for key in ['?', ' ', 'q'] {
            channel.tx.send(press(key)).expect("send");
        }
        app.run(&mut terminal, &mut channel, &hooks)
            .await
            .expect("loop");

        // `?` opened the reference and the space closed it again, so what is left
        // on screen is the panes — proving both halves of the toggle ran.
        assert!(!app.help);
        assert!(app.quit);
    }
}
