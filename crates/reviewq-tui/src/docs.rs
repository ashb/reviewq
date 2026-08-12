//! The screenshots in `docs/imgs`, and the queue they are taken of.
//!
//! Documentation pictures rot: the interface moves, the images stay, and nobody
//! notices until a reader does. So they are generated from a fixture by the same
//! renderer the interface uses, and the committed files are *checked* on every
//! test run — a change to the layout, the palette or the marks fails here until
//! the pictures are regenerated:
//!
//! ```sh
//! REVIEWQ_WRITE_DOCS=1 cargo test -p reviewq-tui docs
//! ```
//!
//! The fixture is deliberately fuller than the ones the behaviour tests use.
//! Those want the smallest queue that exercises one thing; this wants a queue
//! that looks like a real morning — every urgency band, both marks, something
//! deferred, and something that merged and stayed.

use jiff::Timestamp;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use reviewq_core::model::{
    Attention, AttentionReason, MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict,
};
use reviewq_ledger::{Ledger, RepoKey, TrackedReason};

use crate::app::{App, Overlay, Unqueued, test_config};
use crate::theme::{Mode, Theme};
use crate::{svg, ui};

/// The colours apache/airflow paints the labels the fixture uses. Real ones, so
/// the pictures show what the interface does with a forge's palette rather than
/// with something invented to look tidy.
const PALETTE: &[(&str, &str)] = &[
    ("area:task-sdk", "0e8a16"),
    ("area:Scheduler", "1d76db"),
    ("area:Executors-core", "5319e7"),
    ("area:serialization", "b60205"),
    ("kind:feature", "c2e0c6"),
    ("kind:bug", "d73a4a"),
    ("backport", "fbca04"),
    ("needs-review", "000000"),
];

/// Where the committed pictures live, relative to this crate.
const IMAGES: &str = "../../docs/imgs";

/// A PR the queue does not have: merged a week ago, no rule of ours matched it,
/// and nothing about it is in the ledger. Asking for it by number is what the
/// offer-and-show pictures are about, so it must be a number the fixture really
/// does not carry — a row visible on screen would make the offer read as a lie.
const ELSEWHERE: u64 = 69401;

/// Everything happens at one instant, so a regenerated picture differs only
/// where the interface did.
fn now() -> Timestamp {
    "2026-08-12T09:15:00Z".parse().expect("timestamp")
}

fn ts(s: &str) -> Timestamp {
    s.parse().expect("timestamp")
}

fn repo() -> RepoKey {
    RepoKey {
        host: "github.com".into(),
        owner: "apache".into(),
        name: "airflow".into(),
    }
}

/// One PR in the fixture: what it is, why it wants attention, and what I have
/// already done to it.
struct Fixture {
    number: u64,
    title: &'static str,
    author: &'static str,
    head: &'static str,
    state: PrState,
    /// Work in progress: only a mention reaches the queue past one.
    draft: bool,
    /// Silenced by hand: off the queue, on the muted list, reason and all.
    muted: bool,
    /// The labels the PR carries, as the repo paints them.
    labels: &'static [&'static str],
    /// The interest rule or involvement that tracked it.
    tracked: TrackedReason,
    /// Why it wants attention — `None` for one that wants none, which is what
    /// a PR you have reviewed looks like until the author moves.
    reason: Option<AttentionReason>,
    since: &'static str,
    /// When it was opened on the forge, which is not when it was last touched
    /// and not when this ledger first saw it. Lower numbers are older PRs, as
    /// they are on a real forge.
    opened: &'static str,
    mine: MyState,
    deferred: bool,
    body: Option<&'static str>,
    threads: Vec<ThreadState>,
    reviewers: Vec<ReviewerVerdict>,
}

fn interest(rule: &str) -> TrackedReason {
    TrackedReason::Interest {
        rule: rule.into(),
        after_merge: false,
    }
}

/// A morning's queue: one of every urgency band, both marks, one PR set aside
/// and one that merged and stayed.
fn fixtures() -> Vec<Fixture> {
    vec![
        Fixture {
            number: 70135,
            title: "Deferrable mode for S3KeySensor",
            author: "kaxil",
            head: "9f2a1c4",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:task-sdk", "kind:feature", "needs-review"],
            tracked: interest("label area:task-sdk"),
            reason: Some(AttentionReason::Mention { by: "kaxil".into() }),
            since: "2026-08-12T07:05:00Z",
            opened: "2026-08-04T09:12:00Z",
            mine: MyState {
                last_reviewed_sha: Some("4b71e08".into()),
                last_verdict: Some(Verdict::ChangesRequested),
                last_action_at: Some(ts("2026-08-11T16:20:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: Some(BODY),
            threads: vec![
                thread("T1", true, false, "kaxil", "2026-08-12T07:05:00Z"),
                thread("T2", true, true, "ashb", "2026-08-11T16:20:00Z"),
                thread("T3", false, true, "potiuk", "2026-08-10T11:00:00Z"),
            ],
            reviewers: vec![
                reviewer("ashb", Verdict::ChangesRequested, "2026-08-11T16:20:00Z"),
                reviewer("potiuk", Verdict::Approved, "2026-08-11T09:05:00Z"),
            ],
        },
        Fixture {
            number: 70208,
            title: "Fix scheduler loop starving low-priority tasks",
            author: "potiuk",
            head: "c1d9a77",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:Scheduler", "kind:bug"],
            tracked: interest("label area:Scheduler"),
            reason: Some(AttentionReason::ThreadReply {
                by: "potiuk".into(),
                threads: 2,
            }),
            since: "2026-08-12T07:55:00Z",
            opened: "2026-08-06T15:40:00Z",
            mine: MyState {
                last_reviewed_sha: Some("c1d9a77".into()),
                last_verdict: Some(Verdict::Approved),
                last_action_at: Some(ts("2026-08-12T07:10:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 69982,
            title: "Serialize the DAG timetable without the pickle fallback",
            author: "uranusjr",
            head: "aa3f019",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:serialization", "kind:bug", "backport"],
            tracked: interest("path airflow-core/src/airflow/serialization/**"),
            reason: Some(AttentionReason::ReReview {
                new_commits: 3,
                since_sha: "5e14b22".into(),
            }),
            since: "2026-08-12T06:30:00Z",
            opened: "2026-07-28T11:05:00Z",
            mine: MyState {
                last_reviewed_sha: Some("5e14b22".into()),
                last_verdict: Some(Verdict::Approved),
                last_action_at: Some(ts("2026-08-10T14:00:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 70061,
            title: "Cache the DAG bundle manifest between parses",
            author: "jedcunningham",
            head: "e40b1a9",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:Scheduler"],
            tracked: interest("label area:Scheduler"),
            reason: Some(AttentionReason::ResolvedUnanswered {
                by: "jedcunningham".into(),
                threads: 1,
            }),
            since: "2026-08-12T07:20:00Z",
            opened: "2026-08-01T08:25:00Z",
            mine: MyState {
                last_reviewed_sha: Some("e40b1a9".into()),
                last_verdict: Some(Verdict::Commented),
                last_action_at: Some(ts("2026-08-11T15:00:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        // A draft, which nothing but a mention gets past — so a draft on the
        // queue is itself the point: somebody asked for you by name.
        Fixture {
            number: 70390,
            title: "WIP: replace the executor's polling with a watch",
            author: "o-nikolas",
            head: "5c02fae",
            state: PrState::Open,
            draft: true,
            muted: false,
            labels: &["area:Executors-core", "kind:feature"],
            tracked: interest("label area:Executors-core"),
            reason: Some(AttentionReason::Mention {
                by: "o-nikolas".into(),
            }),
            since: "2026-08-12T08:55:00Z",
            opened: "2026-08-11T16:30:00Z",
            mine: MyState::default(),
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        // Reviewed, and now the author's turn: no reason, so it is off the queue
        // and on the waiting list — which is where a PR goes the moment you
        // review it, rather than nowhere.
        Fixture {
            number: 70044,
            title: "Teach the DAG processor to skip unchanged files",
            author: "uranusjr",
            head: "8ae7c31",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:Scheduler", "kind:feature"],
            tracked: interest("label area:Scheduler"),
            reason: None,
            since: "2026-08-11T14:10:00Z",
            opened: "2026-07-31T13:50:00Z",
            mine: MyState {
                last_reviewed_sha: Some("8ae7c31".into()),
                last_verdict: Some(Verdict::ChangesRequested),
                last_action_at: Some(ts("2026-08-11T14:10:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 69914,
            title: "Drop the legacy SLA callback path",
            author: "potiuk",
            head: "24c9f70",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:serialization"],
            tracked: interest("path airflow-core/src/airflow/models/**"),
            reason: None,
            since: "2026-08-10T16:45:00Z",
            opened: "2026-07-24T10:20:00Z",
            mine: MyState {
                last_reviewed_sha: Some("24c9f70".into()),
                last_verdict: Some(Verdict::Approved),
                last_action_at: Some(ts("2026-08-10T16:45:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        // Silenced by hand: absent from the queue, and the whole of the muted
        // list — with the reason a mute hides rather than erases.
        Fixture {
            number: 70255,
            title: "Rename the KubernetesExecutor config section",
            author: "eladkal",
            head: "6b1fa02",
            state: PrState::Open,
            draft: false,
            muted: true,
            labels: &["area:Executors-core"],
            tracked: interest("label area:Executors-core"),
            reason: Some(AttentionReason::ThreadReply {
                by: "eladkal".into(),
                threads: 4,
            }),
            since: "2026-08-12T06:05:00Z",
            opened: "2026-08-07T18:02:00Z",
            mine: MyState {
                last_reviewed_sha: Some("6b1fa02".into()),
                last_verdict: Some(Verdict::Commented),
                last_action_at: Some(ts("2026-08-10T09:30:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 70311,
            title: "Add a retry budget to the task execution API client",
            author: "amoghrajesh",
            head: "77b0cd1",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:task-sdk"],
            tracked: TrackedReason::Involved("review_requested".into()),
            reason: Some(AttentionReason::ReviewRequested {
                team: Some("apache/airflow-committers".into()),
            }),
            since: "2026-08-11T18:05:00Z",
            opened: "2026-08-09T12:45:00Z",
            mine: MyState::default(),
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 70344,
            title: "Docs: describe the new deferrable operator lifecycle",
            author: "shubham-pyc",
            head: "0ac41e5",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["kind:feature"],
            tracked: interest("author FIRST_TIME_CONTRIBUTOR"),
            reason: Some(AttentionReason::NeedsFirstLook {
                rule: "author FIRST_TIME_CONTRIBUTOR".into(),
            }),
            since: "2026-08-11T12:00:00Z",
            opened: "2026-08-10T09:30:00Z",
            mine: MyState::default(),
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 70102,
            title: "Bump the minimum SQLAlchemy to 2.0.36",
            author: "eladkal",
            head: "3d8e440",
            state: PrState::Open,
            draft: false,
            muted: false,
            labels: &["area:Executors-core", "backport"],
            tracked: interest("label area:Executors-core"),
            reason: Some(AttentionReason::NeedsFirstLook {
                rule: "label area:Executors-core".into(),
            }),
            since: "2026-08-09T10:15:00Z",
            opened: "2026-08-03T14:15:00Z",
            mine: MyState {
                deferred_at: Some(ts("2026-08-11T09:00:00Z")),
                ..MyState::default()
            },
            deferred: true,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
        Fixture {
            number: 69757,
            title: "Rework the scheduler's DAG parsing loop",
            author: "dstandish",
            head: "b62ff31",
            state: PrState::Merged,
            draft: false,
            muted: false,
            labels: &["area:Scheduler", "kind:bug"],
            tracked: TrackedReason::Interest {
                rule: "path airflow-core/src/airflow/jobs/**".into(),
                after_merge: true,
            },
            reason: Some(AttentionReason::Mention {
                by: "dstandish".into(),
            }),
            since: "2026-08-12T08:05:00Z",
            opened: "2026-07-16T08:40:00Z",
            mine: MyState {
                done_sha: Some("b62ff31".into()),
                done_at: Some(ts("2026-08-11T17:40:00Z")),
                ..MyState::default()
            },
            deferred: false,
            body: None,
            threads: vec![],
            reviewers: vec![],
        },
    ]
}

/// A PR description with the shapes a real one has: a template's comment, a
/// heading, prose, and a checklist.
const BODY: &str = "\
<!-- Thanks for opening a pull request! Delete this line before submitting. -->

## What this does

Adds a `deferrable` flag to `S3KeySensor`, so a sensor waiting on a key releases
its worker slot instead of holding it for the whole poke interval.

- [x] Trigger and sensor share one `poke` implementation
- [x] Tests for both modes
- [ ] Docs updated — waiting on #70344

Follow-up to the executor work in #69982.
";

fn thread(id: &str, i_own: bool, is_resolved: bool, last_by: &str, at: &str) -> ThreadState {
    ThreadState {
        thread_id: id.into(),
        i_own,
        is_resolved,
        resolved_by: is_resolved.then(|| "potiuk".to_string()),
        last_comment_author: Some(last_by.into()),
        last_comment_at: Some(ts(at)),
        my_last_comment_at: Some(ts("2026-08-11T16:20:00Z")),
    }
}

fn reviewer(login: &str, verdict: Verdict, at: &str) -> ReviewerVerdict {
    ReviewerVerdict {
        login: login.into(),
        verdict,
        at: ts(at),
    }
}

/// A ledger holding [`fixtures`].
fn ledger() -> Ledger {
    let ledger = Ledger::open_in_memory().expect("ledger");
    let repo_id = ledger.ensure_repo(&repo()).expect("repo");
    let palette: Vec<(String, String)> = PALETTE
        .iter()
        .map(|(name, color)| ((*name).to_string(), (*color).to_string()))
        .collect();
    ledger
        .set_label_colours(repo_id, &palette)
        .expect("label colours");
    for f in fixtures() {
        let pr = PrSnapshot {
            number: f.number,
            title: f.title.into(),
            author: f.author.into(),
            author_association: "MEMBER".into(),
            head_sha: f.head.into(),
            base_ref: "main".into(),
            is_draft: f.draft,
            state: f.state,
            updated_at: ts(f.since),
            created_at: Some(ts(f.opened)),
            labels: f.labels.iter().map(|l| (*l).to_string()).collect(),
            milestone: None,
            files: None,
            files_truncated: false,
        };
        ledger
            .upsert_pr(repo_id, &pr, Some(f.tracked.clone()), now())
            .expect("upsert");
        ledger
            .commit_detail(
                repo_id,
                f.number,
                &f.mine,
                &f.threads,
                &f.reviewers,
                &f.reason
                    .clone()
                    .map(|reason| Attention {
                        reason,
                        since: ts(f.since),
                    })
                    .into_iter()
                    .collect::<Vec<_>>(),
                f.body,
                now(),
            )
            .expect("detail")
            .expect_applied();
        // What I set goes in through the actions that own it: `commit_detail`
        // writes the forge-derived half of `my_state` and deliberately leaves
        // the rest alone, so a `done` handed to it would vanish without a word.
        if let (Some(sha), Some(at)) = (&f.mine.done_sha, f.mine.done_at) {
            ledger.set_done(repo_id, f.number, sha, at).expect("done");
        }
        if f.deferred {
            reviewq_app::actions::set_deferred(&ledger, repo_id, f.number, true).expect("defer");
        }
        if f.muted {
            reviewq_app::actions::set_muted(&ledger, repo_id, f.number, true).expect("mute");
        }
    }
    ledger
}

/// An interface over the fixture, ready to be drawn.
fn app(mode: Mode) -> App {
    App::with_ledger(Theme::new(mode), ledger(), test_config()).expect("app")
}

/// One picture: what it is called, and what the interface is doing in it.
///
/// Every one is the same queue at the same size, so a page showing several has
/// no odd one out and a reader can tell what changed between them.
struct Shot {
    name: &'static str,
    /// Rows. Every picture is the same *width*, so a page showing several has
    /// them lining up — but the reference is simply a taller thing than a queue,
    /// and cropping it to match would be a picture of a legend with its last two
    /// sections missing.
    height: u16,
    arrange: fn(&mut App),
}

/// Every picture is this size. Wide enough that a row has room for its title
/// beside the reason that put it there, which is the pair worth reading.
const WIDTH: u16 = 140;
/// Rows. One size for every picture, so a page of them has no odd one out — the
/// reference included: it is taller than this and says so with a scrollbar,
/// which is what a reader on an ordinary terminal sees anyway.
const HEIGHT: u16 = 34;

/// The pictures the documentation references. Adding one here is all it takes;
/// the test writes it and then holds it to account.
const SHOTS: &[Shot] = &[
    Shot {
        name: "queue",
        height: HEIGHT,
        arrange: |_| {},
    },
    Shot {
        name: "queue-light",
        height: HEIGHT,
        arrange: |app| app.theme = Theme::new(Mode::Light),
    },
    Shot {
        name: "reference",
        height: HEIGHT,
        arrange: |app| app.overlay = Overlay::Help { scroll: 0 },
    },
    Shot {
        name: "show-anyway",
        height: HEIGHT,
        arrange: |app| {
            app.overlay = Overlay::Unqueued {
                number: ELSEWHERE,
                why: Unqueued::Unknown,
            };
        },
    },
    Shot {
        name: "showing",
        height: HEIGHT,
        arrange: |app| app.peek = Some(peeked()),
    },
    Shot {
        name: "snooze",
        height: HEIGHT,
        arrange: |app| app.overlay = Overlay::SnoozePresets { number: 70135 },
    },
    Shot {
        name: "muted",
        height: HEIGHT,
        arrange: |app| app.show_muted(),
    },
    Shot {
        name: "waiting",
        height: HEIGHT,
        arrange: |app| app.show_waiting(),
    },
];

/// A merged PR being shown read-only — what `:69757` offers rather than
/// refusing. Read from the fixture ledger, as the real one is when the PR is
/// stored.
fn peeked() -> reviewq_app::peek::Peeked {
    reviewq_app::peek::Peeked {
        repo: repo(),
        show: reviewq_ledger::PrShow {
            pr: PrSnapshot {
                number: ELSEWHERE,
                title: "Drop the deprecated SubDagOperator".into(),
                author: "eladkal".into(),
                author_association: "MEMBER".into(),
                head_sha: "1f7d5e0".into(),
                base_ref: "main".into(),
                is_draft: false,
                state: PrState::Merged,
                updated_at: ts("2026-08-04T15:30:00Z"),
                created_at: Some(ts("2026-07-16T08:40:00Z")),
                labels: vec![],
                milestone: None,
                files: None,
                files_truncated: false,
            },
            body: Some(
                "Removes `SubDagOperator` and its scheduler special-casing. Anything \
                 still importing it should move to a task group.\n"
                    .into(),
            ),
            tracked_reason: None,
            after_merge: false,
            my_state: MyState::default(),
            threads: vec![],
            reviewers: vec![reviewer(
                "potiuk",
                Verdict::Approved,
                "2026-08-04T14:55:00Z",
            )],
            attention: vec![],
        },
        scratch: true,
    }
}

/// Draw one shot as an SVG.
fn draw(shot: &Shot) -> String {
    let mut app = app(Mode::Dark);
    (shot.arrange)(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, shot.height)).expect("terminal");
    let drawn = terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .expect("draw");
    svg::render(drawn.buffer, &app.theme, &test_config().output.svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pictures in `docs/imgs` are what the interface currently draws.
    ///
    /// Fails rather than rewrites, because a picture changing is a thing to look
    /// at before committing — the message says how.
    #[test]
    fn the_committed_screenshots_are_current() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(IMAGES);
        let writing = std::env::var_os("REVIEWQ_WRITE_DOCS").is_some();
        if writing {
            std::fs::create_dir_all(&dir).expect("make docs/imgs");
        }

        let mut stale = Vec::new();
        for shot in SHOTS {
            let path = dir.join(format!("{}.svg", shot.name));
            let drawn = draw(shot);
            if writing {
                std::fs::write(&path, &drawn).expect("write the picture");
                continue;
            }
            let committed = std::fs::read_to_string(&path).unwrap_or_default();
            if committed != drawn {
                stale.push(shot.name);
            }
        }

        assert!(
            stale.is_empty(),
            "these screenshots no longer match what the interface draws: {stale:?}\n\
             regenerate them with `REVIEWQ_WRITE_DOCS=1 cargo test -p reviewq-tui docs`",
        );
    }

    /// The screens as plain text, for reading a change over rather than opening
    /// six files: `cargo test -p reviewq-tui docs::tests::look -- --nocapture`.
    #[test]
    fn look_at_the_screenshots() {
        for shot in SHOTS {
            let mut app = app(Mode::Dark);
            (shot.arrange)(&mut app);
            let mut terminal =
                Terminal::new(TestBackend::new(WIDTH, shot.height)).expect("terminal");
            terminal
                .draw(|frame| ui::draw(frame, &mut app))
                .expect("draw");
            let buffer = terminal.backend().buffer();
            println!("\n=== {} ===", shot.name);
            for y in 0..shot.height {
                let row: String = (0..WIDTH)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                println!("{row}");
            }
        }
    }

    #[test]
    fn the_fixture_queue_shows_a_morning_rather_than_one_case() {
        // What makes the picture worth taking: every urgency band, so the
        // colours and the ordering both have something to say.
        let app = app(Mode::Dark);
        let bands: std::collections::BTreeSet<u8> = app
            .queue
            .iter()
            .filter_map(|item| item.item.top.as_ref())
            .map(reviewq_ledger::AttentionRow::priority)
            .collect();

        assert_eq!(
            bands.len(),
            6,
            "one of every attention reason there is: {bands:?}"
        );
        assert!(
            app.queue.iter().any(|item| item.item.deferred),
            "and something set aside"
        );
        assert!(
            app.queue.iter().any(|item| !item.item.pr.state.is_open()),
            "and something that merged and stayed"
        );
    }
}
