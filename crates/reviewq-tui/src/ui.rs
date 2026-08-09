//! Drawing. Reads [`App`], writes to the frame, decides nothing.
//!
//! No widget names a colour: everything comes from [`Theme`], so the palette is
//! changed in one place. Nor does anything set a background — see the module
//! docs on [`crate::theme`] for why reviewq sits on the terminal's own.

use jiff::{Timestamp, Unit};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap};
use reviewq_core::model::{PrState, Verdict};
use reviewq_ledger::{Located, QueueItem, RepoKey};

use crate::app::{App, Focus};
use crate::keys::{self, Action};
use crate::theme::{Theme, color};

/// Draw the whole screen: header, the queue beside the detail, footer.
pub fn draw(frame: &mut Frame, app: &mut App) {
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
    queue_pane(frame, cols[0], app);
    let lines = detail_pane(frame, cols[1], app);
    // Now that the content has been laid out, its length is known — which is
    // what stops a scroll running off the end of a short description.
    app.set_detail_lines(lines);
    footer(frame, rows[2], app);

    // Last, so it covers the panes rather than being drawn under them.
    if app.help {
        help_overlay(frame, rows[1], &app.theme);
    }
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
    // A status note replaces nothing — it sits after the counts, so what it's
    // reporting on stays visible next to it.
    if let Some(status) = &app.status {
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

fn queue_pane(frame: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let inner = panel(frame, area, "Queue", app.focus == Focus::Queue, t);
    if app.queue.is_empty() {
        frame.render_widget(
            Paragraph::new("Nothing on the queue.")
                .style(Style::default().fg(color(t.dim)))
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    // Keep the selected row on screen by scrolling the window, not the cursor.
    let height = inner.height as usize;
    let first = app.selected.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = app
        .queue
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(index, item)| queue_row(item, index == app.selected, app.repo_count > 1, t))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
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

/// Draw the detail pane, returning how many lines its content came to so the
/// caller can bound scrolling.
fn detail_pane(frame: &mut Frame, area: Rect, app: &App) -> usize {
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
        return 0;
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
        Line::from(Span::styled(
            format!("updated {}", stamp(pr.updated_at)),
            Style::default().fg(color(t.dim)),
        )),
    ];

    if pr.is_draft {
        lines.push(Line::from(Span::styled(
            "draft",
            Style::default().fg(color(t.warn)),
        )));
    }

    let mut silenced = Vec::new();
    if show.my_state.muted {
        silenced.push("muted".to_string());
    }
    if let Some(until) = show.my_state.snoozed_until {
        silenced.push(format!("snoozed until {}", stamp(until)));
    }
    if let Some(at) = show.my_state.deferred_at {
        silenced.push(format!("deferred since {}", stamp(at)));
    }
    if !silenced.is_empty() {
        lines.push(Line::from(Span::styled(
            silenced.join(", "),
            Style::default().fg(color(t.quiet)),
        )));
    }

    if let Some(sha) = &show.my_state.done_sha {
        let superseded = sha != &pr.head_sha;
        let note = if superseded {
            " — superseded by new commits"
        } else {
            ""
        };
        lines.push(Line::from(Span::styled(
            format!("done at {}{note}", short_sha(sha)),
            Style::default().fg(color(if superseded { t.warn } else { t.dim })),
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
        let owned = show.threads.iter().filter(|x| x.i_own).count();
        let unresolved = show.threads.iter().filter(|x| !x.is_resolved).count();
        lines.push(Line::from(""));
        lines.push(section("Threads", t));
        lines.push(Line::from(Span::styled(
            format!(
                "  {} total, {unresolved} unresolved, {owned} you own",
                show.threads.len()
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
        lines.extend(tui_markdown::from_str(body).lines);
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    // Post-wrap, because that's the unit `scroll` moves in: counting the
    // `Line`s would under-count a wrapped description and stop `G` short of
    // the end.
    let total = paragraph.line_count(inner.width);
    frame.render_widget(paragraph.scroll((app.detail_scroll, 0)), inner);
    total
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
            binding.keys,
            Style::default().fg(color(t.key)).bold(),
        ));
        spans.push(Span::styled(
            format!(" {}", footer_label(binding, app.focus)),
            Style::default().fg(color(t.dim)),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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
        (Action::SyncSelected, _) => "sync PR",
        _ => binding.what,
    }
}

/// The key reference, centred over the panes.
///
/// Sized to its content rather than to a fraction of the screen, so it doesn't
/// leave a wide empty box on a big terminal. [`Clear`] blanks the cells beneath
/// instead of painting a background colour, which keeps the overlay from having
/// to guess the terminal's own — the same reason nothing else here fills.
fn help_overlay(frame: &mut Frame, screen: Rect, t: &Theme) {
    let mut rows: Vec<Line> = Vec::new();
    let mut group = "";
    for binding in keys::described() {
        if binding.group != group {
            if !rows.is_empty() {
                rows.push(Line::from(""));
            }
            rows.push(Line::from(Span::styled(
                binding.group.to_string(),
                Style::default()
                    .fg(color(t.focus))
                    .add_modifier(Modifier::BOLD),
            )));
            group = binding.group;
        }
        rows.push(Line::from(vec![
            Span::styled(
                format!("  {:<12}", binding.keys),
                Style::default().fg(color(t.key)),
            ),
            Span::styled(binding.what, Style::default().fg(color(t.text))),
        ]));
    }
    rows.push(Line::from(""));
    rows.push(Line::from(Span::styled(
        "  any key to close",
        Style::default().fg(color(t.dim)),
    )));

    let width = rows
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .saturating_add(4) as u16;
    let height = rows.len().saturating_add(2) as u16;
    let area = centred(screen, width.min(screen.width), height.min(screen.height));

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color(t.focus)))
        .padding(Padding::horizontal(1))
        .title(Span::styled(" Keys ", Style::default().fg(color(t.focus))));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(rows), inner);
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

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn stamp(ts: Timestamp) -> String {
    ts.round(Unit::Second).unwrap_or(ts).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Focus};
    use jiff::Timestamp;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
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
                    Some(TrackedReason::Interest("label area:async".into())),
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
                .expect("detail");
        }
        ledger
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

    #[test]
    fn the_queue_and_the_selected_prs_detail_both_render() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        let rows = render(&mut app, 100, 22);
        let screen = rows.join("\n");

        // Header, both queue rows, and the selection marker on the first.
        assert!(screen.contains("2 on the queue"), "{screen}");
        assert!(screen.contains("▸ #70135"), "{screen}");
        assert!(screen.contains("#70201"), "{screen}");
        // The urgent PR sorted above the less urgent one.
        let row_of = |needle: &str| {
            rows.iter()
                .position(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("{needle} not on screen"))
        };
        assert!(row_of("#70135") < row_of("#70201"));
        // Detail pane followed the selection.
        assert!(
            screen.contains("Add deferrable mode to S3KeySensor"),
            "{screen}"
        );
        assert!(screen.contains("Attention"), "{screen}");
        // The reason string is whatever reviewq-core renders — asserted as it
        // actually reads, so a change to that wording surfaces here. No
        // `mention:` prefix: the discriminant isn't repeated into the evidence.
        assert!(screen.contains("@kaxil mentioned you"), "{screen}");
        assert!(!screen.contains("mention:"), "{screen}");
        assert!(screen.contains("p1"), "{screen}");
        // Footer bindings.
        assert!(screen.contains("quit"), "{screen}");
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_rendering_a_blank_pane() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        ledger.ensure_repo(&repo()).expect("repo");
        let mut app = App::with_ledger(Theme::default(), ledger).expect("app");

        let screen = render(&mut app, 80, 12).join("\n");
        assert!(screen.contains("Nothing on the queue"), "{screen}");
        assert!(screen.contains("Select a PR"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_still_renders_both_panes_without_panicking() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        for (w, h) in [(40u16, 8u16), (60, 10), (200, 40)] {
            let screen = render(&mut app, w, h);
            assert_eq!(screen.len(), h as usize);
        }
    }

    #[test]
    fn the_description_renders_as_markdown_with_template_comments_stripped() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        let screen = render(&mut app, 100, 30).join("\n");

        assert!(screen.contains("Description"), "{screen}");
        assert!(screen.contains("What this does"), "{screen}");
        assert!(screen.contains("Tests added"), "{screen}");
        // The commented template instructions never reach the screen, nor does
        // the comment syntax itself.
        assert!(!screen.contains("Delete this section"), "{screen}");
        assert!(!screen.contains("<!--"), "{screen}");
        // Inline code loses its backticks — the styling carries it instead.
        assert!(screen.contains("deferrable flag"), "{screen}");
        assert!(!screen.contains('`'), "{screen}");
        // tui-markdown keeps a heading's `##` and styles the line rather than
        // dropping the marker. Pinned because it's a visible choice, not
        // because it's the only defensible one.
        assert!(screen.contains("## What this does"), "{screen}");
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
                Some(TrackedReason::Interest("label area:async".into())),
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
            .expect("detail");
        let mut app = App::with_ledger(Theme::default(), ledger).expect("app");

        let screen = render(&mut app, 90, 16).join("\n");
        assert!(screen.contains("No body yet"), "{screen}");
        assert!(!screen.contains("Description"), "{screen}");
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
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        assert_eq!(app.focus, Focus::Queue);
        let queue_focused = render(&mut app, 100, 24).join("\n");
        assert!(queue_focused.contains("move"), "{queue_focused}");
        assert!(queue_focused.contains("Tab pane"), "{queue_focused}");

        app.focus = Focus::Detail;
        let detail_focused = render(&mut app, 100, 24).join("\n");
        assert!(detail_focused.contains("scroll"), "{detail_focused}");
        assert!(detail_focused.contains("Tab pane"), "{detail_focused}");
    }

    #[test]
    fn the_footer_stays_short_and_points_at_the_key_reference() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
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
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        app.status = Some("syncing #70135…".to_string());
        let header = render(&mut app, 100, 20).first().expect("header").clone();

        assert!(header.contains("syncing #70135"), "{header}");
        // What it's reporting on is still readable next to it.
        assert!(header.contains("2 on the queue"), "{header}");
    }

    #[test]
    fn the_help_overlay_lists_every_binding_grouped() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        app.help = true;
        let screen = render(&mut app, 100, 24).join("\n");

        assert!(screen.contains("Keys"), "{screen}");
        for group in ["Navigate", "View", "Session"] {
            assert!(screen.contains(group), "missing group {group}:\n{screen}");
        }
        // Including the ones the footer no longer has room for.
        for what in [
            "page down",
            "page up",
            "first",
            "last",
            "switch pane",
            "quit",
        ] {
            assert!(screen.contains(what), "missing binding {what}:\n{screen}");
        }
        assert!(screen.contains("any key to close"), "{screen}");
    }

    #[test]
    fn the_help_overlay_occludes_what_sits_under_it() {
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        let plain = render(&mut app, 100, 24).join("\n");
        assert!(plain.contains("Adds a deferrable flag"), "{plain}");

        app.help = true;
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
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
        app.help = true;
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
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
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
        let mut app = App::with_ledger(Theme::default(), fixture()).expect("app");
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
    fn short_sha_truncates_and_tolerates_a_short_input() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
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
