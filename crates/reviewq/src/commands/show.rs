//! `reviewq show <number>`: everything the ledger knows about one PR — why it's
//! tracked, every attention reason it holds, and its review threads. Read-only.

use std::io::IsTerminal;
use std::process::ExitCode;

use anyhow::{Result, bail};
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::{PrState, ThreadState, Verdict};
use reviewq_ledger::{PrShow, RepoKey};
use serde::Serialize;

use crate::cli::ShowArgs;
use crate::commands::EXIT_EMPTY;
use reviewq_app::config::{self, Loaded};
use reviewq_app::present;

pub fn run(loaded: &Loaded, args: &ShowArgs) -> Result<ExitCode> {
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
                0 => return not_in_ledger(args.json, target.number),
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
        return not_in_ledger(args.json, target.number);
    };
    let Some(show) = ledger.show(repo_id, target.number)? else {
        return not_in_ledger(args.json, target.number);
    };

    let link = pr_link(&loaded.config, &repo, target.number);
    let url = link.as_ref().map(|l| l.url.as_str());

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json(&show, url))?);
    } else {
        let underline = link.as_ref().is_none_or(|l| l.underline_links);
        print_human(&show, url, underline);
    }
    Ok(ExitCode::SUCCESS)
}

fn not_in_ledger(json: bool, number: u64) -> Result<ExitCode> {
    if json {
        println!("null");
    } else {
        eprintln!("#{number} is not in the ledger — run `reviewq sync`");
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
    // Only once it's known: a row written before the target branch was captured
    // has it empty until the next sync, and "→ " with nothing after it would
    // read as a bug rather than as missing data.
    if !pr.base_ref.is_empty() {
        println!("  → {}", pr.base_ref);
    }
    println!("  updated {}", present::stamp(pr.updated_at));

    if !pr.labels.is_empty() || pr.milestone.is_some() {
        let mut bits = pr.labels.clone();
        if let Some(m) = &pr.milestone {
            bits.push(format!("milestone: {m}"));
        }
        println!("  {}", bits.join(", "));
    }

    let silenced = present::silenced(&show.my_state);
    if !silenced.is_empty() {
        println!(
            "  {}",
            silenced
                .join(", ")
                .if_supports_color(Stdout, |s| s.yellow().to_string())
        );
    }

    if let Some(sha) = &show.my_state.last_reviewed_sha {
        let verdict = show
            .my_state
            .last_verdict
            .map(verdict_word)
            .unwrap_or_else(|| "no verdict".to_string());
        let at = show.my_state.last_action_at.map_or_else(
            || "an unknown time".to_string(),
            |at| format!("at {}", present::stamp(at)),
        );
        println!("  reviewed {} → {verdict}, {at}", present::short_sha(sha));
    }
    if let Some(note) = present::done_note(pr, &show.my_state) {
        println!(
            "  {}",
            if note.superseded {
                note.text
                    .if_supports_color(Stdout, |s| s.yellow())
                    .to_string()
            } else {
                note.text
            }
        );
    }

    if !show.reviewers.is_empty() {
        println!("  reviewers:");
        for r in &show.reviewers {
            println!(
                "    {} @{} {}",
                verdict_word(r.verdict),
                r.login,
                present::stamp(r.at)
            );
        }
    }

    if show.attention.is_empty() {
        println!("  attention: none");
    } else {
        println!("  attention:");
        for a in &show.attention {
            println!(
                "    {} {}",
                format!("[p{}]", a.priority())
                    .if_supports_color(Stdout, |s| s.dimmed().to_string()),
                a.reason
                    .to_string()
                    .if_supports_color(Stdout, |s| s.cyan().to_string()),
            );
        }
    }

    if !show.threads.is_empty() {
        let counts = present::thread_counts(&show.threads);
        println!(
            "  threads: {} ({} you own, {} resolved)",
            counts.total, counts.owned, counts.resolved,
        );
        for t in show.threads.iter().filter(|t| !t.is_resolved) {
            println!("    unresolved: {}", thread_line(t));
        }
    }
}

/// One thread's most recent activity, with mine marked.
fn thread_line(t: &ThreadState) -> String {
    let who = t.last_comment_author.as_deref().unwrap_or("someone");
    let at = t
        .last_comment_at
        .map_or_else(|| "an unknown time".to_string(), present::stamp);
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
    fn styled_is_plain_text_off_a_terminal() {
        // if_supports_color checks real stdout, never a terminal under
        // `cargo test`, so every combination is untouched here regardless.
        for (bold, underline) in [(true, true), (true, false), (false, true), (false, false)] {
            assert_eq!(styled("text", bold, underline), "text");
        }
    }
}
