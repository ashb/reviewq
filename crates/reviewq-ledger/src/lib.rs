//! The SQLite ledger.
//!
//! A thin, typed wrapper over `rusqlite`. It owns the schema and migrations and
//! trades in `reviewq-core` snapshot types; nothing above it writes SQL. The
//! sync API is synchronous, which is fine for a CLI.

mod schema;

use std::collections::BTreeMap;

use jiff::Timestamp;
use reviewq_core::model::{
    Attention, AttentionReason, MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict,
};
use rusqlite::types::Type;
use rusqlite::{Connection, Error::FromSqlConversionFailure, OptionalExtension, params};

pub use schema::SCHEMA_VERSION;

/// What can go wrong in the ledger.
///
/// Typed rather than an opaque string because two of these change what a caller
/// should *do*: a ledger from a newer reviewq needs the binary upgraded, and a
/// busy one needs trying again. An interface handed one prose blob can only print
/// it and hope the reader knows which.
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    /// The file was written by a build that knows more migrations than this one.
    ///
    /// Never run: migrating *down* is not defined, so the alternative to refusing
    /// is corrupting a database the other build still expects to read.
    #[error(
        "the ledger was written by a newer reviewq (this build knows {SCHEMA_VERSION} \
         migrations) — upgrade reviewq, or point $REVIEWQ_DB at another file"
    )]
    FromTheFuture,

    /// Somebody else held the write lock for longer than the busy timeout.
    #[error(
        "another reviewq held the ledger's write lock for more than {}s — it is \
         probably mid-sync; try again",
        BUSY_TIMEOUT.as_secs()
    )]
    Busy {
        /// What SQLite reported.
        #[source]
        source: rusqlite::Error,
    },

    /// A PR the caller expected to be there isn't.
    #[error("#{number} is not stored in the ledger")]
    NotStored {
        /// The PR number asked for.
        number: u64,
    },

    /// A stored value could not be read back as what it should be. The ledger
    /// wrote it, so this means the file has been altered or a format changed
    /// without a migration.
    #[error("the ledger holds a {what} it cannot read back")]
    Corrupt {
        /// What was being decoded.
        what: String,
        /// Why it failed.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Something that was about to be stored could not be encoded. Our own data,
    /// so this is a bug rather than a bad database.
    #[error("could not encode {what} for storage")]
    Encode {
        /// What was being encoded.
        what: String,
        /// Why it failed.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The ledger file itself could not be reached.
    #[error("{doing}")]
    Io {
        /// What was being attempted on disk.
        doing: String,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// Anything else SQLite refused, with what was being attempted.
    #[error("{doing}")]
    Sql {
        /// What the ledger was doing.
        doing: String,
        /// What SQLite reported.
        #[source]
        source: rusqlite::Error,
    },
}

impl From<rusqlite::Error> for LedgerError {
    /// Classify as it converts, so a `?` anywhere in the crate yields [`Busy`]
    /// rather than burying it in a message only a human can read.
    ///
    /// [`Busy`]: LedgerError::Busy
    fn from(source: rusqlite::Error) -> Self {
        if is_busy(&source) {
            Self::Busy { source }
        } else {
            Self::Sql {
                doing: "talking to the ledger".to_string(),
                source,
            }
        }
    }
}

/// Whether SQLite gave up waiting for the write lock.
fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    )
}

/// Every fallible operation here fails with a [`LedgerError`].
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Say what a SQLite failure was for, keeping the busy case distinguishable.
trait Doing<T> {
    /// Wrap a failure with what was being attempted.
    fn doing(self, what: impl Into<String>) -> Result<T>;
}

impl<T> Doing<T> for rusqlite::Result<T> {
    fn doing(self, what: impl Into<String>) -> Result<T> {
        self.map_err(|source| {
            if is_busy(&source) {
                LedgerError::Busy { source }
            } else {
                LedgerError::Sql {
                    doing: what.into(),
                    source,
                }
            }
        })
    }
}

/// Say what was being encoded, when it cannot be turned into storage.
///
/// There is no matching `decoding`: a value read back is decoded inside a
/// `query_map` closure, which must fail with `rusqlite::Error` — so those go
/// through [`decode_err`] and arrive here as [`LedgerError::Corrupt`] via the
/// conversion instead.
trait Encoding<T> {
    /// Wrap a failure to encode something for storage.
    fn encoding(self, what: impl Into<String>) -> Result<T>;
}

impl<T, E> Encoding<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn encoding(self, what: impl Into<String>) -> Result<T> {
        self.map_err(|source| LedgerError::Encode {
            what: what.into(),
            source: Box::new(source),
        })
    }
}

/// Say what was being attempted on the file itself.
trait OnDisk<T> {
    /// Wrap an IO failure with what it was for.
    fn on_disk(self, doing: impl Into<String>) -> Result<T>;
}

impl<T> OnDisk<T> for std::io::Result<T> {
    fn on_disk(self, doing: impl Into<String>) -> Result<T> {
        self.map_err(|source| LedgerError::Io {
            doing: doing.into(),
            source,
        })
    }
}

/// How long a connection waits for whoever holds the write lock before giving
/// up with `SQLITE_BUSY`. A single write is short; a whole detail pass runs
/// many back to back, so a reader that arrives mid-sync may need to wait out
/// several.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The pragmas every connection needs, however it was opened.
///
/// WAL matters as soon as two connections exist at once — a long-running
/// reader alongside a sync that writes. Under the default rollback journal a
/// committing writer takes an exclusive lock over the whole database, and with
/// no busy handler installed a concurrent reader fails immediately rather than
/// waiting; WAL lets readers proceed against the last committed snapshot
/// instead. `journal_mode` persists in the file once set, so this only does
/// real work the first time. An in-memory database can't use WAL and stays
/// `memory`, which is harmless — nothing else ever opens it.
fn prepare_conn(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .doing("enabling foreign keys")?;
    // `PRAGMA journal_mode` reports the mode it settled on as a result row,
    // which plain `pragma_update` rejects; this variant tolerates one.
    conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
        .doing("enabling WAL")?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .doing("setting the busy timeout")?;
    Ok(())
}

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

/// An item from a whole-database read, tagged with the repo it came from. The
/// per-repo reads return bare items because their caller already knows the
/// `repo_id` it asked about; a merged read spanning every repo has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    /// The repo the item belongs to.
    pub repo: RepoKey,
    /// That repo's id, carried because the read already had it.
    ///
    /// Without it a caller holding one of these had to ask
    /// [`ensure_repo`](Ledger::ensure_repo) for the id back — a *write*, on what
    /// is otherwise a read path, once per selection move in the interface.
    pub repo_id: i64,
    /// The item itself.
    pub item: T,
}

/// What [`Ledger::commit_detail`] did with the detail it was offered.
///
/// `must_use` because a caller that drops this has silently accepted that its
/// fetch may have been discarded, which is exactly the case worth reporting.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Committed {
    /// Stored.
    Applied,
    /// Dropped, because the PR already holds a detail fetched later than this
    /// one. Applying it would have moved the PR backwards.
    Superseded {
        /// The watermark already stored, for a caller that wants to say so.
        stored: String,
    },
}

impl Committed {
    /// Panic unless the detail was stored.
    ///
    /// For a caller that has just created the row itself and so cannot be racing
    /// anybody — a test fixture, in practice. Anything reading from the forge
    /// should handle [`Superseded`](Self::Superseded) instead, since two fetches
    /// of one PR really can overlap.
    pub fn expect_applied(self) {
        if let Self::Superseded { stored } = self {
            panic!("expected the detail to be stored, but #? already holds {stored}");
        }
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
    /// Matched an interest rule.
    Interest {
        /// The bare match, e.g. `label area:x`.
        rule: String,
        /// The matching rule asked to keep this PR reviewable after it merges.
        after_merge: bool,
    },
    /// A relationship names me; carries the reason, e.g. `review_requested`.
    Involved(String),
}

impl TrackedReason {
    fn rank(&self) -> u8 {
        match self {
            Self::Interest { .. } => 1,
            Self::Involved(_) => 2,
        }
    }

    /// The string stored in `tracked_reason` and shown to the user.
    pub fn render(&self) -> String {
        match self {
            Self::Interest { rule, .. } => format!("interest: {rule}"),
            Self::Involved(r) => format!("involved: {r}"),
        }
    }
}

/// Which side of a mute a read wants.
///
/// A mute is a statement about what you want shown, so it belongs here rather
/// than in the state machine — which means every queue read has to say which of
/// the two lists it is asking for, and cannot get them mixed up by passing a
/// bare `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Muted {
    /// The queue proper: everything you have not silenced.
    Hidden,
    /// Only what you have silenced.
    Only,
}

/// One repo's stored PR rows, counted by category.
///
/// A sweep stores every PR it sees, so the ledger grows with the repo's activity
/// rather than with the queue — most rows are untracked residue nothing will ever
/// ask for again. Nothing deletes any of it yet, and this exists to show the
/// shape of the growth before anything does: a row can be re-fetched from the
/// forge, so the only irreplaceable ones are those carrying something *I* set,
/// which is why [`mine`](Self::mine) is counted apart from the rest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Census {
    /// Every stored row, tracked or not.
    pub total: u64,
    /// Rows a rule matched or an involvement search named.
    pub tracked: u64,
    /// Rows whose PR is still open.
    pub open: u64,
    /// Rows whose PR merged.
    pub merged: u64,
    /// Rows whose PR was closed unmerged.
    pub closed: u64,
    /// Rows carrying something I set — done, snooze, mute or defer. The forge
    /// cannot give these back.
    pub mine: u64,
    /// How many of [`mine`](Self::mine) are on an untracked row, which is where
    /// "delete the untracked residue" would destroy something.
    pub mine_untracked: u64,
}

/// A tracked PR as read back from the ledger.
#[derive(Debug, Clone)]
pub struct TrackedPr {
    /// The stored snapshot.
    pub pr: PrSnapshot,
    /// The rendered `tracked_reason`.
    pub tracked_reason: String,
    /// Whether the rule that tracked it keeps it reviewable after it merges.
    pub after_merge: bool,
    /// My history on it — carried for the same reason a queue row carries it: a
    /// list wants to say what I have already done to each PR, and a PR that
    /// wants nothing is very often one I have already been through.
    pub my_state: MyState,
}

/// One stored attention reason, as read back from the `attention` table.
///
/// Carries the reason itself rather than a rendering of it: how a reason reads
/// is the frontend's business, so a caller wanting text calls `to_string()` on
/// [`reason`](Self::reason). That's also why a change to the wording in
/// `reviewq-core` applies to already-stored rows — nothing prerendered is kept.
#[derive(Debug, Clone)]
pub struct AttentionRow {
    /// The reason that fired, with its evidence.
    pub reason: AttentionReason,
    /// When the triggering event happened.
    pub since: Timestamp,
}

impl AttentionRow {
    /// Queue priority; 1 is most urgent.
    pub fn priority(&self) -> u8 {
        self.reason.priority()
    }
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
    /// My history on it. Carried by the row rather than read per selection,
    /// because a list wants to show what I have already done to each PR — and
    /// answering that one row at a time is what made it invisible.
    pub my_state: MyState,
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
    /// The PR's description, as raw markdown — rendering it is the frontend's
    /// business. `None` for a PR that has had no detail pass yet, since the
    /// sweep never fetches a body; empty for one that genuinely has none.
    pub body: Option<String>,
    /// The rendered `tracked_reason`, if tracked.
    pub tracked_reason: Option<String>,
    /// Whether the rule that tracked it keeps it reviewable after it merges.
    pub after_merge: bool,
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
                .on_disk(format!("creating ledger dir {}", dir.display()))?;
        }
        let conn = Connection::open(path).doing(format!("opening ledger {}", path.display()))?;
        Self::from_conn(conn)
    }

    /// An in-memory ledger, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(mut conn: Connection) -> Result<Self> {
        prepare_conn(&conn)?;
        schema::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// `repo`'s id, if the ledger already knows it. A read — see
    /// [`ensure_repo`](Self::ensure_repo) for the version that registers one.
    pub fn repo_id(&self, repo: &RepoKey) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
                params![repo.host, repo.owner, repo.name],
                |row| row.get(0),
            )
            .optional()
            .doing(format!("looking up repo {}", repo.slug()))
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
            .doing("checking for a pre-v4 placeholder repo")?;
        if let Some(id) = placeholder {
            self.conn
                .execute(
                    "UPDATE repos SET host = ?2, owner = ?3, name = ?4 WHERE id = ?1",
                    params![id, repo.host, repo.owner, repo.name],
                )
                .doing("adopting the pre-v4 placeholder repo")?;
            return Ok(id);
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO repos (host, owner, name) VALUES (?1, ?2, ?3)",
                params![repo.host, repo.owner, repo.name],
            )
            .doing("registering repo")?;
        self.conn
            .query_row(
                "SELECT id FROM repos WHERE host = ?1 AND owner = ?2 AND name = ?3",
                params![repo.host, repo.owner, repo.name],
                |row| row.get(0),
            )
            .doing("resolving repo id")
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
        tx.commit().doing("committing sweep page")?;
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
            .doing(format!("reading sync_meta {key}"))
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
                   base_ref, created_at, tracked_reason, after_merge,
                   last_reviewed_sha, last_verdict, last_action_at, done_sha,
                   snoozed_until, muted, deferred_at, done_at
            FROM prs
            LEFT JOIN my_state USING (repo_id, number)
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

    /// Record the colours a repo paints its labels, replacing any it has moved
    /// on from.
    ///
    /// Per repo, because a colour is the repo's rather than the label's: the
    /// same name is painted differently in another project, and a table keyed by
    /// name alone would answer for whichever repo was swept last.
    pub fn set_label_colours(&self, repo_id: i64, labels: &[(String, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (name, color) in labels {
            tx.execute(
                "INSERT INTO labels (repo_id, name, color) VALUES (?1, ?2, ?3)
                 ON CONFLICT(repo_id, name) DO UPDATE SET color = excluded.color",
                params![repo_id, name, color],
            )
            .doing(format!("storing the colour of {name}"))?;
        }
        tx.commit().doing("committing label colours")?;
        Ok(())
    }

    /// One repo's label colours, by name — what a frontend needs to paint a row
    /// the way the forge does.
    pub fn label_colours(&self, repo_id: i64) -> Result<BTreeMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, color FROM labels WHERE repo_id = ?1")?;
        let rows = stmt
            .query_map(params![repo_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
        Ok(rows)
    }

    /// One repo's stored PR rows, counted the ways that say what the ledger is
    /// accumulating. See [`Census`].
    pub fn census(&self, repo_id: i64) -> Result<Census> {
        // The join carries the predicate rather than a `WHERE`, so a PR with a
        // `my_state` row that says nothing (every field back at its default)
        // counts as having none — which is what "something I set" means.
        self.conn
            .query_row(
                r"
                SELECT COUNT(*),
                       COALESCE(SUM(p.tracked_reason IS NOT NULL), 0),
                       COALESCE(SUM(p.state = 'OPEN'), 0),
                       COALESCE(SUM(p.state = 'MERGED'), 0),
                       COALESCE(SUM(p.state = 'CLOSED'), 0),
                       COALESCE(SUM(m.number IS NOT NULL), 0),
                       COALESCE(SUM(m.number IS NOT NULL AND p.tracked_reason IS NULL), 0)
                FROM prs p
                LEFT JOIN my_state m
                  ON m.repo_id = p.repo_id AND m.number = p.number
                 AND (m.done_at IS NOT NULL OR m.snoozed_until IS NOT NULL
                      OR m.muted = 1 OR m.deferred_at IS NOT NULL)
                WHERE p.repo_id = ?1
                ",
                params![repo_id],
                |row| {
                    let count = |index: usize| row.get::<_, i64>(index).map(|n| n as u64);
                    Ok(Census {
                        total: count(0)?,
                        tracked: count(1)?,
                        open: count(2)?,
                        merged: count(3)?,
                        closed: count(4)?,
                        mine: count(5)?,
                        mine_untracked: count(6)?,
                    })
                },
            )
            .doing("counting stored PRs")
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
    /// before the last time the PR changed. Open PRs always; a merged PR when
    /// `include_merged` (the per-project post-merge-review opt-in) or when the
    /// rule that tracked it said `after_merge`; closed-unmerged PRs never.
    /// Returns the full snapshot and tracked reason so the caller can classify
    /// without a second read.
    pub fn prs_needing_detail(&self, repo_id: i64, include_merged: bool) -> Result<Vec<TrackedPr>> {
        // Timestamps are stored as fixed-precision RFC3339 (see `commit_detail`
        // and the sweep), so this lexicographic `<` is a correct chronological
        // comparison.
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason, p.after_merge, {MY_STATE_COLUMNS}
            FROM prs p
            LEFT JOIN my_state ms ON ms.repo_id = p.repo_id AND ms.number = p.number
            WHERE p.repo_id = ?1 AND p.tracked_reason IS NOT NULL
              AND (p.state = 'OPEN'
                   OR (p.state = 'MERGED' AND (?2 OR p.after_merge = 1)))
              AND (p.detail_synced_at IS NULL OR p.detail_synced_at < p.updated_at)
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map(params![repo_id, include_merged], row_to_tracked)?
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
            .doing(format!("reading my_state for #{number}"))
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
            .doing(format!("recording done for #{number}"))?;
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
            .doing(format!("snoozing #{number}"))?;
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
            .doing(format!("setting muted for #{number}"))?;
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
            .doing(format!("setting deferred_at for #{number}"))?;
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
            .doing(format!("clearing attention for #{number}"))?;
        Ok(())
    }

    /// Record that a PR's detail can't be fetched because the forge no longer
    /// has it — deleted, or a number that was never a pull request.
    ///
    /// Drops its attention, since a queue row pointing at a PR nobody can open
    /// is worse than no row, and stamps `detail_synced_at` so the detail pass
    /// stops retrying a fetch that will keep failing. It stays tracked and
    /// stored: this is a statement about the forge, not a decision to forget it.
    ///
    /// Self-correcting if the PR comes back — a sweep seeing it again advances
    /// `updated_at` past this stamp, which makes it due for detail once more.
    pub fn mark_detail_unavailable(&self, repo_id: i64, number: u64, now: Timestamp) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
            params![repo_id, number as i64],
        )
        .doing(format!("clearing attention for unreachable #{number}"))?;
        tx.execute(
            "UPDATE prs SET detail_synced_at = ?3 WHERE repo_id = ?1 AND number = ?2",
            params![repo_id, number as i64, now.to_string()],
        )
        .doing(format!(
            "stamping detail_synced_at for unreachable #{number}"
        ))?;
        tx.commit()?;
        Ok(())
    }

    /// Record the state a detail fetch found the PR in — open, merged, or
    /// closed.
    ///
    /// The sweep writes this as part of the whole snapshot, so this exists for
    /// the one path that never sweeps: refreshing a single PR. Without it, a PR
    /// closed on the forge stayed `OPEN` in the ledger however many times you
    /// refreshed it, and went on being listed as waiting on somebody.
    ///
    /// Only the state: everything else a detail fetch knows is committed by
    /// [`commit_detail`](Self::commit_detail), and the rest of the snapshot
    /// (title, labels, milestone) is the sweep's to own.
    pub fn set_state(&self, repo_id: i64, number: u64, state: PrState) -> Result<()> {
        self.conn
            .execute(
                "UPDATE prs SET state = ?3 WHERE repo_id = ?1 AND number = ?2",
                params![repo_id, number as i64, state.as_str()],
            )
            .doing(format!("recording #{number}'s state"))?;
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
            .doing(format!("clearing done attention for #{number}"))?;
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
                // `untracked_at` cleared as well: `track` is what undoes an
                // untrack, and leaving the stamp would let the next sweep drop
                // the reason this just wrote.
                "UPDATE prs SET tracked_reason = ?3, untracked_at = NULL \
                 WHERE repo_id = ?1 AND number = ?2",
                params![
                    repo_id,
                    number as i64,
                    TrackedReason::Involved("manual".into()).render()
                ],
            )
            .doing(format!("force-tracking #{number}"))?;
        Ok(true)
    }

    /// Stop watching a PR: drop the reason it was tracked for, and the attention
    /// it was holding.
    ///
    /// `false`, changing nothing, if the ledger has no such PR.
    ///
    /// The PR stays stored and keeps being swept, so `show` still answers and a
    /// later [`track`](Self::track) has something to put back. What it loses is
    /// its standing on every list — the queue, waiting and muted all ask for a
    /// tracked reason — and, through `untracked_at`, its eligibility to be
    /// tracked again by a rule that still matches it.
    ///
    /// [`MyState`] survives: what you reviewed and when you were done with it
    /// stays true whether or not you are still watching.
    pub fn untrack(&self, repo_id: i64, number: u64, now: Timestamp) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx
            .execute(
                "UPDATE prs SET tracked_reason = NULL, untracked_at = ?3 \
                 WHERE repo_id = ?1 AND number = ?2",
                params![repo_id, number as i64, now.to_string()],
            )
            .doing(format!("untracking #{number}"))?;
        if changed == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
            params![repo_id, number as i64],
        )
        .doing(format!("clearing attention for untracked #{number}"))?;
        tx.commit()?;
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
    ///
    /// Refuses to move a PR backwards. Two fetches of the same PR can be in
    /// flight at once — a `sync` and the interface's refresh key, in separate
    /// processes — and whichever commits second would otherwise win regardless of
    /// which *fetched* second, reverting threads, attention and the description to
    /// an older view of the PR. So the write applies only if the stored watermark
    /// is not newer than `now`, and says which it did.
    ///
    /// Idempotent: committing the same pass twice applies twice and leaves the
    /// same rows, since every part of it is a wholesale replace and `now` compares
    /// equal to itself.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_detail(
        &self,
        repo_id: i64,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        attention: &[Attention],
        body: Option<&str>,
        now: Timestamp,
    ) -> Result<Committed> {
        let tx = self.conn.unchecked_transaction()?;
        // The watermark first, and as one compare-and-set rather than a read
        // followed by a write: a separate read could be answered from before a
        // racing commit landed, and then this write would clobber it. As the
        // opening statement it also takes the write lock up front, so the
        // comparison and the rest of the transaction cannot be interleaved with
        // anybody else's.
        //
        // Stamped at whole-second precision so the lexicographic comparison in
        // `prs_needing_detail` against GitHub's whole-second `updatedAt` is
        // correct. A sub-second stamp would sort *before* an equal-second
        // `updatedAt` (`.` < `Z`), re-fetching that PR every sync forever.
        //
        // `body` is written from the same fetch, so an edited description lands
        // with everything else the detail pass saw. A `None` leaves whatever is
        // stored alone rather than blanking it — a caller with no body to offer
        // isn't asserting the PR has none.
        let stamp = whole_second(now).to_string();
        let applied = tx
            .execute(
                "UPDATE prs SET detail_synced_at = ?3, body = COALESCE(?4, body) \
                 WHERE repo_id = ?1 AND number = ?2 \
                   AND (detail_synced_at IS NULL OR detail_synced_at <= ?3)",
                params![repo_id, number as i64, stamp, body],
            )
            .doing(format!("stamping detail_synced_at for #{number}"))?;
        if applied == 0 {
            // Either the row is gone — a caller bug — or somebody stored a newer
            // detail while this one was being fetched.
            let stored: Option<String> = tx
                .query_row(
                    "SELECT detail_synced_at FROM prs WHERE repo_id = ?1 AND number = ?2",
                    params![repo_id, number as i64],
                    |row| row.get(0),
                )
                .optional()
                .doing(format!("reading #{number}'s detail watermark"))?
                .flatten();
            let Some(stored) = stored else {
                return Err(LedgerError::NotStored { number });
            };
            return Ok(Committed::Superseded { stored });
        }

        write_forge_state(&tx, repo_id, number, my_state)?;
        replace_threads(&tx, repo_id, number, threads)?;
        replace_reviewers(&tx, repo_id, number, reviewers)?;
        replace_attention(&tx, repo_id, number, attention)?;
        tx.commit().doing("committing PR detail")?;
        Ok(Committed::Applied)
    }

    /// Drop attention rows that no longer belong to a queued PR: closed-unmerged
    /// PRs always, and merged PRs except the ones post-merge review keeps —
    /// `include_merged` for the whole project, or the tracking rule's own
    /// `after_merge`. Detail is never re-fetched for the rest (see
    /// [`prs_needing_detail`](Self::prs_needing_detail)), so without this their
    /// stale rows would linger and show up in `show`. Run once at the end of a
    /// sync.
    pub fn clear_archived_attention(&self, repo_id: i64, include_merged: bool) -> Result<()> {
        self.conn
            .execute(
                "DELETE FROM attention WHERE repo_id = ?1 AND pr_number IN
                 (SELECT number FROM prs WHERE repo_id = ?1
                    AND state <> 'OPEN'
                    AND NOT (state = 'MERGED' AND (?2 OR after_merge = 1)))",
                params![repo_id, include_merged],
            )
            .doing("clearing archived attention")?;
        Ok(())
    }

    /// The queue: tracked, open PRs that currently want attention, each with its
    /// highest-priority reason, ordered most-urgent first (priority band, then
    /// oldest within the band, then PR number) — except a deferred PR (see
    /// [`QueueItem::deferred`]), which sorts after every non-deferred item
    /// regardless of priority.
    pub fn queue(&self, repo_id: i64) -> Result<Vec<QueueItem>> {
        self.queued(repo_id, Muted::Hidden)
    }

    /// What a mute is hiding: the same rows [`queue`](Self::queue) leaves out,
    /// in the same order, each with the reason it would be there for.
    ///
    /// The reasons are real — a mute stops nothing being computed, it only stops
    /// it being shown (see `classify`) — which is what makes this answerable at
    /// all, and what makes unmuting immediate rather than a wait for the next
    /// sync to rediscover them.
    pub fn muted(&self, repo_id: i64) -> Result<Vec<QueueItem>> {
        self.queued(repo_id, Muted::Only)
    }

    fn queued(&self, repo_id: i64, muted: Muted) -> Result<Vec<QueueItem>> {
        // Open PRs, plus merged PRs when a project opted into post-merge review
        // (those only carry attention rows when it did). Closed-unmerged never.
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason, a.since, a.payload, {MY_STATE_COLUMNS}
            FROM prs p
            JOIN attention a ON a.repo_id = p.repo_id AND a.pr_number = p.number
            LEFT JOIN my_state ms ON ms.repo_id = p.repo_id AND ms.number = p.number
            WHERE p.repo_id = ?1 AND p.state IN ('OPEN', 'MERGED') AND p.tracked_reason IS NOT NULL
              AND COALESCE(ms.muted, 0) = ?2
            ",
        ))?;
        let muted = i64::from(muted == Muted::Only);
        let rows = stmt
            .query_map(params![repo_id, muted], |row| {
                let pr = snapshot_from_row(row, 0)?;
                let tracked_reason: String = row.get(14)?;
                let attention = attention_from_row(row, 15)?;
                let my_state = my_state_from_row(row, 17)?;
                Ok((pr, tracked_reason, attention, my_state))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items: Vec<QueueItem> = Vec::new();
        for (pr, tracked_reason, attention, my_state) in rows {
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
                    my_state,
                    deferred: false,
                }),
            }
        }
        // A defer only survives if nothing has happened since: the top reason's
        // `since` must not be newer than the moment it was deferred.
        for item in &mut items {
            item.deferred = item
                .my_state
                .deferred_at
                .is_some_and(|deferred_at| item.top.since <= deferred_at);
        }
        items.sort_by(|a, b| {
            (a.deferred, a.top.priority(), a.top.since, a.pr.number).cmp(&(
                b.deferred,
                b.top.priority(),
                b.top.since,
                b.pr.number,
            ))
        });
        Ok(items)
    }

    /// Tracked, open PRs with no attention: seen and understood, waiting on
    /// someone else. Ordered by number.
    ///
    /// A muted PR is not one of these however quiet it is. It is off the queue
    /// because you put it there, not because anybody else has the ball, and
    /// [`muted`](Self::muted) is where it belongs.
    pub fn waiting(&self, repo_id: i64) -> Result<Vec<TrackedPr>> {
        let mut stmt = self.conn.prepare(&format!(
            r"
            SELECT {PR_COLUMNS}, p.tracked_reason, p.after_merge, {MY_STATE_COLUMNS}
            FROM prs p
            LEFT JOIN my_state ms ON ms.repo_id = p.repo_id AND ms.number = p.number
            WHERE p.repo_id = ?1 AND p.state = 'OPEN' AND p.tracked_reason IS NOT NULL
              AND COALESCE(ms.muted, 0) = 0
              AND NOT EXISTS (
                SELECT 1 FROM attention a
                WHERE a.repo_id = p.repo_id AND a.pr_number = p.number
              )
            ORDER BY p.number
            ",
        ))?;
        let rows = stmt
            .query_map(params![repo_id], row_to_tracked)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Run a per-repo read against every repo in [`repos`](Self::repos) and
    /// flatten the results, each tagged with the repo it came from.
    fn across_repos<T>(
        &self,
        read: impl Fn(&Self, i64) -> Result<Vec<T>>,
    ) -> Result<Vec<Located<T>>> {
        let mut out = Vec::new();
        for (repo_id, repo) in self.repos()? {
            out.extend(read(self, repo_id)?.into_iter().map(|item| Located {
                repo: repo.clone(),
                repo_id,
                item,
            }));
        }
        Ok(out)
    }

    /// Every repo's [`queue`](Self::queue), merged into one. Each repo's slice
    /// arrives already sorted, so the merge re-sorts by the same key to
    /// interleave them — with the repo slug as a final tiebreak, so two repos
    /// that happen to share a PR number and an urgency don't order by
    /// whichever was registered first.
    pub fn queue_all(&self) -> Result<Vec<Located<QueueItem>>> {
        self.ordered(Self::queue)
    }

    /// Every repo's [`muted`](Self::muted), merged and ordered like the queue —
    /// so what you silenced reads in the order it would have arrived in.
    pub fn muted_all(&self) -> Result<Vec<Located<QueueItem>>> {
        self.ordered(Self::muted)
    }

    fn ordered(
        &self,
        read: fn(&Self, i64) -> Result<Vec<QueueItem>>,
    ) -> Result<Vec<Located<QueueItem>>> {
        let mut queue = self.across_repos(read)?;
        queue.sort_by_key(|l| {
            (
                l.item.deferred,
                l.item.top.priority(),
                l.item.top.since,
                l.item.pr.number,
                l.repo.slug(),
            )
        });
        Ok(queue)
    }

    /// Every repo's [`waiting`](Self::waiting), merged and ordered by repo then
    /// PR number.
    pub fn waiting_all(&self) -> Result<Vec<Located<TrackedPr>>> {
        let mut waiting = self.across_repos(Self::waiting)?;
        waiting.sort_by_key(|l| (l.repo.slug(), l.item.pr.number));
        Ok(waiting)
    }

    /// Every repo's [`list_tracked`](Self::list_tracked), merged and ordered by
    /// repo then PR number.
    pub fn tracked_all(&self) -> Result<Vec<Located<TrackedPr>>> {
        let mut tracked = self.across_repos(Self::list_tracked)?;
        tracked.sort_by_key(|l| (l.repo.slug(), l.item.pr.number));
        Ok(tracked)
    }

    /// Everything `reviewq show` needs about one PR, or `None` if it is not
    /// stored.
    pub fn show(&self, repo_id: i64, number: u64) -> Result<Option<PrShow>> {
        let base = self
            .conn
            .query_row(
                &format!(
                    "SELECT {PR_COLUMNS}, p.tracked_reason, p.body, p.after_merge FROM prs p \
                     WHERE p.repo_id = ?1 AND p.number = ?2"
                ),
                params![repo_id, number as i64],
                |row| {
                    Ok((
                        snapshot_from_row(row, 0)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, Option<String>>(15)?,
                        row.get::<_, i64>(16)? != 0,
                    ))
                },
            )
            .optional()
            .doing(format!("reading PR #{number}"))?;
        let Some((pr, tracked_reason, body, after_merge)) = base else {
            return Ok(None);
        };

        let my_state = self.my_state(repo_id, number)?;
        let threads = self.threads(repo_id, number)?;
        let reviewers = self.reviewers(repo_id, number)?;
        let attention = self.attention(repo_id, number)?;
        Ok(Some(PrShow {
            pr,
            body,
            tracked_reason,
            after_merge,
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

    /// Every repo this ledger knows that has PR `number`.
    ///
    /// Lets a command naming a bare number work out which repo it belongs to.
    /// `&[]` when no repo has it — a caller's answer either way is the same "not
    /// in the ledger".
    ///
    /// A method rather than a free function over a path: as the latter it opened
    /// its own connection and ran migrations, despite documenting itself as a pure
    /// lookup, and every caller then opened a second one to do anything with the
    /// answer.
    pub fn repos_with_pr(&self, number: u64) -> Result<Vec<RepoKey>> {
        let mut stmt = self.conn.prepare(
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

    /// A PR's attention rows, most-urgent first.
    fn attention(&self, repo_id: i64, number: u64) -> Result<Vec<AttentionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT since, payload FROM attention WHERE repo_id = ?1 AND pr_number = ?2",
        )?;
        let mut rows = stmt
            .query_map(params![repo_id, number as i64], |row| {
                attention_from_row(row, 0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.sort_by_key(|a| (a.priority(), a.since));
        Ok(rows)
    }
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
    let merged = merge_tracking(stored_tracking(conn, repo_id, pr.number)?, reason);
    let is_new = existing_row(conn, repo_id, pr.number)?.is_none();

    let labels = serde_json::to_string(&pr.labels).encoding("a label list")?;
    let files = pr
        .files
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .encoding("a file list")?;

    conn.execute(
        r"
        INSERT INTO prs (
          repo_id, number, title, author, author_association, head_sha, is_draft,
          state, updated_at, labels, milestone, files, files_truncated,
          tracked_reason, first_seen_at, base_ref, after_merge, created_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)
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
          tracked_reason=excluded.tracked_reason,
          base_ref=excluded.base_ref,
          after_merge=excluded.after_merge,
          -- Never back to unknown: a snapshot from a caller that has no opening
          -- date (a fixture, or a response captured before the query asked for
          -- it) must not erase one an earlier sweep learnt.
          created_at=COALESCE(excluded.created_at, prs.created_at)
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
            merged.reason,
            now.to_string(),
            pr.base_ref,
            merged.after_merge as i64,
            pr.created_at.map(|at| at.to_string()),
        ],
    )
    .doing(format!("upserting PR #{}", pr.number))?;
    Ok(is_new)
}

fn set_meta_row(conn: &Connection, repo_id: i64, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_meta (repo_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id, key) DO UPDATE SET value = excluded.value",
        params![repo_id, key, value],
    )
    .doing(format!("writing sync_meta {key}"))?;
    Ok(())
}

fn tracked_reason(conn: &Connection, repo_id: i64, number: u64) -> Result<Option<String>> {
    Ok(stored_tracking(conn, repo_id, number)?.reason)
}

/// What the row already says about why this PR is tracked. All-default when
/// there is no row yet.
fn stored_tracking(conn: &Connection, repo_id: i64, number: u64) -> Result<Tracking> {
    conn.query_row(
        "SELECT tracked_reason, after_merge, untracked_at FROM prs \
         WHERE repo_id = ?1 AND number = ?2",
        params![repo_id, number as i64],
        |row| {
            Ok(Tracking {
                reason: row.get(0)?,
                after_merge: row.get::<_, i64>(1)? != 0,
                untracked: row.get::<_, Option<String>>(2)?.is_some(),
            })
        },
    )
    .optional()
    .doing(format!("reading tracked_reason for #{number}"))
    .map(Option::unwrap_or_default)
}

fn existing_row(conn: &Connection, repo_id: i64, number: u64) -> Result<Option<u64>> {
    conn.query_row(
        "SELECT number FROM prs WHERE repo_id = ?1 AND number = ?2",
        params![repo_id, number as i64],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .doing(format!("checking for PR #{number}"))
    .map(|opt| opt.map(|n| n as u64))
}

/// Why a PR is tracked as the row holds it: the rendered reason, and whether it
/// survives the PR merging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Tracking {
    reason: Option<String>,
    after_merge: bool,
    /// `reviewq untrack` said to stop watching this one, so no rule may track
    /// it again until `reviewq track` says otherwise.
    untracked: bool,
}

/// Merge stored tracking with an incoming reason by precedence: keep the
/// stronger, refresh on a tie, never downgrade. `None` incoming leaves the
/// stored value.
///
/// The post-merge flag does not follow the winning reason: it is the rules'
/// answer, so it changes when a rule match arrives and at no other time, whether
/// or not that match also wins the reason. Only the sweep evaluates rules — an
/// involvement search knows nothing of them, and letting it overwrite the flag
/// on the way past would drop a PR a post-merge rule matched at merge, purely
/// because somebody had also asked you to review it.
fn merge_tracking(stored: Tracking, incoming: Option<&TrackedReason>) -> Tracking {
    // An untracked PR keeps being swept — its title, labels and state stay
    // current, so `show` and a later `track` have something to work with — but
    // nothing a sweep or an involvement search finds may track it again. That
    // is the difference between this and `done`: one says "not now", this says
    // "not until I say so".
    if stored.untracked {
        return Tracking {
            reason: None,
            ..stored
        };
    }
    let after_merge = match incoming {
        Some(TrackedReason::Interest { after_merge, .. }) => *after_merge,
        Some(TrackedReason::Involved(_)) | None => stored.after_merge,
    };
    let reason = match (stored.reason, incoming) {
        (stored, None) => stored,
        (None, Some(new)) => Some(new.render()),
        (Some(old), Some(new)) => Some(if new.rank() >= stored_rank(&old) {
            new.render()
        } else {
            old
        }),
    };
    Tracking {
        reason,
        after_merge,
        untracked: false,
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
     p.files, p.files_truncated, p.base_ref, p.created_at";

/// The `my_state` columns, `ms.`-qualified and in the order
/// [`my_state_from_row`] reads them — the same single-source arrangement as
/// [`PR_COLUMNS`], for the queries that outer-join my history onto a PR.
const MY_STATE_COLUMNS: &str = "ms.last_reviewed_sha, ms.last_verdict, \
     ms.last_action_at, ms.done_sha, ms.snoozed_until, ms.muted, \
     ms.deferred_at, ms.done_at";

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

/// Read a [`PrSnapshot`] from the [`PR_COLUMNS`] starting at `base`.
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
        base_ref: row.get(base + 12)?,
        created_at: row
            .get::<_, Option<String>>(base + 13)?
            .map(|at| parse_ts(&at))
            .transpose()?,
    })
}

fn row_to_tracked(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedPr> {
    Ok(TrackedPr {
        pr: snapshot_from_row(row, 0)?,
        tracked_reason: row.get(14)?,
        after_merge: row.get::<_, i64>(15)? != 0,
        my_state: my_state_from_row(row, 16)?,
    })
}

fn row_to_my_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<MyState> {
    my_state_from_row(row, 0)
}

/// Read a [`MyState`] from the [`MY_STATE_COLUMNS`] starting at `base`.
///
/// Tolerates every column being null, which is what an outer join against a PR
/// nobody has ever acted on returns — the all-default state, not an error.
fn my_state_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<MyState> {
    let verdict: Option<String> = row.get(base + 1)?;
    let last_action_at: Option<String> = row.get(base + 2)?;
    let snoozed_until: Option<String> = row.get(base + 4)?;
    let deferred_at: Option<String> = row.get(base + 6)?;
    let done_at: Option<String> = row.get(base + 7)?;
    Ok(MyState {
        last_reviewed_sha: row.get(base)?,
        last_verdict: verdict.as_deref().and_then(Verdict::from_wire),
        last_action_at: last_action_at.as_deref().map(parse_ts).transpose()?,
        done_sha: row.get(base + 3)?,
        snoozed_until: snoozed_until.as_deref().map(parse_ts).transpose()?,
        muted: row.get::<_, Option<i64>>(base + 5)?.unwrap_or(0) != 0,
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
/// Build an [`AttentionRow`] from `since`, `payload` at `base`.
///
/// The stored `reason` discriminant isn't read: the payload carries the whole
/// variant, discriminant included, so reading both would be two sources for one
/// fact. The column exists for the primary key.
fn attention_from_row(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<AttentionRow> {
    let payload: String = row.get(base + 1)?;
    let reason: AttentionReason = serde_json::from_str(&payload)
        .map_err(|err| FromSqlConversionFailure(base + 1, Type::Text, Box::new(err)))?;
    Ok(AttentionRow {
        reason,
        since: parse_ts(&row.get::<_, String>(base)?)?,
    })
}

/// Whether `candidate` should outrank the current best: lower priority band,
/// or the same band but an older event.
fn attention_is_more_urgent(candidate: &AttentionRow, best: &AttentionRow) -> bool {
    (candidate.priority(), candidate.since) < (best.priority(), best.since)
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
    .doing(format!("writing forge-derived my_state for #{number}"))?;
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
        .doing(format!("writing thread {} for #{number}", t.thread_id))?;
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
        .doing(format!("writing reviewer {} for #{number}", r.login))?;
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
            "INSERT INTO attention (repo_id, pr_number, reason, since, payload) \
             VALUES (?1,?2,?3,?4,?5)",
            params![
                repo_id,
                number as i64,
                a.reason.discriminant(),
                a.since.to_string(),
                serde_json::to_string(&a.reason).encoding("an attention reason")?,
            ],
        )
        .doing({
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
            base_ref: "main".into(),
            is_draft: false,
            state: PrState::Open,
            updated_at: "2026-08-05T12:00:00Z".parse().unwrap(),
            created_at: None,
            labels: vec!["area:task-sdk".into()],
            milestone: Some("3.2.0".into()),
            files: None,
            files_truncated: false,
        }
    }

    fn now() -> Timestamp {
        "2026-08-05T12:00:00Z".parse().unwrap()
    }

    /// A rule match that lets the PR go once it merges — the ordinary case.
    fn interest(rule: &str) -> TrackedReason {
        TrackedReason::Interest {
            rule: rule.into(),
            after_merge: false,
        }
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
            .upsert_pr(a, &pr(1), Some(interest("label x")), now())
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
        let other = RepoKey {
            host: "github.com".into(),
            owner: "someone".into(),
            name: "else".into(),
        };
        let ledger = Ledger::open_in_memory().unwrap();

        assert!(ledger.repos_with_pr(1).unwrap().is_empty());

        let a = ledger.ensure_repo(&repo()).unwrap();
        ledger.upsert_pr(a, &pr(1), None, now()).unwrap();
        let b = ledger.ensure_repo(&other).unwrap();
        ledger.upsert_pr(b, &pr(2), None, now()).unwrap();

        assert_eq!(ledger.repos_with_pr(1).unwrap(), vec![repo()]);
        assert_eq!(ledger.repos_with_pr(2).unwrap(), vec![other]);
        assert!(ledger.repos_with_pr(999).unwrap().is_empty());
    }

    #[test]
    fn repo_id_reads_without_registering() {
        let ledger = Ledger::open_in_memory().unwrap();

        assert_eq!(ledger.repo_id(&repo()).unwrap(), None);
        assert!(
            ledger.repos().unwrap().is_empty(),
            "looking a repo up must not create it"
        );

        let id = ledger.ensure_repo(&repo()).unwrap();
        assert_eq!(ledger.repo_id(&repo()).unwrap(), Some(id));
    }

    #[test]
    fn a_whole_database_read_carries_each_rows_repo_id() {
        // What stops the interface asking `ensure_repo` — a write — for an id the
        // read already had, every time the selection moves.
        let ledger = Ledger::open_in_memory().unwrap();
        let id = ledger.ensure_repo(&repo()).unwrap();
        track(&ledger, id, &pr(1));

        let waiting = ledger.waiting_all().unwrap();

        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].repo_id, id);
        assert_eq!(waiting[0].repo, repo());
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
        let reason = interest("label area:task-sdk");

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
        assert_eq!(tracked[0].pr.base_ref, "main");
        assert_eq!(tracked[0].tracked_reason, "interest: label area:task-sdk");
    }

    /// The target branch has to survive every read that rebuilds a snapshot, not
    /// just the one a test happened to pick: the reads share a column list and a
    /// positional reader, so an index off by one shows up in only some of them.
    #[test]
    fn the_target_branch_reads_back_from_every_snapshot_query() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut backport = pr(1);
        backport.base_ref = "v3-1-test".into();
        track(&ledger, repo_id, &backport);

        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].pr.base_ref,
            "v3-1-test"
        );
        assert_eq!(
            ledger.waiting(repo_id).unwrap()[0].pr.base_ref,
            "v3-1-test",
            "waiting: tracked, open, no attention"
        );
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.base_ref,
            "v3-1-test"
        );
        assert_eq!(
            ledger.prs_needing_detail(repo_id, false).unwrap()[0]
                .pr
                .base_ref,
            "v3-1-test",
            "never detail-synced, so it is due"
        );

        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::Mention { by: "kaxil".into() },
                    "2026-08-05T11:00:00Z",
                )],
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        assert_eq!(
            ledger.queue(repo_id).unwrap()[0].pr.base_ref,
            "v3-1-test",
            "and through the queue, whose row carries attention columns after it"
        );
    }

    /// The same hazard as the target branch, and the same shape of test: one
    /// column list, one positional reader, so an index off by one shows up in
    /// some reads and not others.
    #[test]
    fn when_a_pr_was_opened_reads_back_from_every_snapshot_query() {
        let (ledger, repo_id) = ledger_with_repo();
        let opened: Timestamp = "2026-05-04T08:30:00Z".parse().unwrap();
        let mut old = pr(1);
        old.created_at = Some(opened);
        track(&ledger, repo_id, &old);

        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].pr.created_at,
            Some(opened)
        );
        assert_eq!(
            ledger.waiting(repo_id).unwrap()[0].pr.created_at,
            Some(opened),
            "waiting: tracked, open, no attention"
        );
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.created_at,
            Some(opened)
        );
        assert_eq!(
            ledger.prs_needing_detail(repo_id, false).unwrap()[0]
                .pr
                .created_at,
            Some(opened)
        );

        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::Mention { by: "kaxil".into() },
                    "2026-08-05T11:00:00Z",
                )],
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        assert_eq!(
            ledger.queue(repo_id).unwrap()[0].pr.created_at,
            Some(opened),
            "and through the queue, whose row carries attention columns after it"
        );
    }

    #[test]
    fn a_later_write_without_an_opening_date_keeps_the_one_already_stored() {
        // A PR is opened once. Anything writing a snapshot that doesn't know
        // when — an older capture, a caller that built one by hand — must not
        // be able to erase what a sweep learnt.
        let (ledger, repo_id) = ledger_with_repo();
        let opened: Timestamp = "2026-05-04T08:30:00Z".parse().unwrap();
        let mut swept = pr(1);
        swept.created_at = Some(opened);
        ledger.upsert_pr(repo_id, &swept, None, now()).unwrap();

        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();

        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.created_at,
            Some(opened)
        );
    }

    /// A ledger written before the opening date was captured must still open,
    /// with its rows saying "unknown" rather than a date nobody stored.
    #[test]
    fn an_existing_row_has_no_opening_date_until_a_sweep_learns_one() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();

        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.created_at,
            None,
            "unknown, not the epoch and not first_seen_at"
        );

        let mut swept = pr(1);
        swept.created_at = Some("2026-05-04T08:30:00Z".parse().unwrap());
        ledger.upsert_pr(repo_id, &swept, None, now()).unwrap();
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.created_at,
            swept.created_at
        );
    }

    /// A ledger written before the target branch was captured must still open,
    /// with its rows reading as "unknown" until a sync refreshes them.
    #[test]
    fn an_existing_row_gains_an_empty_target_branch_and_a_sync_fills_it() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();
        // Stand in for a row migration 7 backfilled: the column exists, and no
        // sweep has written a real value into it yet.
        ledger
            .conn
            .execute("UPDATE prs SET base_ref = '' WHERE number = 1", [])
            .unwrap();

        let before = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(before.pr.base_ref, "", "unknown, not a wrong branch");

        ledger.upsert_pr(repo_id, &pr(1), None, now()).unwrap();
        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.base_ref,
            "main"
        );
    }

    #[test]
    fn commit_sweep_page_persists_prs_and_cursor_atomically_and_resumes() {
        let (ledger, repo_id) = ledger_with_repo();
        let page = vec![
            (pr(1), Some(interest("label area:task-sdk"))),
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
        let matched = || interest("label area:task-sdk");
        ledger
            .upsert_pr(repo_id, &pr(1), Some(matched()), now())
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
            .upsert_pr(repo_id, &pr(1), Some(matched()), now())
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
    fn the_census_counts_rows_by_what_would_be_lost_with_them() {
        let (ledger, repo_id) = ledger_with_repo();
        // Tracked and open.
        track(&ledger, repo_id, &pr(1));
        // Untracked residue, of the kind a sweep leaves by the thousand.
        let mut merged = pr(2);
        merged.state = PrState::Merged;
        ledger.upsert_pr(repo_id, &merged, None, now()).unwrap();
        let mut closed = pr(3);
        closed.state = PrState::Closed;
        ledger.upsert_pr(repo_id, &closed, None, now()).unwrap();
        // Untracked, but muted by hand — the row that cannot be re-fetched.
        ledger.set_muted(repo_id, 3, true).unwrap();
        // A `my_state` row that says nothing does not count as mine.
        ledger.set_deferred_at(repo_id, 2, None).unwrap();

        let census = ledger.census(repo_id).unwrap();

        assert_eq!(
            census,
            Census {
                total: 3,
                tracked: 1,
                open: 1,
                merged: 1,
                closed: 1,
                mine: 1,
                mine_untracked: 1,
            }
        );
    }

    #[test]
    fn an_empty_repo_has_an_empty_census() {
        let (ledger, repo_id) = ledger_with_repo();
        assert_eq!(ledger.census(repo_id).unwrap(), Census::default());
    }

    #[test]
    fn label_colours_belong_to_the_repo_that_painted_them() {
        // The same name in two projects is two colours, which is the whole
        // reason this is keyed by repo.
        let (ledger, airflow) = ledger_with_repo();
        let other = ledger
            .ensure_repo(&RepoKey {
                host: "github.com".into(),
                owner: "apache".into(),
                name: "airflow-site".into(),
            })
            .unwrap();

        ledger
            .set_label_colours(airflow, &[("area:docs".into(), "0e8a16".into())])
            .unwrap();
        ledger
            .set_label_colours(other, &[("area:docs".into(), "d73a4a".into())])
            .unwrap();

        assert_eq!(
            ledger.label_colours(airflow).unwrap()["area:docs"],
            "0e8a16"
        );
        assert_eq!(ledger.label_colours(other).unwrap()["area:docs"], "d73a4a");
    }

    #[test]
    fn a_recoloured_label_is_replaced_rather_than_doubled() {
        let (ledger, repo_id) = ledger_with_repo();

        ledger
            .set_label_colours(repo_id, &[("backport".into(), "fbca04".into())])
            .unwrap();
        ledger
            .set_label_colours(repo_id, &[("backport".into(), "000000".into())])
            .unwrap();

        let colours = ledger.label_colours(repo_id).unwrap();
        assert_eq!(colours.len(), 1);
        assert_eq!(colours["backport"], "000000");
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
    fn merge_tracking_keeps_the_stronger_reason() {
        let matched = interest("label x");
        let involved = TrackedReason::Involved("mention".into());
        let stored = |reason: &str| Tracking {
            reason: Some(reason.to_string()),
            after_merge: false,
            untracked: false,
        };
        let merged = |stored, incoming| merge_tracking(stored, incoming).reason;

        assert_eq!(merged(Tracking::default(), None), None);
        assert_eq!(
            merged(Tracking::default(), Some(&matched)).as_deref(),
            Some("interest: label x")
        );
        assert_eq!(
            merged(stored("involved: review_requested"), Some(&matched)).as_deref(),
            Some("involved: review_requested")
        );
        assert_eq!(
            merged(stored("interest: label x"), Some(&involved)).as_deref(),
            Some("involved: mention")
        );
        assert_eq!(
            merged(stored("involved: old"), None).as_deref(),
            Some("involved: old")
        );
        // Whatever the incoming reason, a PR you untracked stays untracked.
        let untracked = Tracking {
            untracked: true,
            ..stored("interest: label x")
        };
        assert_eq!(merged(untracked, Some(&matched)), None);
    }

    #[test]
    fn only_a_rule_match_has_anything_to_say_about_post_merge_review() {
        let keeps = TrackedReason::Interest {
            rule: "path task-sdk/**".into(),
            after_merge: true,
        };
        let kept = merge_tracking(Tracking::default(), Some(&keeps));
        assert!(kept.after_merge);

        // An involvement search evaluates no rules, so being asked to review a PR
        // must not be what decides it stops mattering once it merges — even
        // though the reason it displays becomes the stronger one.
        let involved = TrackedReason::Involved("review_requested".into());
        let both = merge_tracking(kept.clone(), Some(&involved));
        assert_eq!(both.reason.as_deref(), Some("involved: review_requested"));
        assert!(both.after_merge);

        assert!(
            merge_tracking(kept, None).after_merge,
            "and a sweep that says nothing leaves the stored answer alone"
        );

        // A rule that no longer asks for it is the one thing that takes it back.
        let lets_go = TrackedReason::Interest {
            rule: "path task-sdk/**".into(),
            after_merge: false,
        };
        assert!(!merge_tracking(both, Some(&lets_go)).after_merge);
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
            .upsert_pr(repo_id, p, Some(interest("label area:task-sdk")), now())
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
            .commit_detail(repo_id, 1, &state, &[], &[], &[], None, now())
            .unwrap()
            .expect_applied();

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
    fn a_detail_fetched_earlier_cannot_overwrite_one_fetched_later() {
        // Two fetches of one PR overlap — a `sync` and the interface's refresh
        // key, in separate processes. The one that *fetched* later has the truer
        // view, so commit order must not decide it: the loser is dropped whole,
        // rather than reverting the threads, attention and description that the
        // winner stored.
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));

        let winner = ts("2026-08-05T12:00:05Z");
        let loser = ts("2026-08-05T12:00:00Z");
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::Mention { by: "kaxil".into() },
                    "2026-08-05T11:00:00Z",
                )],
                Some("the newer body"),
                winner,
            )
            .unwrap()
            .expect_applied();

        let outcome = ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[],
                Some("the older body"),
                loser,
            )
            .unwrap();

        assert_eq!(
            outcome,
            Committed::Superseded {
                stored: "2026-08-05T12:00:05Z".to_string()
            },
            "the older fetch must be told it was dropped, not silently ignored"
        );
        let shown = ledger.show(repo_id, 1).unwrap().expect("stored");
        assert_eq!(
            shown.body.as_deref(),
            Some("the newer body"),
            "the description the winner stored survives"
        );
        assert_eq!(
            shown.attention.len(),
            1,
            "and so does the attention it computed"
        );
    }

    #[test]
    fn a_detail_committed_twice_leaves_the_same_rows() {
        // Idempotent: the same pass applied again is the same end state, so a
        // retried commit needs no thought about what it might duplicate.
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        let at = ts("2026-08-05T12:00:00Z");
        let commit = || {
            ledger
                .commit_detail(
                    repo_id,
                    1,
                    &MyState::default(),
                    &[ThreadState {
                        thread_id: "T1".into(),
                        i_own: true,
                        is_resolved: false,
                        resolved_by: None,
                        last_comment_author: Some("kaxil".into()),
                        last_comment_at: Some(ts("2026-08-05T11:00:00Z")),
                        my_last_comment_at: Some(ts("2026-08-05T10:00:00Z")),
                    }],
                    &[],
                    &[attn(
                        AttentionReason::Mention { by: "kaxil".into() },
                        "2026-08-05T11:00:00Z",
                    )],
                    Some("body"),
                    at,
                )
                .unwrap()
        };

        assert_eq!(commit(), Committed::Applied);
        assert_eq!(
            commit(),
            Committed::Applied,
            "the same instant is not newer than itself, so a re-run still applies"
        );

        let shown = ledger.show(repo_id, 1).unwrap().expect("stored");
        assert_eq!(shown.threads.len(), 1, "not duplicated");
        assert_eq!(shown.attention.len(), 1, "nor this");
        assert_eq!(shown.body.as_deref(), Some("body"));
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
            .commit_detail(repo_id, 1, &MyState::default(), &[], &[], &[], None, now())
            .unwrap()
            .expect_applied();
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        // Most recently submitted first.
        assert_eq!(show.reviewers, vec![changes_requested, approved]);

        // A second detail pass with nobody left approving replaces the row
        // rather than accumulating alongside it.
        ledger
            .commit_detail(repo_id, 1, &MyState::default(), &[], &[], &[], None, now())
            .unwrap()
            .expect_applied();
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        // #2 only needs a first look, which is the bottom of the table.
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].pr.number, 1);
        assert_eq!(queue[0].top.reason.discriminant(), "mention");
        assert_eq!(queue[1].pr.number, 2);
        assert!(
            queue[0].top.priority() < queue[1].top.priority(),
            "the more urgent band leads, whatever the numbers are"
        );
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

        let waiting = ledger.waiting(repo_id).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].pr.number, 2);
    }

    #[test]
    fn marking_detail_unavailable_drops_it_off_the_queue_and_stops_the_retries() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        wants_attention(
            &ledger,
            repo_id,
            1,
            AttentionReason::Mention {
                by: "potiuk".into(),
            },
            "2026-08-05T09:00:00Z",
        );
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);

        ledger
            .mark_detail_unavailable(repo_id, 1, ts("2026-08-10T12:00:00Z"))
            .unwrap();

        assert!(
            ledger.queue(repo_id).unwrap().is_empty(),
            "a PR the forge can't resolve must not sit on the queue"
        );
        assert!(
            ledger
                .prs_needing_detail(repo_id, false)
                .unwrap()
                .is_empty(),
            "and must not be refetched on every later sync"
        );
        // Still tracked: this records what the forge said, it doesn't forget the PR.
        assert_eq!(ledger.list_tracked(repo_id).unwrap().len(), 1);
    }

    #[test]
    fn a_pr_that_reappears_becomes_due_for_detail_again() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .mark_detail_unavailable(repo_id, 1, ts("2026-08-05T13:00:00Z"))
            .unwrap();
        assert!(
            ledger
                .prs_needing_detail(repo_id, false)
                .unwrap()
                .is_empty()
        );

        // A later sweep sees it again, advancing updated_at past the stamp.
        let mut back = pr(1);
        back.updated_at = ts("2026-08-11T09:00:00Z");
        ledger.upsert_pr(repo_id, &back, None, now()).unwrap();

        assert_eq!(ledger.prs_needing_detail(repo_id, false).unwrap().len(), 1);
    }

    #[test]
    fn an_attention_row_is_rendered_from_storage_not_frozen_at_write_time() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        let reason = AttentionReason::Mention {
            by: "potiuk".into(),
        };
        wants_attention(&ledger, repo_id, 1, reason.clone(), "2026-08-05T09:00:00Z");

        // What comes back is the reason itself, so its prose is whatever
        // reviewq-core renders *now* — not what it rendered when this was
        // synced. That's the point of storing the payload: improving the
        // wording doesn't need a re-sync to take effect.
        let stored = &ledger.show(repo_id, 1).unwrap().expect("stored").attention[0];
        assert_eq!(stored.reason, reason);
        assert_eq!(stored.reason.to_string(), reason.to_string());
        assert_eq!(
            stored.priority(),
            reason.priority(),
            "and its band is the reason's own, read back rather than stored"
        );

        // And no column holds prerendered prose for it to disagree with.
        let columns: Vec<String> = ledger
            .conn
            .prepare("SELECT * FROM attention")
            .unwrap()
            .column_names()
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        assert!(!columns.contains(&"detail".to_string()), "{columns:?}");
    }

    #[test]
    fn a_file_backed_ledger_enables_wal_and_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();

        let mode: String = ledger
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");

        let timeout: i64 = ledger
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, BUSY_TIMEOUT.as_millis() as i64);
    }

    #[test]
    fn a_second_handle_sees_what_the_first_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let writer = Ledger::open(&path).unwrap();
        // Open before the write, as a long-lived reader would be.
        let reader = Ledger::open(&path).unwrap();
        let repo_id = writer.ensure_repo(&repo()).unwrap();

        track(&writer, repo_id, &pr(1));

        let (seen_id, seen) = reader
            .repos()
            .unwrap()
            .into_iter()
            .next()
            .expect("the repo");
        assert_eq!(seen, repo());
        assert_eq!(reader.list_tracked(seen_id).unwrap().len(), 1);
    }

    /// Give an already-tracked PR one attention reason, putting it on the queue.
    fn wants_attention(
        ledger: &Ledger,
        repo_id: i64,
        number: u64,
        reason: AttentionReason,
        since: &str,
    ) {
        ledger
            .commit_detail(
                repo_id,
                number,
                &MyState::default(),
                &[],
                &[],
                &[attn(reason, since)],
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
    }

    fn repo_named(owner: &str) -> RepoKey {
        RepoKey {
            host: "github.com".into(),
            owner: owner.into(),
            name: "repo".into(),
        }
    }

    #[test]
    fn queue_all_interleaves_every_repo_by_urgency() {
        let (ledger, first) = ledger_with_repo();
        let second = ledger.ensure_repo(&repo_named("someone")).unwrap();

        // The urgent PR is on the repo registered second, so concatenating each
        // repo's already-sorted queue would leave it last.
        track(&ledger, first, &pr(1));
        wants_attention(
            &ledger,
            first,
            1,
            AttentionReason::NeedsFirstLook { rule: "x".into() },
            "2026-08-01T00:00:00Z",
        );
        track(&ledger, second, &pr(2));
        wants_attention(
            &ledger,
            second,
            2,
            AttentionReason::Mention {
                by: "potiuk".into(),
            },
            "2026-08-05T09:00:00Z",
        );

        let got: Vec<(String, u64)> = ledger
            .queue_all()
            .unwrap()
            .iter()
            .map(|l| (l.repo.slug(), l.item.pr.number))
            .collect();
        assert_eq!(
            got,
            vec![
                ("someone/repo".to_string(), 2),
                ("apache/airflow".to_string(), 1)
            ]
        );
    }

    #[test]
    fn queue_all_breaks_a_tie_on_repo_not_registration_order() {
        let ledger = Ledger::open_in_memory().unwrap();
        let zzz = ledger.ensure_repo(&repo_named("zzz")).unwrap();
        let aaa = ledger.ensure_repo(&repo_named("aaa")).unwrap();

        // Same number, same reason, same instant — everything but the repo ties.
        for repo_id in [zzz, aaa] {
            track(&ledger, repo_id, &pr(1));
            wants_attention(
                &ledger,
                repo_id,
                1,
                AttentionReason::Mention {
                    by: "potiuk".into(),
                },
                "2026-08-05T09:00:00Z",
            );
        }

        let slugs: Vec<String> = ledger
            .queue_all()
            .unwrap()
            .iter()
            .map(|l| l.repo.slug())
            .collect();
        assert_eq!(slugs, vec!["aaa/repo", "zzz/repo"]);
    }

    #[test]
    fn tracked_all_and_waiting_all_order_by_repo_then_number() {
        let ledger = Ledger::open_in_memory().unwrap();
        let zzz = ledger.ensure_repo(&repo_named("zzz")).unwrap();
        let aaa = ledger.ensure_repo(&repo_named("aaa")).unwrap();
        track(&ledger, zzz, &pr(2));
        track(&ledger, zzz, &pr(1));
        track(&ledger, aaa, &pr(3));

        let expected = vec![
            ("aaa/repo".to_string(), 3),
            ("zzz/repo".to_string(), 1),
            ("zzz/repo".to_string(), 2),
        ];
        let key = |l: &Located<TrackedPr>| (l.repo.slug(), l.item.pr.number);
        assert_eq!(
            ledger
                .tracked_all()
                .unwrap()
                .iter()
                .map(key)
                .collect::<Vec<_>>(),
            expected
        );
        // Nothing has attention, so every tracked PR is also waiting.
        assert_eq!(
            ledger
                .waiting_all()
                .unwrap()
                .iter()
                .map(key)
                .collect::<Vec<_>>(),
            expected
        );
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
                None,
                ts("2026-08-06T00:00:00Z"),
            )
            .unwrap()
            .expect_applied();

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

    #[test]
    fn a_merged_pr_whose_rule_asked_for_it_is_fetched_without_the_project_opt_in() {
        let (ledger, repo_id) = ledger_with_repo();
        let mut merged = pr(1);
        merged.state = PrState::Merged;
        ledger
            .upsert_pr(
                repo_id,
                &merged,
                Some(TrackedReason::Interest {
                    rule: "path task-sdk/**".into(),
                    after_merge: true,
                }),
                now(),
            )
            .unwrap();

        let need = ledger.prs_needing_detail(repo_id, false).unwrap();
        assert_eq!(need.len(), 1);
        assert!(need[0].after_merge, "and it says why it is still here");
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert!(ledger.waiting(repo_id).unwrap().is_empty());
    }

    #[test]
    fn a_queue_row_carries_my_own_history_on_the_pr() {
        // So a list can show what I have already done to it without reading each
        // row in turn — which is what kept `done` invisible outside the detail.
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
                &[mention("potiuk", "2026-08-05T09:00:00Z")],
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        ledger
            .commit_detail(
                repo_id,
                2,
                &MyState::default(),
                &[],
                &[],
                &[mention("kaxil", "2026-08-05T09:00:00Z")],
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        ledger
            .set_done(repo_id, 1, "abc123", ts("2026-08-05T11:00:00Z"))
            .unwrap();

        let queue = ledger.queue(repo_id).unwrap();

        let row = |number: u64| {
            queue
                .iter()
                .find(|item| item.pr.number == number)
                .expect("queued")
        };
        assert_eq!(row(1).my_state.done_sha.as_deref(), Some("abc123"));
        assert_eq!(
            row(2).my_state,
            MyState::default(),
            "a PR nobody has acted on reads as the default, not as an error"
        );
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
    fn clearing_archived_attention_spares_the_merged_prs_a_rule_keeps() {
        // The per-rule half: this project keeps nothing after merge, but the rule
        // that tracked #1 asked for it, so its attention survives while #2's goes.
        let (ledger, repo_id) = ledger_with_repo();
        for (number, after_merge) in [(1, true), (2, false)] {
            let mut merged = pr(number);
            merged.state = PrState::Merged;
            ledger
                .upsert_pr(
                    repo_id,
                    &merged,
                    Some(TrackedReason::Interest {
                        rule: "path task-sdk/**".into(),
                        after_merge,
                    }),
                    now(),
                )
                .unwrap();
            ledger
                .commit_detail(
                    repo_id,
                    number,
                    &MyState::default(),
                    &[],
                    &[],
                    &[mention("potiuk", "2026-08-05T09:00:00Z")],
                    None,
                    now(),
                )
                .unwrap()
                .expect_applied();
        }

        ledger.clear_archived_attention(repo_id, false).unwrap();

        let queue = ledger.queue(repo_id).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].pr.number, 1);
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
                    None,
                    now(),
                )
                .unwrap()
                .expect_applied();
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

        ledger.clear_done_attention(repo_id, 1).unwrap();
        let attention = ledger.show(repo_id, 1).unwrap().unwrap().attention;
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].reason.discriminant(), "review_requested");
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
                Some(interest("label area:task-sdk")),
                now(),
            )
            .unwrap();
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: manual"
        );
    }

    #[test]
    fn untrack_drops_a_pr_from_every_list_and_keeps_a_rule_from_taking_it_back() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        let committed = ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[attn(
                    AttentionReason::Mention { by: "kaxil".into() },
                    "2026-08-11T10:00:00Z",
                )],
                None,
                now(),
            )
            .unwrap();
        assert_eq!(committed, Committed::Applied);
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);

        assert!(ledger.untrack(repo_id, 1, now()).unwrap());

        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());
        assert!(ledger.queue(repo_id).unwrap().is_empty());
        assert!(ledger.waiting(repo_id).unwrap().is_empty());
        assert!(
            ledger.show(repo_id, 1).unwrap().is_some(),
            "still stored — untracking is a decision about watching it, not a delete"
        );

        // The rule that tracked it still matches, and the next sweep says so.
        // Without the stamp this is where the untrack would quietly undo itself.
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(interest("label area:task-sdk")),
                now(),
            )
            .unwrap();
        assert!(
            ledger.list_tracked(repo_id).unwrap().is_empty(),
            "a sweep must not take back a PR you said you were finished with"
        );
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.tracked_reason, None);
    }

    #[test]
    fn track_is_how_an_untracked_pr_comes_back() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger.untrack(repo_id, 1, now()).unwrap();

        assert!(ledger.track(repo_id, 1).unwrap());
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: manual"
        );

        // And the stamp is gone with it, so a sweep may write a real reason
        // over the manual one again.
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(interest("label area:task-sdk")),
                now(),
            )
            .unwrap();
        assert_eq!(ledger.list_tracked(repo_id).unwrap().len(), 1);
    }

    #[test]
    fn untracking_a_pr_the_ledger_never_had_changes_nothing() {
        let (ledger, repo_id) = ledger_with_repo();
        assert!(!ledger.untrack(repo_id, 404, now()).unwrap());
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();

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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
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
                None,
                now(),
            )
            .unwrap()
            .expect_applied();
        assert!(!ledger.queue(repo_id).unwrap()[0].deferred);
    }
}
