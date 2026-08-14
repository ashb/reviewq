//! Styled text as a terminal reads it, for a caller that is not drawing a
//! screen.
//!
//! The sibling of [`svg`](crate::svg): both take what the interface would draw
//! and write it into another medium. This one exists so `reviewq help` can print
//! markdown the way the detail pane shows it — one markdown parser, one palette,
//! one set of decisions about what a heading looks like. A second renderer for
//! the CLI would have been a second answer to all three.
//!
//! It goes through a real [`Buffer`] rather than straight from the parsed lines,
//! for the same reason the pane does: wrapping. A paragraph is one line until
//! something wraps it, and the thing that knows how is the widget — so the
//! widget draws it, at the width the caller has, and this walks the result.
//!
//! Escapes are written by hand rather than through a terminal library: what is
//! needed is a handful of sequences, and the caller may be writing to a pipe, a
//! file, or a string in a test rather than to a terminal at all.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget as _, Wrap};

use crate::theme::{Mode, Rgb, Theme};

/// Render markdown the way the interface would, as text a terminal can print.
///
/// `width` is the column count to wrap to. `colour` off gives the same layout
/// with no escapes at all — for a pipe, a `NO_COLOR` environment, or anywhere
/// the caller would rather have plain text. The structure survives either way:
/// the tables, the bullets and the wrapping are in the characters, not in the
/// styling.
pub fn markdown(md: &str, mode: Mode, width: u16, colour: bool) -> String {
    let theme = Theme::new(mode);
    let md = reflow_wide_tables(md, width.max(20));
    let lines: Vec<Line<'_>> = tui_markdown::from_str(&md)
        .lines
        .into_iter()
        // The fence around a code block is how markdown says "code" to a
        // parser, and the parser has already heard it: what comes back is the
        // block's lines, styled as code, with the fence echoed as text on
        // either side. That text is markup nobody wants to read.
        .filter(|line| !is_fence(line))
        .map(|line| crate::ui::themed_markdown(line, &theme))
        .collect();

    let width = width.max(20);
    let mut out = String::new();
    // A table is already laid out — its own renderer decided every column — so
    // wrapping it is destroying it: the box comes apart mid-row and the reader
    // gets a `┐` on a line of its own. Prose is the opposite: it arrives as one
    // long line and is unreadable until something breaks it. So each block is
    // drawn on its own terms, a table at whatever width it needs.
    for block in blocks(lines) {
        match block.kind {
            // Laid out already: drawn at whatever width it needs, untouched.
            Kind::Table => {
                let at = block
                    .lines
                    .iter()
                    .map(Line::width)
                    .max()
                    .unwrap_or(1)
                    .max(1) as u16;
                draw(&mut out, block.lines, at, false, "", &theme, colour);
            }
            // A wrapped item hangs under its own text: coming back to the
            // margin, it would read as another item.
            Kind::Bullets => {
                for line in block.lines {
                    let hang = " ".repeat(marker(&line).unwrap_or(0));
                    draw(
                        &mut out,
                        vec![line],
                        width.saturating_sub(hang.len() as u16).max(20),
                        true,
                        &hang,
                        &theme,
                        colour,
                    );
                }
            }
            Kind::Prose => draw(&mut out, block.lines, width, true, "", &theme, colour),
        }
    }
    out
}

/// Whether a line is nothing but a code fence.
fn is_fence(line: &Line<'_>) -> bool {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let text = text.trim();
    text.starts_with("```") && !text[3..].contains('`')
}

/// How many columns a list item's marker takes, if the line is one: `- ` is
/// two, `10. ` is four. That width is where the item's text starts, and so
/// where a wrapped continuation belongs.
fn marker(line: &Line<'_>) -> Option<usize> {
    let text: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let mut chars = text.chars();
    match chars.next()? {
        '-' | '*' => (chars.next() == Some(' ')).then_some(2),
        first if first.is_ascii_digit() => {
            let digits = 1 + chars.clone().take_while(char::is_ascii_digit).count();
            let mut after = text[digits..].chars();
            let dot = matches!(after.next(), Some('.' | ')'));
            (dot && after.next() == Some(' ')).then_some(digits + 2)
        }
        _ => None,
    }
}

/// Render lines into `out` at `width`, indenting every row after the first by
/// `hang`.
fn draw(
    out: &mut String,
    lines: Vec<Line<'_>>,
    width: u16,
    wrap: bool,
    hang: &str,
    theme: &Theme,
    colour: bool,
) {
    let mut paragraph = Paragraph::new(ratatui::text::Text::from(lines));
    if wrap {
        // `trim: false` so an indented continuation stays indented.
        paragraph = paragraph.wrap(Wrap { trim: false });
    }
    let height = u16::try_from(paragraph.line_count(width)).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    paragraph.render(area, &mut buffer);
    for row in 0..height {
        if row > 0 {
            out.push_str(hang);
        }
        push_row(out, &buffer, row, theme, colour);
        out.push('\n');
    }
}

/// Turn any table too wide for the page into a list, before it is ever drawn as
/// a box.
///
/// A terminal table cannot reflow: its renderer picks column widths from the
/// content, and at 183 columns of prose in two cells that is what it does. The
/// choice is a box the reader scrolls sideways through, a box broken across
/// lines, or the same facts as prose — and the third is the only one that reads.
///
/// Each row becomes `- **first cell** — header: cell · header: cell`, keeping
/// the header as the label for what would otherwise be an unexplained column. A
/// table with no headers (`| | |`, which is how a two-column layout with no
/// column names is written) just joins its cells.
fn reflow_wide_tables(md: &str, width: u16) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rows: Vec<&str> = Vec::new();

    for line in md.lines() {
        if line.trim_start().starts_with('|') {
            rows.push(line);
            continue;
        }
        flush_table(&mut out, &mut rows, width);
        out.push_str(line);
        out.push('\n');
    }
    flush_table(&mut out, &mut rows, width);
    out
}

/// Write the collected table rows out, as a table or as a list.
fn flush_table(out: &mut String, rows: &mut Vec<&str>, width: u16) {
    if rows.is_empty() {
        return;
    }
    let parsed: Vec<Vec<String>> = rows.iter().map(|row| cells(row)).collect();
    // Per column, not per row: a table is as wide as its columns added up, and
    // each column is as wide as the widest cell in it — which is what the
    // renderer will decide. Summing rows instead under-counts whenever the long
    // cells sit in different rows, and then a table that was going to overflow
    // gets drawn as a box anyway. Each column costs its widest cell, a border
    // and two spaces; the last border is the trailing one.
    let columns = parsed.iter().map(Vec::len).max().unwrap_or(0);
    let natural: usize = (0..columns)
        .map(|column| {
            parsed
                .iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
                + 3
        })
        .sum::<usize>()
        + 1;

    if natural <= width as usize || parsed.len() < 2 {
        for row in rows.iter() {
            out.push_str(row);
            out.push('\n');
        }
    } else {
        // Row 1 is the `---` delimiter, never content.
        let headers = &parsed[0];
        for row in parsed.iter().skip(2) {
            let mut bullet = format!("- **{}**", row.first().map_or("", String::as_str));
            let rest: Vec<String> = row
                .iter()
                .enumerate()
                .skip(1)
                .map(|(column, cell)| match headers.get(column) {
                    Some(header) if !header.is_empty() => format!("{header}: {cell}"),
                    _ => cell.clone(),
                })
                .collect();
            if !rest.is_empty() {
                bullet.push_str(" — ");
                bullet.push_str(&rest.join(" · "));
            }
            out.push_str(&bullet);
            out.push('\n');
        }
    }
    rows.clear();
}

/// A table row's cells, without the pipes that separate them.
///
/// `\|` is a pipe inside a cell rather than a separator — which is how a
/// markdown table writes `list [--all\|--waiting]` — so the split honours the
/// escape and then drops it.
fn cells(row: &str) -> Vec<String> {
    let row = row.trim().trim_start_matches('|').trim_end_matches('|');
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    for c in row.chars() {
        match c {
            _ if escaped => {
                // Only the pipe is a markdown escape worth honouring here; any
                // other backslash is the author's own character.
                if c != '|' {
                    cell.push('\\');
                }
                cell.push(c);
                escaped = false;
            }
            '\\' => escaped = true,
            '|' => out.push(std::mem::take(&mut cell).trim().to_string()),
            _ => cell.push(c),
        }
    }
    out.push(cell.trim().to_string());
    out
}

/// Consecutive lines that want the same treatment.
struct Block<'a> {
    lines: Vec<Line<'a>>,
    kind: Kind,
}

/// What a run of lines is, which is what decides how it is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Drawn by the markdown renderer as a box, so it is laid out already.
    Table,
    /// A list, whose continuations hang under the text rather than the marker.
    Bullets,
    /// Everything else, wrapped to the page.
    Prose,
}

/// Split lines into runs of table and not-table.
///
/// A table line is recognised by the box-drawing character it opens with, which
/// is what `tui-markdown` draws one out of. Crude, and safe in the direction
/// that matters: prose does not begin with `├`, and a table row misread as
/// prose would only be wrapped, which is the old behaviour.
fn blocks(lines: Vec<Line<'_>>) -> Vec<Block<'_>> {
    let mut out: Vec<Block<'_>> = Vec::new();
    for line in lines {
        let head: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .take(2)
            .collect();
        let kind = match head.chars().next() {
            Some('┌' | '│' | '├' | '└' | '┬' | '┴' | '┼') => Kind::Table,
            _ if marker(&line).is_some() => Kind::Bullets,
            _ => Kind::Prose,
        };
        match out.last_mut() {
            Some(block) if block.kind == kind => block.lines.push(line),
            _ => out.push(Block {
                lines: vec![line],
                kind,
            }),
        }
    }
    out
}

/// A drawn buffer as text, row by row.
///
/// What [`markdown`] does to its own buffer, for a caller that has one already
/// — a screen the interface drew.
pub(crate) fn buffer(buffer: &Buffer, theme: &Theme, colour: bool) -> String {
    let mut out = String::new();
    for row in 0..buffer.area.height {
        push_row(&mut out, buffer, row, theme, colour);
        out.push('\n');
    }
    out
}

/// One row of the buffer, its trailing blank cells dropped — a terminal has no
/// use for a line padded to the full width, and a reader copying the output has
/// less.
fn push_row(out: &mut String, buffer: &Buffer, row: u16, theme: &Theme, colour: bool) {
    let width = buffer.area.width;
    let last = (0..width)
        .rev()
        .find(|col| buffer[(*col, row)].symbol().trim() != "")
        .map_or(0, |col| col + 1);

    let mut open: Option<Style> = None;
    for col in 0..last {
        let cell = &buffer[(col, row)];
        let style = Style {
            fg: match cell.fg {
                Color::Rgb(r, g, b) => Rgb { r, g, b },
                // Nothing in the palette is anything else; `Reset` means the
                // terminal's own, which here is the theme's text colour.
                _ => theme.text,
            },
            bold: cell.modifier.contains(Modifier::BOLD),
            dim: cell.modifier.contains(Modifier::DIM),
            italic: cell.modifier.contains(Modifier::ITALIC),
            underline: cell.modifier.contains(Modifier::UNDERLINED),
        };
        if colour && open != Some(style) {
            // Reset before opening the next run, so styles cannot accumulate
            // down a line.
            if open.is_some() {
                out.push_str(RESET);
            }
            out.push_str(&style.escape());
            open = Some(style);
        }
        out.push_str(cell.symbol());
    }
    if colour && open.is_some() {
        out.push_str(RESET);
    }
}

const RESET: &str = "\x1b[0m";

/// The styling of one run of cells — what has to change before a new escape is
/// worth writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Rgb,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    fn escape(&self) -> String {
        let mut out = format!("\x1b[38;2;{};{};{}m", self.fg.r, self.fg.g, self.fg.b);
        for (on, code) in [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
        ] {
            if on {
                out.push_str("\x1b[");
                out.push_str(code);
                out.push('m');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text with every `ESC [ … m` sequence taken out.
    fn strip_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn a_table_survives_as_the_characters_that_draw_it() {
        let rendered = markdown(
            "| Verb | Means |\n|---|---|\n| `done` | up to date |\n",
            Mode::Dark,
            80,
            false,
        );

        assert!(rendered.contains('│'), "{rendered}");
        assert!(rendered.contains("Verb"), "{rendered}");
        assert!(rendered.contains("up to date"), "{rendered}");
        assert!(!rendered.contains('\x1b'), "colour off means no escapes");
    }

    #[test]
    fn a_paragraph_is_wrapped_to_the_width_it_was_given() {
        // The whole reason this goes through a buffer: markdown joins the
        // source's hard-wrapped lines into one paragraph, and something has to
        // break it again at the width in front of the reader.
        let prose = "A deterministic pull-request review queue. It answers one \
                     question — what should I look at next? — and every answer \
                     names the rule that produced it.";
        let rendered = markdown(prose, Mode::Dark, 40, false);

        let widths: Vec<usize> = rendered.lines().map(|l| l.chars().count()).collect();
        assert!(widths.len() > 3, "it wrapped: {widths:?}");
        assert!(
            widths.iter().all(|w| *w <= 40),
            "and none of it overruns: {widths:?}"
        );
        // Wrapping is the only difference — every word survives it.
        let flattened = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            flattened,
            prose.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }

    #[test]
    fn prose_around_a_table_still_wraps() {
        let md = "Some prose that is quite long and will certainly need breaking at forty \
                  columns because it goes on.\n\n| A | B |\n|---|---|\n| 1 | 2 |\n";
        let rendered = markdown(md, Mode::Dark, 40, false);

        let prose: Vec<&str> = rendered
            .lines()
            .filter(|l| !l.starts_with(['┌', '│', '├', '└']) && !l.is_empty())
            .collect();
        assert!(prose.len() > 2, "the prose wrapped: {prose:?}");
        assert!(prose.iter().all(|l| l.chars().count() <= 40), "{prose:?}");
    }

    #[test]
    fn colour_is_the_interfaces_own_and_every_run_closes_it() {
        let rendered = markdown("## Heading\n\nProse here.\n", Mode::Dark, 60, true);

        let heading = Theme::new(Mode::Dark).focus;
        assert!(
            rendered.contains(&format!(
                "\x1b[38;2;{};{};{}m",
                heading.r, heading.g, heading.b
            )),
            "a heading is the palette's accent: {rendered:?}"
        );
        assert!(
            rendered.trim_end().ends_with(RESET),
            "nothing is left open to bleed into the shell prompt: {rendered:?}"
        );
        assert_eq!(
            strip_escapes(&rendered),
            markdown("## Heading\n\nProse here.\n", Mode::Dark, 60, false),
            "the escapes are the whole of the difference"
        );
    }

    #[test]
    fn a_row_stops_at_its_last_written_cell() {
        // A buffer is a rectangle; a line of text is not. Padding every row to
        // the full width would make the output unpasteable and every blank line
        // a run of spaces.
        let rendered = markdown("Short.\n", Mode::Dark, 60, false);
        assert_eq!(rendered, "Short.\n");
    }

    #[test]
    fn a_table_too_wide_for_the_page_becomes_a_list() {
        // 183 columns of two-cell prose is not a table anybody can read in a
        // terminal. The facts survive; the box does not.
        let md = "| | |\n|---|---|\n| `sync [N]` | Fetch from the forge and rebuild \
                  the ledger, refresh one PR, or bring in each repo's palette |\n";
        let rendered = markdown(md, Mode::Dark, 60, false);

        assert!(!rendered.contains('│'), "no box: {rendered}");
        assert!(rendered.starts_with("- sync [N] —"), "{rendered}");
        assert!(rendered.contains("rebuild"), "{rendered}");
        // Wrapped under its own text, so a continuation cannot read as another
        // item.
        let continuation = rendered.lines().nth(1).expect("it wrapped");
        assert!(continuation.starts_with("  "), "{continuation:?}");
    }

    #[test]
    fn a_header_labels_the_cells_it_heads() {
        // Without the header a bullet is a row of unexplained values; with it,
        // each cell says which column it came from.
        let md = "| | Reason | Cleared by |\n|---|---|---|\n                  | 1 | mention | anything you do on the PR, or a done of your own \
                  which is a long cell to force the reflow |\n";
        let rendered = markdown(md, Mode::Dark, 60, false);

        assert!(rendered.contains("Reason: mention"), "{rendered}");
        assert!(rendered.contains("Cleared by: anything"), "{rendered}");
    }

    #[test]
    fn an_escaped_pipe_is_a_pipe_and_not_a_column() {
        // How a markdown table writes a command's alternatives, and a naive
        // split turns one cell into three.
        let md = "| | |\n|---|---|\n| `list [--all\\|--waiting\\|--muted]` | The queue, \
                  everything tracked, or what waits on somebody else — a cell long \
                  enough to reflow |\n";
        let rendered = markdown(md, Mode::Dark, 60, false);

        assert!(
            rendered.contains("list [--all|--waiting|--muted]"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("--waiting\\"),
            "the escape is gone: {rendered}"
        );
    }

    #[test]
    fn a_tables_width_is_its_columns_added_up_not_its_widest_row() {
        // The long cells here are in different rows, so summing rows says it
        // fits and summing columns — which is what the renderer does — says it
        // does not. Getting this wrong drew a box wider than the page.
        let md = "| A | B |\n|---|---|\n                  | a cell that is fairly long here | short |\n                  | short | another cell that is fairly long |\n";
        let rendered = markdown(md, Mode::Dark, 60, false);

        assert!(!rendered.contains('┌'), "reflowed, not boxed: {rendered}");
    }

    #[test]
    fn a_code_block_keeps_its_lines_and_loses_its_fence() {
        // Both halves matter, and they pull in opposite directions: the fence
        // is what tells the parser to keep the lines apart, and it is also the
        // one thing in the block a reader has no use for. So it goes on the way
        // out, not on the way in — indenting the source instead had the parser
        // join a shell session into one paragraph.
        let rendered = markdown(
            "```toml\n[identity]\nlogin = \"ashb\"\n\n[[project]]\nname = \"x\"\n```\n",
            Mode::Dark,
            60,
            false,
        );

        assert!(!rendered.contains("```"), "{rendered:?}");
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines.contains(&"[identity]"), "{lines:?}");
        assert!(lines.contains(&"[[project]]"), "{lines:?}");
        assert!(lines.contains(&"login = \"ashb\""), "{lines:?}");
    }

    #[test]
    fn an_inline_code_span_is_not_mistaken_for_a_fence() {
        let rendered = markdown("Set ``x`` and see.\n", Mode::Dark, 60, false);
        assert!(rendered.contains("Set"), "{rendered:?}");
        assert!(rendered.contains("and see"), "{rendered:?}");
    }

    #[test]
    fn a_numbered_item_hangs_under_its_own_text() {
        // Four columns for `10. `, two for `- `: a continuation back at the
        // margin reads as the next item rather than the rest of this one.
        let md = "1. A first item long enough that it has to wrap somewhere \
                  around here.\n2. A second.\n";
        let rendered = markdown(md, Mode::Dark, 40, false);

        let lines: Vec<&str> = rendered.lines().filter(|l| !l.is_empty()).collect();
        assert!(lines[0].starts_with("1. "), "{lines:?}");
        assert!(
            lines[1].starts_with("   ") && !lines[1].starts_with("    "),
            "the continuation hangs under the text: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("2. ")), "{lines:?}");
    }

    #[test]
    fn a_table_that_fits_is_left_as_a_table() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let rendered = markdown(md, Mode::Dark, 60, false);

        assert!(rendered.contains('┌'), "{rendered}");
        assert!(!rendered.contains("- "), "{rendered}");
    }

    #[test]
    fn the_light_palette_is_a_different_ink() {
        let dark = markdown("## Heading\n", Mode::Dark, 60, true);
        let light = markdown("## Heading\n", Mode::Light, 60, true);

        assert_ne!(
            dark, light,
            "the theme reaches this as it reaches the panes"
        );
    }
}
