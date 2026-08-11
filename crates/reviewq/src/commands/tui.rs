//! `reviewq tui`: hand over to the terminal interface.
//!
//! The interface itself is synchronous and knows nothing about a runtime — it
//! draws, reads a key, and acts. Everything that could block for an unbounded
//! time is a hook, and building those is this module's job, because doing the work
//! off the interface's thread needs the runtime this binary already has.
//!
//! That split is why `reviewq-tui` depends on neither tokio nor the forge: what
//! crosses the boundary is a closure and a channel.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use anyhow::{Context, Result, bail};
use crossterm::clipboard::CopyToClipboard;
use crossterm::event;
use crossterm::execute;
use jiff::Timestamp;
use reviewq_app::config::{Config, Loaded, ThemeMode};
use reviewq_app::sync::RepoSummary;
use reviewq_ledger::RepoKey;
use reviewq_tui::{Hooks, Message};
use tokio::runtime::Handle;

/// How long the input hook waits for a keystroke before letting the loop look for
/// finished work.
///
/// Short enough that a refresh landing feels immediate, long enough that an idle
/// interface is not busy.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The program that opens a URL in whatever the desktop uses for one.
///
/// Every platform spells its own differently and none of them is worth a
/// dependency: this is one argument and one process.
#[cfg(target_os = "macos")]
const URL_OPENER: &str = "open";
#[cfg(target_os = "windows")]
const URL_OPENER: &str = "explorer";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const URL_OPENER: &str = "xdg-open";

pub async fn run(loaded: &Loaded) -> Result<ExitCode> {
    let theme = reviewq_tui::Theme::new(match loaded.config.output.theme {
        ThemeMode::Dark => reviewq_tui::Mode::Dark,
        ThemeMode::Light => reviewq_tui::Mode::Light,
    });
    let config = Arc::new(loaded.config.clone());
    let hooks = live_hooks(Arc::clone(&config));
    // `block_in_place` because the interface owns this thread until the user
    // quits: it tells the runtime to move this worker's other tasks elsewhere
    // first, so the refreshes spawned below still make progress.
    tokio::task::block_in_place(|| reviewq_tui::run(theme, config, &hooks))?;
    Ok(ExitCode::SUCCESS)
}

/// The interface's side effects, done for real.
///
/// `config` is loaded once and shared with every hook that needs it — behind an
/// `Arc` because the closures that reach the forge run on the blocking pool and so
/// must own what they capture.
fn live_hooks(config: Arc<Config>) -> Hooks {
    let for_refresh = Arc::clone(&config);
    let for_sync = Arc::clone(&config);
    let for_fetch = Arc::clone(&config);
    let for_peek = Arc::clone(&config);
    let for_review = Arc::clone(&config);
    let for_mark_read = Arc::clone(&config);
    let for_open = Arc::clone(&config);
    let for_copy = Arc::clone(&config);
    Hooks {
        next_event: Box::new(|| {
            // `block_in_place` because `poll` parks the thread: it tells the
            // runtime to move this worker's other tasks elsewhere first.
            let ready = tokio::task::block_in_place(|| event::poll(POLL_INTERVAL))
                .context("polling the terminal for input")?;
            if !ready {
                return Ok(None);
            }
            Ok(Some(event::read().context("reading a terminal event")?))
        }),
        refresh: Box::new(move |number, tx: Sender<Message>| {
            let config = Arc::clone(&for_refresh);
            // `spawn_blocking` rather than `spawn`, because `sync_one`'s future is
            // not `Send`: it holds a ledger handle across the forge round trip,
            // and `rusqlite::Connection` is `Send` but not `Sync`, so a reference
            // to one cannot cross threads. Driving the future on a single
            // blocking-pool thread sidesteps that — nothing `!Send` ever moves.
            tokio::task::spawn_blocking(move || {
                let outcome =
                    Handle::current().block_on(reviewq_app::sync::sync_one(&config, number));
                // A closed channel means the interface has already exited, so the
                // result has nowhere to go and nothing is waiting for it.
                let _ = tx.send(Message::Refreshed { number, outcome });
            });
        }),
        sync: Box::new(move |tx: Sender<Message>| {
            let config = Arc::clone(&for_sync);
            // `spawn_blocking` for the same reason the refresh above uses it:
            // the sync holds a ledger handle across every forge round trip, and
            // `rusqlite::Connection` is not `Sync`, so its future is not `Send`.
            // The interface reads through a connection of its own meanwhile,
            // which is what WAL is turned on for.
            tokio::task::spawn_blocking(move || {
                let mut progress = ChannelProgress {
                    tx: tx.clone(),
                    summaries: Vec::new(),
                };
                let ran = Handle::current().block_on(reviewq_app::sync::run(
                    &config,
                    false,
                    &mut progress,
                ));
                // The exit code is the CLI's business: here the run either
                // finished or it didn't, and the summaries say what it did.
                let outcome = ran.map(|_| progress.summaries);
                let _ = tx.send(Message::Synced { outcome });
            });
        }),
        fetch: Box::new(move |number| {
            Handle::current()
                .block_on(reviewq_app::sync::track_one(&for_fetch, None, number))
                .map(|_| ())
        }),
        peek: Box::new(move |number| {
            Handle::current().block_on(reviewq_app::peek::peek_one(&for_peek, number))
        }),
        save_screen: Box::new(|picture| {
            // The working directory, because a screenshot is nearly always
            // wanted where you already are — pasted into the PR or the README
            // you have open — and a state directory would hide it.
            let path = std::env::current_dir()
                .context("finding the directory to save into")?
                .join(format!("reviewq-{}.svg", file_stamp(Timestamp::now())));
            std::fs::write(&path, picture)
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(path.display().to_string())
        }),
        open_url: Box::new(move |repo, number| {
            let url = pr_url(&for_open, repo, number)?;
            // Never handed the terminal, unlike the review command: an opener
            // returns straight away and its output (`xdg-open` has opinions about
            // mime caches) would land on top of the queue. So its streams go
            // nowhere and it is reaped off this thread — a browser that has to
            // cold-start can take seconds, and waiting here would freeze the
            // interface for them.
            let mut command = std::process::Command::new(URL_OPENER);
            command
                .arg(&url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut child = command
                .spawn()
                .with_context(|| format!("running {URL_OPENER}"))?;
            tokio::task::spawn_blocking(move || {
                // Reaped rather than left: an unwaited child stays a zombie for as
                // long as the interface runs.
                let _ = child.wait();
            });
            Ok(())
        }),
        copy_url: Box::new(move |repo, number| {
            let url = pr_url(&for_copy, repo, number)?;
            // OSC 52, through the terminal that is already ours — so this works
            // over ssh and inside tmux, where a clipboard library talking to the
            // local display server would put the URL on the wrong machine's
            // clipboard. The terminal may decline (or not support it) silently;
            // there is no reply to wait for.
            execute!(std::io::stdout(), CopyToClipboard::to_clipboard_from(&url))
                .context("writing the clipboard escape sequence")
        }),
        review: Box::new(move |number| {
            let handoff = reviewq_app::review::handoff_for(&for_review, number)?;

            // Keeps the alternate screen — see `reviewq_tui::lend_terminal`.
            reviewq_tui::lend_terminal();

            let ran = handoff
                .command()
                .status()
                .with_context(|| format!("running {:?}", handoff.argv[0]));

            // Taken back whatever happened, so a review command that dies doesn't
            // leave the queue drawing onto a cooked terminal.
            reviewq_tui::reclaim_terminal();
            let status = ran?;
            if !status.success() {
                bail!(
                    "{:?} exited with {}",
                    handoff.argv[0],
                    status
                        .code()
                        .map_or_else(|| "a signal".to_string(), |code| code.to_string())
                );
            }
            Ok(())
        }),
        mark_read: Box::new(move |number| {
            let config = Arc::clone(&for_mark_read);
            tokio::task::spawn_blocking(move || {
                let marked = Handle::current().block_on(async move {
                    let key =
                        reviewq_app::resolve::repo_for(&reviewq_app::resolve::open()?, number)?;
                    let repo = config
                        .repos()
                        .find(|r| r.key() == key)
                        .cloned()
                        .context("the PR's repo is no longer configured")?;
                    reviewq_app::actions::mark_notifications_read(&config, &repo, number).await
                });
                if let Err(err) = marked {
                    tracing::warn!(number, %err, "could not mark GitHub notifications read");
                }
            });
        }),
    }
}

/// A sync's progress, reported to the interface rather than to a terminal it
/// does not own.
///
/// The CLI's implementation writes the same two events to stderr and stdout;
/// this one sends them down the channel the interface already drains, and keeps
/// each repo's summary so the finished run can report what it did in one line.
struct ChannelProgress {
    tx: Sender<Message>,
    summaries: Vec<RepoSummary>,
}

impl reviewq_app::sync::SyncProgress for ChannelProgress {
    fn page(&mut self, what: &str, fetched: usize, total: u32) {
        // A closed channel means the interface has already exited. The sync
        // carries on regardless: what it has fetched is worth committing
        // whether or not anything is left to watch it.
        let _ = self.tx.send(Message::SyncNote {
            note: format!("syncing — {what} {fetched}/{total}"),
        });
    }

    fn repo_finished(&mut self, summary: &RepoSummary) {
        let _ = self.tx.send(Message::SyncNote {
            note: reviewq_app::sync::summary_line(summary),
        });
        self.summaries.push(summary.clone());
    }
}

/// A timestamp as a filename can carry it: RFC 3339 with the colons swapped for
/// dashes, since a colon is a path separator to some tools and a display quirk in
/// the macOS Finder.
fn file_stamp(at: Timestamp) -> String {
    reviewq_app::present::stamp(at).replace(':', "-")
}

/// A PR's page on the forge.
///
/// The forge renders it, because the path shape is the provider's business — the
/// same reason a pasted URL is handed to the forge to *read*. Building an adapter
/// costs nothing and resolves no token, so showing someone where a PR is never
/// waits on a credential helper.
fn pr_url(config: &Config, repo: &RepoKey, number: u64) -> Result<String> {
    let forge = config.forge_for(&repo.host)?;
    Ok(forge.web_url(&repo.owner, &repo.name, number))
}
