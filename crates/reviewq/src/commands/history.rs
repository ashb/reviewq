use std::io::{BufRead, IsTerminal as _};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use reviewq_app::config::Loaded;
use reviewq_core::model::{ActivityKind, ActivityPayload};
use reviewq_ledger::{ActivityEvent, ActivityScope, CleanupPreview, Ledger, RepoKey};
use serde::Serialize;

use crate::cli::{HistoryArgs, HistoryCleanArgs, HistoryOperation};
use crate::colour::{Output, plain};

const PAGE_SIZE: usize = 100;

pub async fn run(loaded: &Loaded, args: &HistoryArgs, output: &impl Output) -> Result<ExitCode> {
    match &args.operation {
        Some(HistoryOperation::Clean(args)) => clean(args, output),
        None => read(loaded, args, output),
    }
}

fn read(loaded: &Loaded, args: &HistoryArgs, output: &impl Output) -> Result<ExitCode> {
    let ledger = reviewq_app::resolve::open()?;
    let global = args.target.is_none();
    let scope = resolve_scope(loaded, &ledger, args.target.as_deref())?;
    let scope = match scope {
        ActivityScope::Pr { repo_id, number } if args.all => {
            ActivityScope::PrAll { repo_id, number }
        }
        scope => scope,
    };

    if args.json {
        print_json(&ledger, scope, output)?;
    } else {
        let mut text = String::new();
        for_each_event(&ledger, scope, |event| {
            text.push_str(&human_line(event, global));
            text.push('\n');
            Ok(())
        })?;
        if text.is_empty() {
            output.println("No activity history.");
        } else {
            super::page_out(output, &text);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_scope(loaded: &Loaded, ledger: &Ledger, target: Option<&str>) -> Result<ActivityScope> {
    let Some(target) = target else {
        return Ok(ActivityScope::All);
    };

    let parsed = reviewq_forge::parse_pull_request_url(&loaded.config.forges, target)?;
    let (repo, number) = match parsed {
        Some(target) => (
            Some(RepoKey {
                host: target.host,
                owner: target.owner,
                name: target.name,
            }),
            target.number,
        ),
        None if target.starts_with("http://") || target.starts_with("https://") => {
            bail!("{target:?} doesn't look like a pull request URL")
        }
        None => (
            None,
            crate::cli::pr_number(target).map_err(anyhow::Error::msg)?,
        ),
    };

    let repo = match repo {
        Some(repo) => repo,
        None => {
            let mut repos = ledger.repos_with_pr(number)?;
            match repos.len() {
                0 => bail!("#{number} is not in the ledger — run `reviewq sync` first"),
                1 => repos.remove(0),
                _ => bail!(
                    "#{number} exists in more than one configured repo ({}) — pass its full URL to pick one",
                    repos
                        .iter()
                        .map(RepoKey::slug)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        }
    };
    let repo_id = ledger
        .repo_id(&repo)?
        .with_context(|| format!("#{number} is not in the ledger — run `reviewq sync` first"))?;
    if ledger.show(repo_id, number)?.is_none() {
        bail!("#{number} is not in the ledger — run `reviewq sync` first");
    }
    Ok(ActivityScope::Pr { repo_id, number })
}

fn for_each_event(
    ledger: &Ledger,
    scope: ActivityScope,
    mut visit: impl FnMut(&ActivityEvent) -> Result<()>,
) -> Result<()> {
    let mut cursor = None;
    loop {
        let page = ledger.activity_page(scope, cursor.as_ref(), PAGE_SIZE)?;
        for event in &page.events {
            visit(event)?;
        }
        let Some(next) = page.next else {
            break;
        };
        cursor = Some(next);
    }
    Ok(())
}

fn print_json(ledger: &Ledger, scope: ActivityScope, output: &impl Output) -> Result<()> {
    output.write(plain("["));
    let mut first = true;
    for_each_event(ledger, scope, |event| {
        output.write(plain(if first { "\n" } else { ",\n" }));
        first = false;
        let json = serde_json::to_string_pretty(&JsonActivityEvent::from(event))?;
        for (index, line) in json.lines().enumerate() {
            if index > 0 {
                output.write(plain("\n"));
            }
            output.write(plain(format!("  {line}")));
        }
        Ok(())
    })?;
    output.println(if first { "]" } else { "\n]" });
    Ok(())
}

fn human_line(event: &ActivityEvent, global: bool) -> String {
    let target = if global {
        format!("{} #{}", event.repo.slug(), event.pr_number)
    } else {
        format!("#{}", event.pr_number)
    };
    format!(
        "{}  {target}  {}",
        reviewq_app::present::stamp(event.occurred_at),
        reviewq_app::present::activity_text(event)
    )
}

#[derive(Serialize)]
struct JsonRepo<'a> {
    host: &'a str,
    owner: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
struct JsonActivityEvent<'a> {
    repository: JsonRepo<'a>,
    pr_number: u64,
    pr_title: &'a str,
    source: reviewq_core::model::ActivitySource,
    kind: ActivityKind,
    relation: reviewq_core::model::ActivityRelation,
    occurred_at: Timestamp,
    recorded_at: Timestamp,
    actor: &'a Option<String>,
    head_sha: &'a Option<String>,
    external_id: &'a Option<String>,
    permalink: &'a Option<String>,
    payload: &'a ActivityPayload,
}

impl<'a> From<&'a ActivityEvent> for JsonActivityEvent<'a> {
    fn from(event: &'a ActivityEvent) -> Self {
        Self {
            repository: JsonRepo {
                host: &event.repo.host,
                owner: &event.repo.owner,
                name: &event.repo.name,
            },
            pr_number: event.pr_number,
            pr_title: &event.pr_title,
            source: event.source,
            kind: event.kind,
            relation: event.relation,
            occurred_at: event.occurred_at,
            recorded_at: event.recorded_at,
            actor: &event.actor,
            head_sha: &event.head_sha,
            external_id: &event.external_id,
            permalink: &event.permalink,
            payload: &event.payload,
        }
    }
}

fn clean(args: &HistoryCleanArgs, output: &impl Output) -> Result<ExitCode> {
    let now = Timestamp::now();
    let cutoff = cleanup_cutoff(now, &args.older_than)?;
    let ledger = reviewq_app::resolve::open()?;
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    clean_at_cutoff(
        &ledger,
        args,
        cutoff,
        interactive,
        &mut stdin.lock(),
        output,
    )
}

fn cleanup_cutoff(now: Timestamp, older_than: &str) -> Result<Timestamp> {
    let future = reviewq_app::actions::snooze_until(now, older_than)?;
    now.checked_sub(future.duration_since(now))
        .with_context(|| format!("duration {older_than:?} out of range"))
}

fn clean_at_cutoff(
    ledger: &Ledger,
    args: &HistoryCleanArgs,
    cutoff: Timestamp,
    interactive: bool,
    input: &mut impl BufRead,
    output: &impl Output,
) -> Result<ExitCode> {
    let preview = ledger.preview_activity_cleanup(cutoff)?;
    output.println(format!("Would remove {}.", cleanup_counts(preview)));

    if args.dry_run {
        return Ok(ExitCode::SUCCESS);
    }
    if !args.yes {
        if !interactive {
            bail!("history cleanup requires --yes when input is not interactive");
        }
        output.write(plain("Remove these events? [y/N] "));
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            output.println("Not removed.");
            return Ok(ExitCode::SUCCESS);
        }
    }

    let removed = ledger.clean_activity(cutoff)?;
    output.println(format!("Removed {}.", cleanup_counts(removed)));
    Ok(ExitCode::SUCCESS)
}

fn cleanup_counts(preview: CleanupPreview) -> String {
    format!(
        "{} {} across {} {}",
        preview.event_count,
        plural(preview.event_count, "event", "events"),
        preview.pr_count,
        plural(preview.pr_count, "pull request", "pull requests")
    )
}

fn plural(count: u64, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use jiff::Timestamp;
    use reviewq_core::model::{ActivityKind, ActivityPayload, ActivitySource, PrSnapshot, PrState};
    use reviewq_ledger::{ActivityScope, Ledger, NewActivityEvent, RepoKey};

    use super::{clean_at_cutoff, cleanup_cutoff};
    use crate::cli::HistoryCleanArgs;
    use crate::colour::testing::FakeOutput;
    use reviewq_ledger::RepoId;

    fn ledger_with_pr() -> (Ledger, RepoId) {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow".into(),
            })
            .unwrap();
        ledger
            .upsert_pr(
                repo_id,
                &PrSnapshot {
                    number: 1,
                    title: "Cleanup".into(),
                    author: "octocat".into(),
                    author_association: "CONTRIBUTOR".into(),
                    head_sha: "abc1234".into(),
                    base_ref: "main".into(),
                    is_draft: false,
                    state: PrState::Open,
                    updated_at: "2026-08-20T00:00:00Z".parse().unwrap(),
                    created_at: None,
                    state_changed_at: None,
                    labels: vec![],
                    milestone: None,
                    files: None,
                    files_truncated: false,
                },
                None,
            )
            .unwrap();
        (ledger, repo_id)
    }

    fn ledger_with_old_activity() -> (Ledger, RepoId) {
        let (ledger, repo_id) = ledger_with_pr();
        ledger
            .record_activity(
                repo_id,
                1,
                &NewActivityEvent {
                    relation: reviewq_core::model::ActivityRelation::Own,
                    source: ActivitySource::Local,
                    kind: ActivityKind::Done,
                    occurred_at: "2024-01-01T00:00:00Z".parse().unwrap(),
                    recorded_at: "2026-08-20T00:00:00Z".parse().unwrap(),
                    actor: None,
                    head_sha: Some("abc1234".into()),
                    external_id: None,
                    permalink: None,
                    payload: ActivityPayload::None,
                },
            )
            .unwrap();
        (ledger, repo_id)
    }

    fn clean_args() -> HistoryCleanArgs {
        HistoryCleanArgs {
            older_than: "52w".into(),
            dry_run: false,
            yes: false,
        }
    }

    fn activity_count(ledger: &Ledger, repo_id: RepoId) -> usize {
        ledger
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
            .unwrap()
            .events
            .len()
    }

    fn activity(source: ActivitySource, kind: ActivityKind, occurred_at: &str) -> NewActivityEvent {
        NewActivityEvent {
            relation: reviewq_core::model::ActivityRelation::Own,
            source,
            kind,
            occurred_at: occurred_at.parse().unwrap(),
            recorded_at: "2026-08-21T00:00:00Z".parse().unwrap(),
            actor: None,
            head_sha: None,
            external_id: (source == ActivitySource::Forge)
                .then(|| format!("{kind:?}-{occurred_at}")),
            permalink: None,
            payload: ActivityPayload::None,
        }
    }

    #[test]
    fn interactive_cleanup_refusal_preserves_the_previewed_events() {
        let (ledger, repo_id) = ledger_with_old_activity();
        let output = FakeOutput::new(false);

        let args = clean_args();
        let cutoff = cleanup_cutoff(
            "2026-08-21T00:00:00Z".parse::<Timestamp>().unwrap(),
            &args.older_than,
        )
        .unwrap();
        clean_at_cutoff(
            &ledger,
            &args,
            cutoff,
            true,
            &mut Cursor::new(b"no\n"),
            &output,
        )
        .unwrap();

        assert_eq!(activity_count(&ledger, repo_id), 1);
        assert_eq!(
            output.stdout.borrow().as_str(),
            "Would remove 1 event across 1 pull request.\nRemove these events? [y/N] Not removed.\n"
        );
    }

    #[test]
    fn interactive_cleanup_confirmation_deletes_the_previewed_events() {
        let (ledger, repo_id) = ledger_with_old_activity();
        let output = FakeOutput::new(false);

        let args = clean_args();
        let cutoff = cleanup_cutoff(
            "2026-08-21T00:00:00Z".parse::<Timestamp>().unwrap(),
            &args.older_than,
        )
        .unwrap();
        clean_at_cutoff(
            &ledger,
            &args,
            cutoff,
            true,
            &mut Cursor::new(b"yes\n"),
            &output,
        )
        .unwrap();

        assert_eq!(activity_count(&ledger, repo_id), 0);
        assert_eq!(
            output.stdout.borrow().as_str(),
            "Would remove 1 event across 1 pull request.\nRemove these events? [y/N] Removed 1 event across 1 pull request.\n"
        );
    }

    #[test]
    fn zero_match_interactive_cleanup_requires_confirmation_before_advancing_retention() {
        let (ledger, repo_id) = ledger_with_pr();
        let output = FakeOutput::new(false);

        clean_at_cutoff(
            &ledger,
            &clean_args(),
            "2026-08-10T00:00:00Z".parse().unwrap(),
            true,
            &mut Cursor::new(b"no\n"),
            &output,
        )
        .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &activity(
                    ActivitySource::Local,
                    ActivityKind::Done,
                    "2026-08-01T00:00:00Z",
                ),
            )
            .unwrap();

        assert_eq!(activity_count(&ledger, repo_id), 1);
        assert_eq!(
            output.stdout.borrow().as_str(),
            "Would remove 0 events across 0 pull requests.\nRemove these events? [y/N] Not removed.\n"
        );
    }

    #[test]
    fn confirmed_zero_match_cleanup_filters_later_local_and_backfill_activity() {
        let (ledger, repo_id) = ledger_with_pr();
        let output = FakeOutput::new(false);
        let cutoff = "2026-08-10T00:00:00Z".parse().unwrap();

        clean_at_cutoff(
            &ledger,
            &clean_args(),
            cutoff,
            true,
            &mut Cursor::new(b"yes\n"),
            &output,
        )
        .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &activity(
                    ActivitySource::Local,
                    ActivityKind::Done,
                    "2026-08-01T00:00:00Z",
                ),
            )
            .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &activity(
                    ActivitySource::Local,
                    ActivityKind::Muted,
                    "2026-08-10T00:00:00Z",
                ),
            )
            .unwrap();
        assert_eq!(
            ledger
                .commit_activity_page(
                    repo_id,
                    1,
                    None,
                    &[
                        activity(
                            ActivitySource::Forge,
                            ActivityKind::Commented,
                            "2026-08-01T00:00:00Z",
                        ),
                        activity(
                            ActivitySource::Forge,
                            ActivityKind::Commented,
                            "2026-08-11T00:00:00Z",
                        ),
                    ],
                    None,
                    None,
                    "2026-08-21T00:00:00Z".parse().unwrap(),
                )
                .unwrap(),
            reviewq_ledger::ActivityPageCommit::Applied { inserted: 1 }
        );

        let events = ledger
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 10)
            .unwrap()
            .events;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].occurred_at,
            "2026-08-11T00:00:00Z".parse().unwrap()
        );
        assert_eq!(events[1].occurred_at, cutoff);
        assert_eq!(
            output.stdout.borrow().as_str(),
            "Would remove 0 events across 0 pull requests.\nRemove these events? [y/N] Removed 0 events across 0 pull requests.\n"
        );
    }

    #[test]
    fn zero_match_noninteractive_cleanup_still_requires_yes() {
        let (ledger, _) = ledger_with_pr();

        let error = clean_at_cutoff(
            &ledger,
            &clean_args(),
            "2026-08-10T00:00:00Z".parse().unwrap(),
            false,
            &mut Cursor::new([]),
            &FakeOutput::new(false),
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires --yes"));
    }

    #[test]
    fn zero_match_dry_run_does_not_advance_retention() {
        let (ledger, repo_id) = ledger_with_pr();
        let mut args = clean_args();
        args.dry_run = true;

        clean_at_cutoff(
            &ledger,
            &args,
            "2026-08-10T00:00:00Z".parse().unwrap(),
            false,
            &mut Cursor::new([]),
            &FakeOutput::new(false),
        )
        .unwrap();
        ledger
            .record_activity(
                repo_id,
                1,
                &activity(
                    ActivitySource::Local,
                    ActivityKind::Done,
                    "2026-08-01T00:00:00Z",
                ),
            )
            .unwrap();

        assert_eq!(activity_count(&ledger, repo_id), 1);
    }

    #[test]
    fn a_real_ledger_lifecycle_event_renders_its_state_transition() {
        let (ledger, repo_id) = ledger_with_old_activity();
        ledger.track(repo_id, 1).unwrap();
        ledger
            .record_state_transition(
                repo_id,
                1,
                PrState::Closed,
                Some("2026-08-20T12:00:00Z".parse().unwrap()),
                "2026-08-20T12:01:00Z".parse().unwrap(),
            )
            .unwrap();

        let event = ledger
            .activity_page(ActivityScope::Pr { repo_id, number: 1 }, None, 1)
            .unwrap()
            .events
            .pop()
            .unwrap();

        assert_eq!(
            reviewq_app::present::activity_text(&event),
            "closed (open → closed)"
        );
    }
}
