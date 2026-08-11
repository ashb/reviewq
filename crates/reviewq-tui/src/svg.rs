//! Drawing a rendered frame as an SVG.
//!
//! A terminal screenshot is a photograph of a font; this is the picture itself —
//! the cells ratatui composed, with their own colours, as text a browser draws.
//! It exists because a queue is the sort of thing you want to paste into an issue
//! or a README, and cropping a terminal window is a poor way to get one.
//!
//! Pure: a [`Buffer`] and a [`Theme`] in, a `String` out. Where that string goes
//! is the frontend's business (see [`Hooks::save_screen`](crate::Hooks)), which
//! also keeps this snapshot-testable without a terminal or a filesystem.
//!
//! Cells are laid on a fixed grid rather than left to the font's own metrics, and
//! each run is stretched to the width its cells occupy — so a viewer resolving a
//! different monospace face than yours gets the same columns, not a slowly
//! diverging one.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use reviewq_app::config::Svg;

use crate::theme::{Rgb, Theme};

/// One cell's width, in SVG user units.
const CELL_W: f32 = 8.4;
/// One cell's height.
const CELL_H: f32 = 18.0;
/// Where a row's text sits inside its cell — the font's baseline.
const BASELINE: f32 = 13.5;
/// Type size. Paired with [`CELL_W`] to be about right for a monospace face, so
/// the per-run stretching has little work to do.
const FONT_SIZE: f32 = 14.0;

/// Draw `buffer` as a standalone SVG document.
pub(crate) fn render(buffer: &Buffer, theme: &Theme, options: &Svg) -> String {
    let area = buffer.area;
    let (cols, rows) = (area.width, area.height);
    let width = f32::from(cols) * CELL_W;
    let height = f32::from(rows) * CELL_H;

    let mut out = String::with_capacity(cols as usize * rows as usize * 8);
    out.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.0}\" height=\"{height:.0}\" \
         viewBox=\"0 0 {width:.0} {height:.0}\" font-family={family} font-size=\"{FONT_SIZE}\">\n",
        family = attribute(&options.font_family),
    ));
    // The imports go in a stylesheet rather than on the root element, because
    // that is the only place an SVG can ask for a font it does not carry.
    if !options.font_css.is_empty() {
        out.push_str("<style>\n");
        for href in &options.font_css {
            out.push_str(&format!("@import url({});\n", attribute(href)));
        }
        out.push_str("</style>\n");
    }
    // The page's own background first, so only the cells that differ from it
    // need a rectangle of their own.
    out.push_str(&format!(
        "<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n",
        hex(theme.bg)
    ));

    for y in 0..rows {
        for run in runs(buffer, y, theme) {
            out.push_str(&run.render(y));
        }
    }
    out.push_str("</svg>\n");
    out
}

/// A stretch of one row's cells sharing a style — drawn as one rectangle and one
/// piece of text rather than as a hundred of each.
struct Run {
    /// Column it starts at.
    at: u16,
    /// The text, one entry per cell it covers.
    text: String,
    /// How many cells that is. Counted rather than measured from `text`, whose
    /// graphemes may be wider than one cell.
    cells: u16,
    fg: Rgb,
    bg: Rgb,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    /// The colour the page is already painted, which a run matching it needs no
    /// rectangle to repeat.
    page_bg: Rgb,
}

impl Run {
    /// Whether `next` can be appended to this run: same everything but position.
    fn joins(&self, next: &Self) -> bool {
        self.fg == next.fg
            && self.bg == next.bg
            && self.bold == next.bold
            && self.dim == next.dim
            && self.italic == next.italic
            && self.underline == next.underline
    }

    fn render(&self, row: u16) -> String {
        let x = f32::from(self.at) * CELL_W;
        let y = f32::from(row) * CELL_H;
        let width = f32::from(self.cells) * CELL_W;

        let mut out = String::new();
        if self.bg != self.page_bg {
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{width:.1}\" height=\"{CELL_H}\" \
                 fill=\"{}\"/>\n",
                hex(self.bg)
            ));
        }
        // Whitespace needs its background and nothing else; skipping the text
        // saves most of the elements on a mostly-empty screen.
        if self.text.trim().is_empty() {
            return out;
        }

        let mut attrs = String::new();
        if self.bold {
            attrs.push_str(" font-weight=\"bold\"");
        }
        if self.italic {
            attrs.push_str(" font-style=\"italic\"");
        }
        if self.underline {
            attrs.push_str(" text-decoration=\"underline\"");
        }
        if self.dim {
            attrs.push_str(" opacity=\"0.65\"");
        }
        out.push_str(&format!(
            "<text x=\"{x:.1}\" y=\"{:.1}\" fill=\"{}\" textLength=\"{width:.1}\" \
             lengthAdjust=\"spacing\" xml:space=\"preserve\"{attrs}>{}</text>\n",
            y + BASELINE,
            hex(self.fg),
            escape(&self.text),
        ));
        out
    }
}

/// One row's cells, grouped into runs of a single style.
fn runs(buffer: &Buffer, row: u16, theme: &Theme) -> Vec<Run> {
    let area = buffer.area;
    let mut out: Vec<Run> = Vec::new();
    for col in 0..area.width {
        let cell = &buffer[(area.x + col, area.y + row)];
        let reversed = cell.modifier.contains(Modifier::REVERSED);
        let (fg, bg) = (colour(cell.fg, theme.text), colour(cell.bg, theme.bg));
        let (fg, bg) = if reversed { (bg, fg) } else { (fg, bg) };
        let style = Run {
            at: col,
            text: cell.symbol().to_string(),
            cells: 1,
            fg,
            bg,
            bold: cell.modifier.contains(Modifier::BOLD),
            dim: cell.modifier.contains(Modifier::DIM),
            italic: cell.modifier.contains(Modifier::ITALIC),
            underline: cell.modifier.contains(Modifier::UNDERLINED),
            page_bg: theme.bg,
        };
        match out.last_mut() {
            Some(last) if last.joins(&style) => {
                last.text.push_str(&style.text);
                last.cells += 1;
            }
            _ => out.push(style),
        }
    }
    out
}

/// Resolve a ratatui colour to an RGB triple.
///
/// The palette is RGB throughout — see [`crate::theme`] — so the only other
/// thing that can arrive is `Reset`, which means "whatever the terminal uses"
/// and here means the theme's own.
fn colour(colour: Color, fallback: Rgb) -> Rgb {
    match colour {
        Color::Rgb(r, g, b) => Rgb { r, g, b },
        // Every other variant is an indexed or named terminal colour, which
        // nothing in this interface asks for — as is `Reset`, which means
        // whatever the terminal uses and here means the theme's own. Neither is
        // guessed at: a guess would be a colour the terminal never showed.
        _ => fallback,
    }
}

fn hex(rgb: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)
}

/// Escape text for an XML text node.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Quote a string as an XML attribute value.
fn attribute(value: &str) -> String {
    let escaped: String = value
        .chars()
        .map(|c| match c {
            '"' => "&quot;".to_string(),
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;
    use ratatui::{Terminal, backend::TestBackend};

    /// A small screen with something of everything the renderer has to carry:
    /// colour, bold, and a character XML would choke on.
    fn drawn() -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(20, 2)).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(vec![
                            Span::styled(
                                "a & b",
                                Style::default()
                                    .fg(ratatui::style::Color::Rgb(1, 2, 3))
                                    .bold(),
                            ),
                            Span::styled(
                                "<c>",
                                Style::default().bg(ratatui::style::Color::Rgb(9, 8, 7)),
                            ),
                        ]),
                        Line::from("plain"),
                    ]),
                    Rect::new(0, 0, 20, 2),
                );
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn a_drawn_screen_becomes_a_standalone_document() {
        let picture = render(&drawn(), &Theme::default(), &Svg::default());
        insta::assert_snapshot!(picture);
    }

    #[test]
    fn markup_in_the_queue_cannot_break_the_document() {
        // PR titles are somebody else's text, and `&` and `<` are ordinary in
        // them — unescaped, either would make the file unopenable.
        let picture = render(&drawn(), &Theme::default(), &Svg::default());
        assert!(picture.contains("a &amp; b"), "{picture}");
        assert!(picture.contains("&lt;c&gt;"), "{picture}");
    }

    #[test]
    fn a_run_of_one_style_is_drawn_once_rather_than_per_cell() {
        // 20x2 cells, and most of them are the same blank: a per-cell renderer
        // would emit forty of everything.
        let picture = render(&drawn(), &Theme::default(), &Svg::default());
        assert!(
            picture.matches("<text").count() <= 4,
            "one text element per run, was {}:\n{picture}",
            picture.matches("<text").count()
        );
    }

    #[test]
    fn asking_for_no_stylesheet_fetches_nothing() {
        let offline = Svg {
            font_css: Vec::new(),
            ..Svg::default()
        };
        let picture = render(&drawn(), &Theme::default(), &offline);
        assert!(!picture.contains("@import"), "{picture}");
        // The XML namespace is a name rather than an address — nothing fetches
        // it — so what must be absent is the stylesheet, not the string "http".
        assert!(!picture.contains("fonts.bunny.net"), "{picture}");
        assert!(!picture.contains("<style>"), "{picture}");
    }
}
