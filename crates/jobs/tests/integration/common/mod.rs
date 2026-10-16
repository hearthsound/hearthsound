//! Helpers shared by the integration tests: a temporary migrated database and
//! the few rows a test needs to arrange by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_db::Database;
use uuid::Uuid;

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

/// Inserts a user, so a user-owned schedule can reference one.
///
/// The values are irrelevant to the tests; only the row's existence matters.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `id`: the id to give the user.
pub async fn insert_user(database: &Database, id: Uuid) {
    let now = now_millis();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .bind("Test User")
    .bind("not-a-hash")
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Sets a schedule's `next_run_at` by hand, to arrange a schedule that is
/// already due, one that was missed while the server was down, or one that has
/// never had a fire time computed.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `id`: the schedule to change.
/// - `next_run_at`: the value to write.
pub async fn set_next_run_at(database: &Database, id: Uuid, next_run_at: Option<i64>) {
    sqlx::query("UPDATE schedules SET next_run_at = ? WHERE id = ?")
        .bind(next_run_at)
        .bind(id.as_bytes().as_slice())
        .execute(database.writer())
        .await
        .unwrap();
}

/// Replaces a schedule's cron expression by hand, to arrange an expression the
/// scheduler has to reject.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `id`: the schedule to change.
/// - `cron`: the expression to write.
pub async fn set_cron(database: &Database, id: Uuid, cron: &str) {
    sqlx::query("UPDATE schedules SET cron = ? WHERE id = ?")
        .bind(cron)
        .bind(id.as_bytes().as_slice())
        .execute(database.writer())
        .await
        .unwrap();
}
