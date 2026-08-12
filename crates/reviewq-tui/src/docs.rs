//! The screenshots in `docs/imgs`: written from the fixture, and held to it.
//!
//! Documentation pictures rot: the interface moves, the images stay, and nobody
//! notices until a reader does. So they are generated from a fixture by the same
//! renderer the interface uses, and the committed files are *checked* on every
//! test run — a change to the layout, the palette or the marks fails here until
//! the pictures are regenerated:
//!
//! ```sh
//! REVIEWQ_WRITE_DOCS=1 cargo test -p reviewq-tui docs
//! ```
//!
//! The fixture itself is [`crate::fixture`], which is not test-only: `reviewq
//! help` draws the same screens into the terminal.

use crate::app::fixture_config;
use crate::fixture::{SHOTS, Shot, draw};

/// Where the committed pictures live, relative to this crate.
const IMAGES: &str = "../../docs/imgs";

/// Every picture is this size. Wide enough that a row has room for its title
/// beside the reason that put it there, which is the pair worth reading.
const WIDTH: u16 = 140;

use crate::svg;
use crate::theme::Mode;

/// Draw one shot as an SVG, at the width every picture shares.
fn svg_of(shot: &Shot) -> String {
    let (buffer, theme) = draw(shot, WIDTH, Mode::Dark);
    svg::render(&buffer, &theme, &fixture_config().output.svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pictures in `docs/imgs` are what the interface currently draws.
    ///
    /// Fails rather than rewrites, because a picture changing is a thing to look
    /// at before committing — the message says how.
    #[test]
    fn the_committed_screenshots_are_current() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(IMAGES);
        let writing = std::env::var_os("REVIEWQ_WRITE_DOCS").is_some();
        if writing {
            std::fs::create_dir_all(&dir).expect("make docs/imgs");
        }

        let mut stale = Vec::new();
        for shot in SHOTS {
            let path = dir.join(format!("{}.svg", shot.name));
            let drawn = svg_of(shot);
            if writing {
                std::fs::write(&path, &drawn).expect("write the picture");
                continue;
            }
            let committed = std::fs::read_to_string(&path).unwrap_or_default();
            if committed != drawn {
                stale.push(shot.name);
            }
        }

        assert!(
            stale.is_empty(),
            "these screenshots no longer match what the interface draws: {stale:?}\n\
             regenerate them with `REVIEWQ_WRITE_DOCS=1 cargo test -p reviewq-tui docs`",
        );
    }

    /// The screens as plain text, for reading a change over rather than opening
    /// six files: `cargo test -p reviewq-tui docs::tests::look -- --nocapture`.
    #[test]
    fn look_at_the_screenshots() {
        for shot in SHOTS {
            let (buffer, _) = draw(shot, WIDTH, Mode::Dark);
            println!("\n=== {} ===", shot.name);
            for y in 0..shot.height {
                let row: String = (0..WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                println!("{row}");
            }
        }
    }

    #[test]
    fn the_fixture_queue_shows_a_morning_rather_than_one_case() {
        // What makes the picture worth taking: every urgency band, so the
        // colours and the ordering both have something to say.
        let app = crate::fixture::app(Mode::Dark);
        let bands: std::collections::BTreeSet<u8> = app
            .queue
            .iter()
            .filter_map(|item| item.item.top.as_ref())
            .map(reviewq_ledger::AttentionRow::priority)
            .collect();

        assert_eq!(
            bands.len(),
            6,
            "one of every attention reason there is: {bands:?}"
        );
        assert!(
            app.queue.iter().any(|item| item.item.deferred),
            "and something set aside"
        );
        assert!(
            app.queue.iter().any(|item| !item.item.pr.state.is_open()),
            "and something that merged and stayed"
        );
    }
}
