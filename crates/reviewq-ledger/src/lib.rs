//! The SQLite ledger.
//!
//! A thin, typed wrapper over `rusqlite`. It owns the schema and migrations and
//! trades in `reviewq-core` snapshot types; nothing above it writes SQL. The
//! sync API is synchronous, which is fine for a CLI.

mod schema;

use anyhow::{Context, Result};
use jiff::Timestamp;
use reviewq_core::model::{
    Attention, AttentionReason, MyState, PrSnapshot, PrState, ThreadState, Verdict,
};
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

/// One stored attention reason, as read back from the `attention` table.
#[derive(Debug, Clone)]
pub struct AttentionRow {
    /// The reason's stable discriminant, e.g. `thread_reply`.
    pub reason: String,
    /// The rendered, human-readable evidence string.
    pub detail: String,
    /// When the triggering event happened.
    pub since: Timestamp,
    /// Queue priority recovered from [`reason`](Self::reason); 1 is most urgent.
    /// A row from a newer build with an unknown reason sorts last.
    pub priority: u8,
}

/// A PR on the queue: its snapshot, why it is tracked, and the single
/// highest-priority reason it currently wants attention for.
#[derive(Debug, Clone)]
pub struct QueueItem {
    /// The stored snapshot.
    pub pr: PrSnapshot,
    /// The rendered `tracked_reason`.
    pub tracked_reason: String,
    /// The reason setting this PR's queue position.
    pub top: AttentionRow,
}

/// Everything `reviewq show` needs about one PR.
#[derive(Debug, Clone)]
pub struct PrShow {
    /// The stored snapshot.
    pub pr: PrSnapshot,
    /// The rendered `tracked_reason`, if tracked.
    pub tracked_reason: Option<String>,
    /// My history on the PR.
    pub my_state: MyState,
    /// Its review threads.
    pub threads: Vec<ThreadState>,
    /// Every attention reason it currently holds, most-urgent first.
    pub attention: Vec<AttentionRow>,
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

    /// Tracked PRs whose detail needs a (re-)fetch: never fetched, or fetched
    /// before the last time the PR changed. Open PRs always; merged PRs only
    /// when `include_merged` (the per-project post-merge-review opt-in);
    /// closed-unmerged PRs never. Returns the full snapshot and tracked reason
    /// so the caller can classify without a second read.
    pub fn prs_needing_detail(&self, include_merged: bool) -> Result<Vec<TrackedPr>> {
        // Timestamps are stored as fixed-precision RFC3339 (see `commit_detail`
        // and the sweep), so this lexicographic `<` is a correct chronological
        // comparison.
        let states = if include_merged {
            "('OPEN', 'MERGED')"
        } else {
            "('OPEN')"
        };
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason FROM prs p
            WHERE p.tracked_reason IS NOT NULL AND p.state IN {states}
              AND (p.detail_synced_at IS NULL OR p.detail_synced_at < p.updated_at)
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map([], row_to_tracked)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// My history on a PR, or the default (all-empty) state if none is stored.
    pub fn my_state(&self, number: u64) -> Result<MyState> {
        self.conn
            .query_row(
                r"
                SELECT last_reviewed_sha, last_verdict, last_action_at, done_sha,
                       snoozed_until, muted
                FROM my_state WHERE number = ?1
                ",
                params![number as i64],
                row_to_my_state,
            )
            .optional()
            .with_context(|| format!("reading my_state for #{number}"))
            .map(Option::unwrap_or_default)
    }

    /// Persist a PR's tier-2 detail and freshly-classified attention in one
    /// transaction: my history, its threads (replaced wholesale), the
    /// attention rows (likewise), and the detail-sync watermark. Atomic so an
    /// interrupted detail pass leaves each PR either fully updated or untouched.
    pub fn commit_detail(
        &self,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        attention: &[Attention],
        now: Timestamp,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        write_my_state(&tx, number, my_state)?;
        replace_threads(&tx, number, threads)?;
        replace_attention(&tx, number, attention)?;
        // Stored at whole-second precision so the lexicographic comparison in
        // `prs_needing_detail` against GitHub's whole-second `updatedAt` is
        // correct. A sub-second stamp would sort *before* an equal-second
        // `updatedAt` (`.` < `Z`), re-fetching that PR every sync forever.
        tx.execute(
            "UPDATE prs SET detail_synced_at = ?2 WHERE number = ?1",
            params![number as i64, whole_second(now).to_string()],
        )
        .with_context(|| format!("stamping detail_synced_at for #{number}"))?;
        tx.commit().context("committing PR detail")?;
        Ok(())
    }

    /// Drop attention rows that no longer belong to a queued PR: closed-unmerged
    /// PRs always, and merged PRs unless `include_merged`. Detail is never
    /// re-fetched for these states (see [`prs_needing_detail`](Self::prs_needing_detail)),
    /// so without this their stale rows would linger and show up in `show`. Run
    /// once at the end of a sync.
    pub fn clear_archived_attention(&self, include_merged: bool) -> Result<()> {
        let keep = if include_merged {
            "('OPEN', 'MERGED')"
        } else {
            "('OPEN')"
        };
        self.conn
            .execute(
                &format!(
                    "DELETE FROM attention WHERE pr_number IN
                     (SELECT number FROM prs WHERE state NOT IN {keep})"
                ),
                [],
            )
            .context("clearing archived attention")?;
        Ok(())
    }

    /// The queue: tracked, open PRs that currently want attention, each with its
    /// highest-priority reason, ordered most-urgent first (priority band, then
    /// oldest within the band, then PR number).
    pub fn queue(&self) -> Result<Vec<QueueItem>> {
        // Open PRs, plus merged PRs when a project opted into post-merge review
        // (those only carry attention rows when it did). Closed-unmerged never.
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason, a.reason, a.detail, a.since
            FROM prs p JOIN attention a ON a.pr_number = p.number
            WHERE p.state IN ('OPEN', 'MERGED') AND p.tracked_reason IS NOT NULL
            ",
        ))?;
        let rows = stmt
            .query_map([], |row| {
                let pr = snapshot_from_row(row, 0)?;
                let tracked_reason: String = row.get(12)?;
                let attention = attention_from_row(row, 13)?;
                Ok((pr, tracked_reason, attention))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items: Vec<QueueItem> = Vec::new();
        for (pr, tracked_reason, attention) in rows {
            match items.iter_mut().find(|i| i.pr.number == pr.number) {
                Some(existing) => {
                    if attention_is_more_urgent(&attention, &existing.top) {
                        existing.top = attention;
                    }
                }
                None => items.push(QueueItem {
                    pr,
                    tracked_reason,
                    top: attention,
                }),
            }
        }
        items.sort_by(|a, b| {
            (a.top.priority, a.top.since, a.pr.number).cmp(&(
                b.top.priority,
                b.top.since,
                b.pr.number,
            ))
        });
        Ok(items)
    }

    /// Tracked, open PRs with no attention: seen and understood, waiting on
    /// someone else. Ordered by number.
    pub fn waiting(&self) -> Result<Vec<TrackedPr>> {
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason
            FROM prs p
            WHERE p.state = 'OPEN' AND p.tracked_reason IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM attention a WHERE a.pr_number = p.number)
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(TrackedPr {
                    pr: snapshot_from_row(row, 0)?,
                    tracked_reason: row.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Everything `reviewq show` needs about one PR, or `None` if it is not
    /// stored.
    pub fn show(&self, number: u64) -> Result<Option<PrShow>> {
        let base = self
            .conn
            .query_row(
                &format!("SELECT {PR_COLUMNS}, p.tracked_reason FROM prs p WHERE p.number = ?1"),
                params![number as i64],
                |row| {
                    Ok((
                        snapshot_from_row(row, 0)?,
                        row.get::<_, Option<String>>(12)?,
                    ))
                },
            )
            .optional()
            .with_context(|| format!("reading PR #{number}"))?;
        let Some((pr, tracked_reason)) = base else {
            return Ok(None);
        };

        let my_state = self.my_state(number)?;
        let threads = self.threads(number)?;
        let attention = self.attention(number)?;
        Ok(Some(PrShow {
            pr,
            tracked_reason,
            my_state,
            threads,
            attention,
        }))
    }

    /// A PR's review threads, ordered by id for stability.
    fn threads(&self, number: u64) -> Result<Vec<ThreadState>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT thread_id, i_own, is_resolved, resolved_by, last_comment_author,
                   last_comment_at, my_last_comment_at
            FROM threads WHERE pr_number = ?1 ORDER BY thread_id
            ",
        )?;
        let rows = stmt
            .query_map(params![number as i64], row_to_thread)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A PR's attention rows, most-urgent first.
    fn attention(&self, number: u64) -> Result<Vec<AttentionRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT reason, detail, since FROM attention WHERE pr_number = ?1")?;
        let mut rows = stmt
            .query_map(params![number as i64], |row| attention_from_row(row, 0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by_key(|a| (a.priority, a.since));
        Ok(rows)
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

/// The `prs` snapshot columns, `p.`-qualified and in the order
/// [`snapshot_from_row`] reads them. A single source for every query that
/// reconstructs a [`PrSnapshot`], so column order and reader cannot drift.
const PR_COLUMNS: &str = "p.number, p.title, p.author, p.author_association, \
     p.head_sha, p.is_draft, p.state, p.updated_at, p.labels, p.milestone, \
     p.files, p.files_truncated";

/// Turn a text-decode failure into the rusqlite error a `query_map` closure
/// must return.
fn decode_err(e: Box<dyn std::error::Error + Send + Sync>) -> rusqlite::Error {
    FromSqlConversionFailure(0, Type::Text, e)
}

fn parse_ts(s: &str) -> rusqlite::Result<Timestamp> {
    s.parse().map_err(|e: jiff::Error| decode_err(e.into()))
}

/// Truncate a timestamp to whole seconds, dropping sub-second precision so
/// stored stamps compare lexicographically against GitHub's whole-second form.
fn whole_second(ts: Timestamp) -> Timestamp {
    Timestamp::from_second(ts.as_second()).unwrap_or(ts)
}

/// Read a [`PrSnapshot`] from the twelve [`PR_COLUMNS`] starting at `base`.
fn snapshot_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<PrSnapshot> {
    let state_str: String = row.get(base + 6)?;
    let labels_str: String = row.get(base + 8)?;
    let files_str: Option<String> = row.get(base + 10)?;

    let state = PrState::from_wire(&state_str)
        .ok_or_else(|| decode_err(format!("bad state {state_str:?}").into()))?;
    let labels: Vec<String> =
        serde_json::from_str(&labels_str).map_err(|e| decode_err(e.into()))?;
    let files: Option<Vec<String>> = files_str
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|e| decode_err(e.into()))?;

    Ok(PrSnapshot {
        number: row.get::<_, i64>(base)? as u64,
        title: row.get(base + 1)?,
        author: row.get(base + 2)?,
        author_association: row.get(base + 3)?,
        head_sha: row.get(base + 4)?,
        is_draft: row.get::<_, i64>(base + 5)? != 0,
        state,
        updated_at: parse_ts(&row.get::<_, String>(base + 7)?)?,
        labels,
        milestone: row.get(base + 9)?,
        files,
        files_truncated: row.get::<_, i64>(base + 11)? != 0,
    })
}

fn row_to_tracked(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedPr> {
    Ok(TrackedPr {
        pr: snapshot_from_row(row, 0)?,
        tracked_reason: row.get(12)?,
    })
}

fn row_to_my_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<MyState> {
    let verdict: Option<String> = row.get(1)?;
    let last_action_at: Option<String> = row.get(2)?;
    let snoozed_until: Option<String> = row.get(4)?;
    Ok(MyState {
        last_reviewed_sha: row.get(0)?,
        last_verdict: verdict.as_deref().and_then(Verdict::from_wire),
        last_action_at: last_action_at.as_deref().map(parse_ts).transpose()?,
        done_sha: row.get(3)?,
        snoozed_until: snoozed_until.as_deref().map(parse_ts).transpose()?,
        muted: row.get::<_, i64>(5)? != 0,
    })
}

fn row_to_thread(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadState> {
    let last_comment_at: Option<String> = row.get(5)?;
    let my_last_comment_at: Option<String> = row.get(6)?;
    Ok(ThreadState {
        thread_id: row.get(0)?,
        i_own: row.get::<_, i64>(1)? != 0,
        is_resolved: row.get::<_, i64>(2)? != 0,
        resolved_by: row.get(3)?,
        last_comment_author: row.get(4)?,
        last_comment_at: last_comment_at.as_deref().map(parse_ts).transpose()?,
        my_last_comment_at: my_last_comment_at.as_deref().map(parse_ts).transpose()?,
    })
}

/// Read an [`AttentionRow`] from `(reason, detail, since)` starting at `base`.
fn attention_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<AttentionRow> {
    let reason: String = row.get(base)?;
    let priority = AttentionReason::priority_for(&reason).unwrap_or(u8::MAX);
    Ok(AttentionRow {
        detail: row.get(base + 1)?,
        since: parse_ts(&row.get::<_, String>(base + 2)?)?,
        reason,
        priority,
    })
}

/// Whether `candidate` should outrank the current best: lower priority band,
/// or the same band but an older event.
fn attention_is_more_urgent(candidate: &AttentionRow, best: &AttentionRow) -> bool {
    (candidate.priority, candidate.since) < (best.priority, best.since)
}

fn write_my_state(conn: &Connection, number: u64, s: &MyState) -> Result<()> {
    conn.execute(
        r"
        INSERT INTO my_state (
          number, last_reviewed_sha, last_verdict, last_action_at, done_sha,
          snoozed_until, muted
        ) VALUES (?1,?2,?3,?4,?5,?6,?7)
        ON CONFLICT(number) DO UPDATE SET
          last_reviewed_sha=excluded.last_reviewed_sha,
          last_verdict=excluded.last_verdict,
          last_action_at=excluded.last_action_at,
          done_sha=excluded.done_sha,
          snoozed_until=excluded.snoozed_until,
          muted=excluded.muted
        ",
        params![
            number as i64,
            s.last_reviewed_sha,
            s.last_verdict.map(|v| v.as_str()),
            s.last_action_at.map(|t| t.to_string()),
            s.done_sha,
            s.snoozed_until.map(|t| t.to_string()),
            s.muted as i64,
        ],
    )
    .with_context(|| format!("writing my_state for #{number}"))?;
    Ok(())
}

fn replace_threads(conn: &Connection, number: u64, threads: &[ThreadState]) -> Result<()> {
    conn.execute(
        "DELETE FROM threads WHERE pr_number = ?1",
        params![number as i64],
    )?;
    for t in threads {
        conn.execute(
            r"
            INSERT INTO threads (
              thread_id, pr_number, i_own, is_resolved, resolved_by,
              last_comment_author, last_comment_at, my_last_comment_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
            ",
            params![
                t.thread_id,
                number as i64,
                t.i_own as i64,
                t.is_resolved as i64,
                t.resolved_by,
                t.last_comment_author,
                t.last_comment_at.map(|x| x.to_string()),
                t.my_last_comment_at.map(|x| x.to_string()),
            ],
        )
        .with_context(|| format!("writing thread {} for #{number}", t.thread_id))?;
    }
    Ok(())
}

fn replace_attention(conn: &Connection, number: u64, attention: &[Attention]) -> Result<()> {
    conn.execute(
        "DELETE FROM attention WHERE pr_number = ?1",
        params![number as i64],
    )?;
    for a in attention {
        conn.execute(
            "INSERT INTO attention (pr_number, reason, detail, since) VALUES (?1,?2,?3,?4)",
            params![
                number as i64,
                a.reason.discriminant(),
                a.reason.to_string(),
                a.since.to_string(),
            ],
        )
        .with_context(|| {
            format!(
                "writing attention {} for #{number}",
                a.reason.discriminant()
            )
        })?;
    }
    Ok(())
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

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn attn(reason: AttentionReason, since: &str) -> Attention {
        Attention {
            reason,
            since: ts(since),
        }
    }

    fn track(ledger: &Ledger, p: &PrSnapshot) {
        ledger
            .upsert_pr(
                p,
                Some(TrackedReason::Interest("label area:task-sdk".into())),
                now(),
            )
            .unwrap();
    }

    #[test]
    fn my_state_round_trips_every_field() {
        let ledger = Ledger::open_in_memory().unwrap();
        track(&ledger, &pr(1));
        let state = MyState {
            last_reviewed_sha: Some("deadbeef".into()),
            last_verdict: Some(Verdict::ChangesRequested),
            last_action_at: Some(ts("2026-08-04T10:00:00Z")),
            done_sha: Some("cafebabe".into()),
            snoozed_until: Some(ts("2026-08-09T00:00:00Z")),
            muted: true,
        };
        ledger.commit_detail(1, &state, &[], &[], now()).unwrap();
        assert_eq!(ledger.my_state(1).unwrap(), state);
    }

    #[test]
    fn my_state_defaults_when_absent() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert_eq!(ledger.my_state(999).unwrap(), MyState::default());
    }

    #[test]
    fn commit_detail_replaces_threads_and_attention_wholesale() {
        let ledger = Ledger::open_in_memory().unwrap();
        track(&ledger, &pr(1));

        let thread = ThreadState {
            thread_id: "T1".into(),
            i_own: true,
            is_resolved: false,
            resolved_by: None,
            last_comment_author: Some("kaxil".into()),
            last_comment_at: Some(ts("2026-08-05T08:30:00Z")),
            my_last_comment_at: Some(ts("2026-08-04T11:00:00Z")),
        };
        let first = [attn(
            AttentionReason::ThreadReply {
                by: "kaxil".into(),
                threads: 1,
            },
            "2026-08-05T08:30:00Z",
        )];
        ledger
            .commit_detail(1, &MyState::default(), &[thread], &first, now())
            .unwrap();

        let show = ledger.show(1).unwrap().unwrap();
        assert_eq!(show.threads.len(), 1);
        assert_eq!(show.attention.len(), 1);
        assert_eq!(
            show.threads[0].last_comment_author.as_deref(),
            Some("kaxil")
        );

        // A second detail pass with nothing wipes the earlier rows rather than
        // accumulating them.
        ledger
            .commit_detail(1, &MyState::default(), &[], &[], now())
            .unwrap();
        let show = ledger.show(1).unwrap().unwrap();
        assert!(show.threads.is_empty());
        assert!(show.attention.is_empty());
    }

    #[test]
    fn queue_orders_by_priority_then_age_and_keeps_the_top_reason() {
        let ledger = Ledger::open_in_memory().unwrap();
        track(&ledger, &pr(1));
        track(&ledger, &pr(2));

        // #1 holds two reasons; the mention (priority 1) must set its position.
        ledger
            .commit_detail(
                1,
                &MyState::default(),
                &[],
                &[
                    attn(
                        AttentionReason::NeedsFirstLook { rule: "x".into() },
                        "2026-08-01T00:00:00Z",
                    ),
                    attn(
                        AttentionReason::Mention {
                            by: "potiuk".into(),
                        },
                        "2026-08-05T09:00:00Z",
                    ),
                ],
                now(),
            )
            .unwrap();
        // #2 only needs a first look (priority 6).
        ledger
            .commit_detail(
                2,
                &MyState::default(),
                &[],
                &[attn(
                    AttentionReason::NeedsFirstLook { rule: "y".into() },
                    "2026-07-01T00:00:00Z",
                )],
                now(),
            )
            .unwrap();

        let queue = ledger.queue().unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].pr.number, 1);
        assert_eq!(queue[0].top.reason, "mention");
        assert_eq!(queue[0].top.priority, 1);
        assert_eq!(queue[1].pr.number, 2);
    }

    #[test]
    fn waiting_is_tracked_open_prs_without_attention() {
        let ledger = Ledger::open_in_memory().unwrap();
        track(&ledger, &pr(1));
        track(&ledger, &pr(2));
        ledger
            .commit_detail(
                1,
                &MyState::default(),
                &[],
                &[attn(
                    AttentionReason::Mention {
                        by: "potiuk".into(),
                    },
                    "2026-08-05T09:00:00Z",
                )],
                now(),
            )
            .unwrap();

        let waiting = ledger.waiting().unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].pr.number, 2);
    }

    #[test]
    fn prs_needing_detail_selects_never_or_stale_synced() {
        let ledger = Ledger::open_in_memory().unwrap();
        track(&ledger, &pr(1));
        track(&ledger, &pr(2));
        // #2 synced after its updatedAt: fresh, so excluded.
        ledger
            .commit_detail(2, &MyState::default(), &[], &[], ts("2026-08-06T00:00:00Z"))
            .unwrap();

        let need = ledger.prs_needing_detail(false).unwrap();
        assert_eq!(need.len(), 1);
        assert_eq!(need[0].pr.number, 1);
        assert_eq!(need[0].pr.head_sha, "abc123");
    }

    #[test]
    fn merged_prs_included_in_detail_only_when_opted_in() {
        let ledger = Ledger::open_in_memory().unwrap();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, &merged);

        assert!(
            ledger.prs_needing_detail(false).unwrap().is_empty(),
            "merged PR skipped without the opt-in"
        );
        assert_eq!(
            ledger.prs_needing_detail(true).unwrap().len(),
            1,
            "merged PR fetched with the opt-in"
        );
    }

    fn mention(by: &str, since: &str) -> Attention {
        attn(AttentionReason::Mention { by: by.into() }, since)
    }

    #[test]
    fn a_closed_pr_is_off_the_queue() {
        let ledger = Ledger::open_in_memory().unwrap();
        let mut closed = pr(1);
        closed.state = PrState::Closed;
        track(&ledger, &closed);
        ledger
            .commit_detail(
                1,
                &MyState::default(),
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();
        assert!(ledger.queue().unwrap().is_empty());
        assert!(ledger.waiting().unwrap().is_empty());
    }

    #[test]
    fn a_merged_pr_with_attention_is_on_the_queue() {
        let ledger = Ledger::open_in_memory().unwrap();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, &merged);
        ledger
            .commit_detail(
                1,
                &MyState::default(),
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();

        let queue = ledger.queue().unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].pr.number, 1);
        // A merged PR is never "waiting" — waiting is the open-and-idle bucket.
        assert!(ledger.waiting().unwrap().is_empty());
    }

    #[test]
    fn clear_archived_attention_respects_the_opt_in() {
        let ledger = Ledger::open_in_memory().unwrap();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, &merged);
        ledger
            .commit_detail(
                1,
                &MyState::default(),
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();

        // With the opt-in, merged attention is kept.
        ledger.clear_archived_attention(true).unwrap();
        assert_eq!(ledger.queue().unwrap().len(), 1);

        // Without it, merged attention is swept away.
        ledger.clear_archived_attention(false).unwrap();
        assert!(ledger.queue().unwrap().is_empty());
        assert!(ledger.show(1).unwrap().unwrap().attention.is_empty());
    }
}
