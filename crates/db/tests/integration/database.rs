//! Integration tests for the database layer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_db::Database;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

async fn open_temp(dir: &Path) -> Database {
    Database::open(&dir.join("hearthsound.db")).await.unwrap()
}

#[tokio::test]
async fn opens_in_wal_mode_with_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;

    let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
        .fetch_one(db.writer())
        .await
        .unwrap();
    assert_eq!(journal, "wal");

    for pool in [db.writer(), db.readers()] {
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    db.close().await;
}

#[tokio::test]
async fn readers_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;

    let result = sqlx::query("CREATE TABLE nope (id INTEGER PRIMARY KEY)")
        .execute(db.readers())
        .await;

    assert!(result.is_err(), "the reader pool must reject writes");
    db.close().await;
}

#[tokio::test]
async fn migrates_a_fresh_database_without_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let migrations = tempfile::tempdir().unwrap();
    std::fs::write(
        migrations.path().join("0001_a.sql"),
        "CREATE TABLE a (id INTEGER PRIMARY KEY);",
    )
    .unwrap();

    let migrator = Migrator::new(migrations.path()).await.unwrap();
    let db = open_temp(dir.path()).await;
    let backups = dir.path().join("backups");

    let outcome = db.migrate(&migrator, &backups).await.unwrap();

    assert_eq!(outcome.from, 0);
    assert_eq!(outcome.to, 1);
    assert!(
        outcome.backup.is_none(),
        "a fresh database holds nothing worth backing up"
    );
    db.close().await;
}

#[tokio::test]
async fn backs_up_before_upgrading_a_populated_database() {
    let dir = tempfile::tempdir().unwrap();
    let migrations = tempfile::tempdir().unwrap();
    std::fs::write(
        migrations.path().join("0001_a.sql"),
        "CREATE TABLE a (id INTEGER PRIMARY KEY);",
    )
    .unwrap();

    let db = open_temp(dir.path()).await;
    let backups = dir.path().join("backups");

    let first = Migrator::new(migrations.path()).await.unwrap();
    let outcome = db.migrate(&first, &backups).await.unwrap();
    assert_eq!(outcome.from, 0);
    assert_eq!(outcome.to, 1);

    std::fs::write(
        migrations.path().join("0002_b.sql"),
        "CREATE TABLE b (id INTEGER PRIMARY KEY);",
    )
    .unwrap();
    let second = Migrator::new(migrations.path()).await.unwrap();
    let outcome = db.migrate(&second, &backups).await.unwrap();

    assert_eq!(outcome.from, 1);
    assert_eq!(outcome.to, 2);
    let backup = outcome.backup.expect("an upgrade must take a backup");
    let name = backup.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("hearthsound-1-to-2-") && name.ends_with(".db"),
        "unexpected backup name `{name}`"
    );

    let url = format!("sqlite://{}", backup.display());
    let backup_pool = SqlitePool::connect(&url).await.unwrap();
    let tables: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'a'",
    )
    .fetch_one(&backup_pool)
    .await
    .unwrap();
    assert_eq!(tables, 1, "the backup must contain the pre-upgrade schema");
    backup_pool.close().await;

    db.close().await;
}
