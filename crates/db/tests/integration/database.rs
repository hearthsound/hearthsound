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

/// Copies the migrations a database that predates the plugin grant has applied.
///
/// The shipped migrations are immutable, so the versions that precede the grant are
/// selected by their numeric prefix and copied byte for byte into `dir`. A migrator
/// built over `dir` therefore sees exactly the schema such a database carried.
fn copy_pre_grant_migrations(dir: &Path) {
    let source = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"));
    let prefixes = ["0001_", "0002_", "0003_", "0004_"];
    let mut copied = 0;
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        if prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            std::fs::copy(entry.path(), dir.join(&name)).unwrap();
            copied += 1;
        }
    }
    assert_eq!(
        copied,
        prefixes.len(),
        "the migrations directory must hold the four migrations that precede the grant"
    );
}

#[tokio::test]
async fn a_pending_plugin_is_granted_its_manifest_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let migrations = tempfile::tempdir().unwrap();
    copy_pre_grant_migrations(migrations.path());

    let db = open_temp(dir.path()).await;
    let backups = dir.path().join("backups");
    let earlier = Migrator::new(migrations.path()).await.unwrap();
    let outcome = db.migrate(&earlier, &backups).await.unwrap();
    assert_eq!(outcome.from, 0);
    assert_eq!(outcome.to, 4);

    // Every row but the control is what an install recorded before this release: the
    // manifest is stored, no grant is recorded, and the plugin is disabled. A manifest
    // always carries a `permissions` object, so an install that requests nothing spells
    // it as `"permissions": {}` rather than omitting the key.
    //
    // The control's grant is written with a space the migration's `json_extract` would
    // not produce, so any rewrite of a row that already holds a grant shows up.
    let rows: [(&str, &str, Option<&str>, i64); 3] = [
        (
            "com.example.granted",
            r#"{"permissions":{"kv":{"maxBytes":1024}}}"#,
            Some(r#"{"kv": {"maxBytes": 1024}}"#),
            1,
        ),
        (
            "com.example.pending-empty",
            r#"{"permissions":{}}"#,
            None,
            0,
        ),
        (
            "com.example.pending-hosts",
            r#"{"permissions":{"http":{"hosts":["api.example.com"]}}}"#,
            None,
            0,
        ),
    ];
    for (index, (id, manifest, grant, enabled)) in rows.into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO plugins \
             (id, version, api, manifest, sha256, source, approved_permissions, enabled, installed_at) \
             VALUES (?, '1.0.0', 'hearthsound:plugin@0.2.0', ?, ?, 'sideload', ?, ?, 1)",
        )
        .bind(id)
        .bind(manifest)
        .bind(vec![index as u8 + 1; 32])
        .bind(grant)
        .bind(enabled)
        .execute(db.writer())
        .await
        .unwrap();
    }

    let outcome = db
        .migrate(&hearthsound_db::MIGRATOR, &backups)
        .await
        .unwrap();
    assert_eq!(outcome.from, 4);
    assert!(
        outcome.to >= 5,
        "the full migrator must apply the grant over the earlier schema"
    );

    let granted: Vec<(String, Option<String>, i64)> =
        sqlx::query_as("SELECT id, approved_permissions, enabled FROM plugins ORDER BY id")
            .fetch_all(db.writer())
            .await
            .unwrap();

    assert_eq!(
        granted,
        vec![
            (
                "com.example.granted".to_owned(),
                Some(r#"{"kv": {"maxBytes": 1024}}"#.to_owned()),
                1,
            ),
            (
                "com.example.pending-empty".to_owned(),
                Some("{}".to_owned()),
                1,
            ),
            (
                "com.example.pending-hosts".to_owned(),
                Some(r#"{"http":{"hosts":["api.example.com"]}}"#.to_owned()),
                1,
            ),
        ],
        "a pending row is granted its stored manifest's permissions and enabled, and an \
         existing grant is left alone"
    );

    db.close().await;
}
