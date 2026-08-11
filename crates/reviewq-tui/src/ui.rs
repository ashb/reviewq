//! Drawing. Reads [`App`], writes to the frame, decides nothing.
//!
//! No widget names a colour: everything comes from [`Theme`], so the palette is
//! changed in one place. Nor does anything set a background — see the module
//! docs on [`crate::theme`] for why reviewq sits on the terminal's own.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap};
use reviewq_app::present;
use reviewq_core::model::{PrState, Verdict};
use reviewq_ledger::{Located, QueueItem, RepoKey};

use crate::app::{App, Focus, Overlay, SNOOZE_PRESETS};
use crate::keys::{self, Action};
use crate::theme::{Theme, color};

/// What the mouse does, for the key reference.
///
/// Not in [`keys::BINDINGS`]: that table maps key chords to actions, and a click
/// is neither — but someone opening the reference to find out what they can press
/// wants to know the mouse works too.
const MOUSE_GESTURES: &[(&str, &str)] = &[
    ("click", "select the row, or focus the pane"),
    ("wheel", "scroll what is under the pointer"),
];

/// Draw the whole screen: header, the queue beside the detail, footer.
pub fn draw(frame: &mut Frame, app: &mut App) {
    // The background first, over everything. ratatui styles are patches — a span
    // that names only a foreground leaves the cell's background alone — so one
    // fill here reaches every cell that nothing else deliberately repaints.
    frame.render_widget(
        Block::new().style(Style::default().bg(color(app.theme.bg))),
        frame.area(),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // The queue is fixed-ish and the detail is what grows: a narrow terminal
    // should shrink the detail pane, not squeeze the titles out of the queue.
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);

    // Tell the app how tall the focused pane came out, so a paging key can move
    // by exactly a screenful. Measured here because this is where the layout is
    // decided — the two border rows are the only thing between pane and rows.
    app.set_page(cols[0].height.saturating_sub(2) as usize);

    header(frame, rows[0], app);
    let queue_inner = queue_pane(frame, cols[0], app);
    let (lines, detail_inner) = detail_pane(frame, cols[1], app);
    // Now that the content has been laid out, its length is known — which is
    // what stops a scroll running off the end of a short description.
    app.set_detail_lines(lines);
    // And where it landed is known, which is what turns a click into a row.
    app.set_pane_areas(queue_inner, detail_inner);
    footer(frame, rows[2], app);

    // Last, so it covers the panes rather than being drawn under them.
    overlay(frame, rows[1], app);
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut spans = vec![
        Span::styled("reviewq", Style::default().fg(color(t.text)).bold()),
        Span::styled("  ", Style::default()),
        Span::styled(
            format!("{} on the queue", app.queue.len()),
            Style::default().fg(color(t.dim)),
        ),
    ];
    if app.repo_count > 1 {
        spans.push(Span::styled(
            format!("  ·  {} repos", app.repo_count),
            Style::default().fg(color(t.dim)),
        ));
    }
    // Work in flight outranks a note about work that finished: it's the thing
    // you're waiting on, and it says the interface hasn't forgotten your key.
    if !app.refreshing.is_empty() {
        let numbers: Vec<String> = app
            .refreshing
            .iter()
            .map(|number| format!("#{number}"))
            .collect();
        spans.push(Span::styled(
            format!("  ·  refreshing {}…", numbers.join(", ")),
            Style::default().fg(color(t.focus)),
        ));
    } else if let Some(status) = &app.status {
        spans.push(Span::styled(
            format!("  ·  {status}"),
            Style::default().fg(color(t.warn)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A bordered, titled, padded panel; returns the area left inside it.
fn panel(frame: &mut Frame, area: Rect, title: &str, focused: bool, t: &Theme) -> Rect {
    let edge = if focused { t.focus } else { t.border };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(edge)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(color(if focused { t.focus } else { t.dim })),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Draw the queue, returning the area its rows occupy so a click can be resolved
/// back to one.
fn queue_pane(frame: &mut Frame, area: Rect, app: &App) -> Rect {
    let t = &app.theme;
    let inner = panel(frame, area, "Queue", app.focus == Focus::Queue, t);
    if app.queue.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing on the queue.")
                .style(Style::default().fg(color(t.dim)))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return inner;
    }

    // The window is held in `App`, moved only when the selection nears an edge —
    // see `keep_selection_visible`.
    let height = inner.height as usize;
    let first = app.queue_scroll.min(app.queue.len().saturating_sub(1));
    let lines: Vec<Line> = app
        .queue
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, item)| queue_row(item, index == app.selected, app.repo_count > 1, t))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    inner
}

fn queue_row(item: &Located<QueueItem>, selected: bool, multi: bool, t: &Theme) -> Line<'static> {
    let q = &item.item;
    let marker = if selected { "▸ " } else { "  " };
    let mut style = Style::default().fg(color(t.text));
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    // Deferred and silenced PRs are still listed, so they need to read as
    // set-aside rather than simply less urgent.
    let reason_colour = if q.deferred {
        t.quiet
    } else if q.top.priority() <= 2 {
        t.urgent
    } else {
        t.focus
    };

    let mut spans = vec![
        Span::styled(marker, Style::default().fg(color(t.focus))),
        Span::styled(
            number_label(multi, &item.repo, q.pr.number),
            Style::default().fg(color(t.dim)),
        ),
        Span::raw("  "),
        Span::styled(
            q.top.reason.to_string(),
            Style::default().fg(color(reason_colour)),
        ),
    ];
    if !q.pr.state.is_open() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[{}]", q.pr.state.as_str()),
            Style::default().fg(color(state_colour(q.pr.state, t))),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(q.pr.title.clone(), style));
    Line::from(spans)
}

/// Draw the detail pane, returning how many lines its content came to — so the
/// caller can bound scrolling — and the area it drew them in.
fn detail_pane(frame: &mut Frame, area: Rect, app: &App) -> (usize, Rect) {
    let t = &app.theme;
    let title = match app.current() {
        Some(item) => number_label(app.repo_count > 1, &item.repo, item.item.pr.number),
        None => "Detail".to_string(),
    };
    let inner = panel(frame, area, &title, app.focus == Focus::Detail, t);

    let Some(show) = &app.detail else {
        frame.render_widget(
            Paragraph::new("Select a PR to see its detail.")
                .style(Style::default().fg(color(t.dim))),
            inner,
        );
        return (0, inner);
    };

    let pr = &show.pr;
    // Declared before `lines` so it outlives them: `tui_markdown` borrows from
    // the string it parses, and drop order is reverse of declaration.
    let description = show.body.as_deref().map(strip_html_comments);
    let mut lines = vec![
        Line::from(Span::styled(
            pr.title.clone(),
            Style::default().fg(color(t.text)).bold(),
        )),
        Line::from(vec![
            Span::styled(
                pr.state.as_str().to_string(),
                Style::default().fg(color(state_colour(pr.state, t))),
            ),
            Span::styled(
                format!(" · @{}", pr.author),
                Style::default().fg(color(t.dim)),
            ),
            Span::styled(
                format!(
                    " · {}",
                    show.tracked_reason.as_deref().unwrap_or("untracked")
                ),
                Style::default().fg(color(t.dim)),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("updated {}", present::stamp(pr.updated_at)),
                Style::default().fg(color(t.dim)),
            ),
            // Which branch the change is aimed at, where it is known — a row
            // stored before it was captured has it empty until the next sync, and
            // an arrow pointing at nothing would read as a bug.
            Span::styled(
                if pr.base_ref.is_empty() {
                    String::new()
                } else {
                    format!(" · → {}", pr.base_ref)
                },
                Style::default().fg(color(t.dim)),
            ),
        ]),
    ];

    if pr.is_draft {
        lines.push(Line::from(Span::styled(
            "draft",
            Style::default().fg(color(t.warn)),
        )));
    }

    let silenced = present::silenced(&show.my_state);
    if !silenced.is_empty() {
        lines.push(Line::from(Span::styled(
            silenced.join(", "),
            Style::default().fg(color(t.quiet)),
        )));
    }

    // Wording shared with `show`; only the colour is this frontend's.
    if let Some(note) = present::done_note(pr, &show.my_state) {
        lines.push(Line::from(Span::styled(
            note.text,
            Style::default().fg(color(if note.superseded { t.warn } else { t.dim })),
        )));
    }

    lines.push(Line::from(""));
    lines.push(section("Attention", t));
    if show.attention.is_empty() {
        lines.push(Line::from(Span::styled(
            "  none",
            Style::default().fg(color(t.dim)),
        )));
    }
    for a in &show.attention {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  p{}  ", a.priority()),
                Style::default().fg(color(t.dim)),
            ),
            Span::styled(
                a.reason.to_string(),
                Style::default().fg(color(if a.priority() <= 2 { t.urgent } else { t.focus })),
            ),
        ]));
    }

    if !show.reviewers.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("Reviewers", t));
        for r in &show.reviewers {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<18}", r.verdict.as_str()),
                    Style::default().fg(color(verdict_colour(r.verdict, t))),
                ),
                Span::styled(format!("@{}", r.login), Style::default().fg(color(t.dim))),
            ]));
        }
    }

    if !show.threads.is_empty() {
        let counts = present::thread_counts(&show.threads);
        lines.push(Line::from(""));
        lines.push(section("Threads", t));
        lines.push(Line::from(Span::styled(
            format!(
                "  {} total, {} you own, {} resolved",
                counts.total, counts.owned, counts.resolved
            ),
            Style::default().fg(color(t.dim)),
        )));
    }

    // The description last, since it's the only unbounded section — the facts
    // above it should never be pushed off the top.
    if let Some(body) = description
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        lines.push(Line::from(""));
        lines.push(section("Description", t));
        lines.extend(
            tui_markdown::from_str(body)
                .lines
                .into_iter()
                .map(|line| themed_markdown(line, t)),
        );
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Post-wrap, because that's the unit `scroll` moves in: counting the
    // `Line`s would under-count a wrapped description and stop `G` short of
    // the end.
    let total = paragraph.line_count(inner.width);
    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), inner);
    (total, inner)
}

/// Restyle a markdown line into the interface's own palette.
///
/// `tui-markdown` colours for a dark terminal and expects to own the screen:
/// prose comes back with *no* foreground at all, which means the terminal's
/// default — light grey on a dark terminal, and so invisible over reviewq's white
/// background — while headings arrive cyan and code arrives white on black. None
/// of those are the theme's, and two of them are illegible in light mode.
///
/// So the colours are replaced rather than adapted: every accent in the palette is
/// already contrast-checked against the background, which is a stronger guarantee
/// than pushing a foreign colour around until it passes. The *structure* survives —
/// bold, italic, and which lines are headings or code.
///
/// It reads `tui-markdown`'s own scheme to know which is which, so a change there
/// would need a change here; [`markdown_keeps_its_structure_in_our_colours`] fails
/// if that scheme moves.
fn themed_markdown<'a>(mut line: Line<'a>, t: &Theme) -> Line<'a> {
    let heading = line.style.fg.is_some();
    let fenced = line.style.bg.is_some();
    let modifiers = line.style.add_modifier;
    line.style = Style::default();
    for span in &mut line.spans {
        let inline_code = span.style.bg.is_some();
        let role = if heading {
            t.focus
        } else if fenced || inline_code {
            t.key
        } else {
            t.text
        };
        span.style = Style::default()
            .fg(color(role))
            .add_modifier(span.style.add_modifier | modifiers);
    }
    line
}

/// Remove `<!-- ... -->` runs from markdown.
///
/// A PR template is mostly commented instructions to the author, which GitHub
/// hides and a reader has never wanted to see. `pulldown-cmark` treats them as
/// HTML blocks and passes them straight through, so several screens of "delete
/// this section" would otherwise be the first thing in the panel.
///
/// An unterminated `<!--` swallows the rest of the body, matching how a browser
/// and GitHub itself both render it.
fn strip_html_comments(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + "-->".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn section(name: &str, t: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        name.to_string(),
        Style::default()
            .fg(color(t.dim))
            .add_modifier(Modifier::BOLD),
    ))
}

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let mut spans = Vec::new();
    for binding in keys::described().filter(|b| b.footer) {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            footer_keys(binding),
            Style::default().fg(color(t.key)).bold(),
        ));
        spans.push(Span::styled(
            format!(" {}", footer_label(binding, app.focus)),
            Style::default().fg(color(t.dim)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A binding's chord as the footer shows it — terser than the overlay's where
/// that buys a column or two, since the footer is what has to keep fitting.
fn footer_keys(binding: &keys::Binding) -> &'static str {
    match binding.action {
        Action::Down => "jk",
        _ => binding.keys,
    }
}

/// A binding's footer label.
///
/// Shorter than the overlay's `what` where that matters — the footer is the
/// thing that has to fit 80 columns as bindings accumulate, and the overlay is
/// where the roomier wording lives. `Down` is the exception that changes meaning
/// rather than length: `j` moving in one pane and scrolling in the other reads
/// as a bug unless the label says which.
fn footer_label(binding: &keys::Binding, focus: Focus) -> &'static str {
    match (binding.action, focus) {
        (Action::Down, Focus::Detail) => "scroll",
        (Action::SwitchPane, _) => "pane",
        (Action::RefreshSelected, _) => "refresh",
        (Action::Review, _) => "review",
        (Action::Done, _) => "done",
        (Action::Snooze, _) => "snooze",
        _ => binding.what,
    }
}

/// Draw whichever overlay is up, over `area`.
fn overlay(frame: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme.clone();
    let t = &t;
    match app.overlay.clone() {
        Overlay::None => {}
        Overlay::Help { scroll } => {
            let max = help_overlay(frame, area, scroll, t);
            app.set_help_max_scroll(max);
        }
        Overlay::Launching { number } => modal(
            frame,
            area,
            &format!(" Reviewing #{number} "),
            vec![Line::from(Span::styled(
                "Handing over to your review command…",
                Style::default().fg(color(t.text)),
            ))],
            t,
        ),
        Overlay::OfferFetch { number } => modal(
            frame,
            area,
            &format!(" #{number} "),
            vec![
                Line::from(Span::styled(
                    "Not tracked. Track it now?",
                    Style::default().fg(color(t.text)),
                )),
                Line::from(""),
                // Deliberately not "not in your ledger": most of these *are*
                // stored, swept and left untracked because no rule matched, and
                // tracking one of those needs no forge at all.
                Line::from(Span::styled(
                    "Fetched first if the ledger has never seen it, as `reviewq track` would.",
                    Style::default().fg(color(t.dim)),
                )),
                Line::from(""),
                keyed_hint(&[("y / ⏎", "track"), ("any other key", "cancel")], t),
            ],
            t,
        ),
        Overlay::Fetching { number } => modal(
            frame,
            area,
            &format!(" #{number} "),
            vec![Line::from(Span::styled(
                "Fetching from the forge…",
                Style::default().fg(color(t.text)),
            ))],
            t,
        ),
        Overlay::ConfirmDone { number } => modal(
            frame,
            area,
            &format!(" Done #{number} "),
            vec![
                Line::from(Span::styled(
                    "Mark it handled at its current head?",
                    Style::default().fg(color(t.text)),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "New commits bring it back. A review requested of you stays.",
                    Style::default().fg(color(t.dim)),
                )),
                Line::from(""),
                keyed_hint(&[("y / ⏎", "yes"), ("any other key", "cancel")], t),
            ],
            t,
        ),
        Overlay::SnoozePresets { number } => {
            let mut lines = vec![Line::from(Span::styled(
                "Suppress everything on it, mentions included, for:",
                Style::default().fg(color(t.dim)),
            ))];
            lines.push(Line::from(""));
            for (key, _, label) in SNOOZE_PRESETS {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {key}   "),
                        Style::default().fg(color(t.key)).bold(),
                    ),
                    Span::styled((*label).to_string(), Style::default().fg(color(t.text))),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(keyed_hint(
                &[("o", "another duration"), ("Esc", "cancel")],
                t,
            ));
            modal(frame, area, &format!(" Snooze #{number} "), lines, t)
        }
        Overlay::SnoozePrompt {
            number,
            input,
            error,
        } => prompt_modal(
            frame,
            area,
            &format!(" Snooze #{number} for "),
            &input,
            "e.g. 12h, 3d, 1w2d",
            error.as_deref(),
            "snooze",
            t,
        ),
        Overlay::JumpPrompt { input, error } => prompt_modal(
            frame,
            area,
            " Go to ",
            &input,
            "a number, #number, or a pasted pull-request URL",
            error.as_deref(),
            "go",
            t,
        ),
    }
}

/// A modal with a one-line text field: what has been typed, a hint at the form
/// it takes, why the last attempt was refused, and how to confirm or cancel.
#[allow(clippy::too_many_arguments)]
fn prompt_modal(
    frame: &mut Frame,
    screen: Rect,
    title: &str,
    input: &str,
    hint: &str,
    error: Option<&str>,
    confirm: &str,
    t: &Theme,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled(input.to_string(), Style::default().fg(color(t.text))),
            // A block stands in for a cursor: the field is one line and takes no
            // editing beyond typing and backspace, so a real one would be more
            // machinery than it earns.
            Span::styled("▏", Style::default().fg(color(t.focus))),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {hint}"),
            Style::default().fg(color(t.dim)),
        )),
    ];
    if let Some(error) = error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {error}"),
            Style::default().fg(color(t.urgent)),
        )));
    }
    lines.push(Line::from(""));
    lines.push(keyed_hint(&[("⏎", confirm), ("Esc", "cancel")], t));
    modal(frame, screen, title, lines, t);
}

/// A row of `key label` pairs, for the bottom of a modal.
fn keyed_hint(pairs: &[(&str, &str)], t: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for (key, label) in pairs {
        if spans.len() > 1 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            (*key).to_string(),
            Style::default().fg(color(t.key)).bold(),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(color(t.dim)),
        ));
    }
    Line::from(spans)
}

/// A bordered box of `lines`, centred and sized to its content.
///
/// [`Clear`]ed *and* filled: `Clear` resets the cells beneath to nothing, which
/// would let the terminal's own background show through the one place the layout
/// is deliberately opaque.
fn modal(frame: &mut Frame, screen: Rect, title: &str, lines: Vec<Line<'static>>, t: &Theme) {
    let width = lines
        .iter()
        .map(Line::width)
        .chain(std::iter::once(title.chars().count()))
        .max()
        .unwrap_or(0)
        .saturating_add(4) as u16;
    let height = lines.len().saturating_add(2) as u16;
    let area = centred(screen, width.min(screen.width), height.min(screen.height));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(t.focus)))
        .style(Style::default().bg(color(t.bg)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title.to_string(),
            Style::default().fg(color(t.focus)),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The key reference, centred over the panes, scrolled to `scroll`.
///
/// Returns how far it can still scroll, so the caller can hold the offset there.
///
/// Sized to its content rather than to a fraction of the screen, so it doesn't
/// leave a wide empty box on a big terminal. [`Clear`] blanks the cells beneath
/// instead of painting a background colour, which keeps the overlay from having
/// to guess the terminal's own — the same reason nothing else here fills.
fn help_overlay(frame: &mut Frame, screen: Rect, scroll: u16, t: &Theme) -> u16 {
    let heading = |text: &str| {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(color(t.focus))
                .add_modifier(Modifier::BOLD),
        ))
    };
    let entry = |keys: &str, what: &str| {
        Line::from(vec![
            Span::styled(format!("  {keys:<12}"), Style::default().fg(color(t.key))),
            Span::styled(what.to_string(), Style::default().fg(color(t.text))),
        ])
    };

    let mut rows: Vec<Line> = Vec::new();
    let mut group = "";
    for binding in keys::described() {
        if binding.group != group {
            if !rows.is_empty() {
                rows.push(Line::from(""));
            }
            rows.push(heading(binding.group));
            group = binding.group;
        }
        rows.push(entry(binding.keys, binding.what));
    }

    rows.push(Line::from(""));
    rows.push(heading("Mouse"));
    for (gesture, what) in MOUSE_GESTURES {
        rows.push(entry(gesture, what));
    }

    rows.push(Line::from(""));
    rows.push(keyed_hint(
        &[("↑↓", "scroll"), ("any other key", "close")],
        t,
    ));

    let width = rows
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .saturating_add(4) as u16;
    let wanted = rows.len().saturating_add(2) as u16;
    let area = centred(screen, width.min(screen.width), wanted.min(screen.height));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(t.focus)))
        .style(Style::default().bg(color(t.bg)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(" Keys ", Style::default().fg(color(t.focus))));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let total = rows.len();
    frame.render_widget(Paragraph::new(rows).scroll((scroll, 0)), inner);
    // What is left below the last visible row. Zero when it all fits, which is
    // what stops a scroll key doing anything on a tall terminal.
    total.saturating_sub(inner.height as usize) as u16
}

/// The `width` x `height` rect at the middle of `area`, clamped to fit.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// `#42`, or `owner/name#42` with more than one repo known — the same rule
/// `list` follows, so the two never disagree about how a PR is named.
fn number_label(multi: bool, repo: &RepoKey, number: u64) -> String {
    if multi {
        format!("{}#{number}", repo.slug())
    } else {
        format!("#{number}")
    }
}

fn state_colour(state: PrState, t: &Theme) -> crate::theme::Rgb {
    match state {
        PrState::Open => t.good,
        PrState::Merged => t.merged,
        PrState::Closed => t.bad,
    }
}

fn verdict_colour(v: Verdict, t: &Theme) -> crate::theme::Rgb {
    match v {
        Verdict::Approved => t.good,
        Verdict::ChangesRequested => t.bad,
        Verdict::Commented => t.dim,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_config;
    use crate::app::{App, Focus, Overlay};
    use crate::theme::Mode;
    use jiff::Timestamp;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use reviewq_core::model::{Attention, AttentionReason, MyState, PrSnapshot};
    use reviewq_ledger::{Ledger, TrackedReason};

    fn repo() -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        }
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().expect("timestamp")
    }

    fn pr(number: u64, title: &str) -> PrSnapshot {
        PrSnapshot {
            number,
            title: title.into(),
            author: "potiuk".into(),
            author_association: "MEMBER".into(),
            head_sha: "abc1234".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-10T09:00:00Z"),
            labels: vec!["area:async".into()],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    /// Shaped like a real PR description: a template's commented instructions,
    /// then a heading, prose and a checklist.
    const BODY: &str = "\
<!-- Thanks for opening a PR! Delete this section before submitting. -->

## What this does

Adds a `deferrable` flag to `S3KeySensor`.

- [x] Tests added
- [ ] Docs updated

```python
sensor = S3KeySensor(deferrable=True)
```
";

    /// A ledger holding two queued PRs: a mention (priority 1) and a
    /// needs-first-look (priority 6), so a render exercises both urgency bands.
    fn fixture() -> Ledger {
        let ledger = Ledger::open_in_memory().expect("in-memory ledger");
        let repo_id = ledger.ensure_repo(&repo()).expect("repo");
        let now = ts("2026-08-10T12:00:00Z");
        for (number, title, reason, since) in [
            (
                70135,
                "Add deferrable mode to S3KeySensor",
                AttentionReason::Mention { by: "kaxil".into() },
                "2026-08-10T09:00:00Z",
            ),
            (
                70201,
                "Tidy up the scheduler loop",
                AttentionReason::NeedsFirstLook {
                    rule: "label area:async".into(),
                },
                "2026-08-09T09:00:00Z",
            ),
        ] {
            ledger
                .upsert_pr(
                    repo_id,
                    &pr(number, title),
                    Some(TrackedReason::Interest {
                        rule: "label area:async".into(),
                        after_merge: false,
                    }),
                    now,
                )
                .expect("upsert");
            ledger
                .commit_detail(
                    repo_id,
                    number,
                    &MyState::default(),
                    &[],
                    &[],
                    &[Attention {
                        reason,
                        since: ts(since),
                    }],
                    Some(BODY),
                    now,
                )
                .expect("detail")
                .expect_applied();
        }
        ledger
    }

    /// Store `pr` as the only thing on the queue: tracked, with one attention row,
    /// since the queue is built from attention rather than from tracking alone.
    fn queue_only(ledger: &Ledger, repo_id: i64, pr: &PrSnapshot) {
        let now = ts("2026-08-10T12:00:00Z");
        ledger
            .upsert_pr(
                repo_id,
                pr,
                Some(TrackedReason::Interest {
                    rule: "label x".into(),
                    after_merge: false,
                }),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                pr.number,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason: AttentionReason::Mention { by: "kaxil".into() },
                    since: ts("2026-08-10T09:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail")
            .expect_applied();
    }

    /// Render at `width`x`height` and return the screen as plain text, one
    /// String per row, trailing blanks trimmed.
    fn render(app: &mut App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Render at `width`x`height` as one string, for a snapshot.
    ///
    /// A whole screen rather than substrings: a layout change then arrives as a
    /// reviewed diff, instead of as an assertion nobody updated because it still
    /// happened to match somewhere else on the grid.
    fn screen(app: &mut App, width: u16, height: u16) -> String {
        render(app, width, height).join("\n")
    }

    /// Every cell's background after a render, as a set — so a hole shows up as an
    /// extra entry rather than having to be hunted for.
    fn backgrounds(app: &mut App, width: u16, height: u16) -> std::collections::BTreeSet<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| format!("{:?}", buffer[(x, y)].bg))
            .collect()
    }

    #[test]
    fn the_palette_paints_its_own_background_everywhere() {
        // reviewq used to leave the background to the terminal, which made
        // switching palettes meaningless: a light palette over a dark terminal is
        // dark text on a dark background. Owning every cell is what makes the
        // choice mean something — and a cell it misses is a hole showing the
        // terminal through.
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");

        assert_eq!(
            backgrounds(&mut app, 100, 22),
            ["Rgb(30, 30, 30)".to_string()].into_iter().collect(),
            "the dark palette's background, and nothing else"
        );

        app.theme = app.theme.toggled();
        assert_eq!(
            backgrounds(&mut app, 100, 22),
            ["Rgb(255, 255, 255)".to_string()].into_iter().collect(),
            "and the light one after toggling"
        );
    }

    #[test]
    fn nothing_falls_back_to_the_terminals_own_foreground() {
        // What made light mode unreadable. `tui-markdown` returns prose with no
        // foreground at all, which means the terminal's — light grey on a dark
        // terminal, invisible over reviewq's white background. Every cell reviewq
        // paints has to name its own colour.
        for mode in [Mode::Dark, Mode::Light] {
            let mut app =
                App::with_ledger(Theme::new(mode), fixture(), test_config()).expect("app");
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
            terminal.draw(|frame| draw(frame, &mut app)).expect("draw");
            let buffer = terminal.backend().buffer().clone();

            let unstyled: Vec<String> = (0..30u16)
                .flat_map(|y| (0..100u16).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let cell = &buffer[(x, y)];
                    cell.symbol().trim() != "" && cell.fg == ratatui::style::Color::Reset
                })
                .map(|(x, y)| format!("({x},{y}) {:?}", buffer[(x, y)].symbol()))
                .take(5)
                .collect();
            assert!(
                unstyled.is_empty(),
                "{mode:?}: cells with no colour of their own: {unstyled:#?}"
            );
        }
    }

    #[test]
    fn markdown_keeps_its_structure_in_our_colours() {
        // The mapping reads `tui-markdown`'s own scheme — a heading is a coloured
        // line, code is a line or span with a background — so this fails if that
        // scheme moves, rather than the description quietly going one flat colour.
        let t = Theme::new(Mode::Light);
        let rendered = tui_markdown::from_str("## Heading\n\nProse `code` here.\n");
        let themed: Vec<Line<'_>> = rendered
            .lines
            .into_iter()
            .map(|line| themed_markdown(line, &t))
            .collect();

        let colours: Vec<_> = themed
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| !span.content.trim().is_empty())
            .map(|span| span.style.fg)
            .collect();
        assert!(
            colours.contains(&Some(color(t.focus))),
            "a heading should take the focus colour: {colours:?}"
        );
        assert!(
            colours.contains(&Some(color(t.key))),
            "inline code should take the key colour: {colours:?}"
        );
        assert!(
            colours.contains(&Some(color(t.text))),
            "prose should take the text colour: {colours:?}"
        );
        assert!(
            themed
                .iter()
                .all(|line| line.style.bg.is_none()
                    && line.spans.iter().all(|s| s.style.bg.is_none())),
            "nothing keeps a background of its own"
        );
    }

    #[test]
    fn an_overlay_is_opaque_rather_than_letting_the_terminal_through() {
        // `Clear` resets the cells beneath it to nothing, which is the one place
        // the fill above would not reach.
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.overlay = Overlay::Help { scroll: 0 };

        assert_eq!(
            backgrounds(&mut app, 100, 24),
            ["Rgb(30, 30, 30)".to_string()].into_iter().collect(),
            "including under the modal"
        );
    }

    #[test]
    fn the_queue_and_the_selected_prs_detail_both_render() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let rows = render(&mut app, 100, 22);

        // The urgent PR above the less urgent one. Ordering is the one thing a
        // snapshot states but does not *assert*, since accepting a reordered diff
        // is easy to do without noticing.
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not on screen"))
        };
        assert!(row_of("#70135") < row_of("#70201"));

        insta::assert_snapshot!(rows.join("\n"));
    }

    #[test]
    fn the_detail_pane_says_which_branch_the_pr_targets() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo()).expect("repo");
        let mut backport = pr(70135, "Fix the thing on 3.1");
        backport.base_ref = "v3-1-test".into();
        queue_only(&ledger, repo_id, &backport);
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        insta::assert_snapshot!(screen(&mut app, 100, 22));
    }

    #[test]
    fn the_detail_pane_says_nothing_about_a_branch_it_has_not_synced_yet() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo()).expect("repo");
        let mut unknown = pr(70135, "Stored before the branch was captured");
        unknown.base_ref = String::new();
        queue_only(&ledger, repo_id, &unknown);
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        let shown = screen(&mut app, 100, 22);

        // Asserted as well as snapshotted: an arrow appearing here would be a
        // diff easy to accept, and it would be pointing at nothing.
        assert!(
            !shown.contains('→'),
            "an arrow pointing at nothing:\n{shown}"
        );
        insta::assert_snapshot!(shown);
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_rendering_a_blank_pane() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        ledger.ensure_repo(&repo()).expect("repo");
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        insta::assert_snapshot!(screen(&mut app, 80, 12));
    }

    #[test]
    fn a_narrow_terminal_still_renders_both_panes_without_panicking() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        for (w, h) in [(40u16, 8u16), (60, 10), (200, 40)] {
            let screen = render(&mut app, w, h);
            assert_eq!(screen.len(), h as usize);
        }
    }

    #[test]
    fn the_description_renders_as_markdown_with_template_comments_stripped() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let shown = screen(&mut app, 100, 30);

        // Kept as assertions rather than left to the snapshot: each is a thing
        // that must *not* appear, and a snapshot gaining one is a diff somebody
        // could accept without reading.
        assert!(!shown.contains("Delete this section"), "{shown}");
        assert!(!shown.contains("<!--"), "{shown}");
        // Inline code loses its backticks — the styling carries it instead. A
        // fenced block keeps its own fence, which `tui-markdown` renders as text,
        // so this checks the prose line rather than the whole screen.
        let prose = shown
            .lines()
            .find(|line| line.contains("Adds a"))
            .expect("the prose line");
        assert!(!prose.contains('`'), "{prose}");

        insta::assert_snapshot!(shown);
    }

    #[test]
    fn a_pr_with_no_stored_description_renders_no_description_section() {
        // On the queue, but its detail pass stored no body — which is what a
        // pre-schema-6 ledger looks like until its next sync.
        let ledger = Ledger::open_in_memory().expect("ledger");
        let repo_id = ledger.ensure_repo(&repo()).expect("repo");
        let now = ts("2026-08-10T12:00:00Z");
        ledger
            .upsert_pr(
                repo_id,
                &pr(1, "No body yet"),
                Some(TrackedReason::Interest {
                    rule: "label area:async".into(),
                    after_merge: false,
                }),
                now,
            )
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[Attention {
                    reason: AttentionReason::Mention { by: "kaxil".into() },
                    since: ts("2026-08-10T09:00:00Z"),
                }],
                None,
                now,
            )
            .expect("detail")
            .expect_applied();
        let mut app = App::with_ledger(Theme::default(), ledger, test_config()).expect("app");

        let shown = screen(&mut app, 90, 16);

        // The section heading must be absent, not merely absent from the snapshot.
        assert!(!shown.contains("Description"), "{shown}");
        insta::assert_snapshot!(shown);
    }

    #[test]
    fn stripping_html_comments_leaves_the_prose() {
        assert_eq!(strip_html_comments("a <!-- x --> b"), "a  b");
        assert_eq!(strip_html_comments("<!--x-->only"), "only");
        assert_eq!(strip_html_comments("no comments"), "no comments");
        assert_eq!(
            strip_html_comments("one <!--a--> two <!--b--> three"),
            "one  two  three"
        );
        // Multi-line, as a PR template is.
        assert_eq!(
            strip_html_comments("keep\n<!--\ndrop\n-->\nkeep"),
            "keep\n\nkeep"
        );
        // Unterminated: everything after it is inside the comment, which is how
        // GitHub renders it too.
        assert_eq!(strip_html_comments("visible <!-- swallowed"), "visible ");
    }

    #[test]
    fn tab_moves_focus_and_the_footer_says_what_the_keys_now_do() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        assert_eq!(app.focus, Focus::Queue);
        let queue_focused = render(&mut app, 100, 24).join("\n");
        assert!(queue_focused.contains("move"), "{queue_focused}");

        app.focus = Focus::Detail;
        let detail_focused = render(&mut app, 100, 24).join("\n");
        // The same key, relabelled for what it now does.
        assert!(detail_focused.contains("scroll"), "{detail_focused}");
        assert!(!detail_focused.contains("jk move"), "{detail_focused}");
    }

    #[test]
    fn the_footer_stays_short_and_points_at_the_key_reference() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let screen = render(&mut app, 100, 24);
        let footer = screen.last().expect("footer row").clone();

        // Only the essentials, and one of them is how to find the rest.
        assert!(footer.contains("? / h keys"), "{footer}");
        assert!(footer.contains("quit"), "{footer}");
        // The bindings that moved into the overlay are not down here.
        assert!(!footer.contains("PgDn"), "{footer}");
        assert!(!footer.contains("first"), "{footer}");
        // Narrow terminals are the reason the rest moved to the overlay, so it
        // has to fit a conventional 80 columns with room to spare.
        assert!(
            footer.chars().count() <= 72,
            "footer is {} cols: {footer}",
            footer.chars().count()
        );
    }

    #[test]
    fn a_status_note_appears_in_the_header_beside_the_counts() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.status = Some("#70135 refreshed — wants attention".to_string());
        let header = render(&mut app, 100, 20).first().expect("header").clone();

        assert!(header.contains("#70135 refreshed"), "{header}");
        // What it's reporting on is still readable next to it.
        assert!(header.contains("2 on the queue"), "{header}");
    }

    #[test]
    fn refreshes_in_flight_are_named_and_outrank_a_finished_note() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.status = Some("an earlier result".to_string());
        app.refreshing.insert(70135);
        app.refreshing.insert(70201);

        let header = render(&mut app, 100, 20).first().expect("header").clone();
        // Both, so two concurrent refreshes are visibly two.
        assert!(header.contains("refreshing #70135, #70201"), "{header}");
        // The thing you're waiting on wins over the thing that already happened.
        assert!(!header.contains("an earlier result"), "{header}");

        // And once the last one lands, the note is what's left.
        app.refreshing.clear();
        let after = render(&mut app, 100, 20).first().expect("header").clone();
        assert!(after.contains("an earlier result"), "{after}");
    }

    #[test]
    fn the_help_overlay_lists_every_binding_grouped() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.overlay = Overlay::Help { scroll: 0 };
        // Tall enough for the whole reference — which grows as bindings are added,
        // so this has room to spare. What it does when it *doesn't* fit is the
        // next test's business.
        let screen = render(&mut app, 100, 40).join("\n");

        // Every binding the table describes reaches the reference — derived from
        // the table rather than listed here, so a new key with no entry fails
        // instead of quietly missing from a snapshot somebody accepted.
        for binding in keys::described() {
            if binding.what.is_empty() {
                continue; // folded into the row above, by design
            }
            assert!(
                screen.contains(binding.what),
                "binding {:?} is missing from the reference:\n{screen}",
                binding.what
            );
        }
        // The mouse gestures are not bindings, so the loop above cannot cover
        // them, and neither is the hint at the bottom.
        for what in MOUSE_GESTURES.iter().map(|(_, what)| *what) {
            assert!(screen.contains(what), "missing gesture {what}:\n{screen}");
        }
        assert!(screen.contains("close"), "{screen}");

        insta::assert_snapshot!(screen);
    }

    #[test]
    fn the_help_overlay_scrolls_when_it_does_not_fit() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.overlay = Overlay::Help { scroll: 0 };

        // Too short for the whole reference: the top shows, the tail does not.
        // "Session" is the marker because it appears nowhere else — the footer's
        // own "q / Esc quit" would make that a false negative.
        let top = render(&mut app, 100, 18).join("\n");
        assert!(top.contains("Navigate"), "{top}");
        assert!(
            !top.contains("Session"),
            "the tail should be below the fold:\n{top}"
        );

        // That render reported how far it can go, so End reaches the end.
        app.on_overlay_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE))
            .expect("scroll to end");
        let bottom = render(&mut app, 100, 18).join("\n");
        assert!(
            bottom.contains("Session"),
            "the tail should be reachable:\n{bottom}"
        );
    }

    #[test]
    fn the_help_overlay_occludes_what_sits_under_it() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        let plain = render(&mut app, 100, 24).join("\n");
        assert!(plain.contains("Adds a deferrable flag"), "{plain}");

        app.overlay = Overlay::Help { scroll: 0 };
        let covered = render(&mut app, 100, 24).join("\n");
        // A line the overlay is drawn across is cut, not shown through it. The
        // overlay is sized to its content, so only the rows and columns it
        // actually occupies are cleared — everything outside it still renders,
        // which is why the header survives below.
        assert!(
            !covered.contains("Adds a deferrable flag"),
            "the overlay should cut the line it covers:\n{covered}"
        );
        assert!(covered.contains("2 on the queue"), "{covered}");
        assert!(covered.contains("Navigate"), "{covered}");
    }

    #[test]
    fn the_help_overlay_fits_a_terminal_too_small_for_it() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        app.overlay = Overlay::Help { scroll: 0 };
        for (w, h) in [(30u16, 6u16), (24, 4), (100, 40)] {
            let screen = render(&mut app, w, h);
            assert_eq!(screen.len(), h as usize);
            assert!(
                screen.iter().all(|row| row.chars().count() <= w as usize),
                "a row overflowed {w} cols at {w}x{h}"
            );
        }
    }

    #[test]
    fn centred_clamps_to_the_area_it_is_given() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let fits = centred(area, 10, 4);
        assert_eq!((fits.x, fits.y, fits.width, fits.height), (5, 3, 10, 4));

        // Asking for more than there is yields the whole area, never a rect
        // that starts off-screen.
        let oversized = centred(area, 100, 100);
        assert_eq!((oversized.x, oversized.y), (0, 0));
        assert_eq!((oversized.width, oversized.height), (20, 10));
    }

    #[test]
    fn scrolling_the_description_moves_the_visible_window() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        // A pane short enough that the description can't all fit.
        let top = render(&mut app, 60, 12).join("\n");
        assert!(top.contains("Add deferrable mode"), "{top}");

        app.focus = Focus::Detail;
        app.detail_scroll = 6;
        let scrolled = render(&mut app, 60, 12).join("\n");
        assert_ne!(top, scrolled, "scrolling changed nothing");
        assert!(
            !scrolled.contains("Add deferrable mode to S3KeySensor"),
            "the title should have scrolled off:\n{scrolled}"
        );
    }

    #[test]
    fn a_render_tells_the_app_how_tall_the_queue_pane_is() {
        let mut app = App::with_ledger(Theme::default(), fixture(), test_config()).expect("app");
        // 22 rows: 1 header, 1 footer, a 20-row pane, less its two borders.
        render(&mut app, 100, 22);
        assert_eq!(app.page(), 18);

        // A pane too short to show a row still pages by one, never by zero —
        // otherwise PageDown would silently do nothing.
        render(&mut app, 100, 3);
        assert_eq!(app.page(), 1);
    }

    #[test]
    fn a_number_label_names_its_repo_only_when_there_is_more_than_one() {
        assert_eq!(number_label(false, &repo(), 42), "#42");
        assert_eq!(number_label(true, &repo(), 42), "apache/airflow#42");
    }

    #[test]
    fn a_state_and_a_verdict_map_to_distinct_colours() {
        let t = Theme::default();
        assert_ne!(
            state_colour(PrState::Open, &t),
            state_colour(PrState::Closed, &t)
        );
        assert_ne!(
            state_colour(PrState::Merged, &t),
            state_colour(PrState::Open, &t)
        );
        assert_ne!(
            verdict_colour(Verdict::Approved, &t),
            verdict_colour(Verdict::ChangesRequested, &t)
        );
    }
}
