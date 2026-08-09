//! What's on screen and what the keys do.
//!
//! Ledger reads happen synchronously on this thread. They're sub-millisecond
//! against local SQLite, so making them async would buy nothing and cost the
//! whole app an executor: `rusqlite::Connection` is `Send` but not `Sync`, so a
//! shared handle across await points is friction with no payoff. Only a sync,
//! which is network-bound, will need to move off this thread.

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use reviewq_ledger::{Ledger, Located, PrShow, QueueItem};

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

    /// Draw, wait for input, repeat, until the user quits.
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| ui::draw(frame, self))?;
            // Blocking read: nothing animates yet, so waking on a timer would
            // only burn cycles redrawing an identical screen.
            if let Event::Key(key) = event::read()? {
                self.on_key(key)?;
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        // Windows reports press and release; acting on both double-fires.
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        // `ctrl-d`/`ctrl-u` move a whole screenful, matching PageDown/PageUp
        // rather than the half-page vim gives them — they're bound as synonyms
        // here because that's how they were asked for.
        let page = self.page() as isize;
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.quit = true,
            (KeyModifiers::CONTROL, KeyCode::Char('d')) => self.scroll(page)?,
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => self.scroll(-page)?,
            (_, KeyCode::PageDown) => self.scroll(page)?,
            (_, KeyCode::PageUp) => self.scroll(-page)?,
            (_, KeyCode::Char('q') | KeyCode::Esc) => self.quit = true,
            (_, KeyCode::Tab | KeyCode::Char('\t')) => self.toggle_focus(),
            (_, KeyCode::Char('j') | KeyCode::Down) => self.scroll(1)?,
            (_, KeyCode::Char('k') | KeyCode::Up) => self.scroll(-1)?,
            (_, KeyCode::Char('g') | KeyCode::Home) => self.scroll_to_start()?,
            (_, KeyCode::Char('G') | KeyCode::End) => self.scroll_to_end()?,
            _ => {}
        }
        Ok(())
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
