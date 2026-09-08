//! Embedded Diesel migrations and compatibility with existing ledgers.

use diesel::{connection::SimpleConnection, prelude::*};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

use crate::{DbConnection, LedgerError, Result};

/// The schema version this build expects.
pub const SCHEMA_VERSION: usize = 11;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[derive(QueryableByName)]
struct UserVersion {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    user_version: i32,
}

/// Bring `conn` up to the latest schema.
///
/// Older reviewq releases recorded migration progress in `PRAGMA user_version`.
/// Before Diesel runs, those versions are copied into Diesel's migration table
/// so only genuinely pending migrations are applied.
pub fn migrate(conn: &mut DbConnection) -> Result<()> {
    let version = user_version(conn)?;
    if version > SCHEMA_VERSION {
        return Err(LedgerError::FromTheFuture);
    }
    let applied = conn.applied_migrations().map_err(corrupt_schema)?;
    let missing = (1..=version)
        .map(|version| format!("{version:014}"))
        .filter(|version| !applied.contains(&version.as_str().into()))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        conn.transaction::<_, LedgerError, _>(|conn| {
            for version in &missing {
                diesel::sql_query(
                    "INSERT OR IGNORE INTO __diesel_schema_migrations (version) VALUES (?)",
                )
                .bind::<diesel::sql_types::Text, _>(version)
                .execute(conn)
                .map_err(corrupt_schema)?;
            }
            Ok(())
        })?;
    }
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(corrupt_schema)?;
    if version != SCHEMA_VERSION {
        conn.batch_execute(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))
            .map_err(corrupt_schema)?;
    }
    Ok(())
}

fn user_version(conn: &mut DbConnection) -> Result<usize> {
    diesel::sql_query("PRAGMA user_version")
        .get_result::<UserVersion>(conn)
        .map(|row| row.user_version as usize)
        .map_err(corrupt_schema)
}

fn corrupt_schema(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> LedgerError {
    let source = source.into();
    if crate::is_busy(source.as_ref()) {
        return LedgerError::Busy { source };
    }
    LedgerError::Corrupt {
        what: "schema this build can migrate".to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{my_state, repos};

    const LEGACY_MIGRATIONS: [&str; SCHEMA_VERSION] = [
        include_str!("../migrations/00000000000001_initial/up.sql"),
        include_str!("../migrations/00000000000002_deferred_and_done_at/up.sql"),
        include_str!("../migrations/00000000000003_reviewers/up.sql"),
        include_str!("../migrations/00000000000004_multi_repo/up.sql"),
        include_str!("../migrations/00000000000005_structured_attention/up.sql"),
        include_str!("../migrations/00000000000006_pr_body/up.sql"),
        include_str!("../migrations/00000000000007_base_ref/up.sql"),
        include_str!("../migrations/00000000000008_after_merge/up.sql"),
        include_str!("../migrations/00000000000009_label_colours/up.sql"),
        include_str!("../migrations/00000000000010_untracked_at/up.sql"),
        include_str!("../migrations/00000000000011_created_at/up.sql"),
    ];

    fn connection() -> DbConnection {
        crate::connection::establish(":memory:").unwrap()
    }

    #[test]
    fn fresh_database_applies_all_migrations() {
        let mut conn = connection();

        migrate(&mut conn).unwrap();

        assert_eq!(user_version(&mut conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn a_current_ledger_can_open_while_another_connection_is_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let writer = crate::Ledger::open(&path).unwrap();
        writer
            .conn
            .borrow_mut()
            .batch_execute("BEGIN IMMEDIATE")
            .unwrap();

        let reader = crate::Ledger::open(&path).unwrap();

        assert!(reader.repos().unwrap().is_empty());
    }

    #[test]
    fn a_write_lock_during_legacy_adoption_is_reported_as_busy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let mut writer = crate::connection::establish(path.to_str().unwrap()).unwrap();
        crate::prepare_conn(&mut writer).unwrap();
        for migration in LEGACY_MIGRATIONS {
            writer.batch_execute(migration).unwrap();
        }
        writer
            .batch_execute("PRAGMA user_version = 11; BEGIN IMMEDIATE")
            .unwrap();
        let mut reader = crate::connection::establish(path.to_str().unwrap()).unwrap();

        let result = migrate(&mut reader);

        assert!(
            matches!(result, Err(LedgerError::Busy { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn every_legacy_schema_version_upgrades_to_the_latest() {
        for version in 1..=SCHEMA_VERSION {
            let mut conn = connection();
            for migration in &LEGACY_MIGRATIONS[..version] {
                conn.batch_execute(migration).unwrap();
            }
            conn.batch_execute(&format!("PRAGMA user_version = {version}"))
                .unwrap();

            migrate(&mut conn).unwrap();

            assert_eq!(user_version(&mut conn).unwrap(), SCHEMA_VERSION);
        }
    }

    #[test]
    fn fresh_database_creates_no_placeholder_repo() {
        let mut conn = connection();
        migrate(&mut conn).unwrap();

        let count = repos::table.count().get_result::<i64>(&mut conn).unwrap();

        assert_eq!(count, 0);
    }

    #[test]
    fn multi_repo_migration_attributes_existing_data_to_a_placeholder() {
        let mut conn = connection();
        for migration in &LEGACY_MIGRATIONS[..3] {
            conn.batch_execute(migration).unwrap();
        }
        conn.batch_execute(
            "INSERT INTO prs (number, title, author, author_association, head_sha, \
             is_draft, state, updated_at, labels, first_seen_at) \
             VALUES (1, 'a PR', 'octocat', 'CONTRIBUTOR', 'abc123', 0, 'OPEN', \
             '2026-08-05T12:00:00Z', '[]', '2026-08-05T12:00:00Z'); \
             INSERT INTO my_state (number, muted) VALUES (1, 1); \
             PRAGMA user_version = 3;",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let repo = repos::table
            .find(1_i64)
            .select((repos::host, repos::owner, repos::name))
            .first::<(String, String, String)>(&mut conn)
            .unwrap();
        assert_eq!(repo, (String::new(), String::new(), String::new()));
        let muted = my_state::table
            .find((1_i64, 1_i64))
            .select(my_state::muted)
            .first::<bool>(&mut conn)
            .unwrap();
        assert!(muted);
    }
}
