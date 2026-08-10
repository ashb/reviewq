//! The one thing about the interface that only a real terminal can show: that
//! it takes the terminal over and gives it back.
//!
//! Everything else — layout, keys, the loop, what a refresh does to the screen —
//! is covered deterministically in `reviewq-tui`, against a `TestBackend` with
//! scripted messages. Those run in milliseconds and never flake. This exists
//! for the part they structurally cannot see: the alternate screen, raw mode,
//! and whether they're still in force after the process exits.
//!
//! Reads until the expected text appears, with a deadline, rather than sleeping
//! for a guessed interval — a slow machine should make this slower, not red.
//!
//! Assertions here stay to single words and escape sequences on purpose.
//! ratatui writes only the cells that changed, so a run of text arrives split by
//! cursor jumps with the unchanged spaces never sent at all — `any key to close`
//! goes out as `[21;35Hany[21;39Hkey[21;43Hto close`. Matching phrases against
//! raw pty bytes would need a terminal emulator to replay them into a grid,
//! which is precisely what `TestBackend` already does in the unit tests. So
//! content is asserted there; this asserts the terminal's state.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Long enough for a cold binary to draw on a loaded machine, short enough that
/// a hang fails the suite rather than stalling it.
const DEADLINE: Duration = Duration::from_secs(20);

/// A `reviewq tui` running in a pty, with its output collected as it arrives.
struct Session {
    writer: Box<dyn Write + Send>,
    output: std::sync::Arc<std::sync::Mutex<String>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Session {
    /// Start the interface against `db`, which need not exist — an empty ledger
    /// still draws, and pointing at the real one would be rude.
    fn start(db: &std::path::Path) -> Self {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open pty");

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_reviewq"));
        command.arg("tui");
        command.env("REVIEWQ_DB", db);
        // Nothing here should read the real config, and a missing one must not
        // stop the interface opening.
        command.env("REVIEWQ_CONFIG", "/nonexistent/reviewq/config.toml");
        command.env("TERM", "xterm-256color");
        let child = pty.slave.spawn_command(command).expect("spawn");

        let mut reader = pty.master.try_clone_reader().expect("reader");
        let writer = pty.master.take_writer().expect("writer");
        let output = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let collected = std::sync::Arc::clone(&output);
        // Drained on a thread: a pty buffer that fills would block the child
        // mid-draw, which looks exactly like a hang.
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                collected
                    .lock()
                    .expect("lock")
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        });

        Self {
            writer,
            output,
            child,
        }
    }

    fn seen(&self) -> String {
        self.output.lock().expect("lock").clone()
    }

    /// Wait for `needle` to appear, failing with everything seen so far.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if self.seen().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("never saw {needle:?}. Output so far:\n{}", self.seen());
    }

    fn press(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("write");
        self.writer.flush().expect("flush");
    }

    /// Wait for it to exit, returning its status code.
    fn wait_for_exit(&mut self) -> u32 {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.exit_code();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("did not exit. Output so far:\n{}", self.seen());
    }
}

#[test]
fn it_takes_the_terminal_over_and_gives_it_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut session = Session::start(&dir.path().join("ledger.db"));

    // Drew its opening frame, which means the layout survived a real terminal
    // rather than only a TestBackend. Single words, for the reason in the module
    // docs.
    session.wait_for("Queue");
    session.wait_for("quit");

    // The key reference opens over it, then closes — worth proving here because
    // it draws `Clear` against a real terminal's own background rather than a
    // buffer that starts blank.
    session.press("?");
    session.wait_for("Keys");
    session.wait_for("Navigate");
    session.press(" ");

    session.press("q");
    assert_eq!(session.wait_for_exit(), 0, "should exit cleanly");

    let seen = session.seen();
    // Raw mode and the alternate screen are both given back. `?1049l` leaves the
    // alternate screen; without it a shell is left staring at the interface's
    // last frame with its own scrollback hidden.
    assert!(
        seen.contains("\u{1b}[?1049l"),
        "never left the alternate screen. Output:\n{seen:?}"
    );
    // It entered it in the first place, so the assertion above means a matched
    // pair rather than a terminal that was never taken over.
    assert!(
        seen.contains("\u{1b}[?1049h"),
        "never entered the alternate screen. Output:\n{seen:?}"
    );
}
