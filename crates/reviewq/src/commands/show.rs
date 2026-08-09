//! `reviewq show <number>`: everything the ledger knows about one PR — why it's
//! tracked, every attention reason it holds, and its review threads. Read-only.

use std::io::IsTerminal;
use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::{MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict};
use reviewq_ledger::{Ledger, PrShow};
use serde::Serialize;

use crate::cli::ShowArgs;
use crate::commands::EXIT_EMPTY;
use crate::{config, paths};

pub fn run(config_path: Option<&Path>, args: &ShowArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let Some(show) = ledger.show(args.number)? else {
        if args.json {
            println!("null");
        } else {
            eprintln!("#{} is not in the ledger — run `reviewq sync`", args.number);
        }
        return Ok(ExitCode::from(EXIT_EMPTY));
    };

    let link = pr_link(config_path, args.number);
    let url = link.as_ref().map(|l| l.url.as_str());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json(&show, url))?);
    } else {
        let underline = link.as_ref().is_none_or(|l| l.underline_links);
        print_human(&show, url, underline);
    }
    Ok(ExitCode::SUCCESS)
}

/// The PR's forge URL, and whether to underline it once hyperlinked, both
/// best-effort. `show` otherwise never touches config or the forge — it
/// works purely off the ledger — so this must not turn a config or token
/// problem into a failed `show`, and must not autocreate a config file the
/// way `config::load`'s default path does; it only reads one already there.
struct PrLink {
    url: String,
    underline_links: bool,
}

fn pr_link(config_path: Option<&Path>, number: u64) -> Option<PrLink> {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => paths::config_file().ok()?,
    };
    if !path.exists() {
        return None;
    }
    let loaded = config::load(config_path).ok()?;
    let (_project, repo) = loaded.config.sole_repo().ok()?;
    let forge = loaded.config.forge_for(repo).ok()?;
    Some(PrLink {
        url: forge.web_url(&repo.owner, &repo.name, number),
        underline_links: loaded.config.output.underline_links,
    })
}

/// Wrap `text` in an OSC 8 terminal hyperlink to `url`. Terminal-gated: piped
/// to a file, a pager without OSC 8 support, or `--json` (which never calls
/// this), the escape sequence would be noise rather than a feature.
fn hyperlink(text: &str, url: Option<&str>) -> String {
    render_hyperlink(text, url, std::io::stdout().is_terminal())
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
fn styled(text: &str, bold: bool, underline: bool) -> String {
    text.if_supports_color(Stdout, |s| match (bold, underline) {
        (true, true) => s.bold().underline().to_string(),
        (true, false) => s.bold().to_string(),
        (false, true) => s.underline().to_string(),
        (false, false) => s.to_string(),
    })
    .to_string()
}

fn print_human(show: &PrShow, url: Option<&str>, underline_links: bool) {
    let pr = &show.pr;
    // Underlining a plain (non-hyperlinked) title would suggest it's
    // clickable when it isn't, so it's tied to whether there's a url at all.
    let underline = url.is_some() && underline_links;
    let header = format!(
        "{} {}",
        styled(&format!("#{}", pr.number), true, underline),
        styled(&pr.title, false, underline),
    );
    println!("{}", hyperlink(&header, url));
    println!(
        "  {} · @{} · {}{}",
        state_word(pr.state),
        pr.author,
        show.tracked_reason.as_deref().unwrap_or("untracked"),
        draft_tag(pr.is_draft),
    );
    println!("  updated {}", fmt_ts(pr.updated_at));

    if !pr.labels.is_empty() || pr.milestone.is_some() {
        let mut bits = pr.labels.clone();
        if let Some(m) = &pr.milestone {
            bits.push(format!("milestone: {m}"));
        }
        println!("  {}", bits.join(", "));
    }

    let silenced = silenced_bits(&show.my_state);
    if !silenced.is_empty() {
        println!(
            "  {}",
            silenced
                .join(", ")
                .if_supports_color(Stdout, |s| s.yellow().to_string())
        );
    }

    for line in my_history_lines(pr, &show.my_state) {
        println!("  {line}");
    }

    if !show.reviewers.is_empty() {
        println!("  reviewers:");
        for r in &show.reviewers {
            println!("    {}", reviewer_line(r));
        }
    }

    if show.attention.is_empty() {
        println!("  attention: none");
    } else {
        println!("  attention:");
        for a in &show.attention {
            println!(
                "    {} {}",
                format!("[p{}]", a.priority).if_supports_color(Stdout, |s| s.dimmed().to_string()),
                a.detail.if_supports_color(Stdout, |s| s.cyan().to_string()),
            );
        }
    }

    if !show.threads.is_empty() {
        let owned = show.threads.iter().filter(|t| t.i_own).count();
        let resolved = show.threads.iter().filter(|t| t.is_resolved).count();
        println!(
            "  threads: {} ({} you own, {} resolved)",
            show.threads.len(),
            owned,
            resolved,
        );
        for t in show.threads.iter().filter(|t| !t.is_resolved) {
            println!("    unresolved: {}", thread_line(t));
        }
    }
}

/// Local state a sync never surfaces, so nothing else would tell you a PR
/// looks quiet only because it's muted, snoozed or deferred.
fn silenced_bits(my: &MyState) -> Vec<String> {
    let mut bits = Vec::new();
    if my.muted {
        bits.push("muted".to_string());
    }
    if let Some(until) = my.snoozed_until {
        bits.push(format!("snoozed until {}", fmt_ts(until)));
    }
    if let Some(at) = my.deferred_at {
        bits.push(format!("deferred since {}", fmt_ts(at)));
    }
    bits
}

/// My own review/done history — absent entirely for a PR I've never acted on.
fn my_history_lines(pr: &PrSnapshot, my: &MyState) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(sha) = &my.last_reviewed_sha {
        let verdict = my
            .last_verdict
            .map(verdict_word)
            .unwrap_or_else(|| "no verdict".to_string());
        let at = my
            .last_action_at
            .map(fmt_ts)
            .unwrap_or_else(|| "unknown time".to_string());
        lines.push(format!("reviewed {} → {verdict}, {at}", short_sha(sha)));
    }
    if let Some(sha) = &my.done_sha {
        let at = my
            .done_at
            .map(fmt_ts)
            .unwrap_or_else(|| "unknown time".to_string());
        let stale = if sha != &pr.head_sha {
            format!(
                " — {}",
                "superseded by new commits since".if_supports_color(Stdout, |s| s.yellow())
            )
        } else {
            String::new()
        };
        lines.push(format!("done at {} on {at}{stale}", short_sha(sha)));
    }
    lines
}

fn reviewer_line(r: &ReviewerVerdict) -> String {
    format!("{} @{} {}", verdict_word(r.verdict), r.login, fmt_ts(r.at))
}

fn thread_line(t: &ThreadState) -> String {
    let who = t.last_comment_author.as_deref().unwrap_or("someone");
    let at = t
        .last_comment_at
        .map(fmt_ts)
        .unwrap_or_else(|| "unknown time".to_string());
    let owned = if t.i_own {
        format!(" {}", "(you own)".if_supports_color(Stdout, |s| s.dimmed()))
    } else {
        String::new()
    };
    format!("@{who} at {at}{owned}")
}

/// The PR's lifecycle state, coloured so a merged or closed PR reads
/// differently from an open one at a glance.
fn state_word(state: PrState) -> String {
    let s = state.as_str();
    match state {
        PrState::Open => format!("{}", s.if_supports_color(Stdout, |s| s.green())),
        PrState::Merged => format!("{}", s.if_supports_color(Stdout, |s| s.magenta())),
        PrState::Closed => format!("{}", s.if_supports_color(Stdout, |s| s.red())),
    }
}

fn draft_tag(is_draft: bool) -> String {
    if is_draft {
        format!(" · {}", "draft".if_supports_color(Stdout, |s| s.yellow()))
    } else {
        String::new()
    }
}

/// A review verdict, coloured the same way everywhere it appears (my own
/// history, and every other reviewer's).
fn verdict_word(v: Verdict) -> String {
    let s = v.as_str();
    match v {
        Verdict::Approved => format!("{}", s.if_supports_color(Stdout, |s| s.green())),
        Verdict::ChangesRequested => format!("{}", s.if_supports_color(Stdout, |s| s.red())),
        Verdict::Commented => format!("{}", s.if_supports_color(Stdout, |s| s.dimmed())),
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn fmt_ts(ts: Timestamp) -> String {
    ts.round(jiff::Unit::Second).unwrap_or(ts).to_string()
}

#[derive(Serialize)]
struct ShowJson<'a> {
    number: u64,
    url: Option<&'a str>,
    state: &'a str,
    title: &'a str,
    author: &'a str,
    is_draft: bool,
    updated_at: String,
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
    detail: &'a str,
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
        is_draft: show.pr.is_draft,
        updated_at: show.pr.updated_at.to_string(),
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
                reason: &a.reason,
                detail: &a.detail,
                priority: a.priority,
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
            is_draft: false,
            state: PrState::Open,
            updated_at: ts("2026-08-05T09:00:00Z"),
            labels: vec![],
            milestone: None,
            files: None,
            files_truncated: false,
        }
    }

    #[test]
    fn silenced_bits_is_empty_when_nothing_is_silenced() {
        assert!(silenced_bits(&MyState::default()).is_empty());
    }

    #[test]
    fn silenced_bits_reports_every_active_silencer() {
        let mine = MyState {
            muted: true,
            snoozed_until: Some(ts("2026-08-10T00:00:00Z")),
            deferred_at: Some(ts("2026-08-05T09:00:00Z")),
            ..Default::default()
        };
        let bits = silenced_bits(&mine);
        assert_eq!(
            bits,
            vec![
                "muted".to_string(),
                "snoozed until 2026-08-10T00:00:00Z".to_string(),
                "deferred since 2026-08-05T09:00:00Z".to_string(),
            ]
        );
    }

    #[test]
    fn my_history_lines_is_empty_with_no_prior_action() {
        assert!(my_history_lines(&pr(), &MyState::default()).is_empty());
    }

    #[test]
    fn my_history_lines_flags_a_done_superseded_by_new_commits() {
        let mine = MyState {
            done_sha: Some("head0000".into()),
            done_at: Some(ts("2026-08-05T10:00:00Z")),
            ..Default::default()
        };
        let mut newer = pr();
        newer.head_sha = "head1111".into();

        assert!(my_history_lines(&newer, &mine)[0].contains("superseded by new commits"));
        assert!(!my_history_lines(&pr(), &mine)[0].contains("superseded"));
    }

    #[test]
    fn reviewer_line_names_the_verdict_reviewer_and_time() {
        let r = ReviewerVerdict {
            login: "kaxil".into(),
            verdict: reviewq_core::model::Verdict::Approved,
            at: ts("2026-08-03T09:00:00Z"),
        };
        assert_eq!(reviewer_line(&r), "APPROVED @kaxil 2026-08-03T09:00:00Z");
    }

    #[test]
    fn short_sha_truncates_to_seven_characters() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
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

    #[test]
    fn styled_is_plain_text_off_a_terminal() {
        // if_supports_color checks real stdout, never a terminal under
        // `cargo test`, so every combination is untouched here regardless.
        for (bold, underline) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(styled("text", bold, underline), "text");
        }
    }
}
