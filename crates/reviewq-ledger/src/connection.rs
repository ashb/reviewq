use diesel::{Connection as _, ConnectionResult};

pub(super) type DbConnection = diesel::sqlite::SqliteConnection;

pub(super) fn establish(database_url: &str) -> ConnectionResult<DbConnection> {
    DbConnection::establish(database_url)
}
