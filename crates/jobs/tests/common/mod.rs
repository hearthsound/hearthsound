//! Helpers shared by the integration tests: a temporary migrated database and
//! the few rows a test needs to arrange by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_db::Database;

/// Opens a fresh temporary database with the server schema applied.
///
/// # Parameters
///
/// - `dir`: the temporary directory the database file lives in.
///
/// # Returns
///
/// The open database.
pub async fn open_database(dir: &Path) -> Database {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    database
        .migrate(&hearthsound_db::MIGRATOR, &dir.join("backups"))
        .await
        .unwrap();
    database
}

/// Returns the current time as unix milliseconds.
pub fn now_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
