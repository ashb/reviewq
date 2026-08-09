//! The SQLite ledger.
//!
//! A thin, typed wrapper over `rusqlite`. It owns the schema and migrations and
//! trades in `reviewq-core` snapshot types; nothing above it writes SQL. The
//! sync API is synchronous, which is fine for a CLI.

mod schema;

use anyhow::{Context, Result};
use jiff::Timestamp;
use reviewq_core::model::{
    Attention, AttentionReason, MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict,
};
use rusqlite::types::Type;
use rusqlite::{Connection, Error::FromSqlConversionFailure, OptionalExtension, params};

pub use schema::SCHEMA_VERSION;

/// Identifies a repo the ledger tracks state for: the forge host it lives on,
/// plus owner/name on that host. Distinct from `reviewq`'s own `RepoRef` —
/// the ledger doesn't depend on the CLI crate's config types.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoKey {
    /// The forge host, e.g. `github.com` or a GitHub Enterprise hostname.
    pub host: String,
    /// The repo's owner (user or org login).
    pub owner: String,
    /// The repo's name.
    pub name: String,
}

impl RepoKey {
    /// `owner/name`, matching `reviewq`'s own `RepoRef::slug`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// An open ledger — one handle over the whole database, every repo it knows
/// about included. Every method that reads or writes PR-scoped state takes
/// the `repo_id` [`ensure_repo`](Self::ensure_repo) resolves, rather than the
/// handle itself being scoped to one repo: a project with several repos
/// shares a single `Ledger`.
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
    /// `reviewq defer` was called and nothing has happened since (`top.since`
    /// predates it): sorted after every non-deferred item regardless of
    /// priority, but still shown rather than hidden.
    pub deferred: bool,
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
    /// Every reviewer's most recent submitted verdict, not just mine.
    pub reviewers: Vec<ReviewerVerdict>,
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

    /// Get-or-create `repo`'s row in `repos`, returning its id — every method
    /// below takes the `repo_id` this resolves, once per repo a caller cares
    /// about, rather than the `Ledger` itself being scoped to one. Idempotent.
    ///
    /// The very first call after upgrading past schema v3 adopts the
    /// anonymous placeholder [`schema`]'s migration 4 leaves for whatever
    /// pre-v4 data existed (that database was single-repo only, so there's
    /// exactly one legitimate owner for it) — preserving its `my_state` et al.
    /// rather than leaving them attributed to a repo nothing will ever query
    /// by that name. Every call after that, for any repo, is a plain
    /// get-or-create.
    pub fn ensure_repo(&self, repo: &RepoKey) -> Result<i64> {
        let placeholder: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM repos WHERE host = '' AND owner = '' AND name = ''",
                [],
                |row| row.get(0),
            )
            .optional()
            .context("checking for a pre-v4 placeholder repo")?;
        if let Some(id) = placeholder {
            self.conn
                .execute(
                    "UPDATE repos SET host = ?2, owner = ?3, name = ?4 WHERE id = ?1",
                    params![id, repo.host, repo.owner, repo.name],
                )
                .context("adopting the pre-v4 placeholder repo")?;
            return Ok(id);
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO repos (host, owner, name) VALUES (?1, ?2, ?3)",
                params![repo.host, repo.owner, repo.name],
            )
            .context("registering repo")?;
        self.conn
            .query_row(
                "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
                params![repo.host, repo.owner, repo.name],
                |row| row.get(0),
            )
            .context("resolving repo id")
    }

    /// Every repo this ledger knows about, in no particular order — what
    /// `list`/`next` iterate to build a queue spanning every repo. Ledger-only,
    /// like every other read here: it reflects whatever has actually been
    /// synced, not what a (possibly stale, possibly absent) config currently
    /// says should exist.
    pub fn repos(&self) -> Result<Vec<(i64, RepoKey)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, host, owner, name FROM repos")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    RepoKey {
                        host: row.get(1)?,
                        owner: row.get(2)?,
                        name: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Insert or update a PR, merging its tracked reason with any already
    /// stored. Returns `true` if the PR was newly inserted.
    pub fn upsert_pr(
        &self,
        repo_id: i64,
        pr: &PrSnapshot,
        reason: Option<TrackedReason>,
        now: Timestamp,
    ) -> Result<bool> {
        upsert_row(&self.conn, repo_id, pr, reason.as_ref(), now)
    }

    /// Persist a whole sweep page and advance the cursor in one transaction, so
    /// an interrupted sync leaves a consistent checkpoint (and so the page's
    /// writes are one commit, not one per PR). Returns how many PRs were new.
    pub fn commit_sweep_page(
        &self,
        repo_id: i64,
        prs: &[(PrSnapshot, Option<TrackedReason>)],
        now: Timestamp,
        cursor_key: &str,
        cursor_value: &str,
    ) -> Result<u64> {
        let tx = self.conn.unchecked_transaction()?;
        let mut new = 0;
        for (pr, reason) in prs {
            if upsert_row(&tx, repo_id, pr, reason.as_ref(), now)? {
                new += 1;
            }
        }
        set_meta_row(&tx, repo_id, cursor_key, cursor_value)?;
        tx.commit().context("committing sweep page")?;
        Ok(new)
    }

    /// A metadata value, e.g. the sync cursor.
    pub fn get_meta(&self, repo_id: i64, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM sync_meta WHERE repo_id = ?1 AND key = ?2",
                params![repo_id, key],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("reading sync_meta {key}"))
    }

    /// Set a metadata value.
    pub fn set_meta(&self, repo_id: i64, key: &str, value: &str) -> Result<()> {
        set_meta_row(&self.conn, repo_id, key, value)
    }

    /// Every tracked PR, ordered by number.
    pub fn list_tracked(&self, repo_id: i64) -> Result<Vec<TrackedPr>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT number, title, author, author_association, head_sha, is_draft,
                   state, updated_at, labels, milestone, files, files_truncated,
                   tracked_reason
            FROM prs
            WHERE repo_id = ?1 AND tracked_reason IS NOT NULL
            ORDER BY number
            ",
        )?;
        let rows = stmt
            .query_map(params![repo_id], row_to_tracked)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(tracked, total)` PR counts, for the sync summary.
    pub fn counts(&self, repo_id: i64) -> Result<(u64, u64)> {
        let tracked = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE repo_id = ?1 AND tracked_reason IS NOT NULL",
            params![repo_id],
            |row| row.get::<_, i64>(0),
        )?;
        let total = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE repo_id = ?1",
            params![repo_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok((tracked as u64, total as u64))
    }

    /// Count of stored PRs whose file list GitHub truncated and that matched no
    /// rule — the "unknown, not non-matching" set `doctor` should surface.
    pub fn count_truncated_untracked(&self, repo_id: i64) -> Result<u64> {
        let n = self.conn.query_row(
            "SELECT COUNT(*) FROM prs WHERE repo_id = ?1 AND files_truncated = 1 AND tracked_reason IS NULL",
            params![repo_id],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(n as u64)
    }

    /// Tracked PRs whose detail needs a (re-)fetch: never fetched, or fetched
    /// before the last time the PR changed. Open PRs always; merged PRs only
    /// when `include_merged` (the per-project post-merge-review opt-in);
    /// closed-unmerged PRs never. Returns the full snapshot and tracked reason
    /// so the caller can classify without a second read.
    pub fn prs_needing_detail(&self, repo_id: i64, include_merged: bool) -> Result<Vec<TrackedPr>> {
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
            WHERE p.repo_id = ?1 AND p.tracked_reason IS NOT NULL AND p.state IN {states}
              AND (p.detail_synced_at IS NULL OR p.detail_synced_at < p.updated_at)
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map(params![repo_id], row_to_tracked)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// My history on a PR, or the default (all-empty) state if none is stored.
    pub fn my_state(&self, repo_id: i64, number: u64) -> Result<MyState> {
        self.conn
            .query_row(
                r"
                SELECT last_reviewed_sha, last_verdict, last_action_at, done_sha,
                       snoozed_until, muted, deferred_at, done_at
                FROM my_state WHERE repo_id = ?1 AND number = ?2
                ",
                params![repo_id, number as i64],
                row_to_my_state,
            )
            .optional()
            .with_context(|| format!("reading my_state for #{number}"))
            .map(Option::unwrap_or_default)
    }

    /// Record `reviewq done`: the head SHA I've acknowledged, and when.
    /// Touches only `done_sha`/`done_at` — never `last_reviewed_sha`,
    /// `last_verdict` or `last_action_at` (forge-derived, owned by
    /// [`commit_detail`](Self::commit_detail)'s next run) nor any other
    /// user-set field — so this and a concurrent `sync` can never lose each
    /// other's write, in either direction. The PR must already be in the
    /// ledger (a foreign key error otherwise); callers check with
    /// [`show`](Self::show) first for a clearer message.
    pub fn set_done(
        &self,
        repo_id: i64,
        number: u64,
        done_sha: &str,
        done_at: Timestamp,
    ) -> Result<()> {
        self.conn
            .execute(
                r"
                INSERT INTO my_state (repo_id, number, done_sha, done_at) VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(repo_id, number) DO UPDATE SET
                  done_sha = excluded.done_sha,
                  done_at = excluded.done_at
                ",
                params![repo_id, number as i64, done_sha, done_at.to_string()],
            )
            .with_context(|| format!("recording done for #{number}"))?;
        Ok(())
    }

    /// Record `reviewq snooze`. Touches only `snoozed_until`; see
    /// [`set_done`](Self::set_done) for why that matters.
    pub fn set_snoozed_until(&self, repo_id: i64, number: u64, until: Timestamp) -> Result<()> {
        self.conn
            .execute(
                r"
                INSERT INTO my_state (repo_id, number, snoozed_until) VALUES (?1, ?2, ?3)
                ON CONFLICT(repo_id, number) DO UPDATE SET snoozed_until = excluded.snoozed_until
                ",
                params![repo_id, number as i64, until.to_string()],
            )
            .with_context(|| format!("snoozing #{number}"))?;
        Ok(())
    }

    /// Record `reviewq mute`/`unmute`. Touches only `muted`.
    pub fn set_muted(&self, repo_id: i64, number: u64, muted: bool) -> Result<()> {
        self.conn
            .execute(
                r"
                INSERT INTO my_state (repo_id, number, muted) VALUES (?1, ?2, ?3)
                ON CONFLICT(repo_id, number) DO UPDATE SET muted = excluded.muted
                ",
                params![repo_id, number as i64, muted as i64],
            )
            .with_context(|| format!("setting muted for #{number}"))?;
        Ok(())
    }

    /// Record `reviewq defer`/`undefer`. Touches only `deferred_at`.
    pub fn set_deferred_at(
        &self,
        repo_id: i64,
        number: u64,
        deferred_at: Option<Timestamp>,
    ) -> Result<()> {
        self.conn
            .execute(
                r"
                INSERT INTO my_state (repo_id, number, deferred_at) VALUES (?1, ?2, ?3)
                ON CONFLICT(repo_id, number) DO UPDATE SET deferred_at = excluded.deferred_at
                ",
                params![repo_id, number as i64, deferred_at.map(|t| t.to_string())],
            )
            .with_context(|| format!("setting deferred_at for #{number}"))?;
        Ok(())
    }

    /// Drop a PR's attention rows immediately, without waiting for the next
    /// sync to reclassify — how `snooze` and `mute` take effect on the queue
    /// right away. Clears every reason, `review_requested` included: that's
    /// what snooze/mute both mean (`classify` suppresses everything for
    /// either). `done` uses the narrower
    /// [`clear_done_attention`](Self::clear_done_attention) instead.
    pub fn clear_attention(&self, repo_id: i64, number: u64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
                params![repo_id, number as i64],
            )
            .with_context(|| format!("clearing attention for #{number}"))?;
        Ok(())
    }

    /// The instant-hide half of `reviewq done`: every reason `done` is allowed
    /// to clear per the reason table, but not `review_requested` — only my
    /// review or the request being withdrawn clears that one.
    pub fn clear_done_attention(&self, repo_id: i64, number: u64) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM attention WHERE repo_id = ?1 AND pr_number = ?2 AND reason != 'review_requested'",
                params![repo_id, number as i64],
            )
            .with_context(|| format!("clearing done attention for #{number}"))?;
        Ok(())
    }

    /// Force-track a PR that matched no interest rule and named nobody.
    /// Returns `false`, changing nothing, if the PR is already tracked —
    /// `track` is a fallback for the untracked case, not a way to relabel an
    /// existing tracked reason (an unconditional overwrite here could drop a
    /// PR's `interest:` reason, and with it `needs_first_look`, permanently:
    /// [`merge_reason`] never downgrades `involved:` back down). The PR must
    /// already have a row (from a sweep); the caller checks with
    /// [`show`](Self::show) first.
    pub fn track(&self, repo_id: i64, number: u64) -> Result<bool> {
        if tracked_reason(&self.conn, repo_id, number)?.is_some() {
            return Ok(false);
        }
        self.conn
            .execute(
                "UPDATE prs SET tracked_reason = ?3 WHERE repo_id = ?1 AND number = ?2",
                params![
                    repo_id,
                    number as i64,
                    TrackedReason::Involved("manual".into()).render()
                ],
            )
            .with_context(|| format!("force-tracking #{number}"))?;
        Ok(true)
    }

    /// Persist a PR's tier-2 detail and freshly-classified attention in one
    /// transaction: the forge-derived half of my history (`last_reviewed_sha`,
    /// `last_verdict`, `last_action_at` — see
    /// [`write_forge_state`]), its threads (replaced wholesale), every
    /// reviewer's verdict (likewise), the attention rows (likewise), and the
    /// detail-sync watermark. Atomic so an interrupted detail pass leaves each
    /// PR either fully updated or untouched. `my_state` is read by the caller
    /// beforehand for [`classify`](reviewq_core::model::classify) to decide
    /// against, but only its forge-derived fields are written back here — the
    /// user-set fields (`done_sha`, `snoozed_until`, `muted`, `deferred_at`,
    /// `done_at`) are never touched, so a `reviewq done`/`snooze`/`mute`/`defer`
    /// racing this call can never be lost, in either direction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_detail(
        &self,
        repo_id: i64,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        attention: &[Attention],
        now: Timestamp,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        write_forge_state(&tx, repo_id, number, my_state)?;
        replace_threads(&tx, repo_id, number, threads)?;
        replace_reviewers(&tx, repo_id, number, reviewers)?;
        replace_attention(&tx, repo_id, number, attention)?;
        // Stored at whole-second precision so the lexicographic comparison in
        // `prs_needing_detail` against GitHub's whole-second `updatedAt` is
        // correct. A sub-second stamp would sort *before* an equal-second
        // `updatedAt` (`.` < `Z`), re-fetching that PR every sync forever.
        tx.execute(
            "UPDATE prs SET detail_synced_at = ?3 WHERE repo_id = ?1 AND number = ?2",
            params![repo_id, number as i64, whole_second(now).to_string()],
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
    pub fn clear_archived_attention(&self, repo_id: i64, include_merged: bool) -> Result<()> {
        let keep = if include_merged {
            "('OPEN', 'MERGED')"
        } else {
            "('OPEN')"
        };
        self.conn
            .execute(
                &format!(
                    "DELETE FROM attention WHERE repo_id = ?1 AND pr_number IN
                     (SELECT number FROM prs WHERE repo_id = ?1 AND state NOT IN {keep})"
                ),
                params![repo_id],
            )
            .context("clearing archived attention")?;
        Ok(())
    }

    /// The queue: tracked, open PRs that currently want attention, each with its
    /// highest-priority reason, ordered most-urgent first (priority band, then
    /// oldest within the band, then PR number) — except a deferred PR (see
    /// [`QueueItem::deferred`]), which sorts after every non-deferred item
    /// regardless of priority.
    pub fn queue(&self, repo_id: i64) -> Result<Vec<QueueItem>> {
        // Open PRs, plus merged PRs when a project opted into post-merge review
        // (those only carry attention rows when it did). Closed-unmerged never.
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason, a.reason, a.detail, a.since,
                   ms.deferred_at
            FROM prs p
            JOIN attention a ON a.repo_id = p.repo_id AND a.pr_number = p.number
            LEFT JOIN my_state ms ON ms.repo_id = p.repo_id AND ms.number = p.number
            WHERE p.repo_id = ?1 AND p.state IN ('OPEN', 'MERGED') AND p.tracked_reason IS NOT NULL
            ",
        ))?;
        let rows = stmt
            .query_map(params![repo_id], |row| {
                let pr = snapshot_from_row(row, 0)?;
                let tracked_reason: String = row.get(12)?;
                let attention = attention_from_row(row, 13)?;
                let deferred_at: Option<String> = row.get(16)?;
                let deferred_at = deferred_at.as_deref().map(parse_ts).transpose()?;
                Ok((pr, tracked_reason, attention, deferred_at))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items: Vec<QueueItem> = Vec::new();
        let mut deferred_since: std::collections::HashMap<u64, Timestamp> =
            std::collections::HashMap::new();
        for (pr, tracked_reason, attention, deferred_at) in rows {
            if let Some(deferred_at) = deferred_at {
                deferred_since.insert(pr.number, deferred_at);
            }
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
                    deferred: false,
                }),
            }
        }
        // A defer only survives if nothing has happened since: the top reason's
        // `since` must not be newer than the moment it was deferred.
        for item in &mut items {
            item.deferred = deferred_since
                .get(&item.pr.number)
                .is_some_and(|&deferred_at| item.top.since <= deferred_at);
        }
        items.sort_by(|a, b| {
            (a.deferred, a.top.priority, a.top.since, a.pr.number).cmp(&(
                b.deferred,
                b.top.priority,
                b.top.since,
                b.pr.number,
            ))
        });
        Ok(items)
    }

    /// Tracked, open PRs with no attention: seen and understood, waiting on
    /// someone else. Ordered by number.
    pub fn waiting(&self, repo_id: i64) -> Result<Vec<TrackedPr>> {
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason
            FROM prs p
            WHERE p.repo_id = ?1 AND p.state = 'OPEN' AND p.tracked_reason IS NOT NULL
              AND NOT EXISTS (
                SELECT 1 FROM attention a
                WHERE a.repo_id = p.repo_id AND a.pr_number = p.number
              )
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map(params![repo_id], |row| {
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
    pub fn show(&self, repo_id: i64, number: u64) -> Result<Option<PrShow>> {
        let base = self
            .conn
            .query_row(
                &format!(
                    "SELECT {PR_COLUMNS}, p.tracked_reason FROM prs p \
                     WHERE p.repo_id = ?1 AND p.number = ?2"
                ),
                params![repo_id, number as i64],
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

        let my_state = self.my_state(repo_id, number)?;
        let threads = self.threads(repo_id, number)?;
        let reviewers = self.reviewers(repo_id, number)?;
        let attention = self.attention(repo_id, number)?;
        Ok(Some(PrShow {
            pr,
            tracked_reason,
            my_state,
            threads,
            reviewers,
            attention,
        }))
    }

    /// A PR's reviewers, most recently submitted first.
    fn reviewers(&self, repo_id: i64, number: u64) -> Result<Vec<ReviewerVerdict>> {
        let mut stmt = self.conn.prepare(
            "SELECT login, verdict, submitted_at FROM reviewers \
             WHERE repo_id = ?1 AND pr_number = ?2 ORDER BY submitted_at DESC",
        )?;
        let rows = stmt
            .query_map(params![repo_id, number as i64], row_to_reviewer)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A PR's review threads, ordered by id for stability.
    fn threads(&self, repo_id: i64, number: u64) -> Result<Vec<ThreadState>> {
        let mut stmt = self.conn.prepare(
            r"
            SELECT thread_id, i_own, is_resolved, resolved_by, last_comment_author,
                   last_comment_at, my_last_comment_at
            FROM threads WHERE repo_id = ?1 AND pr_number = ?2 ORDER BY thread_id
            ",
        )?;
        let rows = stmt
            .query_map(params![repo_id, number as i64], row_to_thread)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A PR's attention rows, most-urgent first.
    fn attention(&self, repo_id: i64, number: u64) -> Result<Vec<AttentionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT reason, detail, since FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
        )?;
        let mut rows = stmt
            .query_map(params![repo_id, number as i64], |row| {
                attention_from_row(row, 0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by_key(|a| (a.priority, a.since));
        Ok(rows)
    }
}

/// Every repo (already known to this ledger file) that has PR `number`. Lets
/// a command that names a bare number but has no config to consult — `done`,
/// `mute`, `show`, ... are ledger-only — resolve which repo it belongs to,
/// without needing a `RepoKey` up front the way [`Ledger::ensure_repo`] does.
/// `Ok(&[])` both when the file doesn't exist yet (nothing has ever been
/// synced) and when it exists but has never heard of this number — either
/// way, the caller's answer is the same "not in the ledger".
///
/// Doesn't create the file: unlike `Ledger::open`, a pure lookup should never
/// have the side effect of creating a database that didn't exist.
pub fn repos_with_pr(path: &std::path::Path, number: u64) -> Result<Vec<RepoKey>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut conn =
        Connection::open(path).with_context(|| format!("opening ledger {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .context("enabling foreign keys")?;
    schema::migrate(&mut conn)?;
    let mut stmt = conn.prepare(
        "SELECT r.host, r.owner, r.name FROM prs p \
         JOIN repos r ON r.id = p.repo_id WHERE p.number = ?1",
    )?;
    let rows = stmt
        .query_map(params![number as i64], |row| {
            Ok(RepoKey {
                host: row.get(0)?,
                owner: row.get(1)?,
                name: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Insert or update one PR row against `conn` (a connection or an open
/// transaction), merging its tracked reason with any already stored. Returns
/// `true` if the row was newly inserted.
fn upsert_row(
    conn: &Connection,
    repo_id: i64,
    pr: &PrSnapshot,
    reason: Option<&TrackedReason>,
    now: Timestamp,
) -> Result<bool> {
    let merged = merge_reason(tracked_reason(conn, repo_id, pr.number)?.as_deref(), reason);
    let is_new = existing_row(conn, repo_id, pr.number)?.is_none();

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
          repo_id, number, title, author, author_association, head_sha, is_draft,
          state, updated_at, labels, milestone, files, files_truncated,
          tracked_reason, first_seen_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)
        ON CONFLICT(repo_id, number) DO UPDATE SET
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
            repo_id,
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

fn set_meta_row(conn: &Connection, repo_id: i64, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_meta (repo_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
        params![repo_id, key, value],
    )
    .with_context(|| format!("writing sync_meta {key}"))?;
    Ok(())
}

fn tracked_reason(conn: &Connection, repo_id: i64, number: u64) -> Result<Option<String>> {
    conn.query_row(
        "SELECT tracked_reason FROM prs WHERE repo_id = ?1 AND number = ?2",
        params![repo_id, number as i64],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .with_context(|| format!("reading tracked_reason for #{number}"))
    .map(Option::flatten)
}

fn existing_row(conn: &Connection, repo_id: i64, number: u64) -> Result<Option<u64>> {
    conn.query_row(
        "SELECT number FROM prs WHERE repo_id = ?1 AND number = ?2",
        params![repo_id, number as i64],
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
    let deferred_at: Option<String> = row.get(6)?;
    let done_at: Option<String> = row.get(7)?;
    Ok(MyState {
        last_reviewed_sha: row.get(0)?,
        last_verdict: verdict.as_deref().and_then(Verdict::from_wire),
        last_action_at: last_action_at.as_deref().map(parse_ts).transpose()?,
        done_sha: row.get(3)?,
        snoozed_until: snoozed_until.as_deref().map(parse_ts).transpose()?,
        muted: row.get::<_, i64>(5)? != 0,
        deferred_at: deferred_at.as_deref().map(parse_ts).transpose()?,
        done_at: done_at.as_deref().map(parse_ts).transpose()?,
    })
}

fn row_to_reviewer(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewerVerdict> {
    let verdict_str: String = row.get(1)?;
    let verdict = Verdict::from_wire(&verdict_str)
        .ok_or_else(|| decode_err(format!("bad verdict {verdict_str:?}").into()))?;
    Ok(ReviewerVerdict {
        login: row.get(0)?,
        verdict,
        at: parse_ts(&row.get::<_, String>(2)?)?,
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

/// Write only the forge-derived third of `my_state` — `last_reviewed_sha`,
/// `last_verdict`, `last_action_at` — the fields GitHub itself reports and
/// [`commit_detail`](Ledger::commit_detail) overlays fresh on every sync.
/// Never writes `done_sha`/`snoozed_until`/`muted`/`deferred_at`/`done_at`,
/// even though `s` (as read by the caller) carries whatever those happened to
/// be at read time: writing them back here would risk clobbering a
/// concurrent `reviewq done`/`snooze`/`mute`/`defer` with a stale copy. Each
/// of those has its own targeted setter (`Ledger::set_done`, etc.) that writes
/// only its own column, for the same reason in reverse.
fn write_forge_state(conn: &Connection, repo_id: i64, number: u64, s: &MyState) -> Result<()> {
    conn.execute(
        r"
        INSERT INTO my_state (repo_id, number, last_reviewed_sha, last_verdict, last_action_at)
        VALUES (?1, ?2, ?3, ?4, ?5)
        ON CONFLICT(repo_id, number) DO UPDATE SET
          last_reviewed_sha=excluded.last_reviewed_sha,
          last_verdict=excluded.last_verdict,
          last_action_at=excluded.last_action_at
        ",
        params![
            repo_id,
            number as i64,
            s.last_reviewed_sha,
            s.last_verdict.map(|v| v.as_str()),
            s.last_action_at.map(|t| t.to_string()),
        ],
    )
    .with_context(|| format!("writing forge-derived my_state for #{number}"))?;
    Ok(())
}

fn replace_threads(
    conn: &Connection,
    repo_id: i64,
    number: u64,
    threads: &[ThreadState],
) -> Result<()> {
    conn.execute(
        "DELETE FROM threads WHERE repo_id = ?1 AND pr_number = ?2",
        params![repo_id, number as i64],
    )?;
    for t in threads {
        conn.execute(
            r"
            INSERT INTO threads (
              thread_id, repo_id, pr_number, i_own, is_resolved, resolved_by,
              last_comment_author, last_comment_at, my_last_comment_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
            ",
            params![
                t.thread_id,
                repo_id,
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

fn replace_reviewers(
    conn: &Connection,
    repo_id: i64,
    number: u64,
    reviewers: &[ReviewerVerdict],
) -> Result<()> {
    conn.execute(
        "DELETE FROM reviewers WHERE repo_id = ?1 AND pr_number = ?2",
        params![repo_id, number as i64],
    )?;
    for r in reviewers {
        conn.execute(
            "INSERT INTO reviewers (repo_id, pr_number, login, verdict, submitted_at) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                repo_id,
                number as i64,
                r.login,
                r.verdict.as_str(),
                r.at.to_string()
            ],
        )
        .with_context(|| format!("writing reviewer {} for #{number}", r.login))?;
    }
    Ok(())
}

fn replace_attention(
    conn: &Connection,
    repo_id: i64,
    number: u64,
    attention: &[Attention],
) -> Result<()> {
    conn.execute(
        "DELETE FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
        params![repo_id, number as i64],
    )?;
    for a in attention {
        conn.execute(
            "INSERT INTO attention (repo_id, pr_number, reason, detail, since) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                repo_id,
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

    fn repo() -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: "apache".into(),
            name: "airflow".into(),
        }
    }

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

    /// A ready-to-use ledger and the id of one repo already registered in it —
    /// what almost every test below needs and doesn't care to set up itself.
    fn ledger_with_repo() -> (Ledger, i64) {
        let ledger = Ledger::open_in_memory().unwrap();
        let repo_id = ledger.ensure_repo(&repo()).unwrap();
        (ledger, repo_id)
    }

    #[test]
    fn repos_lists_every_registered_repo() {
        let ledger = Ledger::open_in_memory().unwrap();
        assert!(ledger.repos().unwrap().is_empty());

        let other = RepoKey {
            host: "github.com".into(),
            owner: "someone".into(),
            name: "else".into(),
        };
        let a = ledger.ensure_repo(&repo()).unwrap();
        let b = ledger.ensure_repo(&other).unwrap();

        let mut got = ledger.repos().unwrap();
        got.sort_by_key(|(id, _)| *id);
        assert_eq!(got, vec![(a, repo()), (b, other)]);
    }

    #[test]
    fn two_repos_on_the_same_database_dont_collide_on_pr_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = Ledger::open(&path).unwrap();
        let a = ledger.ensure_repo(&repo()).unwrap();
        let b = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "someone".into(),
                name: "else".into(),
            })
            .unwrap();

        ledger
            .upsert_pr(
                a,
                &pr(1),
                Some(TrackedReason::Interest("label x".into())),
                now(),
            )
            .unwrap();
        ledger.upsert_pr(b, &pr(1), None, now()).unwrap();
        ledger.set_muted(b, 1, true).unwrap();

        assert_eq!(ledger.list_tracked(a).unwrap().len(), 1);
        assert!(
            ledger.list_tracked(b).unwrap().is_empty(),
            "the other repo's PR #1 was untracked, independently of a's"
        );
        assert!(
            !ledger.my_state(a, 1).unwrap().muted,
            "each repo's my_state is independent"
        );
        assert!(ledger.my_state(b, 1).unwrap().muted);
    }

    #[test]
    fn ensure_repo_adopts_a_pre_v4_placeholder_once() {
        let ledger = Ledger::open_in_memory().unwrap();
        // What migration 4 leaves behind on a real upgrade: a blank
        // placeholder row, FK-referenced by pre-existing data.
        ledger
            .conn
            .execute(
                "INSERT INTO repos (id, host, owner, name) VALUES (1, '', '', '')",
                [],
            )
            .unwrap();
        ledger
            .conn
            .execute(
                "INSERT INTO prs (repo_id, number, title, author, author_association, \
                 head_sha, is_draft, state, updated_at, labels, first_seen_at) \
                 VALUES (1, 1, 'a PR', 'octocat', 'CONTRIBUTOR', 'abc123', 0, 'OPEN', \
                 '2026-08-05T12:00:00Z', '[]', '2026-08-05T12:00:00Z')",
                [],
            )
            .unwrap();
        ledger
            .conn
            .execute(
                "INSERT INTO my_state (repo_id, number, muted) VALUES (1, 1, 1)",
                [],
            )
            .unwrap();

        let id = ledger.ensure_repo(&repo()).unwrap();
        assert_eq!(
            id, 1,
            "adopted the placeholder rather than creating a new row"
        );
        assert!(
            ledger.my_state(id, 1).unwrap().muted,
            "the legacy row's state survives under the adopted id"
        );

        // A second, different repo just gets a normal new row.
        let other = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "someone".into(),
                name: "else".into(),
            })
            .unwrap();
        assert_ne!(other, id);

        // Calling it again for the first repo is a stable no-op.
        assert_eq!(ledger.ensure_repo(&repo()).unwrap(), id);
    }

    #[test]
    fn repos_with_pr_finds_the_owning_repo_without_a_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let other = RepoKey {
            host: "github.com".into(),
            owner: "someone".into(),
            name: "else".into(),
        };

        assert!(repos_with_pr(&path, 1).unwrap().is_empty());

        let ledger = Ledger::open(&path).unwrap();
        let a = ledger.ensure_repo(&repo()).unwrap();
        ledger.upsert_pr(a, &pr(1), None, now()).unwrap();
        let b = ledger.ensure_repo(&other).unwrap();
        ledger.upsert_pr(b, &pr(2), None, now()).unwrap();

        assert_eq!(repos_with_pr(&path, 1).unwrap(), vec![repo()]);
        assert_eq!(repos_with_pr(&path, 2).unwrap(), vec![other]);
        assert!(repos_with_pr(&path, 999).unwrap().is_empty());
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
        let (ledger, repo_id) = ledger_with_repo();
        let reason = TrackedReason::Interest("label area:task-sdk".into());

        assert!(
            ledger
                .upsert_pr(repo_id, &pr(1), Some(reason.clone()), now())
                .unwrap()
        );
        assert!(
            !ledger
                .upsert_pr(repo_id, &pr(1), Some(reason), now())
                .unwrap()
        );

        let tracked = ledger.list_tracked(repo_id).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].pr.number, 1);
        assert_eq!(tracked[0].pr.milestone.as_deref(), Some("3.2.0"));
        assert_eq!(tracked[0].pr.updated_at, now());
        assert_eq!(tracked[0].tracked_reason, "interest: label area:task-sdk");
    }

    #[test]
    fn commit_sweep_page_persists_prs_and_cursor_atomically_and_resumes() {
        let (ledger, repo_id) = ledger_with_repo();
        let page = vec![
            (
                pr(1),
                Some(TrackedReason::Interest("label area:task-sdk".into())),
            ),
            (pr(2), None),
        ];

        let new = ledger
            .commit_sweep_page(
                repo_id,
                &page,
                now(),
                "last_sync_at",
                "2026-08-05T12:00:00Z",
            )
            .unwrap();
        assert_eq!(new, 2, "both PRs were newly inserted");
        // ...but only the one with a reason is tracked.
        assert_eq!(ledger.counts(repo_id).unwrap(), (1, 2));
        assert_eq!(
            ledger.get_meta(repo_id, "last_sync_at").unwrap().as_deref(),
            Some("2026-08-05T12:00:00Z")
        );

        // Re-committing the same page (a resume over the overlap) is a no-op for
        // the "new" count and just advances the cursor.
        let again = ledger
            .commit_sweep_page(
                repo_id,
                &page,
                now(),
                "last_sync_at",
                "2026-08-05T12:05:00Z",
            )
            .unwrap();
        assert_eq!(again, 0);
        assert_eq!(
            ledger.get_meta(repo_id, "last_sync_at").unwrap().as_deref(),
            Some("2026-08-05T12:05:00Z")
        );
    }

    #[test]
    fn untracked_prs_are_stored_but_not_listed() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();
        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());
        assert_eq!(ledger.counts(repo_id).unwrap(), (0, 1));
    }

    #[test]
    fn first_seen_at_survives_a_later_upsert() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                None,
                "2026-01-01T00:00:00Z".parse().unwrap(),
            )
            .unwrap();
        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();
        let seen: String = ledger
            .conn
            .query_row(
                "SELECT first_seen_at FROM prs WHERE repo_id = ?1 AND number = 1",
                params![repo_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(seen, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn involvement_beats_interest_and_is_not_downgraded() {
        let (ledger, repo_id) = ledger_with_repo();
        let interest = || TrackedReason::Interest("label area:task-sdk".into());
        ledger
            .upsert_pr(repo_id, &pr(1), Some(interest()), now())
            .unwrap();

        // The involvement search upserts the same PR as involved.
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Involved("review_requested".into())),
                now(),
            )
            .unwrap();
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: review_requested"
        );

        // A later sweep re-asserting interest must not clobber involvement.
        ledger
            .upsert_pr(repo_id, &pr(1), Some(interest()), now())
            .unwrap();
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: review_requested"
        );
    }

    #[test]
    fn meta_round_trips() {
        let (ledger, repo_id) = ledger_with_repo();
        assert_eq!(ledger.get_meta(repo_id, "cursor").unwrap(), None);
        ledger
            .set_meta(repo_id, "cursor", "2026-08-05T12:00:00Z")
            .unwrap();
        ledger
            .set_meta(repo_id, "cursor", "2026-08-05T13:00:00Z")
            .unwrap();
        assert_eq!(
            ledger.get_meta(repo_id, "cursor").unwrap().as_deref(),
            Some("2026-08-05T13:00:00Z")
        );
    }

    #[test]
    fn truncated_untracked_are_counted() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut p = pr(1);
        p.files = Some(vec!["docs/x.rst".into()]);
        p.files_truncated = true;
        ledger.upsert_pr(repo_id, &p, None, now()).unwrap();
        assert_eq!(ledger.count_truncated_untracked(repo_id).unwrap(), 1);
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

    fn track(ledger: &Ledger, repo_id: i64, p: &PrSnapshot) {
        ledger
            .upsert_pr(
                repo_id,
                p,
                Some(TrackedReason::Interest("label area:task-sdk".into())),
                now(),
            )
            .unwrap();
    }

    #[test]
    fn commit_detail_writes_only_the_forge_derived_fields() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        let state = MyState {
            last_reviewed_sha: Some("deadbeef".into()),
            last_verdict: Some(Verdict::ChangesRequested),
            last_action_at: Some(ts("2026-08-04T10:00:00Z")),
            // A real caller (sync) would have read these off a prior state
            // before overlaying the three forge fields above; commit_detail
            // must not write them back regardless of what's in `state`.
            done_sha: Some("cafebabe".into()),
            snoozed_until: Some(ts("2026-08-09T00:00:00Z")),
            muted: true,
            deferred_at: Some(ts("2026-08-06T00:00:00Z")),
            done_at: Some(ts("2026-08-06T00:00:00Z")),
        };
        ledger
            .commit_detail(repo_id, 1, &state, &[], &[], &[], now())
            .unwrap();

        let stored = ledger.my_state(repo_id, 1).unwrap();
        assert_eq!(stored.last_reviewed_sha, state.last_reviewed_sha);
        assert_eq!(stored.last_verdict, state.last_verdict);
        assert_eq!(stored.last_action_at, state.last_action_at);
        assert_eq!(stored.done_sha, None);
        assert_eq!(stored.snoozed_until, None);
        assert!(!stored.muted);
        assert_eq!(stored.deferred_at, None);
        assert_eq!(stored.done_at, None);
    }

    #[test]
    fn commit_detail_never_clobbers_a_concurrent_user_action() {
        // The scenario the M4 review flagged: `reviewq done` sets `done_at`,
        // then a `sync` that was already mid-flight (and so read `my_state`
        // before `done` ran) commits its own forge-derived overlay. The
        // done_at set moments ago must survive that commit untouched.
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .set_done(repo_id, 1, "head0000", ts("2026-08-05T10:00:00Z"))
            .unwrap();

        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState {
                    last_action_at: Some(ts("2026-08-05T09:00:00Z")),
                    ..Default::default()
                },
                &[],
                &[],
                &[],
                now(),
            )
            .unwrap();

        let stored = ledger.my_state(repo_id, 1).unwrap();
        assert_eq!(stored.done_sha.as_deref(), Some("head0000"));
        assert_eq!(stored.done_at, Some(ts("2026-08-05T10:00:00Z")));
        assert_eq!(stored.last_action_at, Some(ts("2026-08-05T09:00:00Z")));
    }

    #[test]
    fn set_done_touches_only_its_own_columns() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger.set_muted(repo_id, 1, true).unwrap();

        ledger
            .set_done(repo_id, 1, "abc123", ts("2026-08-05T10:00:00Z"))
            .unwrap();

        let stored = ledger.my_state(repo_id, 1).unwrap();
        assert_eq!(stored.done_sha.as_deref(), Some("abc123"));
        assert_eq!(stored.done_at, Some(ts("2026-08-05T10:00:00Z")));
        assert!(stored.muted, "an unrelated field must survive");
    }

    #[test]
    fn set_snoozed_until_and_set_deferred_at_round_trip() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));

        ledger
            .set_snoozed_until(repo_id, 1, ts("2026-08-09T00:00:00Z"))
            .unwrap();
        assert_eq!(
            ledger.my_state(repo_id, 1).unwrap().snoozed_until,
            Some(ts("2026-08-09T00:00:00Z"))
        );

        ledger
            .set_deferred_at(repo_id, 1, Some(ts("2026-08-06T00:00:00Z")))
            .unwrap();
        assert_eq!(
            ledger.my_state(repo_id, 1).unwrap().deferred_at,
            Some(ts("2026-08-06T00:00:00Z"))
        );

        ledger.set_deferred_at(repo_id, 1, None).unwrap();
        assert_eq!(ledger.my_state(repo_id, 1).unwrap().deferred_at, None);
    }

    #[test]
    fn my_state_defaults_when_absent() {
        let (ledger, repo_id) = ledger_with_repo();
        assert_eq!(ledger.my_state(repo_id, 999).unwrap(), MyState::default());
    }

    #[test]
    fn commit_detail_replaces_threads_and_attention_wholesale() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));

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
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[thread],
                &[],
                &first,
                now(),
            )
            .unwrap();

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.threads.len(), 1);
        assert_eq!(show.attention.len(), 1);
        assert_eq!(
            show.threads[0].last_comment_author.as_deref(),
            Some("kaxil")
        );

        // A second detail pass with nothing wipes the earlier rows rather than
        // accumulating them.
        ledger
            .commit_detail(repo_id, 1, &MyState::default(), &[], &[], &[], now())
            .unwrap();
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert!(show.threads.is_empty());
        assert!(show.attention.is_empty());
    }

    #[test]
    fn commit_detail_replaces_reviewers_wholesale() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));

        let approved = ReviewerVerdict {
            login: "kaxil".into(),
            verdict: Verdict::Approved,
            at: ts("2026-08-05T08:00:00Z"),
        };
        let changes_requested = ReviewerVerdict {
            login: "uranusjr".into(),
            verdict: Verdict::ChangesRequested,
            at: ts("2026-08-05T09:00:00Z"),
        };
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[approved.clone(), changes_requested.clone()],
                &[],
                now(),
            )
            .unwrap();

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        // Most recently submitted first.
        assert_eq!(show.reviewers, vec![changes_requested, approved]);

        // A second detail pass with nobody left approving replaces the row
        // rather than accumulating alongside it.
        ledger
            .commit_detail(repo_id, 1, &MyState::default(), &[], &[], &[], now())
            .unwrap();
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .reviewers
                .is_empty()
        );
    }

    #[test]
    fn queue_orders_by_priority_then_age_and_keeps_the_top_reason() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        track(&ledger, repo_id, &pr(2));

        // #1 holds two reasons; the mention (priority 1) must set its position.
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
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
                repo_id,
                2,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::NeedsFirstLook { rule: "y".into() },
                    "2026-07-01T00:00:00Z",
                )],
                now(),
            )
            .unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].pr.number, 1);
        assert_eq!(queue[0].top.reason, "mention");
        assert_eq!(queue[0].top.priority, 1);
        assert_eq!(queue[1].pr.number, 2);
    }

    #[test]
    fn waiting_is_tracked_open_prs_without_attention() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        track(&ledger, repo_id, &pr(2));
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
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

        let waiting = ledger.waiting(repo_id).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].pr.number, 2);
    }

    #[test]
    fn prs_needing_detail_selects_never_or_stale_synced() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        track(&ledger, repo_id, &pr(2));
        // #2 synced after its updatedAt: fresh, so excluded.
        ledger
            .commit_detail(
                repo_id,
                2,
                &MyState::default(),
                &[],
                &[],
                &[],
                ts("2026-08-06T00:00:00Z"),
            )
            .unwrap();

        let need = ledger.prs_needing_detail(repo_id, false).unwrap();
        assert_eq!(need.len(), 1);
        assert_eq!(need[0].pr.number, 1);
        assert_eq!(need[0].pr.head_sha, "abc123");
    }

    #[test]
    fn merged_prs_included_in_detail_only_when_opted_in() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, repo_id, &merged);

        assert!(
            ledger
                .prs_needing_detail(repo_id, false)
                .unwrap()
                .is_empty(),
            "merged PR skipped without the opt-in"
        );
        assert_eq!(
            ledger.prs_needing_detail(repo_id, true).unwrap().len(),
            1,
            "merged PR fetched with the opt-in"
        );
    }

    fn mention(by: &str, since: &str) -> Attention {
        attn(AttentionReason::Mention { by: by.into() }, since)
    }

    #[test]
    fn a_closed_pr_is_off_the_queue() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut closed = pr(1);
        closed.state = PrState::Closed;
        track(&ledger, repo_id, &closed);
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert!(ledger.waiting(repo_id).unwrap().is_empty());
    }

    #[test]
    fn a_merged_pr_with_attention_is_on_the_queue() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, repo_id, &merged);
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].pr.number, 1);
        // A merged PR is never "waiting" — waiting is the open-and-idle bucket.
        assert!(ledger.waiting(repo_id).unwrap().is_empty());
    }

    #[test]
    fn clear_archived_attention_respects_the_opt_in() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        track(&ledger, repo_id, &merged);
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();

        // With the opt-in, merged attention is kept.
        ledger.clear_archived_attention(repo_id, true).unwrap();
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);

        // Without it, merged attention is swept away.
        ledger.clear_archived_attention(repo_id, false).unwrap();
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert!(
            ledger
                .show(repo_id, 1)
                .unwrap()
                .unwrap()
                .attention
                .is_empty()
        );
    }

    #[test]
    fn set_muted_writes_without_touching_threads_or_attention() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();

        ledger.set_muted(repo_id, 1, true).unwrap();
        assert!(ledger.my_state(repo_id, 1).unwrap().muted);
        // Unlike clear_attention, a bare state write leaves attention alone —
        // the command layer calls both, but they're independent operations.
        assert_eq!(ledger.show(repo_id, 1).unwrap().unwrap().attention.len(), 1);
    }

    #[test]
    fn clear_attention_drops_only_the_named_pr() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        track(&ledger, repo_id, &pr(2));
        for number in [1, 2] {
            ledger
                .commit_detail(
                    repo_id,
                    number,
                    &MyState::default(),
                    &[],
                    &[],
                    &[mention("potiuk", "2026-08-05T09:00:00Z")],
                    now(),
                )
                .unwrap();
        }

        ledger.clear_attention(repo_id, 1).unwrap();
        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].pr.number, 2);
    }

    #[test]
    fn clear_done_attention_preserves_review_requested() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[
                    mention("potiuk", "2026-08-05T09:00:00Z"),
                    attn(
                        AttentionReason::ReviewRequested { team: None },
                        "2026-08-05T09:00:00Z",
                    ),
                ],
                now(),
            )
            .unwrap();

        ledger.clear_done_attention(repo_id, 1).unwrap();
        let attention = ledger.show(repo_id, 1).unwrap().unwrap().attention;
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].reason, "review_requested");
    }

    #[test]
    fn track_sets_involved_manual_and_does_not_downgrade() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();
        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());

        assert!(ledger.track(repo_id, 1).unwrap());
        let tracked = ledger.list_tracked(repo_id).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].tracked_reason, "involved: manual");

        // A later sweep re-asserting interest must not clobber it.
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Interest("label area:task-sdk".into())),
                now(),
            )
            .unwrap();
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: manual"
        );
    }

    #[test]
    fn track_is_a_no_op_on_an_already_tracked_pr() {
        let (ledger, repo_id) = ledger_with_repo();
        // Tracked by interest, not by track() — this is the case that must
        // never be silently downgraded to `involved: manual`, which would
        // permanently drop needs_first_look (interest_detail only strips an
        // `interest:` prefix, and merge_reason never demotes `involved:` back).
        track(&ledger, repo_id, &pr(1));

        assert!(
            !ledger.track(repo_id, 1).unwrap(),
            "already tracked — a no-op"
        );
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "interest: label area:task-sdk"
        );
    }

    #[test]
    fn a_deferred_pr_sorts_after_every_non_deferred_item_regardless_of_priority() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        track(&ledger, repo_id, &pr(2));

        // #1 holds the most urgent reason there is...
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();
        // ...but gets deferred after that mention fired.
        ledger
            .set_deferred_at(repo_id, 1, Some(ts("2026-08-05T10:00:00Z")))
            .unwrap();
        // #2 only needs a first look — the least urgent reason — and stays put.
        ledger
            .commit_detail(
                repo_id,
                2,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::NeedsFirstLook { rule: "y".into() },
                    "2026-07-01T00:00:00Z",
                )],
                now(),
            )
            .unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].pr.number, 2, "the deferred PR sorts last");
        assert!(!queue[0].deferred);
        assert_eq!(queue[1].pr.number, 1);
        assert!(queue[1].deferred);
    }

    #[test]
    fn a_defer_clears_itself_once_something_new_happens() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                now(),
            )
            .unwrap();
        ledger
            .set_deferred_at(repo_id, 1, Some(ts("2026-08-05T10:00:00Z")))
            .unwrap();
        assert!(ledger.queue(repo_id).unwrap()[0].deferred);

        // A fresh sync reclassifies with a newer mention — after the defer.
        ledger
            .commit_detail(
                repo_id,
                1,
                &ledger.my_state(repo_id, 1).unwrap(),
                &[],
                &[],
                &[mention("potiuk", "2026-08-05T11:00:00Z")],
                now(),
            )
            .unwrap();
        assert!(!ledger.queue(repo_id).unwrap()[0].deferred);
    }
}
