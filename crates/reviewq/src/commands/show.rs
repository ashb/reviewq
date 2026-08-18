//! `reviewq show <number>`: everything the ledger knows about one PR — why it's
//! tracked, every attention reason it holds, and its review threads. Read-only.

use std::process::ExitCode;

use anyhow::{Result, bail};
use crossterm::style::Stylize;
use reviewq_core::model::{PrState, ThreadState, Verdict};
use reviewq_ledger::{PrShow, RepoKey};
use serde::Serialize;

use crate::cli::ShowArgs;
use crate::colour::{self, Output, Span};
use crate::commands::EXIT_EMPTY;
use reviewq_app::config::{self, Loaded};
use reviewq_app::present;

pub fn run(loaded: &Loaded, args: &ShowArgs, output: &impl Output) -> Result<ExitCode> {
    let target = &args.target;
    // One handle for the whole command: resolving the repo and reading the PR are
    // two reads on the same connection.
    let ledger = reviewq_app::resolve::open()?;
    // A full URL already names its repo — no need to search for it, and it
    // disambiguates a number that's ambiguous across configured repos.
    let repo = match &target.repo {
        Some(url) => RepoKey {
            host: url.host.clone(),
            owner: url.owner.clone(),
            name: url.name.clone(),
        },
        None => {
            let mut repos = ledger.repos_with_pr(target.number)?;
            match repos.len() {
                0 => return not_in_ledger(output, args.json, target.number),
                1 => repos.remove(0),
                _ => bail!(
                    "#{} exists in more than one configured repo ({}) — pass its full URL to pick one",
                    target.number,
                    repos
                        .iter()
                        .map(|r| format!("{}/{}", r.owner, r.name))
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            }
        }
    };
    // A read: `show` is read-only, so a repo the ledger has never heard of is
    // "not in the ledger", not a row to create on the way to saying so.
    let Some(repo_id) = ledger.repo_id(&repo)? else {
        return not_in_ledger(output, args.json, target.number);
    };
    let Some(show) = ledger.show(repo_id, target.number)? else {
        return not_in_ledger(output, args.json, target.number);
    };

    let link = pr_link(&loaded.config, &repo, target.number);
    let url = link.as_ref().map(|l| l.url.as_str());

    if args.json {
        output.println(serde_json::to_string_pretty(&json(&show, url))?);
    } else {
        let underline = link.as_ref().is_none_or(|l| l.underline_links);
        print_human(
            output,
            &show,
            url,
            underline,
            &loaded.config.output.icons.branch,
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn not_in_ledger(output: &impl Output, json: bool, number: u64) -> Result<ExitCode> {
    if json {
        output.println("null");
    } else {
        output.eprintln(format!(
            "#{number} is not in the ledger — run `reviewq sync`"
        ));
    }
    Ok(ExitCode::from(EXIT_EMPTY))
}

/// The PR's forge URL, and whether to underline it once hyperlinked.
///
/// `None` only when the repo's host resolves to no adapter — a repo stored under
/// a config that has since changed, say. No token is resolved to render a URL, so
/// a locked credential helper still gets you a link.
struct PrLink {
    url: String,
    underline_links: bool,
}

fn pr_link(config: &config::Config, repo: &RepoKey, number: u64) -> Option<PrLink> {
    let forge = config.forge_for(&repo.host).ok()?;
    Some(PrLink {
        url: forge.web_url(&repo.owner, &repo.name, number),
        underline_links: config.output.underline_links,
    })
}

/// Wrap `text` in an OSC 8 terminal hyperlink to `url`. Terminal-gated: piped
/// to a file, a pager without OSC 8 support, or `--json` (which never calls
/// this), the escape sequence would be noise rather than a feature.
fn hyperlink(output: &impl Output, text: &str, url: Option<&str>) -> String {
    render_hyperlink(text, url, output.stdout_is_terminal())
}

fn render_hyperlink(text: &str, url: Option<&str>, is_terminal: bool) -> String {
    match url {
        Some(url) if is_terminal => format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\"),
        _ => text.to_string(),
    }
}

/// Style `text`, optionally bold and/or underlined. Each call produces one
/// self-contained styled span (its own reset), so callers can concatenate
/// several without one span's reset clobbering another's still-open style.
/// `icon value`, or the value alone when the icon is configured away — so a
/// glyph nobody's font can draw costs nothing but the glyph.
fn labelled(icon: &str, value: &str) -> String {
    if icon.is_empty() {
        value.to_string()
    } else {
        format!("{icon} {value}")
    }
}

fn styled(text: &str, bold: bool, underline: bool) -> Span {
    let text = text.to_string();
    match (bold, underline) {
        (true, true) => text.bold().underlined().into(),
        (true, false) => text.bold().into(),
        (false, true) => text.underlined().into(),
        (false, false) => colour::plain(text),
    }
}

fn print_human(
    output: &impl Output,
    show: &PrShow,
    url: Option<&str>,
    underline_links: bool,
    branch_icon: &str,
) {
    let pr = &show.pr;
    // Underlining a plain (non-hyperlinked) title would suggest it's
    // clickable when it isn't, so it's tied to whether there's a url at all.
    let underline = url.is_some() && underline_links;
    let header = format!(
        "{} {}",
        output.render(styled(&format!("#{}", pr.number), true, underline)),
        output.render(styled(&pr.title, false, underline)),
    );
    output.println(hyperlink(output, &header, url));
    output.println(format!(
        "  {} · @{} · {}{}",
        output.render(state_word(pr.state)),
        pr.author,
        show.tracked_reason.as_deref().unwrap_or("untracked"),
        output.render(draft_tag(pr.is_draft)),
    ));
    // Only once it's known: a row written before the target branch was captured
    // has it empty until the next sync, and an icon with nothing after it would
    // read as a bug rather than as missing data.
    if !pr.base_ref.is_empty() {
        output.println(format!("  {}", labelled(branch_icon, &pr.base_ref)));
    }
    // Opened first, then updated: the pair reads as the PR's life, and the one
    // that says how long somebody has been waiting comes first. Silent about an
    // opening date a row predating its capture does not have.
    match pr.created_at {
        Some(created) => output.println(format!(
            "  opened {} · updated {}",
            present::day(created),
            present::stamp(pr.updated_at)
        )),
        None => output.println(format!("  updated {}", present::stamp(pr.updated_at))),
    }

    if !pr.labels.is_empty() || pr.milestone.is_some() {
        let mut bits = pr.labels.clone();
        if let Some(m) = &pr.milestone {
            bits.push(format!("milestone: {m}"));
        }
        output.println(format!("  {}", bits.join(", ")));
    }

    let silenced = present::silenced(&show.my_state);
    if !silenced.is_empty() {
        output.line(vec![
            colour::plain("  "),
            Span::from(silenced.join(", ").dark_yellow()),
        ]);
    }

    if let Some(sha) = &show.my_state.last_reviewed_sha {
        let verdict = show
            .my_state
            .last_verdict
            .map(|v| output.render(verdict_word(v)))
            .unwrap_or_else(|| "no verdict".to_string());
        let at = show.my_state.last_action_at.map_or_else(
            || "an unknown time".to_string(),
            |at| format!("at {}", present::stamp(at)),
        );
        output.println(format!(
            "  reviewed {} → {verdict}, {at}",
            present::short_sha(sha)
        ));
    }
    if let Some(note) = present::done_note(pr, &show.my_state) {
        let note = if note.superseded {
            Span::from(note.text.dark_yellow())
        } else {
            colour::plain(note.text)
        };
        output.line(vec![colour::plain("  "), note]);
    }

    if !show.reviewers.is_empty() {
        output.println("  reviewers:");
        for r in &show.reviewers {
            output.println(format!(
                "    {} @{} {}",
                output.render(verdict_word(r.verdict)),
                r.login,
                present::stamp(r.at)
            ));
        }
    }

    if show.attention.is_empty() {
        output.println("  attention: none");
    } else {
        output.println("  attention:");
        for a in &show.attention {
            output.println(format!(
                "    {} {}",
                output.render(Span::from(format!("[p{}]", a.priority()).dim())),
                output.render(Span::from(a.reason.to_string().dark_cyan())),
            ));
        }
    }

    if !show.threads.is_empty() {
        let counts = present::thread_counts(&show.threads);
        output.println(format!(
            "  threads: {} ({} you own, {} resolved)",
            counts.total, counts.owned, counts.resolved,
        ));
        for t in show.threads.iter().filter(|t| !t.is_resolved) {
            output.println(format!("    unresolved: {}", output.render(thread_line(t))));
        }
    }
}

/// One thread's most recent activity, with mine marked.
fn thread_line(t: &ThreadState) -> Vec<Span> {
    let who = t.last_comment_author.as_deref().unwrap_or("someone");
    let at = t
        .last_comment_at
        .map_or_else(|| "an unknown time".to_string(), present::stamp);
    let mut spans = vec![colour::plain(format!("@{who} at {at}"))];
    if t.i_own {
        spans.push(colour::plain(" "));
        spans.push("(you own)".to_string().dim().into());
    }
    spans
}

/// The PR's lifecycle state, coloured so a merged or closed PR reads
/// differently from an open one at a glance.
fn state_word(state: PrState) -> Span {
    let s = state.as_str().to_string();
    match state {
        PrState::Open => s.dark_green().into(),
        PrState::Merged => s.dark_magenta().into(),
        PrState::Closed => s.dark_red().into(),
    }
}

fn draft_tag(is_draft: bool) -> Vec<Span> {
    if is_draft {
        vec![
            colour::plain(" · "),
            "draft".to_string().dark_yellow().into(),
        ]
    } else {
        Vec::new()
    }
}

/// A review verdict, coloured the same way everywhere it appears (my own
/// history, and every other reviewer's).
fn verdict_word(v: Verdict) -> Span {
    let s = v.as_str().to_string();
    match v {
        Verdict::Approved => s.dark_green().into(),
        Verdict::ChangesRequested => s.dark_red().into(),
        Verdict::Commented => s.dim().into(),
    }
}

#[derive(Serialize)]
struct ShowJson<'a> {
    number: u64,
    url: Option<&'a str>,
    state: &'a str,
    title: &'a str,
    author: &'a str,
    /// The branch the PR targets. Omitted when a row predates its capture and no
    /// sync has refreshed it yet, rather than reported as an empty branch name.
    #[serde(skip_serializing_if = "Option::is_none")]
    base_ref: Option<&'a str>,
    is_draft: bool,
    updated_at: String,
    /// When the PR was opened on the forge. Omitted, like `base_ref`, when the
    /// row predates its capture and no sync has refreshed it — an absent date
    /// is not the epoch.
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    labels: &'a [String],
    milestone: Option<&'a str>,
    tracked_reason: Option<&'a str>,
    muted: bool,
    snoozed_until: Option<String>,
    deferred_at: Option<String>,
    last_reviewed_sha: Option<&'a str>,
    last_verdict: Option<&'a str>,
    last_action_at: Option<String>,
    done_sha: Option<&'a str>,
    done_at: Option<String>,
    reviewers: Vec<ReviewerJson<'a>>,
    attention: Vec<AttentionJson<'a>>,
    threads: Vec<ThreadJson<'a>>,
}

#[derive(Serialize)]
struct ReviewerJson<'a> {
    login: &'a str,
    verdict: &'a str,
    at: String,
}

#[derive(Serialize)]
struct AttentionJson<'a> {
    reason: &'a str,
    /// Rendered on the way out — the ledger stores the reason, not its prose.
    detail: String,
    priority: u8,
    since: String,
}

#[derive(Serialize)]
struct ThreadJson<'a> {
    i_own: bool,
    is_resolved: bool,
    resolved_by: Option<&'a str>,
    last_comment_author: Option<&'a str>,
    last_comment_at: Option<String>,
}

fn json<'a>(show: &'a PrShow, url: Option<&'a str>) -> ShowJson<'a> {
    ShowJson {
        number: show.pr.number,
        url,
        state: show.pr.state.as_str(),
        title: &show.pr.title,
        author: &show.pr.author,
        base_ref: Some(show.pr.base_ref.as_str()).filter(|b| !b.is_empty()),
        is_draft: show.pr.is_draft,
        updated_at: show.pr.updated_at.to_string(),
        created_at: show.pr.created_at.map(|at| at.to_string()),
        labels: &show.pr.labels,
        milestone: show.pr.milestone.as_deref(),
        tracked_reason: show.tracked_reason.as_deref(),
        muted: show.my_state.muted,
        snoozed_until: show.my_state.snoozed_until.map(|t| t.to_string()),
        deferred_at: show.my_state.deferred_at.map(|t| t.to_string()),
        last_reviewed_sha: show.my_state.last_reviewed_sha.as_deref(),
        last_verdict: show.my_state.last_verdict.map(|v| v.as_str()),
        last_action_at: show.my_state.last_action_at.map(|t| t.to_string()),
        done_sha: show.my_state.done_sha.as_deref(),
        done_at: show.my_state.done_at.map(|t| t.to_string()),
        reviewers: show
            .reviewers
            .iter()
            .map(|r| ReviewerJson {
                login: &r.login,
                verdict: r.verdict.as_str(),
                at: r.at.to_string(),
            })
            .collect(),
        attention: show
            .attention
            .iter()
            .map(|a| AttentionJson {
                reason: a.reason.discriminant(),
                detail: a.reason.to_string(),
                priority: a.priority(),
                since: a.since.to_string(),
            })
            .collect(),
        threads: show
            .threads
            .iter()
            .map(|t| ThreadJson {
                i_own: t.i_own,
                is_resolved: t.is_resolved,
                resolved_by: t.resolved_by.as_deref(),
                last_comment_author: t.last_comment_author.as_deref(),
                last_comment_at: t.last_comment_at.map(|ts| ts.to_string()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colour::testing::FakeOutput;
    use jiff::Timestamp;
    use reviewq_core::model::{MyState, PrSnapshot};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn pr() -> PrSnapshot {
        PrSnapshot {
            number: 1,
            title: "t".into(),
            author: "octocat".into(),
            author_association: "MEMBER".into(),
            head_sha: "head0000".into(),
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-05T09:00:00Z"),
            created_at: None,
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    #[test]
    fn render_hyperlink_wraps_in_osc8_on_a_terminal_with_a_url() {
        assert_eq!(
            render_hyperlink("#1 title", Some("https://example.com/pull/1"), true),
            "\x1b]8;;https://example.com/pull/1\x1b\\#1 title\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn render_hyperlink_is_plain_text_off_a_terminal() {
        assert_eq!(
            render_hyperlink("#1 title", Some("https://example.com/pull/1"), false),
            "#1 title"
        );
    }

    #[test]
    fn render_hyperlink_is_plain_text_without_a_url() {
        assert_eq!(render_hyperlink("#1 title", None, true), "#1 title");
    }

    /// A `PrShow` around `pr`, with nothing else going on.
    fn show_of(pr: PrSnapshot) -> PrShow {
        PrShow {
            pr,
            body: None,
            tracked_reason: Some("interest: label x".into()),
            after_merge: false,
            my_state: MyState::default(),
            threads: vec![],
            reviewers: vec![],
            attention: vec![],
        }
    }

    #[test]
    fn human_output_is_written_through_the_output_sink() {
        let output = FakeOutput::new(false);

        print_human(&output, &show_of(pr()), None, false, "->");

        assert_eq!(
            *output.lines.borrow(),
            [
                "#1 t",
                "  OPEN · @octocat · interest: label x",
                "  -> main",
                "  updated 2026-08-05T09:00:00Z",
                "  attention: none",
            ]
        );
    }

    #[test]
    fn json_reports_the_target_branch() {
        let mut backport = pr();
        backport.base_ref = "v3-1-test".into();

        assert_eq!(json(&show_of(backport), None).base_ref, Some("v3-1-test"));
    }

    #[test]
    fn json_omits_a_target_branch_that_is_not_known_yet() {
        // A row stored before the branch was captured, and not yet re-synced:
        // absent says "unknown", where `""` would claim a branch with no name.
        let mut unknown = pr();
        unknown.base_ref = String::new();

        assert_eq!(json(&show_of(unknown), None).base_ref, None);
    }

    #[test]
    fn json_reports_when_the_pr_was_opened_to_the_second() {
        // The human line shows the day; the machine one keeps the instant,
        // which is what anything computing an age off this would want.
        let mut opened = pr();
        opened.created_at = Some(ts("2026-07-22T09:14:33Z"));

        assert_eq!(
            json(&show_of(opened), None).created_at.as_deref(),
            Some("2026-07-22T09:14:33Z")
        );
    }

    #[test]
    fn json_omits_an_opening_date_that_is_not_known_yet() {
        assert_eq!(json(&show_of(pr()), None).created_at, None);
    }

    #[test]
    fn a_labelled_value_drops_the_icon_when_there_is_none() {
        // Both frontends follow this rule, so a config that says a font cannot
        // draw the glyph reads the same wherever the branch is shown.
        assert_eq!(labelled("\u{f419}", "v3-1-test"), "\u{f419} v3-1-test");
        assert_eq!(labelled("->", "main"), "-> main");
        assert_eq!(labelled("", "main"), "main");
    }

    #[test]
    fn styled_is_plain_text_with_colour_off_and_carries_a_style_when_on() {
        for (bold, underline) in [(true, true), (true, false), (false, true), (false, false)] {
            let span = styled("text", bold, underline);
            assert_eq!(colour::render(false, span), "text");
        }

        for (bold, underline) in [(true, true), (true, false), (false, true)] {
            assert_ne!(
                colour::render(true, styled("text", bold, underline)),
                "text",
                "bold={bold} underline={underline} should carry an escape sequence"
            );
        }
    }
}
