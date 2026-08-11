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

/// What the same two thresholds become on a light background.
///
/// The accents are a *dark* palette's — base16-ocean — so on white they start
/// nowhere near legible and `readable` only lifts them as far as it is told. At
/// the dark thresholds the result technically passed and looked washed out: the
/// focus accent, which is the selected pane's border and the selection marker,
/// came out at 3.04:1, and every other accent sat just over 4.5. The same ratio is
/// not the same legibility in both directions, so the light palette asks for more.
const LIGHT_TEXT_CONTRAST: f64 = 7.0;
/// See [`LIGHT_TEXT_CONTRAST`].
const LIGHT_DIM_CONTRAST: f64 = 4.5;

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
        let (text_min, dim_min) = match mode {
            Mode::Dark => (TEXT_CONTRAST, DIM_CONTRAST),
            Mode::Light => (LIGHT_TEXT_CONTRAST, LIGHT_DIM_CONTRAST),
        };
        Self {
            text: readable(opposite, bg, text_min),
            dim: readable(mix(bg, opposite, 0.55), bg, dim_min),
            border: readable(mix(bg, BLUE, 0.5), bg, dim_min),
            focus: readable(TEAL, bg, dim_min),
            urgent: readable(RED, bg, text_min),
            good: readable(GREEN, bg, text_min),
            bad: readable(RED, bg, text_min),
            merged: readable(MAGENTA, bg, text_min),
            quiet: readable(BLUE, bg, dim_min),
            warn: readable(ORANGE, bg, text_min),
            key: readable(GOLD, bg, text_min),
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
impl Theme {
    /// A colour from somewhere else, made legible here without losing its hue.
    ///
    /// For the labels: GitHub picks those to sit on its own chips, so plenty of
    /// them are unreadable as text on this background — `000000` is a real label
    /// colour. Lightness moves until it clears the same threshold the palette's
    /// own accents do, which keeps `area:Scheduler` recognisably the green the
    /// forge shows while keeping it readable.
    pub fn adapt(&self, colour: Rgb) -> Rgb {
        let min = match self.mode {
            Mode::Dark => DIM_CONTRAST,
            Mode::Light => LIGHT_DIM_CONTRAST,
        };
        readable(colour, self.bg, min)
    }
}

/// Parse a forge's six hex digits, with or without a leading `#`.
///
/// `None` for anything else, which a caller draws in its own colour rather than
/// guessing — the forge could hand us any string, and a label is not worth an
/// error.
pub fn from_hex(hex: &str) -> Option<Rgb> {
    let hex = hex.strip_prefix('#').unwrap_or(hex);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some(Rgb {
        r: channel(0)?,
        g: channel(2)?,
        b: channel(4)?,
    })
}

fn readable(fg: Rgb, bg: Rgb, min: f64) -> Rgb {
    if contrast(fg, bg) >= min {
        return fg;
    }
    // Lightness only, in HSL — not a mix toward black or white.
    //
    // Mixing scales every channel by the same factor, which preserves the ratios
    // between them and so the hue. It does not preserve *saturation*: HSL measures
    // that against the distance to the nearest extreme, so dragging a colour
    // toward black flattens it. Lifting for a dark background happened to be
    // lossless — the accents are already light — but darkening for a light one
    // took gold from 0.71 saturation to 0.26, which is the whole reason light mode
    // looked like mud.
    let hsl = to_hsl(fg);
    let darken = luminance(bg) >= 0.5;
    // 64 steps resolves finer than an 8-bit channel can express, so the first
    // passing step is effectively the least adjustment that works.
    for step in 1..=64 {
        let t = f64::from(step) / 64.0;
        let l = if darken {
            hsl.l * (1.0 - t)
        } else {
            hsl.l + (1.0 - hsl.l) * t
        };
        let candidate = from_hsl(Hsl { l, ..hsl });
        if contrast(candidate, bg) >= min {
            return candidate;
        }
    }
    // Nothing at this hue reaches the threshold, so fall back to the extreme.
    if darken {
        rgb(0x00, 0x00, 0x00)
    } else {
        rgb(0xff, 0xff, 0xff)
    }
}

/// A colour as hue, saturation and lightness, which is the space an accent has to
/// be adapted in: legibility is a lightness problem, and everything that makes the
/// colour recognisable is in the other two.
#[derive(Debug, Clone, Copy)]
struct Hsl {
    /// Degrees around the wheel, 0.0..360.0.
    h: f64,
    /// 0.0 grey, 1.0 fully saturated.
    s: f64,
    /// 0.0 black, 1.0 white.
    l: f64,
}

fn to_hsl(c: Rgb) -> Hsl {
    let (r, g, b) = (
        f64::from(c.r) / 255.0,
        f64::from(c.g) / 255.0,
        f64::from(c.b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let span = max - min;
    if span.abs() < f64::EPSILON {
        // A grey has no hue to preserve, and stays a grey however it is moved.
        return Hsl { h: 0.0, s: 0.0, l };
    }
    let s = if l < 0.5 {
        span / (max + min)
    } else {
        span / (2.0 - max - min)
    };
    let h = if max == r {
        60.0 * (((g - b) / span) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / span + 2.0)
    } else {
        60.0 * ((r - g) / span + 4.0)
    };
    Hsl {
        h: if h < 0.0 { h + 360.0 } else { h },
        s,
        l,
    }
}

fn from_hsl(hsl: Hsl) -> Rgb {
    let Hsl { h, s, l } = hsl;
    if s.abs() < f64::EPSILON {
        let v = channel(l);
        return rgb(v, v, v);
    }
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime % 2.0 - 1.0).abs());
    let (r, g, b) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    rgb(channel(r + m), channel(g + m), channel(b + m))
}

/// A 0.0..1.0 channel as the nearest byte, clamped.
fn channel(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
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
        // The light thresholds, which are higher — see `LIGHT_TEXT_CONTRAST`. Every
        // field is listed, `focus` and `border` included: they were the ones that
        // technically passed at the dark thresholds and were unreadable in
        // practice.
        let t = Theme::new(Mode::Light);
        for (name, c, min) in [
            ("text", t.text, LIGHT_TEXT_CONTRAST),
            ("urgent", t.urgent, LIGHT_TEXT_CONTRAST),
            ("good", t.good, LIGHT_TEXT_CONTRAST),
            ("bad", t.bad, LIGHT_TEXT_CONTRAST),
            ("merged", t.merged, LIGHT_TEXT_CONTRAST),
            ("warn", t.warn, LIGHT_TEXT_CONTRAST),
            ("key", t.key, LIGHT_TEXT_CONTRAST),
            ("dim", t.dim, LIGHT_DIM_CONTRAST),
            ("border", t.border, LIGHT_DIM_CONTRAST),
            ("focus", t.focus, LIGHT_DIM_CONTRAST),
            ("quiet", t.quiet, LIGHT_DIM_CONTRAST),
        ] {
            let got = contrast(c, LIGHT_BG);
            assert!(got >= min, "{name} at {} is only {got:.2}:1", hex(c));
        }
    }

    #[test]
    fn no_light_accent_is_left_at_the_dark_thresholds() {
        // What the complaint was: at 3:1 the focus accent — the selected pane's
        // border and the selection marker — was 3.04:1 on white, which passes a
        // guideline and cannot be seen.
        let t = Theme::new(Mode::Light);
        for (name, c) in [("border", t.border), ("focus", t.focus), ("quiet", t.quiet)] {
            let got = contrast(c, LIGHT_BG);
            assert!(
                got > DIM_CONTRAST + 1.0,
                "{name} is {got:.2}:1, barely over the dark floor"
            );
        }
    }

    #[test]
    fn a_forge_colour_is_read_with_or_without_its_hash() {
        assert_eq!(from_hex("0e8a16"), Some(rgb(0x0e, 0x8a, 0x16)));
        assert_eq!(from_hex("#0e8a16"), Some(rgb(0x0e, 0x8a, 0x16)));
        // Anything else is drawn in our own colour rather than guessed at.
        assert_eq!(from_hex("green"), None);
        assert_eq!(from_hex("0e8a1"), None);
        assert_eq!(from_hex(""), None);
    }

    #[test]
    fn a_label_colour_is_made_legible_without_losing_its_hue() {
        // GitHub picks these to sit on its own chips: `000000` is a real label
        // colour, and unreadable as text on a dark background.
        for mode in [Mode::Dark, Mode::Light] {
            let theme = Theme::new(mode);
            let black = from_hex("000000").expect("parses");
            let adapted = theme.adapt(black);

            assert!(
                contrast(adapted, theme.bg) >= 3.0,
                "{mode:?}: {} on {}",
                hex(adapted),
                hex(theme.bg)
            );
        }

        // One that already reads is left exactly as the forge chose it.
        let dark = Theme::new(Mode::Dark);
        let green = from_hex("0e8a16").expect("parses");
        assert_eq!(dark.adapt(green), dark.adapt(dark.adapt(green)));

        // And a hue survives being made legible: airflow's blue stays blue.
        let blue = from_hex("1d76db").expect("parses");
        let adapted = Theme::new(Mode::Light).adapt(blue);
        assert!(
            (to_hsl(adapted).h - to_hsl(blue).h).abs() < 1.0,
            "hue moved from {} to {}",
            to_hsl(blue).h,
            to_hsl(adapted).h
        );
    }

    #[test]
    fn adapting_an_accent_keeps_its_hue() {
        let t = Theme::new(Mode::Light);
        assert!(
            t.urgent.r > t.urgent.g && t.urgent.r > t.urgent.b,
            "{}",
            hex(t.urgent)
        );
        assert!(
            t.good.g > t.good.r && t.good.g > t.good.b,
            "{}",
            hex(t.good)
        );
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
    /// HSL saturation, as the eye reads "how much colour is in this".
    fn saturation(c: Rgb) -> f64 {
        to_hsl(c).s
    }

    #[test]
    fn adapting_an_accent_keeps_its_saturation() {
        // The light palette's real defect. Darkening by mixing toward black
        // preserved each hue and quietly drained it — gold arrived at 0.26
        // saturation from 0.71, which is what "everything is unreadable mud" was.
        // Moving lightness in HSL leaves the other two axes alone.
        for (mode, bg) in [(Mode::Dark, DARK_BG), (Mode::Light, LIGHT_BG)] {
            let t = Theme::new(mode);
            for (name, adapted, base) in [
                ("urgent", t.urgent, RED),
                ("good", t.good, GREEN),
                ("merged", t.merged, MAGENTA),
                ("warn", t.warn, ORANGE),
                ("key", t.key, GOLD),
                ("focus", t.focus, TEAL),
            ] {
                let (was, now) = (saturation(base), saturation(adapted));
                assert!(
                    now >= was - 0.02,
                    "{mode:?} {name} lost saturation: {was:.2} → {now:.2} ({})",
                    hex(adapted)
                );
                assert!(contrast(adapted, bg) >= DIM_CONTRAST);
            }
        }
    }

    #[test]
    fn hsl_round_trips_every_accent() {
        for c in [RED, GREEN, BLUE, TEAL, GOLD, ORANGE, MAGENTA] {
            let back = from_hsl(to_hsl(c));
            // Within one rounding step per channel: the conversion is lossy only
            // in the last bit, which no eye and no terminal can tell apart.
            assert!(
                back.r.abs_diff(c.r) <= 1 && back.g.abs_diff(c.g) <= 1 && back.b.abs_diff(c.b) <= 1,
                "{} became {}",
                hex(c),
                hex(back)
            );
        }
    }

    #[test]
    fn a_grey_has_no_hue_to_lose() {
        let grey = rgb(0x80, 0x80, 0x80);
        let hsl = to_hsl(grey);
        assert_eq!(hsl.s, 0.0);
        assert_eq!(from_hsl(hsl), grey);
        let darkened = readable(grey, LIGHT_BG, LIGHT_DIM_CONTRAST);
        assert_eq!(darkened.r, darkened.g, "{}", hex(darkened));
        assert_eq!(darkened.g, darkened.b, "{}", hex(darkened));
    }
}
