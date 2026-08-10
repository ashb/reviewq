//! Every key the interface responds to, declared once.
//!
//! Dispatch, the footer and the help overlay all read [`BINDINGS`], so a key
//! cannot do one thing and be advertised as another. The `r` key this replaces
//! was labelled "reload" while doing something the label didn't pin down, which
//! is the failure mode a single table exists to prevent.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What the user asked for, independent of which key asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the interface.
    Quit,
    /// Move the keyboard between the queue and the description.
    SwitchPane,
    /// One row down, in whichever pane has focus.
    Down,
    /// One row up.
    Up,
    /// A screenful down.
    PageDown,
    /// A screenful up.
    PageUp,
    /// Jump to the first row.
    First,
    /// Jump to the last row.
    Last,
    /// Jump to a PR named by number.
    Jump,
    /// Fetch the selected PR's detail from the forge.
    RefreshSelected,
    /// Hand the selected PR to the configured review command.
    Review,
    /// Mark the selected PR handled at its current head.
    Done,
    /// Suppress the selected PR for a while.
    Snooze,
    /// Open the selected PR's page in a browser.
    OpenInBrowser,
    /// Put the selected PR's URL on the clipboard.
    CopyUrl,
    /// Mute the selected PR, or unmute it.
    ToggleMute,
    /// Sink the selected PR to the bottom of the queue, or restore it.
    ToggleDefer,
    /// Show or hide the key reference.
    Help,
}

/// One chord: a key, and whether Ctrl must be held.
///
/// Only Ctrl is part of the match. A terminal reports `G` as `Char('G')` with
/// Shift already applied, so requiring an exact modifier set would break every
/// capital-letter binding — while ignoring Ctrl entirely would make `ctrl-d`
/// and `d` the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    /// Ctrl must be held for this chord to match.
    pub ctrl: bool,
    /// The key itself.
    pub code: KeyCode,
}

/// A chord with no modifier.
const fn key(code: KeyCode) -> Chord {
    Chord { ctrl: false, code }
}

/// A chord with Ctrl held.
const fn ctrl(code: KeyCode) -> Chord {
    Chord { ctrl: true, code }
}

/// An action, the chords that reach it, and how to describe both.
pub struct Binding {
    /// What pressing it does.
    pub action: Action,
    /// Every chord bound to this action.
    pub chords: &'static [Chord],
    /// How those chords read on screen. Written out rather than derived from
    /// `chords`, because `↑↓/jk` is friendlier than `Up, Down, j, k`.
    pub keys: &'static str,
    /// What it does, in the imperative.
    pub what: &'static str,
    /// The heading it sits under in the help overlay.
    pub group: &'static str,
    /// Also shown in the footer. The footer has room for the handful you need
    /// before you know the rest exists — everything else lives in the overlay.
    pub footer: bool,
}

/// The heading order in the help overlay, which is declaration order here.
pub const BINDINGS: &[Binding] = &[
    Binding {
        action: Action::Down,
        chords: &[key(KeyCode::Char('j')), key(KeyCode::Down)],
        keys: "↑↓ / jk",
        what: "move",
        group: "Navigate",
        footer: true,
    },
    Binding {
        action: Action::Up,
        chords: &[key(KeyCode::Char('k')), key(KeyCode::Up)],
        // Folded into the row above for display; the footer and overlay would
        // otherwise spend two rows on one gesture.
        keys: "",
        what: "",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::PageDown,
        chords: &[key(KeyCode::PageDown), ctrl(KeyCode::Char('d'))],
        keys: "PgDn / ^D",
        what: "page down",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::PageUp,
        chords: &[key(KeyCode::PageUp), ctrl(KeyCode::Char('u'))],
        keys: "PgUp / ^U",
        what: "page up",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::First,
        chords: &[key(KeyCode::Char('g')), key(KeyCode::Home)],
        keys: "g / Home",
        what: "first",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::Last,
        chords: &[key(KeyCode::Char('G')), key(KeyCode::End)],
        keys: "G / End",
        what: "last",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::Jump,
        chords: &[key(KeyCode::Char(':'))],
        keys: ":",
        // `:` rather than a letter because `:42` is already how vim says "go to
        // 42", and because a prompt there can grow to understand commands
        // without the key moving — which is how vim got `:w` alongside `:42`.
        what: "go to a PR by number",
        group: "Navigate",
        footer: false,
    },
    Binding {
        action: Action::SwitchPane,
        chords: &[key(KeyCode::Tab)],
        keys: "Tab",
        what: "switch pane",
        group: "View",
        footer: false,
    },
    Binding {
        action: Action::Review,
        chords: &[key(KeyCode::Enter)],
        keys: "⏎",
        what: "review it — hand off to your review command",
        group: "Act on the PR",
        footer: true,
    },
    Binding {
        action: Action::Done,
        chords: &[key(KeyCode::Char('d'))],
        keys: "d",
        what: "done — handled at this head",
        group: "Act on the PR",
        footer: true,
    },
    Binding {
        action: Action::Snooze,
        chords: &[key(KeyCode::Char('z'))],
        keys: "z",
        what: "snooze for a while",
        group: "Act on the PR",
        footer: true,
    },
    Binding {
        action: Action::OpenInBrowser,
        chords: &[key(KeyCode::Char('o'))],
        keys: "o",
        what: "open it in your browser",
        group: "Act on the PR",
        footer: false,
    },
    Binding {
        action: Action::CopyUrl,
        // `y` as well as `c`, because yanking is what this is in vim's terms; `u`
        // is left alone for the undo that a queue of destructive-ish actions will
        // eventually want.
        chords: &[key(KeyCode::Char('c')), key(KeyCode::Char('y'))],
        keys: "c / y",
        what: "copy its URL",
        group: "Act on the PR",
        footer: false,
    },
    Binding {
        action: Action::ToggleMute,
        chords: &[key(KeyCode::Char('m'))],
        keys: "m",
        what: "mute, or unmute",
        group: "Act on the PR",
        footer: false,
    },
    Binding {
        action: Action::ToggleDefer,
        chords: &[key(KeyCode::Char('f'))],
        keys: "f",
        what: "defer to the bottom, or restore",
        group: "Act on the PR",
        footer: false,
    },
    Binding {
        action: Action::RefreshSelected,
        chords: &[key(KeyCode::Char('r'))],
        keys: "r",
        what: "refresh from the forge",
        group: "Forge",
        footer: false,
    },
    Binding {
        action: Action::Help,
        chords: &[key(KeyCode::Char('?')), key(KeyCode::Char('h'))],
        keys: "? / h",
        what: "keys",
        group: "Session",
        footer: true,
    },
    Binding {
        action: Action::Quit,
        chords: &[
            key(KeyCode::Char('q')),
            key(KeyCode::Esc),
            ctrl(KeyCode::Char('c')),
        ],
        keys: "q / Esc",
        what: "quit",
        group: "Session",
        footer: true,
    },
];

/// The action `key` asks for, if any.
pub fn action_for(key: KeyEvent) -> Option<Action> {
    let pressed = Chord {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        code: key.code,
    };
    BINDINGS
        .iter()
        .find(|binding| binding.chords.contains(&pressed))
        .map(|binding| binding.action)
}

/// The bindings with something to display, in declaration order.
///
/// Skips the rows folded into a neighbour (see [`Action::Up`]), so a caller
/// listing bindings doesn't render a blank line for them.
pub fn described() -> impl Iterator<Item = &'static Binding> {
    BINDINGS.iter().filter(|b| !b.keys.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press_ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn every_declared_chord_resolves_to_its_own_action() {
        for binding in BINDINGS {
            for chord in binding.chords {
                let modifiers = if chord.ctrl {
                    KeyModifiers::CONTROL
                } else {
                    KeyModifiers::NONE
                };
                assert_eq!(
                    action_for(KeyEvent::new(chord.code, modifiers)),
                    Some(binding.action),
                    "{:?} did not resolve to {:?}",
                    chord,
                    binding.action
                );
            }
        }
    }

    #[test]
    fn no_chord_is_bound_twice() {
        let mut seen = Vec::new();
        for binding in BINDINGS {
            for chord in binding.chords {
                assert!(!seen.contains(chord), "{chord:?} is bound more than once");
                seen.push(*chord);
            }
        }
    }

    #[test]
    fn ctrl_is_part_of_the_match() {
        // `ctrl-d` pages while a bare `d` marks done: two unrelated actions on
        // the same letter, which only holds because Ctrl is part of the match.
        assert_eq!(
            action_for(press_ctrl(KeyCode::Char('d'))),
            Some(Action::PageDown)
        );
        assert_eq!(action_for(press(KeyCode::Char('d'))), Some(Action::Done));
        // And `ctrl-q` is not `q`.
        assert_eq!(action_for(press(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(action_for(press_ctrl(KeyCode::Char('q'))), None);
    }

    #[test]
    fn a_capital_letter_binding_matches_despite_shift() {
        // Terminals report `G` as Char('G') with SHIFT applied. Shift is not
        // part of the match, so this resolves; requiring an exact modifier set
        // would silently break it.
        let shifted = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(action_for(shifted), Some(Action::Last));
    }

    #[test]
    fn an_unbound_key_asks_for_nothing() {
        assert_eq!(action_for(press(KeyCode::Char('x'))), None);
        assert_eq!(action_for(press(KeyCode::F(4))), None);
    }

    #[test]
    fn every_described_binding_has_a_group_and_a_description() {
        for binding in described() {
            assert!(
                !binding.what.is_empty(),
                "{:?} has no label",
                binding.action
            );
            assert!(
                !binding.group.is_empty(),
                "{:?} has no group",
                binding.action
            );
        }
    }

    #[test]
    fn the_footer_shows_help_and_quit_so_the_rest_is_discoverable() {
        let footer: Vec<Action> = described().filter(|b| b.footer).map(|b| b.action).collect();
        assert!(footer.contains(&Action::Help), "{footer:?}");
        assert!(footer.contains(&Action::Quit), "{footer:?}");
        // The real constraint is width, which the renderer's own test measures;
        // this is a nudge to reconsider rather than a hard limit.
        assert!(footer.len() <= 6, "footer has grown to {footer:?}");
    }
}
