//! The colours reviewq paints with, and why they're these ones.
//!
//! reviewq hands PRs off to wiff, so the two sit in the same terminal minutes
//! apart and ought to look like siblings. wiff derives its diff chrome from a
//! syntect syntax theme and layers six fixed accent hues over it — the
//! base16-ocean accents — keeping each hue's meaning constant across themes and
//! adapting only its lightness for contrast.
//!
//! reviewq reuses that accent layer and nothing else: it renders no file content,
//! so a syntax theme would buy it nothing.
//!
//! It does fill its own background, as wiff does. It did not, once, on the
//! grounds that chrome should sit on the terminal's own and inherit whatever
//! theme is there — which is defensible until the palette can be switched, and
//! then it isn't: a light palette over a dark terminal is dark text on a dark
//! background, and a switch that leaves the background alone changes a few
//! foreground hues and calls itself a theme. Owning the background is what makes
//! the choice mean anything.
//!
//! The cost is real and accepted: reviewq no longer composes with terminal
//! transparency or a background image, and a terminal theme it doesn't match is
//! a terminal theme it now covers.
//!
//! The accents are duplicated here as constants rather than imported. wiff is a
//! git dependency on another project's unstable internals, and `wiff-diff`
//! would pull syntect in for machinery reviewq has no use for; six hex values
//! are the cheaper coupling. They are the base16-ocean accents, so the shared
//! reference is a published palette rather than one project's private choice.

use ratatui::style::Color;

/// A colour with 8 bits per channel, before it becomes a ratatui [`Color`].
///
/// The palette is computed in RGB because contrast is: adapting a hue for
/// legibility means moving its lightness, which needs real channel values, not
/// a terminal colour index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    Rgb { r, g, b }
}

/// Render an [`Rgb`] for ratatui. Call sites read
/// `Style::default().fg(color(theme.urgent))`, mirroring how wiff's renderer
/// reads, so a hue is never written inline at a call site.
pub fn color(c: Rgb) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

// The base16-ocean accents. The first six are exactly wiff's, so a colour means
// the same thing in both tools.
const GOLD: Rgb = rgb(0xeb, 0xcb, 0x8b);
const ORANGE: Rgb = rgb(0xd0, 0x87, 0x70);
const GREEN: Rgb = rgb(0xa3, 0xbe, 0x8c);
const BLUE: Rgb = rgb(0x8f, 0xa1, 0xb3);
const TEAL: Rgb = rgb(0x96, 0xb5, 0xb4);
const RED: Rgb = rgb(0xbf, 0x61, 0x6a);
/// base16-ocean's magenta. wiff has no use for it, but the CLI already colours
/// a merged PR magenta, and `list`/`show` and the TUI disagreeing on what
/// merged looks like would be worse than using a hue wiff happens not to.
const MAGENTA: Rgb = rgb(0xb4, 0x8e, 0xad);

/// The contrast ratio body text must clear against the background — WCAG AA.
const TEXT_CONTRAST: f64 = 4.5;

/// The contrast ratio dimmed, secondary text must clear. WCAG AA for large
/// text: secondary text is allowed to recede, but not to become unreadable.
const DIM_CONTRAST: f64 = 3.0;

/// The dark palette's background: painted, and the reference every accent in that
/// palette is made legible against.
const DARK_BG: Rgb = rgb(0x1e, 0x1e, 0x1e);

/// The light palette's background. See [`DARK_BG`].
const LIGHT_BG: Rgb = rgb(0xff, 0xff, 0xff);

/// Which reference background the palette is adapted for.
///
/// Detecting this is unreliable — it needs an OSC 11 query the terminal may
/// ignore — so it's configured rather than guessed, and defaults to dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Adapt for a dark terminal background.
    #[default]
    Dark,
    /// Adapt for a light terminal background.
    Light,
}

/// The semantic palette.
///
/// Every field names a *meaning*, never a hue, so re-theming is one edit here
/// rather than a sweep through the widgets — and so a call site can't quietly
/// invent a seventh colour.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Ordinary text: a PR title, a header.
    pub text: Rgb,
    /// Secondary text that should recede: timestamps, counts, PR numbers, the
    /// reason a PR is tracked.
    pub dim: Rgb,
    /// A panel border at rest.
    pub border: Rgb,
    /// The border of the panel holding focus, and the marker on the selected
    /// row.
    pub focus: Rgb,
    /// The most urgent attention reasons — the top priority bands, the ones the
    /// queue exists to surface.
    pub urgent: Rgb,
    /// Something good: an approving verdict, an open PR.
    pub good: Rgb,
    /// Something adverse: a changes-requested verdict, a closed-unmerged PR.
    pub bad: Rgb,
    /// A merged PR.
    pub merged: Rgb,
    /// Something the user asked to be quiet about: muted, snoozed, deferred.
    pub quiet: Rgb,
    /// A caveat worth reading before acting: a done superseded by new commits,
    /// a draft, a sweep that hit the search cap.
    pub warn: Rgb,
    /// The key letter in a footer binding, as distinct from its label.
    pub key: Rgb,
    /// The background everything is painted on, and the reference every accent
    /// above was made legible against.
    pub bg: Rgb,
    /// Which of the two palettes this is, so the interface can offer the other.
    pub mode: Mode,
}

impl Theme {
    /// The palette for `mode`.
    pub fn new(mode: Mode) -> Self {
        match mode {
            Mode::Dark => Self::against(DARK_BG, mode),
            Mode::Light => Self::against(LIGHT_BG, mode),
        }
    }

    /// The palette for the other background.
    ///
    /// For the session only — nothing is written back to config, because the
    /// terminal's background is a fact about the terminal rather than a
    /// preference, and the next session should go on believing what it was told.
    pub fn toggled(&self) -> Self {
        Self::new(match self.mode {
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::Dark,
        })
    }

    /// Adapt every accent to stay legible against `bg`.
    ///
    /// `text` and `dim` are derived rather than named: they're the terminal's
    /// own foreground conceptually, so they're built by pushing the background
    /// toward its opposite until each clears its threshold. Everything else
    /// keeps its hue and moves only in lightness.
    fn against(bg: Rgb, mode: Mode) -> Self {
        let opposite = if luminance(bg) < 0.5 {
            rgb(0xff, 0xff, 0xff)
        } else {
            rgb(0x00, 0x00, 0x00)
        };
        Self {
            text: readable(opposite, bg, TEXT_CONTRAST),
            dim: readable(mix(bg, opposite, 0.55), bg, DIM_CONTRAST),
            border: readable(mix(bg, BLUE, 0.5), bg, DIM_CONTRAST),
            focus: readable(TEAL, bg, DIM_CONTRAST),
            urgent: readable(RED, bg, TEXT_CONTRAST),
            good: readable(GREEN, bg, TEXT_CONTRAST),
            bad: readable(RED, bg, TEXT_CONTRAST),
            merged: readable(MAGENTA, bg, TEXT_CONTRAST),
            quiet: readable(BLUE, bg, DIM_CONTRAST),
            warn: readable(ORANGE, bg, TEXT_CONTRAST),
            key: readable(GOLD, bg, TEXT_CONTRAST),
            bg,
            mode,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(Mode::default())
    }
}

/// One channel's contribution to relative luminance, per WCAG 2.1.
fn channel_luminance(c: u8) -> f64 {
    let c = c as f64 / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// Relative luminance, 0.0 (black) to 1.0 (white).
fn luminance(c: Rgb) -> f64 {
    0.2126 * channel_luminance(c.r)
        + 0.7152 * channel_luminance(c.g)
        + 0.0722 * channel_luminance(c.b)
}

/// The WCAG contrast ratio between two colours: 1.0 for identical, 21.0 for
/// black against white.
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Linear blend: `t` of 0.0 is all `a`, 1.0 is all `b`.
fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let lerp = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    rgb(lerp(a.r, b.r), lerp(a.g, b.g), lerp(a.b, b.b))
}

/// Lift `fg` away from `bg` — toward white on a dark background, black on a
/// light one — until it clears `min` contrast.
///
/// This is what lets one set of hues serve both modes: the hue survives, only
/// its lightness moves. A colour already clearing `min` is returned untouched,
/// so on a dark terminal most accents come through exactly as base16-ocean
/// specifies them.
fn readable(fg: Rgb, bg: Rgb, min: f64) -> Rgb {
    if contrast(fg, bg) >= min {
        return fg;
    }
    let target = if luminance(bg) < 0.5 {
        rgb(0xff, 0xff, 0xff)
    } else {
        rgb(0x00, 0x00, 0x00)
    };
    // 32 steps resolves finer than an 8-bit channel can express across the
    // range, so the first passing step is effectively the least adjustment.
    for step in 1..=32 {
        let candidate = mix(fg, target, f64::from(step) / 32.0);
        if contrast(candidate, bg) >= min {
            return candidate;
        }
    }
    target
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
    }

    #[test]
    fn contrast_spans_the_wcag_range() {
        let black = rgb(0, 0, 0);
        let white = rgb(0xff, 0xff, 0xff);
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 0.01);
    }

    #[test]
    fn every_dark_accent_clears_its_threshold() {
        let t = Theme::new(Mode::Dark);
        for (name, c, min) in [
            ("text", t.text, TEXT_CONTRAST),
            ("urgent", t.urgent, TEXT_CONTRAST),
            ("good", t.good, TEXT_CONTRAST),
            ("bad", t.bad, TEXT_CONTRAST),
            ("merged", t.merged, TEXT_CONTRAST),
            ("warn", t.warn, TEXT_CONTRAST),
            ("key", t.key, TEXT_CONTRAST),
            ("dim", t.dim, DIM_CONTRAST),
            ("border", t.border, DIM_CONTRAST),
            ("focus", t.focus, DIM_CONTRAST),
            ("quiet", t.quiet, DIM_CONTRAST),
        ] {
            let got = contrast(c, DARK_BG);
            assert!(got >= min, "{name} at {} is only {got:.2}:1", hex(c));
        }
    }

    #[test]
    fn every_light_accent_clears_its_threshold() {
        let t = Theme::new(Mode::Light);
        for (name, c, min) in [
            ("text", t.text, TEXT_CONTRAST),
            ("urgent", t.urgent, TEXT_CONTRAST),
            ("good", t.good, TEXT_CONTRAST),
            ("merged", t.merged, TEXT_CONTRAST),
            ("warn", t.warn, TEXT_CONTRAST),
            ("key", t.key, TEXT_CONTRAST),
            ("dim", t.dim, DIM_CONTRAST),
            ("quiet", t.quiet, DIM_CONTRAST),
        ] {
            let got = contrast(c, LIGHT_BG);
            assert!(got >= min, "{name} at {} is only {got:.2}:1", hex(c));
        }
    }

    #[test]
    fn a_dark_terminal_gets_the_base16_ocean_hues_untouched() {
        // The whole point of sharing wiff's accents is that they arrive
        // unmodified in the common case; only a light background moves them.
        let t = Theme::new(Mode::Dark);
        assert_eq!(hex(t.good), "#a3be8c");
        assert_eq!(hex(t.warn), "#d08770");
        assert_eq!(hex(t.key), "#ebcb8b");
        assert_eq!(hex(t.merged), "#b48ead");
    }

    #[test]
    fn readable_leaves_an_already_legible_colour_alone() {
        assert_eq!(readable(GREEN, DARK_BG, TEXT_CONTRAST), GREEN);
    }

    #[test]
    fn readable_lifts_a_colour_that_would_be_illegible() {
        // base16-ocean's red is nowhere near 4.5:1 on white.
        let lifted = readable(RED, LIGHT_BG, TEXT_CONTRAST);
        assert_ne!(lifted, RED);
        assert!(contrast(lifted, LIGHT_BG) >= TEXT_CONTRAST);
        // Still recognisably red: the hue moved in lightness, not around the
        // wheel.
        assert!(
            lifted.r > lifted.g && lifted.r > lifted.b,
            "{}",
            hex(lifted)
        );
    }

    #[test]
    fn mix_interpolates_between_its_ends() {
        let black = rgb(0, 0, 0);
        let white = rgb(0xff, 0xff, 0xff);
        assert_eq!(mix(black, white, 0.0), black);
        assert_eq!(mix(black, white, 1.0), white);
        assert_eq!(hex(mix(black, white, 0.5)), "#808080");
    }

    #[test]
    fn color_renders_for_ratatui() {
        assert_eq!(color(GREEN), Color::Rgb(0xa3, 0xbe, 0x8c));
    }
}
