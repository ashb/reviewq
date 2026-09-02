//! The SQLite ledger.
//!
//! A thin, typed wrapper over Diesel. It owns the schema and migrations and
//! trades in `reviewq-core` snapshot types; nothing above it writes SQL. The
//! sync API is synchronous, which is fine for a CLI.

mod activity;
mod attention_activity;
mod connection;
mod db_types;
mod detail_activity;
mod migrations;
mod models;
mod schema;

use std::{cell::RefCell, collections::BTreeMap};

use connection::DbConnection;
use db_types::{DbPrState, DbTimestamp};
use diesel::{
    connection::SimpleConnection,
    deserialize::FromSqlRow,
    dsl::{case_when, count, count_star},
    expression::AsExpression,
    prelude::*,
    sql_types::{BigInt, Text},
    sqlite::Sqlite,
    upsert::excluded,
};
use jiff::Timestamp;
use models::{
    AttentionRecord, ForgeState, LabelRecord, MyStateRecord, NewPr, Pr, PrSummary, ReviewerRecord,
    ThreadRecord,
};
use reviewq_core::model::{
    Attention, AttentionReason, MyState, PrSnapshot, PrState, ReviewerVerdict, ThreadState, Verdict,
};
use schema::{attention, labels, my_state, prs, repos, reviewers, sync_meta, threads};

pub use activity::{
    ActivityBackfill, ActivityCursor, ActivityEvent, ActivityEventId, ActivityIncremental,
    ActivityPage, ActivityPageCommit, ActivityRateLimitUnit, ActivityScope, CleanupPreview,
    NewActivityEvent,
};
use activity::{record_state_transition_row, request_activity_refresh, whole_second};
pub use migrations::SCHEMA_VERSION;

/// A repository row in this ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, AsExpression, FromSqlRow)]
#[diesel(sql_type = BigInt)]
pub struct RepoId(i64);

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

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

    /// Diesel requires a UTF-8 SQLite database URL.
    #[error("database path is not valid UTF-8: {}", path.display())]
    InvalidPath {
        /// The path that could not be represented without changing it.
        path: std::path::PathBuf,
    },

    /// Somebody else held the write lock for longer than the busy timeout.
    #[error(
        "another reviewq held the ledger's write lock for more than {}s — it is \
         probably mid-sync; try again",
        BUSY_TIMEOUT.as_secs()
    )]
    Busy {
        /// What SQLite reported.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
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
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl From<diesel::result::Error> for LedgerError {
    /// Classify as it converts, so a `?` anywhere in the crate yields [`Busy`]
    /// rather than burying it in a message only a human can read.
    ///
    /// [`Busy`]: LedgerError::Busy
    fn from(source: diesel::result::Error) -> Self {
        classify_sql_error(source, "talking to the ledger")
    }
}

/// Whether SQLite gave up waiting for the write lock.
fn is_busy(mut err: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if matches!(
            err.downcast_ref::<diesel::result::Error>(),
            Some(diesel::result::Error::DatabaseError(_, information))
                if matches!(information.message(), "database is locked" | "database table is locked")
        ) {
            return true;
        }
        let Some(source) = err.source() else {
            return false;
        };
        err = source;
    }
}

fn classify_sql_error(source: diesel::result::Error, doing: impl Into<String>) -> LedgerError {
    if is_busy(&source) {
        return LedgerError::Busy {
            source: Box::new(source),
        };
    }
    if let diesel::result::Error::DeserializationError(source) = source {
        return LedgerError::Corrupt {
            what: "stored value".into(),
            source,
        };
    }
    LedgerError::Sql {
        doing: doing.into(),
        source: Box::new(source),
    }
}

/// Every fallible operation here fails with a [`LedgerError`].
pub type Result<T> = std::result::Result<T, LedgerError>;

/// Say what a SQLite failure was for, keeping the busy case distinguishable.
trait Doing<T> {
    /// Wrap a failure with what was being attempted.
    fn doing(self, what: impl Into<String>) -> Result<T>;
}

impl<T> Doing<T> for diesel::QueryResult<T> {
    fn doing(self, what: impl Into<String>) -> Result<T> {
        self.map_err(|source| classify_sql_error(source, what))
    }
}

/// Say what was being encoded, when it cannot be turned into storage.
///
/// Values read from storage are decoded separately and reported as
/// [`LedgerError::Corrupt`].
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
fn prepare_conn(conn: &mut DbConnection) -> Result<()> {
    conn.batch_execute("PRAGMA foreign_keys = ON;")
        .doing("enabling foreign keys")?;
    conn.batch_execute(&format!(
        "PRAGMA busy_timeout = {};",
        BUSY_TIMEOUT.as_millis()
    ))
    .doing("setting the busy timeout")?;
    conn.batch_execute("PRAGMA journal_mode = WAL;")
        .doing("enabling WAL")?;
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
    pub repo_id: RepoId,
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
        stored: Timestamp,
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
    conn: RefCell<DbConnection>,
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

/// Which tracked PRs a detail pass should fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Those whose detail is older than the PR — the ordinary sync.
    Stale,
    /// Every one of them, however quiet.
    ///
    /// What a reason *means* is decided when a PR's detail is classified, so a
    /// build that adds a reason changes nothing about a PR nobody has touched
    /// since. This is how that gets applied without waiting for the world to
    /// move: it costs a fetch per tracked PR, which is why it is asked for
    /// rather than done.
    Every,
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
        let database_url = path.to_str().ok_or_else(|| LedgerError::InvalidPath {
            path: path.to_owned(),
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .on_disk(format!("creating ledger dir {}", dir.display()))?;
        }
        let conn = connection::establish(database_url).map_err(|source| LedgerError::Sql {
            doing: format!("opening ledger {}", path.display()),
            source: Box::new(source),
        })?;
        Self::from_conn(conn)
    }

    /// An in-memory ledger, for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = connection::establish(":memory:").map_err(|source| LedgerError::Sql {
            doing: "opening an in-memory ledger".into(),
            source: Box::new(source),
        })?;
        Self::from_conn(conn)
    }

    fn from_conn(mut conn: DbConnection) -> Result<Self> {
        prepare_conn(&mut conn)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: RefCell::new(conn),
        })
    }

    /// `repo`'s id, if the ledger already knows it. A read — see
    /// [`ensure_repo`](Self::ensure_repo) for the version that registers one.
    pub fn repo_id(&self, repo: &RepoKey) -> Result<Option<RepoId>> {
        repos::table
            .filter(repos::host.eq(&repo.host))
            .filter(repos::owner.eq(&repo.owner))
            .filter(repos::name.eq(&repo.name))
            .select(repos::id)
            .first::<RepoId>(&mut *self.conn.borrow_mut())
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
    pub fn ensure_repo(&self, repo: &RepoKey) -> Result<RepoId> {
        let conn = &mut *self.conn.borrow_mut();
        let placeholder = repos::table
            .filter(repos::host.eq(""))
            .filter(repos::owner.eq(""))
            .filter(repos::name.eq(""))
            .select(repos::id)
            .first::<RepoId>(conn)
            .optional()
            .doing("checking for a pre-v4 placeholder repo")?;
        if let Some(id) = placeholder {
            diesel::update(repos::table.find(id))
                .set((
                    repos::host.eq(&repo.host),
                    repos::owner.eq(&repo.owner),
                    repos::name.eq(&repo.name),
                ))
                .execute(conn)
                .doing("adopting the pre-v4 placeholder repo")?;
            return Ok(id);
        }
        diesel::insert_into(repos::table)
            .values((
                repos::host.eq(&repo.host),
                repos::owner.eq(&repo.owner),
                repos::name.eq(&repo.name),
            ))
            .on_conflict_do_nothing()
            .execute(conn)
            .doing("registering repo")?;
        repos::table
            .filter(repos::host.eq(&repo.host))
            .filter(repos::owner.eq(&repo.owner))
            .filter(repos::name.eq(&repo.name))
            .select(repos::id)
            .first::<RepoId>(conn)
            .doing("resolving repo id")
    }

    /// Every repo this ledger knows about, in no particular order — what
    /// `list`/`next` iterate to build a queue spanning every repo. Ledger-only,
    /// like every other read here: it reflects whatever has actually been
    /// synced, not what a (possibly stale, possibly absent) config currently
    /// says should exist.
    pub fn repos(&self) -> Result<Vec<(RepoId, RepoKey)>> {
        repos::table
            .select((repos::id, repos::host, repos::owner, repos::name))
            .load::<(RepoId, String, String, String)>(&mut *self.conn.borrow_mut())
            .doing("listing repositories")
            .map(|rows| {
                rows.into_iter()
                    .map(|(id, host, owner, name)| (id, RepoKey { host, owner, name }))
                    .collect()
            })
    }

    /// Insert or update a PR, merging its tracked reason with any already
    /// stored. Returns whether the PR was newly inserted.
    pub fn upsert_pr(
        &self,
        repo_id: RepoId,
        pr: &PrSnapshot,
        reason: Option<TrackedReason>,
    ) -> Result<bool> {
        self.conn.borrow_mut().immediate_transaction(|conn| {
            upsert_row(conn, repo_id, pr, reason.as_ref(), Timestamp::now())
        })
    }

    /// Persist a whole sweep page and advance the cursor in one transaction, so
    /// an interrupted sync leaves a consistent checkpoint (and so the page's
    /// writes are one commit, not one per PR). Returns how many PRs were new.
    pub fn commit_sweep_page(
        &self,
        repo_id: RepoId,
        prs: &[(PrSnapshot, Option<TrackedReason>)],
        cursor_key: &str,
        cursor_value: &str,
    ) -> Result<u64> {
        let now = Timestamp::now();
        self.conn.borrow_mut().immediate_transaction(|conn| {
            let mut inserted_count = 0;
            for (pr, reason) in prs {
                inserted_count += u64::from(upsert_row(conn, repo_id, pr, reason.as_ref(), now)?);
            }
            set_meta_row(conn, repo_id, cursor_key, cursor_value)?;
            Ok(inserted_count)
        })
    }

    /// A metadata value, e.g. the sync cursor.
    pub fn get_meta(&self, repo_id: RepoId, key: &str) -> Result<Option<String>> {
        sync_meta::table
            .find((repo_id, key))
            .select(sync_meta::value)
            .first(&mut *self.conn.borrow_mut())
            .optional()
            .doing(format!("reading sync_meta {key}"))
    }

    /// Set a metadata value.
    pub fn set_meta(&self, repo_id: RepoId, key: &str, value: &str) -> Result<()> {
        set_meta_row(&mut self.conn.borrow_mut(), repo_id, key, value)
    }

    /// Every tracked PR, ordered by number.
    pub fn list_tracked(&self, repo_id: RepoId) -> Result<Vec<TrackedPr>> {
        tracked_prs(repo_id)
            .load::<(Pr, Option<String>, bool, Option<MyStateRecord>)>(&mut *self.conn.borrow_mut())
            .doing("listing tracked PRs")?
            .into_iter()
            .map(|(pr, reason, after_merge, state)| {
                tracked_from_stored(pr, reason, after_merge, state)
            })
            .collect()
    }

    /// `(tracked, total)` PR counts, for the sync summary.
    pub fn counts(&self, repo_id: RepoId) -> Result<(u64, u64)> {
        prs::table
            .filter(prs::repo_id.eq(repo_id))
            .select((
                count(case_when(
                    prs::tracked_reason.is_not_null(),
                    1_i64.into_sql::<BigInt>(),
                )),
                count_star(),
            ))
            .first::<(i64, i64)>(&mut *self.conn.borrow_mut())
            .doing("counting PRs")
            .map(|(tracked, total)| (tracked as u64, total as u64))
    }

    /// Record the colours a repo paints its labels, replacing any it has moved
    /// on from.
    ///
    /// Per repo, because a colour is the repo's rather than the label's: the
    /// same name is painted differently in another project, and a table keyed by
    /// name alone would answer for whichever repo was swept last.
    pub fn set_label_colours(&self, repo_id: RepoId, labels: &[(String, String)]) -> Result<()> {
        if labels.is_empty() {
            return Ok(());
        }
        let records = labels
            .iter()
            .map(|(name, color)| LabelRecord {
                repo_id,
                name: name.clone(),
                color: color.clone(),
            })
            .collect::<Vec<_>>();
        diesel::insert_into(labels::table)
            .values(&records)
            .on_conflict((labels::repo_id, labels::name))
            .do_update()
            .set(labels::color.eq(excluded(labels::color)))
            .execute(&mut *self.conn.borrow_mut())
            .doing("storing label colours")?;
        Ok(())
    }

    /// One repo's label colours, by name — what a frontend needs to paint a row
    /// the way the forge does.
    pub fn label_colours(&self, repo_id: RepoId) -> Result<BTreeMap<String, String>> {
        labels::table
            .filter(labels::repo_id.eq(repo_id))
            .select((labels::name, labels::color))
            .load::<(String, String)>(&mut *self.conn.borrow_mut())
            .doing("reading label colours")
            .map(IntoIterator::into_iter)
            .map(Iterator::collect)
    }

    /// One repo's stored PR rows, counted the ways that say what the ledger is
    /// accumulating. See [`Census`].
    pub fn census(&self, repo_id: RepoId) -> Result<Census> {
        let mine = my_state::done_at
            .nullable()
            .is_not_null()
            .or(my_state::snoozed_until.nullable().is_not_null())
            .or(my_state::muted.nullable().eq(true))
            .or(my_state::deferred_at.nullable().is_not_null());
        let (total, tracked, open, merged, closed, mine, mine_untracked) = prs::table
            .left_join(
                my_state::table.on(my_state::repo_id
                    .eq(prs::repo_id)
                    .and(my_state::number.eq(prs::number))),
            )
            .filter(prs::repo_id.eq(repo_id))
            .select((
                count_star(),
                count(case_when(
                    prs::tracked_reason.is_not_null(),
                    1_i64.into_sql::<BigInt>(),
                )),
                count(case_when(
                    prs::state.eq(DbPrState::from(PrState::Open)),
                    1_i64.into_sql::<BigInt>(),
                )),
                count(case_when(
                    prs::state.eq(DbPrState::from(PrState::Merged)),
                    1_i64.into_sql::<BigInt>(),
                )),
                count(case_when(
                    prs::state.eq(DbPrState::from(PrState::Closed)),
                    1_i64.into_sql::<BigInt>(),
                )),
                count(case_when(mine, 1_i64.into_sql::<BigInt>())),
                count(case_when(
                    mine.and(prs::tracked_reason.is_null()),
                    1_i64.into_sql::<BigInt>(),
                )),
            ))
            .get_result::<(i64, i64, i64, i64, i64, i64, i64)>(&mut *self.conn.borrow_mut())
            .doing("counting stored PRs")?;
        Ok(Census {
            total: total as u64,
            tracked: tracked as u64,
            open: open as u64,
            merged: merged as u64,
            closed: closed as u64,
            mine: mine as u64,
            mine_untracked: mine_untracked as u64,
        })
    }

    /// Count of stored PRs whose file list GitHub truncated and that matched no
    /// rule — the "unknown, not non-matching" set `doctor` should surface.
    pub fn count_truncated_untracked(&self, repo_id: RepoId) -> Result<u64> {
        let n = prs::table
            .filter(prs::repo_id.eq(repo_id))
            .filter(prs::files_truncated.eq(true))
            .filter(prs::tracked_reason.is_null())
            .count()
            .get_result::<i64>(&mut *self.conn.borrow_mut())?;
        Ok(n as u64)
    }

    /// Tracked PRs whose detail needs a (re-)fetch: never fetched, or fetched
    /// before the last time the PR changed — or every tracked PR, when asked.
    /// Open PRs always; a merged PR when `include_merged` (the per-project
    /// post-merge-review opt-in) or when the rule that tracked it said
    /// `after_merge`; closed-unmerged PRs never. Returns the full snapshot and
    /// tracked reason so the caller can classify without a second read.
    pub fn prs_needing_detail(
        &self,
        repo_id: RepoId,
        include_merged: bool,
        which: Detail,
    ) -> Result<Vec<TrackedPr>> {
        let mut query = tracked_prs(repo_id).into_boxed();
        query = if include_merged {
            query.filter(
                prs::state
                    .eq(DbPrState::from(PrState::Open))
                    .or(prs::state.eq(DbPrState::from(PrState::Merged))),
            )
        } else {
            query.filter(
                prs::state.eq(DbPrState::from(PrState::Open)).or(prs::state
                    .eq(DbPrState::from(PrState::Merged))
                    .and(prs::after_merge.eq(true))),
            )
        };
        if which == Detail::Stale {
            query = query.filter(
                prs::detail_synced_at
                    .is_null()
                    .or(prs::detail_synced_at.lt(prs::updated_at.nullable())),
            );
        }
        query
            .load::<(Pr, Option<String>, bool, Option<MyStateRecord>)>(&mut *self.conn.borrow_mut())
            .doing("listing PRs needing detail")?
            .into_iter()
            .map(|(pr, reason, after_merge, state)| {
                tracked_from_stored(pr, reason, after_merge, state)
            })
            .collect()
    }

    /// My history on a PR, or the default (all-empty) state if none is stored.
    pub fn my_state(&self, repo_id: RepoId, number: u64) -> Result<MyState> {
        load_my_state(&mut self.conn.borrow_mut(), repo_id, number)?
            .map(my_state_from_stored)
            .transpose()
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
        repo_id: RepoId,
        number: u64,
        done_sha: &str,
        done_at: Timestamp,
    ) -> Result<()> {
        let done_at = DbTimestamp::from(done_at);
        diesel::insert_into(my_state::table)
            .values((
                my_state::repo_id.eq(repo_id),
                my_state::number.eq(number as i64),
                my_state::done_sha.eq(done_sha),
                my_state::done_at.eq(&done_at),
            ))
            .on_conflict((my_state::repo_id, my_state::number))
            .do_update()
            .set((
                my_state::done_sha.eq(done_sha),
                my_state::done_at.eq(&done_at),
            ))
            .execute(&mut *self.conn.borrow_mut())
            .doing(format!("recording done for #{number}"))?;
        Ok(())
    }

    /// Record `reviewq snooze`. Touches only `snoozed_until`; see
    /// [`set_done`](Self::set_done) for why that matters.
    pub fn set_snoozed_until(&self, repo_id: RepoId, number: u64, until: Timestamp) -> Result<()> {
        let until = DbTimestamp::from(until);
        diesel::insert_into(my_state::table)
            .values((
                my_state::repo_id.eq(repo_id),
                my_state::number.eq(number as i64),
                my_state::snoozed_until.eq(&until),
            ))
            .on_conflict((my_state::repo_id, my_state::number))
            .do_update()
            .set(my_state::snoozed_until.eq(&until))
            .execute(&mut *self.conn.borrow_mut())
            .doing(format!("snoozing #{number}"))?;
        Ok(())
    }

    /// Record `reviewq mute`/`unmute`. Touches only `muted`.
    pub fn set_muted(&self, repo_id: RepoId, number: u64, muted: bool) -> Result<()> {
        diesel::insert_into(my_state::table)
            .values((
                my_state::repo_id.eq(repo_id),
                my_state::number.eq(number as i64),
                my_state::muted.eq(muted),
            ))
            .on_conflict((my_state::repo_id, my_state::number))
            .do_update()
            .set(my_state::muted.eq(muted))
            .execute(&mut *self.conn.borrow_mut())
            .doing(format!("setting muted for #{number}"))?;
        Ok(())
    }

    /// Record `reviewq defer`/`undefer`. Touches only `deferred_at`.
    pub fn set_deferred_at(
        &self,
        repo_id: RepoId,
        number: u64,
        deferred_at: Option<Timestamp>,
    ) -> Result<()> {
        let deferred_at = deferred_at.map(DbTimestamp::from);
        diesel::insert_into(my_state::table)
            .values((
                my_state::repo_id.eq(repo_id),
                my_state::number.eq(number as i64),
                my_state::deferred_at.eq(&deferred_at),
            ))
            .on_conflict((my_state::repo_id, my_state::number))
            .do_update()
            .set(my_state::deferred_at.eq(&deferred_at))
            .execute(&mut *self.conn.borrow_mut())
            .doing(format!("setting deferred_at for #{number}"))?;
        Ok(())
    }

    /// Drop a PR's attention rows immediately, without waiting for the next
    /// sync to reclassify — how `snooze` and `mute` take effect on the queue
    /// right away. Clears every reason, `review_requested` included: that's
    /// what snooze/mute both mean (`classify` suppresses everything for
    /// either). `done` uses the narrower
    /// [`clear_done_attention`](Self::clear_done_attention) instead.
    pub fn clear_attention(&self, repo_id: RepoId, number: u64) -> Result<()> {
        diesel::delete(
            attention::table
                .filter(attention::repo_id.eq(repo_id))
                .filter(attention::pr_number.eq(number as i64)),
        )
        .execute(&mut *self.conn.borrow_mut())
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
    pub fn mark_detail_unavailable(
        &self,
        repo_id: RepoId,
        number: u64,
        now: Timestamp,
    ) -> Result<()> {
        self.conn.borrow_mut().transaction(|conn| {
            let prior_attention = attention_activity::load(conn, repo_id, number)?;
            diesel::delete(
                attention::table
                    .filter(attention::repo_id.eq(repo_id))
                    .filter(attention::pr_number.eq(number as i64)),
            )
            .execute(conn)
            .doing(format!("clearing attention for unreachable #{number}"))?;
            diesel::update(prs::table.find((repo_id, number as i64)))
                .set(prs::detail_synced_at.eq(DbTimestamp::from(now)))
                .execute(conn)
                .doing(format!(
                    "stamping detail_synced_at for unreachable #{number}"
                ))?;
            attention_activity::record(conn, repo_id, number, prior_attention, now)?;
            Ok(())
        })
    }

    /// The instant-hide half of `reviewq done`: every reason `done` is allowed
    /// to clear per the reason table, but not `review_requested` — only my
    /// review or the request being withdrawn clears that one.
    pub fn clear_done_attention(&self, repo_id: RepoId, number: u64) -> Result<()> {
        diesel::delete(
            attention::table
                .filter(attention::repo_id.eq(repo_id))
                .filter(attention::pr_number.eq(number as i64))
                .filter(attention::reason.ne("review_requested")),
        )
        .execute(&mut *self.conn.borrow_mut())
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
    pub fn track(&self, repo_id: RepoId, number: u64) -> Result<bool> {
        let conn = &mut *self.conn.borrow_mut();
        if tracked_reason(conn, repo_id, number)?.is_some() {
            return Ok(false);
        }
        diesel::update(prs::table.find((repo_id, number as i64)))
            .set((
                prs::tracked_reason.eq(TrackedReason::Involved("manual".into()).render()),
                prs::untracked_at.eq(None::<String>),
            ))
            .execute(conn)
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
    pub fn untrack(&self, repo_id: RepoId, number: u64, now: Timestamp) -> Result<bool> {
        self.conn.borrow_mut().transaction(|conn| {
            let changed = diesel::update(prs::table.find((repo_id, number as i64)))
                .set((
                    prs::tracked_reason.eq(None::<String>),
                    prs::untracked_at.eq(DbTimestamp::from(now)),
                ))
                .execute(conn)
                .doing(format!("untracking #{number}"))?;
            if changed == 0 {
                return Ok(false);
            }
            diesel::delete(
                attention::table
                    .filter(attention::repo_id.eq(repo_id))
                    .filter(attention::pr_number.eq(number as i64)),
            )
            .execute(conn)
            .doing(format!("clearing attention for untracked #{number}"))?;
            Ok(true)
        })
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
        repo_id: RepoId,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        attention: &[Attention],
        body: Option<&str>,
        now: Timestamp,
    ) -> Result<Committed> {
        self.commit_detail_inner(
            repo_id, number, my_state, threads, reviewers, attention, body, None, None, now,
        )
    }

    /// Persist tier-2 detail and the lifecycle state fetched with it behind one
    /// watermark compare-and-set and in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_detail_with_lifecycle(
        &self,
        repo_id: RepoId,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        attention: &[Attention],
        body: Option<&str>,
        state: PrState,
        state_changed_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<Committed> {
        self.commit_detail_inner(
            repo_id,
            number,
            my_state,
            threads,
            reviewers,
            attention,
            body,
            Some((state, state_changed_at)),
            None,
            now,
        )
    }

    /// Record attention evidence and classify the fetched detail in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn commit_detail_with_activity(
        &self,
        repo_id: RepoId,
        pr: &PrSnapshot,
        mine: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        body: &str,
        state_changed_at: Option<Timestamp>,
        events: &[NewActivityEvent],
        ctx: &reviewq_core::model::ClassifyCtx<'_>,
        now: Timestamp,
    ) -> Result<Committed> {
        self.commit_detail_inner(
            repo_id,
            pr.number,
            mine,
            threads,
            reviewers,
            &[],
            Some(body),
            Some((pr.state, state_changed_at)),
            Some((pr, events, ctx)),
            now,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_detail_inner(
        &self,
        repo_id: RepoId,
        number: u64,
        my_state: &MyState,
        threads: &[ThreadState],
        reviewers: &[ReviewerVerdict],
        attention: &[Attention],
        body: Option<&str>,
        lifecycle: Option<(PrState, Option<Timestamp>)>,
        activity: Option<detail_activity::DetailActivity<'_>>,
        now: Timestamp,
    ) -> Result<Committed> {
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
        let stamp = DbTimestamp::from(whole_second(now));
        self.conn.borrow_mut().transaction(|conn| {
            let target = prs::table.find((repo_id, number as i64)).filter(
                prs::detail_synced_at
                    .is_null()
                    .or(prs::detail_synced_at.le(&stamp)),
            );
            let applied = diesel::update(target)
                .set((
                    prs::detail_synced_at.eq(&stamp),
                    body.map(|body| prs::body.eq(body)),
                ))
                .execute(conn)
                .doing(format!("stamping detail_synced_at for #{number}"))?;
            if applied == 0 {
                let stored = prs::table
                    .find((repo_id, number as i64))
                    .select(prs::detail_synced_at)
                    .first::<Option<DbTimestamp>>(conn)
                    .optional()
                    .doing(format!("reading #{number}'s detail watermark"))?
                    .flatten();
                let Some(stored) = stored else {
                    return Err(LedgerError::NotStored { number });
                };
                return Ok(Committed::Superseded {
                    stored: stored.into_timestamp(),
                });
            }

            let prior_attention = attention_activity::load(conn, repo_id, number)?;
            if let Some((state, state_changed_at)) = lifecycle {
                record_state_transition_row(conn, repo_id, number, state, state_changed_at, now)?;
            }
            let had_attention = diesel::select(diesel::dsl::exists(
                schema::attention::table
                    .filter(schema::attention::repo_id.eq(repo_id))
                    .filter(schema::attention::pr_number.eq(number as i64)),
            ))
            .get_result::<bool>(conn)
            .doing(format!("checking attention before refreshing #{number}"))?;
            let classified;
            let attention = if let Some((pr, events, ctx)) = activity {
                let (resolutions, reviewed_at) = detail_activity::observe_detail(
                    conn, repo_id, number, threads, events, ctx.viewer, now,
                )?;
                let ctx = reviewq_core::model::ClassifyCtx {
                    resolutions: &resolutions,
                    reviewed_at,
                    ..ctx.clone()
                };
                classified = reviewq_core::model::classify(pr, my_state, threads, now, &ctx);
                &classified
            } else {
                attention
            };
            write_forge_state(conn, repo_id, number, my_state)?;
            if activity.is_none() {
                replace_threads(conn, repo_id, number, threads, None)?;
            }
            replace_reviewers(conn, repo_id, number, reviewers)?;
            replace_attention(conn, repo_id, number, attention)?;
            if activity.is_some() {
                attention_activity::record(conn, repo_id, number, prior_attention, now)?;
            }
            if had_attention || !attention.is_empty() {
                request_activity_refresh(conn, repo_id, number)?;
            }
            Ok(Committed::Applied)
        })
    }

    /// Drop attention rows that no longer belong to a queued PR: closed-unmerged
    /// PRs always, and merged PRs except the ones post-merge review keeps —
    /// `include_merged` for the whole project, or the tracking rule's own
    /// `after_merge`. Detail is never re-fetched for the rest (see
    /// [`prs_needing_detail`](Self::prs_needing_detail)), so without this their
    /// stale rows would linger and show up in `show`. Run once at the end of a
    /// sync.
    pub fn clear_archived_attention(
        &self,
        repo_id: RepoId,
        include_merged: bool,
        now: Timestamp,
    ) -> Result<()> {
        let mut archived = prs::table
            .filter(prs::repo_id.eq(repo_id))
            .filter(prs::state.ne(DbPrState::from(PrState::Open)))
            .filter(diesel::dsl::exists(
                attention::table
                    .filter(attention::repo_id.eq(prs::repo_id))
                    .filter(attention::pr_number.eq(prs::number)),
            ))
            .select(prs::number)
            .into_boxed();
        if include_merged {
            archived = archived.filter(prs::state.ne(DbPrState::from(PrState::Merged)));
        } else {
            archived = archived.filter(
                prs::state
                    .ne(DbPrState::from(PrState::Merged))
                    .or(prs::after_merge.eq(false)),
            );
        }
        self.conn.borrow_mut().transaction(|conn| {
            let numbers = archived.load::<i64>(conn).doing("reading archived PRs")?;
            for number in numbers {
                let number = number as u64;
                let before = attention_activity::load(conn, repo_id, number)?;
                replace_attention(conn, repo_id, number, &[])?;
                attention_activity::record(conn, repo_id, number, before, now)?;
            }
            Ok(())
        })
    }

    /// The queue: tracked, open PRs that currently want attention, each with its
    /// highest-priority reason, ordered most-urgent first (priority band, then
    /// oldest within the band, then PR number) — except a deferred PR (see
    /// [`QueueItem::deferred`]), which sorts after every non-deferred item
    /// regardless of priority.
    pub fn queue(&self, repo_id: RepoId) -> Result<Vec<QueueItem>> {
        self.queued(repo_id, Muted::Hidden)
    }

    /// What a mute is hiding: the same rows [`queue`](Self::queue) leaves out,
    /// in the same order, each with the reason it would be there for.
    ///
    /// The reasons are real — a mute stops nothing being computed, it only stops
    /// it being shown (see `classify`) — which is what makes this answerable at
    /// all, and what makes unmuting immediate rather than a wait for the next
    /// sync to rediscover them.
    pub fn muted(&self, repo_id: RepoId) -> Result<Vec<QueueItem>> {
        self.queued(repo_id, Muted::Only)
    }

    fn queued(&self, repo_id: RepoId, muted: Muted) -> Result<Vec<QueueItem>> {
        let mut query = prs::table
            .inner_join(
                attention::table.on(attention::repo_id
                    .eq(prs::repo_id)
                    .and(attention::pr_number.eq(prs::number))),
            )
            .left_join(
                my_state::table.on(my_state::repo_id
                    .eq(prs::repo_id)
                    .and(my_state::number.eq(prs::number))),
            )
            .filter(prs::repo_id.eq(repo_id))
            .filter(
                prs::state
                    .eq(DbPrState::from(PrState::Open))
                    .or(prs::state.eq(DbPrState::from(PrState::Merged))),
            )
            .filter(prs::tracked_reason.is_not_null())
            .select((
                Pr::as_select(),
                prs::tracked_reason,
                AttentionRecord::as_select(),
                Option::<MyStateRecord>::as_select(),
            ))
            .into_boxed();
        query = match muted {
            Muted::Hidden => query.filter(
                my_state::muted
                    .eq(false)
                    .nullable()
                    .or(my_state::muted.is_null()),
            ),
            Muted::Only => query.filter(my_state::muted.eq(true).nullable()),
        };
        let rows = query
            .load::<(Pr, Option<String>, AttentionRecord, Option<MyStateRecord>)>(
                &mut *self.conn.borrow_mut(),
            )
            .doing("reading queued PRs")?;

        let mut items: Vec<QueueItem> = Vec::new();
        for (pr, tracked_reason, attention, my_state) in rows {
            let tracked_reason = tracked_reason
                .ok_or_else(|| corrupt_message("tracked reason", "tracked row has no reason"))?;
            let pr = snapshot_from_stored(pr)?;
            let attention = attention_from_stored(attention)?;
            let my_state = my_state
                .map(my_state_from_stored)
                .transpose()?
                .unwrap_or_default();
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
            item.deferred = item.my_state.is_deferred(Some(item.top.since));
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
    pub fn waiting(&self, repo_id: RepoId) -> Result<Vec<TrackedPr>> {
        tracked_prs(repo_id)
            .left_join(
                attention::table.on(attention::repo_id
                    .eq(prs::repo_id)
                    .and(attention::pr_number.eq(prs::number))),
            )
            .filter(prs::state.eq(DbPrState::from(PrState::Open)))
            .filter(
                my_state::muted
                    .eq(false)
                    .nullable()
                    .or(my_state::muted.is_null()),
            )
            .filter(attention::repo_id.is_null())
            .load::<(Pr, Option<String>, bool, Option<MyStateRecord>)>(&mut *self.conn.borrow_mut())
            .doing("listing waiting PRs")?
            .into_iter()
            .map(|(pr, reason, after_merge, state)| {
                tracked_from_stored(pr, reason, after_merge, state)
            })
            .collect()
    }

    /// Run a per-repo read against every repo in [`repos`](Self::repos) and
    /// flatten the results, each tagged with the repo it came from.
    fn across_repos<T>(
        &self,
        read: impl Fn(&Self, RepoId) -> Result<Vec<T>>,
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
        read: fn(&Self, RepoId) -> Result<Vec<QueueItem>>,
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
    pub fn show(&self, repo_id: RepoId, number: u64) -> Result<Option<PrShow>> {
        let base = prs::table
            .find((repo_id, number as i64))
            .select((
                Pr::as_select(),
                prs::tracked_reason,
                prs::body,
                prs::after_merge,
            ))
            .first::<(Pr, Option<String>, Option<String>, bool)>(&mut *self.conn.borrow_mut())
            .optional()
            .doing(format!("reading PR #{number}"))?;
        let Some((stored, tracked_reason, body, after_merge)) = base else {
            return Ok(None);
        };
        let pr = snapshot_from_stored(stored)?;

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
    fn reviewers(&self, repo_id: RepoId, number: u64) -> Result<Vec<ReviewerVerdict>> {
        reviewers::table
            .filter(reviewers::repo_id.eq(repo_id))
            .filter(reviewers::pr_number.eq(number as i64))
            .order(reviewers::submitted_at.desc())
            .select(ReviewerRecord::as_select())
            .load::<ReviewerRecord>(&mut *self.conn.borrow_mut())
            .doing(format!("reading reviewers for #{number}"))?
            .into_iter()
            .map(reviewer_from_stored)
            .collect()
    }

    /// A PR's review threads, ordered by id for stability.
    fn threads(&self, repo_id: RepoId, number: u64) -> Result<Vec<ThreadState>> {
        threads::table
            .filter(threads::repo_id.eq(repo_id))
            .filter(threads::pr_number.eq(number as i64))
            .order(threads::thread_id)
            .select(ThreadRecord::as_select())
            .load::<ThreadRecord>(&mut *self.conn.borrow_mut())
            .doing(format!("reading threads for #{number}"))?
            .into_iter()
            .map(thread_from_stored)
            .collect()
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
        prs::table
            .inner_join(repos::table)
            .filter(prs::number.eq(number as i64))
            .select((repos::host, repos::owner, repos::name))
            .load::<(String, String, String)>(&mut *self.conn.borrow_mut())
            .doing(format!("finding repositories with PR #{number}"))
            .map(|rows| {
                rows.into_iter()
                    .map(|(host, owner, name)| RepoKey { host, owner, name })
                    .collect()
            })
    }

    /// A PR's attention rows, most-urgent first.
    fn attention(&self, repo_id: RepoId, number: u64) -> Result<Vec<AttentionRow>> {
        let mut rows = attention::table
            .filter(attention::repo_id.eq(repo_id))
            .filter(attention::pr_number.eq(number as i64))
            .select(AttentionRecord::as_select())
            .load::<AttentionRecord>(&mut *self.conn.borrow_mut())
            .doing(format!("reading attention for #{number}"))?
            .into_iter()
            .map(attention_from_stored)
            .collect::<Result<Vec<_>>>()?;
        rows.sort_by_key(|a| (a.priority(), a.since));
        Ok(rows)
    }
}

#[diesel::dsl::auto_type]
fn tracked_prs(repo_id: RepoId) -> _ {
    let pr: diesel::dsl::AsSelect<Pr, Sqlite> = Pr::as_select();
    let state: diesel::dsl::AsSelect<Option<MyStateRecord>, Sqlite> =
        Option::<MyStateRecord>::as_select();
    prs::table
        .left_join(
            my_state::table.on(my_state::repo_id
                .eq(prs::repo_id)
                .and(my_state::number.eq(prs::number))),
        )
        .filter(prs::repo_id.eq(repo_id))
        .filter(prs::tracked_reason.is_not_null())
        .order(prs::number)
        .select((pr, prs::tracked_reason, prs::after_merge, state))
}

/// Insert or update one PR, preserving its original first-seen timestamp.
fn upsert_row(
    conn: &mut DbConnection,
    repo_id: RepoId,
    pr: &PrSnapshot,
    reason: Option<&TrackedReason>,
    now: Timestamp,
) -> Result<bool> {
    let stored = prs::table
        .find((repo_id, pr.number as i64))
        .select((
            prs::updated_at,
            prs::tracked_reason,
            prs::after_merge,
            prs::untracked_at,
        ))
        .first::<(DbTimestamp, Option<String>, bool, Option<DbTimestamp>)>(conn)
        .optional()
        .doing(format!(
            "reading #{}'s sweep watermark and tracking",
            pr.number
        ))?;
    let is_new = stored.is_none();
    let (stored_at, tracking) = match stored {
        Some((updated_at, reason, after_merge, untracked_at)) => (
            Some(updated_at.into_timestamp()),
            Tracking {
                reason,
                after_merge,
                untracked: untracked_at.is_some(),
            },
        ),
        None => (None, Tracking::default()),
    };
    let merged = merge_tracking(tracking, reason);
    if stored_at.is_some_and(|stored_at| pr.updated_at <= stored_at) {
        diesel::update(prs::table.find((repo_id, pr.number as i64)))
            .set((
                prs::tracked_reason.eq(merged.reason),
                prs::after_merge.eq(merged.after_merge),
                prs::base_ref.eq(case_when::<_, _, Text>(prs::base_ref.eq(""), &pr.base_ref)
                    .otherwise(prs::base_ref)),
                prs::created_at.eq(case_when(
                    prs::created_at.is_null(),
                    pr.created_at.map(DbTimestamp::from),
                )
                .otherwise(prs::created_at)),
            ))
            .execute(conn)
            .doing(format!("merging tracking for stale PR #{}", pr.number))?;
        return Ok(false);
    }
    let record = NewPr {
        repo_id,
        pr: Pr::try_from(pr)?,
        tracked_reason: merged.reason,
        first_seen_at: DbTimestamp::from(now),
        detail_synced_at: None,
        body: None,
        after_merge: merged.after_merge,
        untracked_at: None,
    };
    let summary = PrSummary {
        title: &record.pr.title,
        author: &record.pr.author,
        author_association: &record.pr.author_association,
        head_sha: &record.pr.head_sha,
        is_draft: record.pr.is_draft,
        updated_at: &record.pr.updated_at,
        labels: &record.pr.labels,
        milestone: record.pr.milestone.as_deref(),
        files: record.pr.files.as_deref(),
        files_truncated: record.pr.files_truncated,
        base_ref: &record.pr.base_ref,
        created_at: record.pr.created_at.as_ref(),
    };
    diesel::insert_into(prs::table)
        .values(&record)
        .on_conflict((prs::repo_id, prs::number))
        .do_update()
        .set((
            &summary,
            prs::tracked_reason.eq(&record.tracked_reason),
            prs::after_merge.eq(record.after_merge),
        ))
        .execute(conn)
        .doing(format!("upserting PR #{}", pr.number))?;
    if !is_new {
        record_state_transition_row(conn, repo_id, pr.number, pr.state, pr.state_changed_at, now)?;
    }
    Ok(is_new)
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
        (Some(old), Some(new)) => {
            let new = new.render();
            Some(if stored_rank(&new) >= stored_rank(&old) {
                new
            } else {
                old
            })
        }
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

fn set_meta_row(conn: &mut DbConnection, repo_id: RepoId, key: &str, value: &str) -> Result<()> {
    diesel::insert_into(sync_meta::table)
        .values((
            sync_meta::repo_id.eq(repo_id),
            sync_meta::key.eq(key),
            sync_meta::value.eq(value),
        ))
        .on_conflict((sync_meta::repo_id, sync_meta::key))
        .do_update()
        .set(sync_meta::value.eq(excluded(sync_meta::value)))
        .execute(conn)
        .doing(format!("writing sync_meta {key}"))?;
    Ok(())
}

fn existing_row(conn: &mut DbConnection, repo_id: RepoId, number: u64) -> Result<Option<u64>> {
    prs::table
        .find((repo_id, number as i64))
        .select(prs::number)
        .first::<i64>(conn)
        .optional()
        .doing(format!("checking for PR #{number}"))
        .map(|number| number.map(|number| number as u64))
}

fn tracked_reason(conn: &mut DbConnection, repo_id: RepoId, number: u64) -> Result<Option<String>> {
    prs::table
        .find((repo_id, number as i64))
        .select(prs::tracked_reason)
        .first::<Option<String>>(conn)
        .optional()
        .doing(format!("reading tracked_reason for #{number}"))
        .map(Option::flatten)
}

fn corrupt(
    what: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> LedgerError {
    LedgerError::Corrupt {
        what: what.into(),
        source: Box::new(source),
    }
}

fn corrupt_message(what: impl Into<String>, message: impl Into<String>) -> LedgerError {
    corrupt(what, std::io::Error::other(message.into()))
}

fn snapshot_from_stored(row: Pr) -> Result<PrSnapshot> {
    let labels: Vec<String> =
        serde_json::from_str(&row.labels).map_err(|source| corrupt("label list", source))?;
    let files: Option<Vec<String>> = row
        .files
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .map_err(|source| corrupt("file list", source))?;

    Ok(PrSnapshot {
        number: row.number as u64,
        title: row.title,
        author: row.author,
        author_association: row.author_association,
        head_sha: row.head_sha,
        is_draft: row.is_draft,
        state: row.state.into_state(),
        updated_at: row.updated_at.into_timestamp(),
        labels,
        milestone: row.milestone,
        files,
        files_truncated: row.files_truncated,
        base_ref: row.base_ref,
        created_at: row.created_at.map(DbTimestamp::into_timestamp),
        state_changed_at: row.state_changed_at.map(DbTimestamp::into_timestamp),
    })
}

fn tracked_from_stored(
    row: Pr,
    tracked_reason: Option<String>,
    after_merge: bool,
    state: Option<MyStateRecord>,
) -> Result<TrackedPr> {
    let tracked_reason = tracked_reason
        .ok_or_else(|| corrupt_message("tracked reason", "tracked row has no reason"))?;
    Ok(TrackedPr {
        pr: snapshot_from_stored(row)?,
        tracked_reason,
        after_merge,
        my_state: state
            .map(my_state_from_stored)
            .transpose()?
            .unwrap_or_default(),
    })
}

fn load_my_state(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
) -> Result<Option<MyStateRecord>> {
    my_state::table
        .find((repo_id, number as i64))
        .select(MyStateRecord::as_select())
        .first(conn)
        .optional()
        .doing(format!("reading my_state for #{number}"))
}

fn my_state_from_stored(row: MyStateRecord) -> Result<MyState> {
    let MyStateRecord {
        repo_id: _,
        number: _,
        last_reviewed_sha,
        last_verdict,
        last_action_at,
        done_sha,
        snoozed_until,
        muted,
        deferred_at,
        done_at,
    } = row;
    Ok(MyState {
        last_reviewed_sha,
        last_verdict: last_verdict.as_deref().and_then(Verdict::from_wire),
        last_action_at: last_action_at.map(DbTimestamp::into_timestamp),
        done_sha,
        snoozed_until: snoozed_until.map(DbTimestamp::into_timestamp),
        muted,
        deferred_at: deferred_at.map(DbTimestamp::into_timestamp),
        done_at: done_at.map(DbTimestamp::into_timestamp),
    })
}

fn reviewer_from_stored(row: ReviewerRecord) -> Result<ReviewerVerdict> {
    let verdict = Verdict::from_wire(&row.verdict).ok_or_else(|| {
        corrupt_message("review verdict", format!("bad verdict {:?}", row.verdict))
    })?;
    Ok(ReviewerVerdict {
        login: row.login,
        verdict,
        at: row.submitted_at.into_timestamp(),
    })
}

fn thread_from_stored(row: ThreadRecord) -> Result<ThreadState> {
    Ok(ThreadState {
        thread_id: row.thread_id,
        i_own: row.i_own,
        is_resolved: row.is_resolved,
        resolved_by: row.resolved_by,
        last_comment_author: row.last_comment_author,
        last_comment_at: row.last_comment_at.map(DbTimestamp::into_timestamp),
        my_last_comment_at: row.my_last_comment_at.map(DbTimestamp::into_timestamp),
    })
}

/// Build an [`AttentionRow`] from `since`, `payload` at `base`.
///
/// The stored `reason` discriminant isn't read: the payload carries the whole
/// variant, discriminant included, so reading both would be two sources for one
/// fact. The column exists for the primary key.
fn attention_from_stored(row: AttentionRecord) -> Result<AttentionRow> {
    let reason: AttentionReason =
        serde_json::from_str(&row.payload).map_err(|source| corrupt("attention reason", source))?;
    Ok(AttentionRow {
        reason,
        since: row.since.into_timestamp(),
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
fn write_forge_state(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    s: &MyState,
) -> Result<()> {
    let last_action_at = s.last_action_at.map(DbTimestamp::from);
    let state = ForgeState {
        repo_id,
        number: number as i64,
        last_reviewed_sha: s.last_reviewed_sha.as_deref(),
        last_verdict: s.last_verdict.map(|verdict| verdict.as_str()),
        last_action_at: last_action_at.as_ref(),
    };
    diesel::insert_into(my_state::table)
        .values(&state)
        .on_conflict((my_state::repo_id, my_state::number))
        .do_update()
        .set(&state)
        .execute(conn)
        .doing(format!("writing forge-derived my_state for #{number}"))?;
    Ok(())
}

fn replace_threads(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    threads: &[ThreadState],
    resolution_events: Option<&BTreeMap<String, ActivityEventId>>,
) -> Result<()> {
    let retained;
    let resolution_events = if let Some(events) = resolution_events {
        events
    } else {
        retained = schema::threads::table
            .filter(schema::threads::repo_id.eq(repo_id))
            .filter(schema::threads::pr_number.eq(number as i64))
            .filter(schema::threads::resolution_event_id.is_not_null())
            .select((
                schema::threads::thread_id,
                schema::threads::resolution_event_id.assume_not_null(),
            ))
            .load::<(String, ActivityEventId)>(conn)
            .doing("reading retained thread resolutions")?
            .into_iter()
            .collect();
        &retained
    };
    diesel::delete(
        schema::threads::table
            .filter(schema::threads::repo_id.eq(repo_id))
            .filter(schema::threads::pr_number.eq(number as i64)),
    )
    .execute(conn)?;
    if !threads.is_empty() {
        let records = threads
            .iter()
            .map(|thread| ThreadRecord {
                thread_id: thread.thread_id.clone(),
                repo_id,
                pr_number: number as i64,
                i_own: thread.i_own,
                is_resolved: thread.is_resolved,
                resolved_by: thread.resolved_by.clone(),
                last_comment_author: thread.last_comment_author.clone(),
                last_comment_at: thread.last_comment_at.map(DbTimestamp::from),
                my_last_comment_at: thread.my_last_comment_at.map(DbTimestamp::from),
                resolution_event_id: thread
                    .is_resolved
                    .then(|| resolution_events.get(&thread.thread_id).copied())
                    .flatten(),
            })
            .collect::<Vec<_>>();
        diesel::insert_into(schema::threads::table)
            .values(&records)
            .execute(conn)
            .doing(format!("writing threads for #{number}"))?;
    }
    Ok(())
}

fn replace_reviewers(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    reviewers: &[ReviewerVerdict],
) -> Result<()> {
    diesel::delete(
        schema::reviewers::table
            .filter(schema::reviewers::repo_id.eq(repo_id))
            .filter(schema::reviewers::pr_number.eq(number as i64)),
    )
    .execute(conn)?;
    if !reviewers.is_empty() {
        let records = reviewers
            .iter()
            .map(|reviewer| ReviewerRecord {
                repo_id,
                pr_number: number as i64,
                login: reviewer.login.clone(),
                verdict: reviewer.verdict.as_str().to_owned(),
                submitted_at: DbTimestamp::from(reviewer.at),
            })
            .collect::<Vec<_>>();
        diesel::insert_into(schema::reviewers::table)
            .values(&records)
            .execute(conn)
            .doing(format!("writing reviewers for #{number}"))?;
    }
    Ok(())
}

fn replace_attention(
    conn: &mut DbConnection,
    repo_id: RepoId,
    number: u64,
    attention: &[Attention],
) -> Result<()> {
    diesel::delete(
        schema::attention::table
            .filter(schema::attention::repo_id.eq(repo_id))
            .filter(schema::attention::pr_number.eq(number as i64)),
    )
    .execute(conn)?;
    if !attention.is_empty() {
        let records = attention
            .iter()
            .map(|attention| {
                Ok(AttentionRecord {
                    repo_id,
                    pr_number: number as i64,
                    reason: attention.reason.discriminant().to_owned(),
                    since: DbTimestamp::from(attention.since),
                    payload: serde_json::to_string(&attention.reason)
                        .encoding("an attention reason")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        diesel::insert_into(schema::attention::table)
            .values(&records)
            .execute(conn)
            .doing(format!("writing attention for #{number}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reviewq_core::model::ActivityKind;

    #[derive(QueryableByName)]
    struct UserVersion {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        user_version: i32,
    }

    #[derive(QueryableByName)]
    struct BusyTimeout {
        #[diesel(sql_type = diesel::sql_types::Integer)]
        timeout: i32,
    }

    #[derive(QueryableByName)]
    struct JournalMode {
        #[diesel(sql_type = diesel::sql_types::Text)]
        journal_mode: String,
    }

    #[derive(QueryableByName)]
    struct TableColumn {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
    }

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
            state_changed_at: None,
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
    fn ledger_with_repo() -> (Ledger, RepoId) {
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
            .upsert_pr(a, &pr(1), Some(interest("label x")))
            .unwrap();
        ledger.upsert_pr(b, &pr(1), None).unwrap();
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
        {
            let conn = &mut *ledger.conn.borrow_mut();
            diesel::insert_into(repos::table)
                .values((
                    repos::id.eq(1_i64),
                    repos::host.eq(""),
                    repos::owner.eq(""),
                    repos::name.eq(""),
                ))
                .execute(conn)
                .unwrap();
            diesel::insert_into(prs::table)
                .values((
                    prs::repo_id.eq(1_i64),
                    prs::number.eq(1_i64),
                    prs::title.eq("a PR"),
                    prs::author.eq("octocat"),
                    prs::author_association.eq("CONTRIBUTOR"),
                    prs::head_sha.eq("abc123"),
                    prs::is_draft.eq(false),
                    prs::state.eq("OPEN"),
                    prs::updated_at.eq("2026-08-05T12:00:00Z"),
                    prs::labels.eq("[]"),
                    prs::first_seen_at.eq("2026-08-05T12:00:00Z"),
                ))
                .execute(conn)
                .unwrap();
            diesel::insert_into(my_state::table)
                .values((
                    my_state::repo_id.eq(1_i64),
                    my_state::number.eq(1_i64),
                    my_state::muted.eq(true),
                ))
                .execute(conn)
                .unwrap();
        }

        let id = ledger.ensure_repo(&repo()).unwrap();
        assert_eq!(
            id,
            RepoId(1),
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
        ledger.upsert_pr(a, &pr(1), None).unwrap();
        let b = ledger.ensure_repo(&other).unwrap();
        ledger.upsert_pr(b, &pr(2), None).unwrap();

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
        let version = diesel::sql_query("PRAGMA user_version")
            .get_result::<UserVersion>(&mut *ledger.conn.borrow_mut())
            .unwrap()
            .user_version;
        assert_eq!(version as usize, SCHEMA_VERSION);
    }

    #[test]
    fn a_newer_ledger_is_refused() {
        let mut conn = connection::establish(":memory:").unwrap();
        conn.batch_execute(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        // A DB past the last known migration, with no down-migrations defined,
        // is refused rather than run against.
        assert!(migrations::migrate(&mut conn).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_database_path_is_rejected_without_changing_it() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![0xff]));

        let Err(error) = Ledger::open(&path) else {
            panic!("non-UTF-8 path was accepted");
        };

        assert!(matches!(
            error,
            LedgerError::InvalidPath { path: rejected } if rejected == path
        ));
    }

    #[test]
    fn upsert_reports_insertion_then_updates() {
        let (ledger, repo_id) = ledger_with_repo();
        let reason = interest("label area:task-sdk");

        assert!(
            ledger
                .upsert_pr(repo_id, &pr(1), Some(reason.clone()))
                .unwrap()
        );
        assert!(!ledger.upsert_pr(repo_id, &pr(1), Some(reason)).unwrap());

        let tracked = ledger.list_tracked(repo_id).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].pr.number, 1);
        assert_eq!(tracked[0].pr.milestone.as_deref(), Some("3.2.0"));
        assert_eq!(tracked[0].pr.updated_at, now());
        assert_eq!(tracked[0].pr.base_ref, "main");
        assert_eq!(tracked[0].tracked_reason, "interest: label area:task-sdk");
    }

    #[test]
    fn concurrent_upserts_report_only_one_insertion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let first = Ledger::open(&path).unwrap();
        let repo_id = first.ensure_repo(&repo()).unwrap();
        let second = Ledger::open(&path).unwrap();
        let ready = std::sync::Barrier::new(2);

        let mut inserted = std::thread::scope(|scope| {
            let handles = [first, second].map(|ledger| {
                let ready = &ready;
                scope.spawn(move || {
                    ready.wait();
                    ledger.upsert_pr(repo_id, &pr(1), None).unwrap()
                })
            });
            handles.map(|handle| handle.join().unwrap())
        });

        inserted.sort();
        assert_eq!(inserted, [false, true]);
    }

    #[test]
    fn upsert_outcomes_are_scoped_to_the_repository_and_pr() {
        let (ledger, first) = ledger_with_repo();
        let second = ledger.ensure_repo(&repo_named("another")).unwrap();
        for (number, reason) in [
            (1, None),
            (2, Some(interest("label"))),
            (3, Some(TrackedReason::Involved("mentioned".into()))),
        ] {
            for repo_id in [first, second] {
                let mut snapshot = pr(number);
                assert!(
                    ledger
                        .upsert_pr(repo_id, &snapshot, reason.clone())
                        .unwrap()
                );
                snapshot.title = "Updated title".into();
                snapshot.updated_at = ts("2026-08-05T12:01:00Z");
                assert!(
                    !ledger
                        .upsert_pr(repo_id, &snapshot, reason.clone())
                        .unwrap()
                );
                assert!(
                    !ledger
                        .upsert_pr(repo_id, &snapshot, reason.clone())
                        .unwrap()
                );
                assert_eq!(
                    ledger.show(repo_id, number).unwrap().unwrap().pr.title,
                    "Updated title"
                );
            }
        }
    }

    #[test]
    fn a_failed_sweep_page_rolls_back_upserts_and_keeps_the_cursor() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.set_meta(repo_id, "cursor", "before").unwrap();
        ledger
            .conn
            .borrow_mut()
            .batch_execute(
                "CREATE TRIGGER reject_pr BEFORE INSERT ON prs WHEN NEW.number = 2
             BEGIN SELECT RAISE(ABORT, 'rejected PR'); END;",
            )
            .unwrap();

        assert!(
            ledger
                .commit_sweep_page(repo_id, &[(pr(1), None), (pr(2), None)], "cursor", "after")
                .is_err()
        );
        assert!(ledger.show(repo_id, 1).unwrap().is_none());
        assert_eq!(
            ledger.get_meta(repo_id, "cursor").unwrap().as_deref(),
            Some("before")
        );
        assert!(ledger.upsert_pr(repo_id, &pr(1), None).unwrap());
    }

    #[test]
    fn pr_states_round_trip_through_the_database_binding() {
        let (ledger, repo_id) = ledger_with_repo();
        for (index, (state, stored)) in [
            (PrState::Open, "OPEN"),
            (PrState::Closed, "CLOSED"),
            (PrState::Merged, "MERGED"),
        ]
        .into_iter()
        .enumerate()
        {
            let mut snapshot = pr(1);
            snapshot.state = state;
            snapshot.updated_at =
                Timestamp::from_second(snapshot.updated_at.as_second() + index as i64).unwrap();
            ledger.upsert_pr(repo_id, &snapshot, None).unwrap();
            let raw = prs::table
                .find((repo_id, 1_i64))
                .select(prs::state)
                .first::<String>(&mut *ledger.conn.borrow_mut())
                .unwrap();
            assert_eq!(raw, stored);
            let decoded = prs::table
                .find((repo_id, 1_i64))
                .select(prs::state)
                .first::<DbPrState>(&mut *ledger.conn.borrow_mut())
                .unwrap();
            assert_eq!(decoded.into_state(), state);
            assert_eq!(ledger.show(repo_id, 1).unwrap().unwrap().pr.state, state);
        }
    }

    #[test]
    fn an_invalid_stored_pr_state_is_reported_as_corrupt_with_its_column() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        diesel::update(prs::table.find((repo_id, 1_i64)))
            .set(prs::state.eq("INVALID_STATE"))
            .execute(&mut *ledger.conn.borrow_mut())
            .unwrap();

        let Err(LedgerError::Corrupt { source, .. }) = ledger.show(repo_id, 1) else {
            panic!("invalid PR state was not reported as corrupt");
        };
        assert!(source.to_string().contains("field 'state'"), "{source}");
        assert!(source.to_string().contains("INVALID_STATE"), "{source}");
    }

    #[test]
    fn an_invalid_stored_timestamp_is_reported_as_corrupt() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        diesel::update(prs::table.find((repo_id, 1_i64)))
            .set(prs::updated_at.eq("not a timestamp"))
            .execute(&mut *ledger.conn.borrow_mut())
            .unwrap();

        let Err(error) = ledger.show(repo_id, 1) else {
            panic!("invalid timestamp was accepted");
        };

        let LedgerError::Corrupt { source, .. } = error else {
            panic!("{error:?}");
        };
        assert!(source.to_string().contains("updated_at"), "{source}");
        assert!(source.source().is_some(), "{source}");
    }

    #[test]
    fn a_non_timestamp_deserialization_failure_is_corrupt_and_retains_its_source() {
        let source = diesel::result::Error::DeserializationError(Box::new(
            diesel::result::UnexpectedNullError,
        ));

        let error = classify_sql_error(source, "reading a PR");

        let LedgerError::Corrupt { source, .. } = error else {
            panic!("{error:?}");
        };
        assert!(source.is::<diesel::result::UnexpectedNullError>());
    }

    /// The target branch has to survive every read that rebuilds a snapshot.
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
            ledger
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()[0]
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
            ledger
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()[0]
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
        ledger.upsert_pr(repo_id, &swept, None).unwrap();

        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();

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
        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();

        assert_eq!(
            ledger.show(repo_id, 1).unwrap().unwrap().pr.created_at,
            None,
            "unknown, not the epoch and not first_seen_at"
        );

        let mut swept = pr(1);
        swept.created_at = Some("2026-05-04T08:30:00Z".parse().unwrap());
        ledger.upsert_pr(repo_id, &swept, None).unwrap();
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
        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();
        // Stand in for a row migration 7 backfilled: the column exists, and no
        // sweep has written a real value into it yet.
        diesel::update(prs::table.filter(prs::number.eq(1_i64)))
            .set(prs::base_ref.eq(""))
            .execute(&mut *ledger.conn.borrow_mut())
            .unwrap();

        let before = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(before.pr.base_ref, "", "unknown, not a wrong branch");

        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();
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
            .commit_sweep_page(repo_id, &page, "last_sync_at", "2026-08-05T12:00:00Z")
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
            .commit_sweep_page(repo_id, &page, "last_sync_at", "2026-08-05T12:05:00Z")
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
        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();
        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());
        assert_eq!(ledger.counts(repo_id).unwrap(), (0, 1));
    }

    #[test]
    fn first_seen_at_survives_a_later_upsert() {
        let (ledger, repo_id) = ledger_with_repo();
        let before = Timestamp::now();
        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();
        let seen = prs::table
            .find((repo_id, 1_i64))
            .select(prs::first_seen_at)
            .first::<DbTimestamp>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        let seen = seen.into_timestamp();
        assert!(seen >= before && seen <= Timestamp::now());

        assert!(!ledger.upsert_pr(repo_id, &pr(1), None).unwrap());

        let after = prs::table
            .find((repo_id, 1_i64))
            .select(prs::first_seen_at)
            .first::<DbTimestamp>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(after.into_timestamp(), seen);
    }

    #[test]
    fn involvement_beats_interest_and_is_not_downgraded() {
        let (ledger, repo_id) = ledger_with_repo();
        let matched = || interest("label area:task-sdk");
        ledger.upsert_pr(repo_id, &pr(1), Some(matched())).unwrap();

        // The involvement search upserts the same PR as involved.
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Involved("review_requested".into())),
            )
            .unwrap();
        assert_eq!(
            ledger.list_tracked(repo_id).unwrap()[0].tracked_reason,
            "involved: review_requested"
        );

        // A later sweep re-asserting interest must not clobber involvement.
        ledger.upsert_pr(repo_id, &pr(1), Some(matched())).unwrap();
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
        ledger.upsert_pr(repo_id, &merged, None).unwrap();
        let mut closed = pr(3);
        closed.state = PrState::Closed;
        ledger.upsert_pr(repo_id, &closed, None).unwrap();
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
        ledger.upsert_pr(repo_id, &p, None).unwrap();
        assert_eq!(ledger.count_truncated_untracked(repo_id).unwrap(), 1);
    }

    #[test]
    fn upserting_keeps_the_stronger_tracking_reason() {
        let (ledger, repo_id) = ledger_with_repo();
        let matched = interest("label x");
        let involved = TrackedReason::Involved("mention".into());
        for (number, (initial, incoming, expected)) in [
            (None, None, None),
            (None, Some(matched.clone()), Some("interest: label x")),
            (
                Some(involved.clone()),
                Some(matched.clone()),
                Some("involved: mention"),
            ),
            (
                Some(matched.clone()),
                Some(involved.clone()),
                Some("involved: mention"),
            ),
            (Some(involved.clone()), None, Some("involved: mention")),
            (
                Some(interest("old")),
                Some(matched),
                Some("interest: label x"),
            ),
            (
                Some(TrackedReason::Involved("old".into())),
                Some(involved),
                Some("involved: mention"),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let snapshot = pr(number as u64 + 1);
            ledger.upsert_pr(repo_id, &snapshot, initial).unwrap();
            ledger.upsert_pr(repo_id, &snapshot, incoming).unwrap();
            assert_eq!(
                ledger
                    .show(repo_id, snapshot.number)
                    .unwrap()
                    .unwrap()
                    .tracked_reason
                    .as_deref(),
                expected
            );
        }
    }

    #[test]
    fn only_a_rule_match_has_anything_to_say_about_post_merge_review() {
        let (ledger, repo_id) = ledger_with_repo();
        let keeps = TrackedReason::Interest {
            rule: "path task-sdk/**".into(),
            after_merge: true,
        };
        let involved = TrackedReason::Involved("review_requested".into());
        let lets_go = TrackedReason::Interest {
            rule: "path task-sdk/**".into(),
            after_merge: false,
        };
        for (incoming, expected_reason, after_merge) in [
            (Some(keeps), "interest: path task-sdk/**", true),
            (Some(involved), "involved: review_requested", true),
            (None, "involved: review_requested", true),
            (Some(lets_go), "involved: review_requested", false),
        ] {
            ledger.upsert_pr(repo_id, &pr(1), incoming).unwrap();
            let stored = ledger.show(repo_id, 1).unwrap().unwrap();
            assert_eq!(stored.tracked_reason.as_deref(), Some(expected_reason));
            assert_eq!(stored.after_merge, after_merge);
        }
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

    fn track(ledger: &Ledger, repo_id: RepoId, p: &PrSnapshot) {
        ledger
            .upsert_pr(repo_id, p, Some(interest("label area:task-sdk")))
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
                stored: "2026-08-05T12:00:05Z".parse().unwrap()
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
    fn superseded_detail_cannot_change_lifecycle_projection_or_history() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        let winner = ts("2026-08-05T12:00:05Z");
        let loser = ts("2026-08-05T12:00:00Z");

        ledger
            .commit_detail_with_lifecycle(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[],
                None,
                PrState::Open,
                None,
                winner,
            )
            .unwrap()
            .expect_applied();
        assert_eq!(
            ledger
                .commit_detail_with_lifecycle(
                    repo_id,
                    1,
                    &MyState::default(),
                    &[],
                    &[],
                    &[],
                    None,
                    PrState::Closed,
                    Some(ts("2026-08-05T11:55:00Z")),
                    loser,
                )
                .unwrap(),
            Committed::Superseded {
                stored: ts("2026-08-05T12:00:05Z"),
            }
        );

        let shown = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(shown.pr.state, PrState::Open);
        assert_eq!(shown.pr.state_changed_at, None);
        assert!(
            ledger
                .activity_page(ActivityScope::All, None, 100)
                .unwrap()
                .events
                .iter()
                .all(|event| event.kind != ActivityKind::PrClosed)
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
    fn committing_detail_preserves_an_omitted_body_but_accepts_an_empty_one() {
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        for (body, expected) in [
            (None, None),
            (Some("body"), Some("body")),
            (None, Some("body")),
            (Some(""), Some("")),
        ] {
            ledger
                .commit_detail(repo_id, 1, &MyState::default(), &[], &[], &[], body, now())
                .unwrap()
                .expect_applied();
            assert_eq!(
                ledger.show(repo_id, 1).unwrap().unwrap().body.as_deref(),
                expected
            );
        }
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
                .prs_needing_detail(repo_id, false, Detail::Stale)
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
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()
                .is_empty()
        );

        // A later sweep sees it again, advancing updated_at past the stamp.
        let mut back = pr(1);
        back.updated_at = ts("2026-08-11T09:00:00Z");
        ledger.upsert_pr(repo_id, &back, None).unwrap();

        assert_eq!(
            ledger
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()
                .len(),
            1
        );
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
        let columns = diesel::sql_query("PRAGMA table_info(attention)")
            .load::<TableColumn>(&mut *ledger.conn.borrow_mut())
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert!(!columns.contains(&"detail".to_string()), "{columns:?}");
    }

    #[test]
    fn a_file_backed_ledger_enables_wal_and_a_busy_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::open(&dir.path().join("ledger.sqlite")).unwrap();

        let mode = diesel::sql_query("PRAGMA journal_mode")
            .get_result::<JournalMode>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(mode.journal_mode, "wal");

        let timeout = diesel::sql_query("PRAGMA busy_timeout")
            .get_result::<BusyTimeout>(&mut *ledger.conn.borrow_mut())
            .unwrap();
        assert_eq!(timeout.timeout as u128, BUSY_TIMEOUT.as_millis());
    }

    #[test]
    fn a_locked_database_is_reported_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = Ledger::open(&path).unwrap();
        let repo_id = ledger.ensure_repo(&repo()).unwrap();
        ledger
            .conn
            .borrow_mut()
            .batch_execute("PRAGMA busy_timeout = 0")
            .unwrap();
        let mut locker = connection::establish(path.to_str().unwrap()).unwrap();
        locker.batch_execute("BEGIN IMMEDIATE").unwrap();

        let error = ledger.set_meta(repo_id, "cursor", "value").unwrap_err();

        assert!(matches!(error, LedgerError::Busy { .. }), "{error:?}");
        locker.batch_execute("ROLLBACK").unwrap();
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
        repo_id: RepoId,
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

        let need = ledger
            .prs_needing_detail(repo_id, false, Detail::Stale)
            .unwrap();
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
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()
                .is_empty(),
            "merged PR skipped without the opt-in"
        );
        assert_eq!(
            ledger
                .prs_needing_detail(repo_id, true, Detail::Stale)
                .unwrap()
                .len(),
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
            )
            .unwrap();

        let need = ledger
            .prs_needing_detail(repo_id, false, Detail::Stale)
            .unwrap();
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
        ledger
            .clear_archived_attention(repo_id, true, now())
            .unwrap();
        assert_eq!(ledger.queue(repo_id).unwrap().len(), 1);

        // Without it, merged attention is swept away.
        ledger
            .clear_archived_attention(repo_id, false, now())
            .unwrap();
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

        ledger
            .clear_archived_attention(repo_id, false, now())
            .unwrap();

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
    fn asking_for_everything_returns_prs_whose_detail_is_current() {
        // What a reason means is decided when a detail is classified, so a
        // build that learns a new one has nothing to say about a PR nobody has
        // touched — until this asks for it anyway.
        let (ledger, repo_id) = ledger_with_repo();
        track(&ledger, repo_id, &pr(1));
        ledger
            .commit_detail(
                repo_id,
                1,
                &MyState::default(),
                &[],
                &[],
                &[],
                None,
                // Detailed after the PR last changed, so nothing is due.
                "2026-08-09T00:00:00Z".parse().unwrap(),
            )
            .unwrap()
            .expect_applied();

        assert!(
            ledger
                .prs_needing_detail(repo_id, false, Detail::Stale)
                .unwrap()
                .is_empty(),
            "an ordinary sync leaves it alone"
        );
        assert_eq!(
            ledger
                .prs_needing_detail(repo_id, false, Detail::Every)
                .unwrap()
                .len(),
            1,
            "and asking for everything means everything"
        );
    }

    #[test]
    fn track_sets_involved_manual_and_does_not_downgrade() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger.upsert_pr(repo_id, &pr(1), None).unwrap();
        assert!(ledger.list_tracked(repo_id).unwrap().is_empty());

        assert!(ledger.track(repo_id, 1).unwrap());
        let tracked = ledger.list_tracked(repo_id).unwrap();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked[0].tracked_reason, "involved: manual");

        // A later sweep re-asserting interest must not clobber it.
        ledger
            .upsert_pr(repo_id, &pr(1), Some(interest("label area:task-sdk")))
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
            .upsert_pr(repo_id, &pr(1), Some(interest("label area:task-sdk")))
            .unwrap();
        assert!(
            ledger.list_tracked(repo_id).unwrap().is_empty(),
            "a sweep must not take back a PR you said you were finished with"
        );
        let show = ledger.show(repo_id, 1).unwrap().unwrap();
        assert_eq!(show.tracked_reason, None);
    }

    #[test]
    fn an_untracked_pr_keeps_its_post_merge_setting_when_a_rule_still_matches() {
        let (ledger, repo_id) = ledger_with_repo();
        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Interest {
                    rule: "label area:task-sdk".into(),
                    after_merge: true,
                }),
            )
            .unwrap();
        ledger.untrack(repo_id, 1, now()).unwrap();

        ledger
            .upsert_pr(
                repo_id,
                &pr(1),
                Some(TrackedReason::Interest {
                    rule: "label area:task-sdk".into(),
                    after_merge: false,
                }),
            )
            .unwrap();
        ledger.track(repo_id, 1).unwrap();

        assert!(ledger.list_tracked(repo_id).unwrap()[0].after_merge);
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
            .upsert_pr(repo_id, &pr(1), Some(interest("label area:task-sdk")))
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
