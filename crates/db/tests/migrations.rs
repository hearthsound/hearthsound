//! Integration tests for opening and migrating a database file.
//!
//! The baseline replaced every migration of the releases before v0.1.7, so a
//! fresh file migrates to version 1 in one step and a file stamped by an older
//! release is refused with a message that says what to do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_db::{Database, DbError, MIGRATOR};

/// Opens a database in a fresh temporary directory.
async fn fresh(dir: &std::path::Path) -> Database {
    Database::open(&dir.join("hearthsound.db")).await.unwrap()
}

#[tokio::test]
async fn a_fresh_database_migrates_to_the_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let database = fresh(dir.path()).await;
    let outcome = database
        .migrate(&MIGRATOR, &dir.path().join("backups"))
        .await
        .unwrap();

    assert_eq!(outcome.from, 0);
    assert_eq!(outcome.to, 1);
    assert_eq!(database.migration_version().await.unwrap(), 1);
    assert!(
        outcome.backup.is_none(),
        "a fresh file holds nothing to back up"
    );
}

#[tokio::test]
async fn migrating_a_current_database_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = fresh(dir.path()).await;
    database
        .migrate(&MIGRATOR, &dir.path().join("backups"))
        .await
        .unwrap();
    let again = database
        .migrate(&MIGRATOR, &dir.path().join("backups"))
        .await
        .unwrap();

    assert_eq!(again.from, 1);
    assert_eq!(again.to, 1);
    assert!(again.backup.is_none());
}

#[tokio::test]
async fn a_database_from_an_older_release_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = fresh(dir.path()).await;
    {
        let mut connection = database.writer().acquire().await.unwrap();
        sqlx::query(
            "CREATE TABLE _sqlx_migrations (version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, success BOOLEAN NOT NULL, \
             checksum BLOB NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .execute(&mut *connection)
        .await
        .unwrap();
        for version in 1..=18 {
            sqlx::query(
                "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
                 VALUES (?, 'initial', 1, x'00', 0)",
            )
            .bind(version)
            .execute(&mut *connection)
            .await
            .unwrap();
        }
    }

    let error = database
        .migrate(&MIGRATOR, &dir.path().join("backups"))
        .await
        .unwrap_err();

    assert!(matches!(error, DbError::LegacySchema), "{error}");
    assert!(error.to_string().contains("move the data directory aside"));
}
