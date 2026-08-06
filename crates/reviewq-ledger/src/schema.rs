//! The database schema and its migrations.
//!
//! Migrations run through `rusqlite_migration`, which tracks the applied
//! version in `PRAGMA user_version`. Migration 1 creates the whole schema the
//! design calls for, including the `my_state`, `threads` and `attention` tables
//! that only get populated once the state machine lands; creating them now
//! keeps a single, stable v1.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

/// The schema version this build expects — the number of migrations defined.
pub const SCHEMA_VERSION: usize = 2;

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

static MIGRATIONS: LazyLock<Migrations<'static>> =
    LazyLock::new(|| Migrations::new(vec![M::up(MIGRATION_1), M::up(MIGRATION_2)]));

/// Bring `conn` up to the latest schema. A database from a newer build (a
/// version past the last migration this build knows) is refused by
/// `rusqlite_migration` rather than run against blindly.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    MIGRATIONS
        .to_latest(conn)
        .context("running ledger migrations")
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
}
