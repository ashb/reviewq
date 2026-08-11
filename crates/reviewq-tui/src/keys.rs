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
    /// Leave whatever is being looked at: the muted or waiting list first, and
    /// the interface itself once the queue is what's up.
    Back,
    /// Move the keyboard between the queue and the description.
    SwitchPane,
    /// Adapt the palette for the other terminal background.
    ToggleTheme,
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
    /// Sweep every configured repo, in the background.
    SyncAll,
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
    /// Stop watching the selected PR altogether.
    Untrack,
    /// Swap the list between the queue and what has been muted.
    ShowMuted,
    /// Swap the list between the queue and what is waiting on somebody else.
    ShowWaiting,
    /// Show or hide the key reference.
    Help,
    /// Save what is on screen as an SVG.
    SaveSvg,
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
    /// Kept out of the reference and the footer.
    ///
    /// The table is still where the key is declared, so it cannot collide with
    /// another and cannot do something the reference contradicts — it simply has
    /// nothing to say to somebody who isn't looking for it. For a shortcut that
    /// serves the person writing the documentation rather than the person
    /// reading it, that is the honest arrangement; a bare match arm in `dispatch`
    /// would have put the key outside the one table that knows about keys.
    pub hidden: bool,
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
        hidden: false,
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
        hidden: false,
    },
    Binding {
        action: Action::PageDown,
        chords: &[key(KeyCode::PageDown), ctrl(KeyCode::Char('d'))],
        keys: "PgDn / ^D",
        what: "page down",
        group: "Navigate",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::PageUp,
        chords: &[key(KeyCode::PageUp), ctrl(KeyCode::Char('u'))],
        keys: "PgUp / ^U",
        what: "page up",
        group: "Navigate",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::First,
        chords: &[key(KeyCode::Char('g')), key(KeyCode::Home)],
        keys: "g / Home",
        what: "first",
        group: "Navigate",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::Last,
        chords: &[key(KeyCode::Char('G')), key(KeyCode::End)],
        keys: "G / End",
        what: "last",
        group: "Navigate",
        footer: false,
        hidden: false,
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
        hidden: false,
    },
    Binding {
        action: Action::SwitchPane,
        chords: &[key(KeyCode::Tab)],
        keys: "Tab",
        what: "switch pane",
        group: "View",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::ToggleTheme,
        chords: &[key(KeyCode::Char('t'))],
        keys: "t",
        what: "adapt for a light or dark terminal",
        group: "View",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::ShowWaiting,
        chords: &[key(KeyCode::Char('W'))],
        keys: "W",
        what: "show what waits on someone else, or go back",
        group: "View",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::ShowMuted,
        chords: &[key(KeyCode::Char('M'))],
        keys: "M",
        what: "show what you have muted, or go back",
        group: "View",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::Review,
        chords: &[key(KeyCode::Enter)],
        keys: "⏎",
        what: "review it — hand off to your review command",
        group: "Act on the PR",
        footer: true,
        hidden: false,
    },
    Binding {
        action: Action::Done,
        chords: &[key(KeyCode::Char('d'))],
        keys: "d",
        what: "done — handled at this head",
        group: "Act on the PR",
        footer: true,
        hidden: false,
    },
    Binding {
        action: Action::Snooze,
        chords: &[key(KeyCode::Char('z'))],
        keys: "z",
        what: "snooze for a while",
        group: "Act on the PR",
        footer: true,
        hidden: false,
    },
    Binding {
        action: Action::OpenInBrowser,
        chords: &[key(KeyCode::Char('o'))],
        keys: "o",
        what: "open it in your browser",
        group: "Act on the PR",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::CopyUrl,
        // `y` as well as `c`, because yanking is what this is in vim's terms.
        // `u` was held back here for a future undo and has since gone to
        // `untrack`, which is itself undone by tracking the PR again.
        chords: &[key(KeyCode::Char('c')), key(KeyCode::Char('y'))],
        keys: "c / y",
        what: "copy its URL",
        group: "Act on the PR",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::ToggleMute,
        chords: &[key(KeyCode::Char('m'))],
        keys: "m",
        what: "mute, or unmute",
        group: "Act on the PR",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::ToggleDefer,
        chords: &[key(KeyCode::Char('f'))],
        keys: "f",
        what: "defer to the bottom, or restore",
        group: "Act on the PR",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::Untrack,
        chords: &[key(KeyCode::Char('u'))],
        keys: "u",
        what: "untrack — stop watching it",
        group: "Act on the PR",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::RefreshSelected,
        chords: &[key(KeyCode::Char('r'))],
        keys: "r",
        what: "refresh from the forge",
        group: "Forge",
        footer: false,
        hidden: false,
    },
    Binding {
        // Capitalised like the two list keys, and for the same reason: `r` acts
        // on the row under the cursor, this acts on everything.
        action: Action::SyncAll,
        chords: &[key(KeyCode::Char('S'))],
        keys: "S",
        what: "sync every repo, in the background",
        group: "Forge",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::Help,
        chords: &[key(KeyCode::Char('?')), key(KeyCode::Char('h'))],
        keys: "? / h",
        what: "keys",
        group: "Session",
        footer: true,
        hidden: false,
    },
    Binding {
        // Esc rather than q, because Esc is already how every overlay and every
        // shown PR is left: one key that always means "out of this", whatever
        // this is. On the queue there is nothing left to leave, so it quits.
        action: Action::Back,
        chords: &[key(KeyCode::Esc)],
        keys: "Esc",
        what: "back to the queue, or quit",
        group: "Session",
        footer: true,
        hidden: false,
    },
    Binding {
        action: Action::Quit,
        chords: &[key(KeyCode::Char('q')), ctrl(KeyCode::Char('c'))],
        keys: "q",
        what: "quit, from wherever you are",
        group: "Session",
        footer: false,
        hidden: false,
    },
    Binding {
        action: Action::SaveSvg,
        // F12 because nothing types it by accident, and because ctrl-s is XOFF
        // on a terminal that still honours flow control — which would freeze the
        // interface rather than photograph it.
        chords: &[key(KeyCode::F(12))],
        keys: "F12",
        what: "save the screen as an SVG",
        group: "Session",
        footer: false,
        hidden: true,
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
/// Skips the rows folded into a neighbour (see [`Action::Up`]) so a caller
/// listing bindings doesn't render a blank line for them, and the
/// [`hidden`](Binding::hidden) ones, which are deliberately not advertised.
pub fn described() -> impl Iterator<Item = &'static Binding> {
    BINDINGS.iter().filter(|b| !b.keys.is_empty() && !b.hidden)
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
    fn a_hidden_binding_still_works_but_is_never_advertised() {
        // The point of keeping it in the table rather than special-casing it in
        // `dispatch`: it is checked for collisions like any other key, and the
        // two places that list keys skip it because it says to.
        assert_eq!(
            action_for(press(KeyCode::F(12))),
            Some(Action::SaveSvg),
            "the key works"
        );
        assert!(
            !described().any(|b| b.action == Action::SaveSvg),
            "and appears in neither the reference nor the footer"
        );
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
    fn the_footer_shows_help_and_the_way_out_so_the_rest_is_discoverable() {
        let footer: Vec<Action> = described().filter(|b| b.footer).map(|b| b.action).collect();
        assert!(footer.contains(&Action::Help), "{footer:?}");
        // The way out is `Back`, which is Esc: on the queue it quits, and in a
        // list it leaves the list. `q` is in the reference rather than here —
        // one exit advertised, and it is the one that cannot lose you the queue.
        assert!(footer.contains(&Action::Back), "{footer:?}");
        // The real constraint is width, which the renderer's own test measures;
        // this is a nudge to reconsider rather than a hard limit.
        assert!(footer.len() <= 6, "footer has grown to {footer:?}");
    }
}
