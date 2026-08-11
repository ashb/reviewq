//! The database schema and its migrations.
//!
//! Migrations run through `rusqlite_migration`, which tracks the applied
//! version in `PRAGMA user_version`. Migration 1 creates the whole schema the
//! design calls for, including the `my_state`, `threads` and `attention` tables
//! that only get populated once the state machine lands; creating them now
//! keeps a single, stable v1.

use std::sync::LazyLock;

use rusqlite::Connection;
use rusqlite_migration::{M, MigrationDefinitionError, Migrations};

use crate::{LedgerError, Result};

/// The schema version this build expects — the number of migrations defined.
pub const SCHEMA_VERSION: usize = 9;

const MIGRATION_1: &str = r"
CREATE TABLE prs (
  number            INTEGER PRIMARY KEY,
  title             TEXT NOT NULL,
  author            TEXT NOT NULL,
  author_association TEXT NOT NULL,
  head_sha          TEXT NOT NULL,
  is_draft          INTEGER NOT NULL,
  state             TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  labels            TEXT NOT NULL,
  milestone         TEXT,
  files             TEXT,
  files_truncated   INTEGER NOT NULL DEFAULT 0,
  tracked_reason    TEXT,
  first_seen_at     TEXT NOT NULL,
  detail_synced_at  TEXT
);

CREATE TABLE my_state (
  number            INTEGER PRIMARY KEY REFERENCES prs(number),
  last_reviewed_sha TEXT,
  last_verdict      TEXT,
  last_action_at    TEXT,
  done_sha          TEXT,
  snoozed_until     TEXT,
  muted             INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE threads (
  thread_id           TEXT PRIMARY KEY,
  pr_number           INTEGER NOT NULL REFERENCES prs(number),
  i_own               INTEGER NOT NULL,
  is_resolved         INTEGER NOT NULL,
  resolved_by         TEXT,
  last_comment_author TEXT,
  last_comment_at     TEXT,
  my_last_comment_at  TEXT
);

CREATE TABLE attention (
  pr_number         INTEGER NOT NULL REFERENCES prs(number),
  reason            TEXT NOT NULL,
  detail            TEXT NOT NULL,
  since             TEXT NOT NULL,
  PRIMARY KEY (pr_number, reason)
);

CREATE TABLE sync_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

/// `reviewq defer`'s queue-ordering hint and `reviewq done`'s local
/// acknowledgement stamp: see [`MyState::deferred_at`] and
/// [`MyState::done_at`] (`reviewq_core::model::MyState`).
const MIGRATION_2: &str = "
ALTER TABLE my_state ADD COLUMN deferred_at TEXT;
ALTER TABLE my_state ADD COLUMN done_at TEXT;
";

/// Every reviewer's most recent submitted verdict, replaced wholesale on each
/// detail sync — same lifecycle as `threads`. See
/// [`ReviewerVerdict`](reviewq_core::model::ReviewerVerdict).
const MIGRATION_3: &str = "
CREATE TABLE reviewers (
  pr_number     INTEGER NOT NULL REFERENCES prs(number),
  login         TEXT NOT NULL,
  verdict       TEXT NOT NULL,
  submitted_at  TEXT NOT NULL,
  PRIMARY KEY (pr_number, login)
);
";

/// Every PR-scoped table keyed by a bare number, so two repos both having a
/// PR #42 would collide. Rebuilds `prs` and its dependants with a `repo_id`
/// folded into their primary keys, and adds the `repos` table that assigns
/// each host/owner/name triple one.
///
/// Every pre-v4 database holds at most one repo's worth of data by
/// construction (that was the whole limitation this migration lifts), so the
/// copy steps below attribute all existing rows to `repo_id = 1` unconditionally
/// and insert a placeholder `repos` row (blank host/owner/name) to match —
/// plain migration SQL has no way to know the real one. [`Ledger::ensure_repo`]
/// adopts that placeholder into whichever repo is first resolved after the
/// upgrade, giving the legacy data its real identity without this migration
/// needing any input. SQLite can't change a table's primary key in place,
/// hence the create-copy-drop-rename dance for each table.
const MIGRATION_4: &str = r"
CREATE TABLE repos (
  id    INTEGER PRIMARY KEY,
  host  TEXT NOT NULL,
  owner TEXT NOT NULL,
  name  TEXT NOT NULL,
  UNIQUE (host, owner, name)
);

INSERT INTO repos (id, host, owner, name)
SELECT 1, '', '', ''
WHERE EXISTS (SELECT 1 FROM prs) OR EXISTS (SELECT 1 FROM sync_meta);

-- Every _v4 table below references the other _v4 tables, not the old ones —
-- so nothing here depends on the old tables' drop order, which matters
-- because SQLite's own foreign_keys enforcement refuses to DROP a table
-- while a FK-enforced child still holds rows referencing it (dropping old
-- `prs` first, with old `my_state` etc. still populated and still pointing
-- at it, fails exactly that check). All the old tables are dropped only
-- once every new one is fully built and populated.

CREATE TABLE prs_v4 (
  repo_id             INTEGER NOT NULL REFERENCES repos(id),
  number              INTEGER NOT NULL,
  title               TEXT NOT NULL,
  author              TEXT NOT NULL,
  author_association  TEXT NOT NULL,
  head_sha            TEXT NOT NULL,
  is_draft            INTEGER NOT NULL,
  state               TEXT NOT NULL,
  updated_at          TEXT NOT NULL,
  labels              TEXT NOT NULL,
  milestone           TEXT,
  files               TEXT,
  files_truncated     INTEGER NOT NULL DEFAULT 0,
  tracked_reason      TEXT,
  first_seen_at       TEXT NOT NULL,
  detail_synced_at    TEXT,
  PRIMARY KEY (repo_id, number)
);
INSERT INTO prs_v4
  SELECT 1, number, title, author, author_association, head_sha, is_draft,
         state, updated_at, labels, milestone, files, files_truncated,
         tracked_reason, first_seen_at, detail_synced_at
  FROM prs;

CREATE TABLE my_state_v4 (
  repo_id           INTEGER NOT NULL,
  number            INTEGER NOT NULL,
  last_reviewed_sha TEXT,
  last_verdict      TEXT,
  last_action_at    TEXT,
  done_sha          TEXT,
  snoozed_until     TEXT,
  muted             INTEGER NOT NULL DEFAULT 0,
  deferred_at       TEXT,
  done_at           TEXT,
  PRIMARY KEY (repo_id, number),
  FOREIGN KEY (repo_id, number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO my_state_v4
  SELECT 1, number, last_reviewed_sha, last_verdict, last_action_at, done_sha,
         snoozed_until, muted, deferred_at, done_at
  FROM my_state;

CREATE TABLE threads_v4 (
  thread_id           TEXT PRIMARY KEY,
  repo_id             INTEGER NOT NULL,
  pr_number           INTEGER NOT NULL,
  i_own               INTEGER NOT NULL,
  is_resolved         INTEGER NOT NULL,
  resolved_by         TEXT,
  last_comment_author TEXT,
  last_comment_at     TEXT,
  my_last_comment_at  TEXT,
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO threads_v4
  SELECT thread_id, 1, pr_number, i_own, is_resolved, resolved_by,
         last_comment_author, last_comment_at, my_last_comment_at
  FROM threads;

CREATE TABLE attention_v4 (
  repo_id     INTEGER NOT NULL,
  pr_number   INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  detail      TEXT NOT NULL,
  since       TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, reason),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO attention_v4
  SELECT 1, pr_number, reason, detail, since FROM attention;

CREATE TABLE reviewers_v4 (
  repo_id       INTEGER NOT NULL,
  pr_number     INTEGER NOT NULL,
  login         TEXT NOT NULL,
  verdict       TEXT NOT NULL,
  submitted_at  TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, login),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs_v4(repo_id, number)
);
INSERT INTO reviewers_v4
  SELECT 1, pr_number, login, verdict, submitted_at FROM reviewers;

CREATE TABLE sync_meta_v4 (
  repo_id INTEGER NOT NULL REFERENCES repos(id),
  key     TEXT NOT NULL,
  value   TEXT NOT NULL,
  PRIMARY KEY (repo_id, key)
);
INSERT INTO sync_meta_v4
  SELECT 1, key, value FROM sync_meta;

-- Children before the parent: an old child with rows still referencing the
-- old `prs` blocks dropping `prs` first.
DROP TABLE my_state;
DROP TABLE threads;
DROP TABLE attention;
DROP TABLE reviewers;
DROP TABLE prs;
DROP TABLE sync_meta;

ALTER TABLE prs_v4 RENAME TO prs;
ALTER TABLE my_state_v4 RENAME TO my_state;
ALTER TABLE threads_v4 RENAME TO threads;
ALTER TABLE attention_v4 RENAME TO attention;
ALTER TABLE reviewers_v4 RENAME TO reviewers;
ALTER TABLE sync_meta_v4 RENAME TO sync_meta;
";

/// `attention.detail` held the *rendered* reason string, which is a display
/// concern and does not belong in storage: improving the wording left every
/// stored row showing the old text, and since `prs_needing_detail` only
/// refetches PRs whose detail is stale, a quiet PR would have kept it forever.
///
/// The table now stores the reason itself, serialised, in `payload`, and the
/// string is rendered on read. `reason` stays as the discriminant — that's
/// data, and the primary key needs it to keep one row per reason kind.
///
/// Existing rows cannot be carried over, because prose cannot be parsed back
/// into a variant. They are dropped, and every PR that had one is marked
/// detail-stale so the next sync recomputes it. Attention is derived data — a
/// detail pass is what produces it — so a sync rebuilds all of it. The queue is
/// empty in between, which `list` already explains how to fix.
const MIGRATION_5: &str = r"
-- Before dropping the rows, mark their PRs for a fresh detail pass.
UPDATE prs SET detail_synced_at = NULL
WHERE EXISTS (
  SELECT 1 FROM attention a
  WHERE a.repo_id = prs.repo_id AND a.pr_number = prs.number
);

CREATE TABLE attention_v5 (
  repo_id     INTEGER NOT NULL,
  pr_number   INTEGER NOT NULL,
  reason      TEXT NOT NULL,
  since       TEXT NOT NULL,
  payload     TEXT NOT NULL,
  PRIMARY KEY (repo_id, pr_number, reason),
  FOREIGN KEY (repo_id, pr_number) REFERENCES prs(repo_id, number)
);

DROP TABLE attention;
ALTER TABLE attention_v5 RENAME TO attention;
";

/// The PR description, for display. Null until the PR's next detail pass —
/// which a sweep will schedule anyway, since anything that edits a body also
/// moves `updated_at`. Nothing classifies on it, so a null body is only ever a
/// missing panel, never a wrong queue.
const MIGRATION_6: &str = r"
ALTER TABLE prs ADD COLUMN body TEXT;
";

/// The branch the PR targets. Empty rather than null on an existing row: the
/// sweep overwrites it on the next sync, and until then "unknown" and "no target
/// branch" would be the same thing anyway.
const MIGRATION_7: &str = r"
ALTER TABLE prs ADD COLUMN base_ref TEXT NOT NULL DEFAULT '';
";

/// Whether the interest rule that tracked this PR keeps it reviewable after it
/// merges — the per-rule half of post-merge review, beside the per-project
/// `include_merged`. Stored rather than recomputed because the queries that
/// decide which merged PRs still get a detail pass, and whose attention rows are
/// cleared, are the ledger's own and cannot evaluate a glob.
///
/// Nought on an existing row, which is what every rule meant until now. The next
/// sweep rewrites it for anything a rule still matches.
const MIGRATION_8: &str = r"
ALTER TABLE prs ADD COLUMN after_merge INTEGER NOT NULL DEFAULT 0;
";

/// The colour a repo paints each of its labels, six hex digits and no `#`.
///
/// Per repo because that is what a label colour is: the same `area:Scheduler`
/// is a different colour in another project, so a table keyed by name alone
/// would be wrong the moment a second repo used the name.
///
/// Only the labels a sweep has actually seen are here, which is exactly the set
/// anything asks to draw. A row is replaced when the colour changes upstream,
/// so nothing accumulates but the repo's own palette.
const MIGRATION_9: &str = r"
CREATE TABLE labels (
  repo_id INTEGER NOT NULL REFERENCES repos(id),
  name    TEXT NOT NULL,
  color   TEXT NOT NULL,
  PRIMARY KEY (repo_id, name)
);
";

static MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
        M::up(MIGRATION_4),
        M::up(MIGRATION_5),
        M::up(MIGRATION_6),
        M::up(MIGRATION_7),
        M::up(MIGRATION_8),
        M::up(MIGRATION_9),
    ])
});

/// Bring `conn` up to the latest schema. A database from a newer build (a
/// version past the last migration this build knows) is refused by
/// `rusqlite_migration` rather than run against blindly.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    MIGRATIONS.to_latest(conn).map_err(|err| match err {
        // The one failure a user can act on, and the one that must never be
        // retried: migrating down is not defined, so a database ahead of this
        // build is left exactly as it is.
        rusqlite_migration::Error::MigrationDefinition(
            MigrationDefinitionError::DatabaseTooFarAhead,
        ) => LedgerError::FromTheFuture,
        other => LedgerError::Corrupt {
            what: "schema this build can migrate".to_string(),
            source: Box::new(other),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_valid() {
        // rusqlite_migration validates the whole set (parseable SQL, sane
        // ordering) here rather than at first run.
        MIGRATIONS.validate().expect("migrations validate");
    }

    #[test]
    fn migration_4_on_a_fresh_database_creates_no_placeholder_repo() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let repos: i64 = conn
            .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(repos, 0, "no pre-existing data means nothing to backfill");
    }

    /// Migration 4's own half of the pre-v4 upgrade story: it must leave a
    /// placeholder repo (so the rebuilt tables' FKs resolve) attributing all
    /// existing rows to it, without knowing or guessing a real identity.
    /// `Ledger::ensure_repo`'s tests cover the other half — adopting it.
    #[test]
    fn migration_4_attributes_pre_existing_data_to_a_placeholder_repo() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_1).unwrap();
        conn.execute_batch(MIGRATION_2).unwrap();
        conn.execute_batch(MIGRATION_3).unwrap();
        conn.execute(
            "INSERT INTO prs (number, title, author, author_association, head_sha, \
             is_draft, state, updated_at, labels, first_seen_at) \
             VALUES (1, 'a PR', 'octocat', 'CONTRIBUTOR', 'abc123', 0, 'OPEN', \
             '2026-08-05T12:00:00Z', '[]', '2026-08-05T12:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO my_state (number, muted) VALUES (1, 1)", [])
            .unwrap();
        conn.pragma_update(None, "user_version", 3i64).unwrap();

        migrate(&mut conn).unwrap();

        let (host, owner, name): (String, String, String) = conn
            .query_row(
                "SELECT host, owner, name FROM repos WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (host, owner, name),
            (String::new(), String::new(), String::new())
        );

        let muted: i64 = conn
            .query_row(
                "SELECT muted FROM my_state WHERE repo_id = 1 AND number = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(muted, 1, "pre-existing mute state survives the migration");
    }
}
