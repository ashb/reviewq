//! The SQLite ledger.
//!
//! A thin, typed wrapper over `rusqlite`. It owns the schema and migrations and
//! trades in `reviewq-core` snapshot types; nothing above it writes SQL. The
//! sync API is synchronous, which is fine for a CLI.

mod schema;

use anyhow::{Context, Result};
use jiff::Timestamp;
use reviewq_core::model::{PrSnapshot, PrState};
use rusqlite::types::Type;
use rusqlite::{Connection, Error::FromSqlConversionFailure, OptionalExtension, params};

pub use schema::SCHEMA_VERSION;

/// An open ledger.
pub struct Ledger {
    conn: Connection,
}

/// Why a PR is tracked, before it is rendered into the stored `tracked_reason`.
///
/// Ordered by strength: a relationship that names me
/// ([`Involved`](Self::Involved)) is a more concrete reason to care than a rule
/// I happen to watch ([`Interest`](Self::Interest)), so it wins when a PR has
/// both and is never downgraded back to interest on a later sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedReason {
    /// Matched an interest rule; carries the bare match, e.g. `label area:x`.
    Interest(String),
    /// A relationship names me; carries the reason, e.g. `review_requested`.
    Involved(String),
}

impl TrackedReason {
    fn rank(&self) -> u8 {
        match self {
            Self::Interest(_) => 1,
            Self::Involved(_) => 2,
        }
    }

    /// The string stored in `tracked_reason` and shown to the user.
    pub fn render(&self) -> String {
        match self {
            Self::Interest(m) => format!("interest: {m}"),
            Self::Involved(r) => format!("involved: {r}"),
        }
    }
}

/// A tracked PR as read back from the ledger.
#[derive(Debug, Clone)]
pub struct TrackedPr {
    /// The stored snapshot.
    pub pr: PrSnapshot,
    /// The rendered `tracked_reason`.
    pub tracked_reason: String,
}

impl Ledger {
    /// Open (creating if absent) the ledger at `path` and migrate it.
    pub fn open(path: &std::path::Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating ledger dir {}", dir.display()))?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("opening ledger {}", path.display()))?;
        Self::from_conn(conn)
    }

    /// An in-memory ledger, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign keys")?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Insert or update a PR, merging its tracked reason with any already
    /// stored. Returns `true` if the PR was newly inserted.
    pub fn upsert_pr(
        &self,
        pr: &PrSnapshot,
        reason: Option<TrackedReason>,
        now: Timestamp,
    ) -> Result<bool> {
        upsert_row(&self.conn, pr, reason.as_ref(), now)
    }

    /// Persist a whole sweep page and advance the cursor in one transaction, so
    /// an interrupted sync leaves a consistent checkpoint (and so the page's
    /// writes are one commit, not one per PR). Returns how many PRs were new.
    pub fn commit_sweep_page(
        &self,
        prs: &[(PrSnapshot, Option<TrackedReason>)],
        now: Timestamp,
        cursor_key: &str,
        cursor_value: &str,
    ) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        let mut new = 0;
        for (pr, reason) in prs {
            if upsert_row(&tx, pr, reason.as_ref(), now)? {
                new += 1;
            }
        }
        set_meta_row(&tx, cursor_key, cursor_value)?;
        tx.commit().context("committing sweep page")?;
        Ok(new)
    }

    /// A metadata value, e.g. the sync cursor.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM sync_meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("reading sync_meta {key}"))
    }

    /// Set a metadata value.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        set_meta_row(&self.conn, key, value)
    }

    /// Every tracked PR, ordered by number.
    pub fn list_tracked(&self) -> Result<Vec<TrackedPr>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT number, title, author, author_association, head_sha, is_draft,
                   state, updated_at, labels, milestone, files, files_truncated,
                   tracked_reason
            FROM prs
            WHERE tracked_reason IS NOT NULL
            ORDER BY number
            ",
        )?;
        let rows = stmt
            .query_map([], row_to_tracked)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(tracked, total)` PR counts, for the sync summary.
    pub fn counts(&self) -> Result<(u64, u64)> {
        let tracked = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE tracked_reason IS NOT NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let total = self
            .conn
            .query_row("SELECT COUNT(*) FROM prs", [], |row| row.get::<_, i64>(0))?;
        Ok((tracked as u64, total as u64))
    }

    /// Count of stored PRs whose file list GitHub truncated and that matched no
    /// rule — the "unknown, not non-matching" set `doctor` should surface.
    pub fn count_truncated_untracked(&self) -> Result<u64> {
        let n = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE files_truncated = 1 AND tracked_reason IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(n as u64)
    }
}

/// Insert or update one PR row against `conn` (a connection or an open
/// transaction), merging its tracked reason with any already stored. Returns
/// `true` if the row was newly inserted.
fn upsert_row(
    conn: &Connection,
    pr: &PrSnapshot,
    reason: Option<&TrackedReason>,
    now: Timestamp,
) -> Result<bool> {
    let merged = merge_reason(tracked_reason(conn, pr.number)?.as_deref(), reason);
    let is_new = existing_row(conn, pr.number)?.is_none();

    let labels = serde_json::to_string(&pr.labels).context("encoding labels")?;
    let files = pr
        .files
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("encoding files")?;

    conn.execute(
        r"
        INSERT INTO prs (
          number, title, author, author_association, head_sha, is_draft,
          state, updated_at, labels, milestone, files, files_truncated,
          tracked_reason, first_seen_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
        ON CONFLICT(number) DO UPDATE SET
          title=excluded.title,
          author=excluded.author,
          author_association=excluded.author_association,
          head_sha=excluded.head_sha,
          is_draft=excluded.is_draft,
          state=excluded.state,
          updated_at=excluded.updated_at,
          labels=excluded.labels,
          milestone=excluded.milestone,
          files=excluded.files,
          files_truncated=excluded.files_truncated,
          tracked_reason=excluded.tracked_reason
        ",
        params![
            pr.number as i64,
            pr.title,
            pr.author,
            pr.author_association,
            pr.head_sha,
            pr.is_draft as i64,
            pr.state.as_str(),
            pr.updated_at.to_string(),
            labels,
            pr.milestone,
            files,
            pr.files_truncated as i64,
            merged,
            now.to_string(),
        ],
    )
    .with_context(|| format!("upserting PR #{}", pr.number))?;
    Ok(is_new)
}

fn set_meta_row(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .with_context(|| format!("writing sync_meta {key}"))?;
    Ok(())
}

fn tracked_reason(conn: &Connection, number: u64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT tracked_reason FROM prs WHERE number = ?1",
        params![number as i64],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .with_context(|| format!("reading tracked_reason for #{number}"))
    .map(Option::flatten)
}

fn existing_row(conn: &Connection, number: u64) -> Result<Option<u64>> {
    conn.query_row(
        "SELECT number FROM prs WHERE number = ?1",
        params![number as i64],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .with_context(|| format!("checking for PR #{number}"))
    .map(|opt| opt.map(|n| n as u64))
}

/// Merge a stored reason with an incoming one by precedence: keep the stronger,
/// refresh on a tie, never downgrade. `None` incoming leaves the stored value.
fn merge_reason(existing: Option<&str>, incoming: Option<&TrackedReason>) -> Option<String> {
    match (existing, incoming) {
        (existing, None) => existing.map(str::to_string),
        (None, Some(new)) => Some(new.render()),
        (Some(old), Some(new)) => {
            if new.rank() >= stored_rank(old) {
                Some(new.render())
            } else {
                Some(old.to_string())
            }
        }
    }
}

fn stored_rank(reason: &str) -> u8 {
    if reason.starts_with("involved:") {
        2
    } else if reason.starts_with("interest:") {
        1
    } else {
        0
    }
}

fn row_to_tracked(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedPr> {
    let number: i64 = row.get(0)?;
    let state_str: String = row.get(6)?;
    let updated_str: String = row.get(7)?;
    let labels_str: String = row.get(8)?;
    let files_str: Option<String> = row.get(10)?;

    let boxed =
        |e: Box<dyn std::error::Error + Send + Sync>| FromSqlConversionFailure(0, Type::Text, e);

    let state = PrState::from_wire(&state_str)
        .ok_or_else(|| boxed(format!("bad state {state_str:?}").into()))?;
    let updated_at: Timestamp = updated_str
        .parse()
        .map_err(|e: jiff::Error| boxed(e.into()))?;
    let labels: Vec<String> = serde_json::from_str(&labels_str).map_err(|e| boxed(Box::new(e)))?;
    let files: Option<Vec<String>> = files_str
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| boxed(Box::new(e)))?;

    Ok(TrackedPr {
        pr: PrSnapshot {
            number: number as u64,
            title: row.get(1)?,
            author: row.get(2)?,
            author_association: row.get(3)?,
            head_sha: row.get(4)?,
            is_draft: row.get::<_, i64>(5)? != 0,
            state,
            updated_at,
            labels,
            milestone: row.get(9)?,
            files,
            files_truncated: row.get::<_, i64>(11)? != 0,
        },
        tracked_reason: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64) -> PrSnapshot {
        PrSnapshot {
            number,
            title: format!("PR {number}"),
            author: "octocat".into(),
            author_association: "CONTRIBUTOR".into(),
            head_sha: "abc123".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: "2026-08-05T12:00:00Z".parse().unwrap(),
            labels: vec!["area:task-sdk".into()],
            milestone: Some("3.2.0".into()),
            files: None,
            files_truncated: false,
        }
    }

    fn now() -> Timestamp {
        "2026-08-05T12:00:00Z".parse().unwrap()
    }

    #[test]
    fn migrate_sets_the_expected_version() {
        let ledger = Ledger::open_in_memory().unwrap();
        let v: i64 = ledger
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v as usize, SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_ledger_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", (SCHEMA_VERSION + 1) as i64)
            .unwrap();
        // A DB past the last known migration, with no down-migrations defined,
        // is refused rather than run against.
        assert!(schema::migrate(&mut conn).is_err());
    }

    #[test]
    fn upsert_reports_new_then_not_new_and_round_trips() {
        let ledger = Ledger::open_in_memory().unwrap();
        let reason = TrackedReason::Interest("label area:task-sdk".into());

        assert!(
            ledger
                .upsert_pr(&pr(1), Some(reason.clone()), now())
                .unwrap()
        );
        assert!(!ledger.upsert_pr(&pr(1), Some(reason), now()).unwrap());

        let tracked = ledger.list_tracked().unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].pr.number, 1);
        assert_eq!(tracked[0].pr.milestone.as_deref(), Some("3.2.0"));
        assert_eq!(tracked[0].pr.updated_at, now());
        assert_eq!(tracked[0].tracked_reason, "interest: label area:task-sdk");
    }

    #[test]
    fn commit_sweep_page_persists_prs_and_cursor_atomically_and_resumes() {
        let ledger = Ledger::open_in_memory().unwrap();
        let page = vec![
            (
                pr(1),
                Some(TrackedReason::Interest("label area:task-sdk".into())),
            ),
            (pr(2), None),
        ];

        let new = ledger
            .commit_sweep_page(&page, now(), "last_sync_at", "2026-08-05T12:00:00Z")
            .unwrap();
        assert_eq!(new, 2, "both PRs were newly inserted");
        // ...but only the one with a reason is tracked.
        assert_eq!(ledger.counts().unwrap(), (1, 2));
        assert_eq!(
            ledger.get_meta("last_sync_at").unwrap().as_deref(),
            Some("2026-08-05T12:00:00Z")
        );

        // Re-committing the same page (a resume over the overlap) is a no-op for
        // the "new" count and just advances the cursor.
        let again = ledger
            .commit_sweep_page(&page, now(), "last_sync_at", "2026-08-05T12:05:00Z")
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(
            ledger.get_meta("last_sync_at").unwrap().as_deref(),
            Some("2026-08-05T12:05:00Z")
        );
    }

    #[test]
    fn untracked_prs_are_stored_but_not_listed() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.upsert_pr(&pr(1), None, now()).unwrap();
        assert!(ledger.list_tracked().unwrap().is_empty());
        assert_eq!(ledger.counts().unwrap(), (0, 1));
    }

    #[test]
    fn first_seen_at_survives_a_later_upsert() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .upsert_pr(&pr(1), None, "2026-01-01T00:00:00Z".parse().unwrap())
            .unwrap();
        ledger.upsert_pr(&pr(1), None, now()).unwrap();
        let seen: String = ledger
            .conn
            .query_row("SELECT first_seen_at FROM prs WHERE number = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(seen, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn involvement_beats_interest_and_is_not_downgraded() {
        let ledger = Ledger::open_in_memory().unwrap();
        let interest = || TrackedReason::Interest("label area:task-sdk".into());
        ledger.upsert_pr(&pr(1), Some(interest()), now()).unwrap();

        // The involvement search upserts the same PR as involved.
        ledger
            .upsert_pr(
                &pr(1),
                Some(TrackedReason::Involved("review_requested".into())),
                now(),
            )
            .unwrap();
        assert_eq!(
            ledger.list_tracked().unwrap()[0].tracked_reason,
            "involved: review_requested"
        );

        // A later sweep re-asserting interest must not clobber involvement.
        ledger.upsert_pr(&pr(1), Some(interest()), now()).unwrap();
        assert_eq!(
            ledger.list_tracked().unwrap()[0].tracked_reason,
            "involved: review_requested"
        );
    }

    #[test]
    fn meta_round_trips() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert_eq!(ledger.get_meta("cursor").unwrap(), None);
        ledger.set_meta("cursor", "2026-08-05T12:00:00Z").unwrap();
        ledger.set_meta("cursor", "2026-08-05T13:00:00Z").unwrap();
        assert_eq!(
            ledger.get_meta("cursor").unwrap().as_deref(),
            Some("2026-08-05T13:00:00Z")
        );
    }

    #[test]
    fn truncated_untracked_are_counted() {
        let ledger = Ledger::open_in_memory().unwrap();
        let mut p = pr(1);
        p.files = Some(vec!["docs/x.rst".into()]);
        p.files_truncated = true;
        ledger.upsert_pr(&p, None, now()).unwrap();
        assert_eq!(ledger.count_truncated_untracked().unwrap(), 1);
    }

    #[test]
    fn merge_reason_keeps_the_stronger() {
        let interest = TrackedReason::Interest("label x".into());
        let involved = TrackedReason::Involved("mention".into());

        assert_eq!(merge_reason(None, None), None);
        assert_eq!(
            merge_reason(None, Some(&interest)).as_deref(),
            Some("interest: label x")
        );
        assert_eq!(
            merge_reason(Some("involved: review_requested"), Some(&interest)).as_deref(),
            Some("involved: review_requested")
        );
        assert_eq!(
            merge_reason(Some("interest: label x"), Some(&involved)).as_deref(),
            Some("involved: mention")
        );
        assert_eq!(
            merge_reason(Some("involved: old"), None).as_deref(),
            Some("involved: old")
        );
    }
}
