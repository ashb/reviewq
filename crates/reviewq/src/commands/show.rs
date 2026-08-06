//! `reviewq show <number>`: everything the ledger knows about one PR — why it's
//! tracked, every attention reason it holds, and its review threads. Read-only.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;
use jiff::Timestamp;
use owo_colors::{OwoColorize as _, Stream::Stdout};
use reviewq_core::model::{MyState, PrSnapshot, ThreadState};
use reviewq_ledger::{Ledger, PrShow};
use serde::Serialize;

use crate::cli::ShowArgs;
use crate::commands::EXIT_EMPTY;
use crate::paths;

pub fn run(_config_path: Option<&Path>, args: &ShowArgs) -> Result<ExitCode> {
    let ledger = Ledger::open(&paths::database_file()?)?;
    let Some(show) = ledger.show(args.number)? else {
        if args.json {
            println!("null");
        } else {
            eprintln!("#{} is not in the ledger — run `reviewq sync`", args.number);
        }
        return Ok(ExitCode::from(EXIT_EMPTY));
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&json(&show))?);
    } else {
        print_human(&show);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_human(show: &PrShow) {
    let pr = &show.pr;
    println!(
        "{} {}",
        format!("#{}", pr.number).if_supports_color(Stdout, |s| s.bold().to_string()),
        pr.title,
    );
    println!(
        "  {} · @{} · {}{}",
        pr.state.as_str(),
        pr.author,
        show.tracked_reason.as_deref().unwrap_or("untracked"),
        if pr.is_draft { " · draft" } else { "" },
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
            .map(|v| v.as_str().to_string())
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
            " — superseded by new commits since"
        } else {
            ""
        };
        lines.push(format!("done at {} on {at}{stale}", short_sha(sha)));
    }
    lines
}

fn thread_line(t: &ThreadState) -> String {
    let who = t.last_comment_author.as_deref().unwrap_or("someone");
    let at = t
        .last_comment_at
        .map(fmt_ts)
        .unwrap_or_else(|| "unknown time".to_string());
    let owned = if t.i_own { " (you own)" } else { "" };
    format!("@{who} at {at}{owned}")
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
    attention: Vec<AttentionJson<'a>>,
    threads: Vec<ThreadJson<'a>>,
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

fn json(show: &PrShow) -> ShowJson<'_> {
    ShowJson {
        number: show.pr.number,
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
    use reviewq_core::model::PrState;

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
    fn short_sha_truncates_to_seven_characters() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
    }
}
