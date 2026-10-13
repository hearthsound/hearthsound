//! Tests for the bundled schema: it applies to a fresh database, and the shared
//! search index follows inserts, renames, and deletes of tracks, albums, and
//! artists without one entity kind disturbing another.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_db::{Database, MIGRATOR};
use sqlx::SqlitePool;

async fn migrated(dir: &std::path::Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Returns `(entity_kind, entity_id)` for every index row matching `query`.
async fn search(pool: &SqlitePool, query: &str) -> Vec<(String, Vec<u8>)> {
    sqlx::query_as(
        "SELECT entity_kind, entity_id FROM search_fts WHERE search_fts MATCH ? ORDER BY rowid",
    )
    .bind(query)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn insert_track(pool: &SqlitePool, id: &[u8], title: &str, artist: &str) {
    sqlx::query(
        "INSERT INTO tracks (id, title, sort_title, display_artist, origin, availability,
                             created_at, updated_at)
         VALUES (?, ?, ?, ?, 'local', 'local', 0, 0)",
    )
    .bind(id)
    .bind(title)
    .bind(title.to_lowercase())
    .bind(artist)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn schema_applies_and_accepts_a_plugin_without_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;

    sqlx::query(
        "INSERT INTO plugins (id, version, api, manifest, sha256, source, installed_at)
         VALUES ('app.hearthsound.deezer', '0.1.0', 'hearthsound:plugin@0.1.0', '{}', x'00',
                 'sideload', 0)",
    )
    .execute(db.writer())
    .await
    .unwrap();

    db.close().await;
}

#[tokio::test]
async fn index_follows_track_inserts_renames_and_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let pool = db.writer();

    insert_track(pool, &[1], "Caf\u{e9} del Mar", "Energy 52").await;
    assert_eq!(search(pool, "cafe").await, vec![("track".into(), vec![1])]);
    assert_eq!(
        search(pool, "energy").await,
        vec![("track".into(), vec![1])]
    );

    sqlx::query("UPDATE tracks SET title = 'Sunset' WHERE id = x'01'")
        .execute(pool)
        .await
        .unwrap();
    assert!(
        search(pool, "cafe").await.is_empty(),
        "old title must be gone"
    );
    assert_eq!(
        search(pool, "sunset").await,
        vec![("track".into(), vec![1])]
    );

    sqlx::query("UPDATE tracks SET availability = 'cached' WHERE id = x'01'")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        search(pool, "sunset").await.len(),
        1,
        "a non-text update must not duplicate the index row"
    );

    sqlx::query("DELETE FROM tracks WHERE id = x'01'")
        .execute(pool)
        .await
        .unwrap();
    assert!(search(pool, "sunset").await.is_empty());

    db.close().await;
}

#[tokio::test]
async fn entity_kinds_with_equal_rowids_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let pool = db.writer();

    // The first row of each table gets rowid 1 in its own table.
    insert_track(pool, &[1], "Harmony", "Nobody").await;
    sqlx::query(
        "INSERT INTO albums (id, title, sort_title, display_artist, origin, created_at, updated_at)
         VALUES (x'02', 'Harmony', 'harmony', 'Nobody', 'local', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO artists (id, name, sort_name, origin, created_at, updated_at)
         VALUES (x'03', 'Harmony', 'harmony', 'local', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();

    let mut kinds: Vec<String> = search(pool, "harmony")
        .await
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    kinds.sort();
    assert_eq!(kinds, vec!["album", "artist", "track"]);

    sqlx::query("DELETE FROM albums WHERE id = x'02'")
        .execute(pool)
        .await
        .unwrap();
    let remaining = search(pool, "harmony").await;
    assert_eq!(
        remaining,
        vec![("track".into(), vec![1]), ("artist".into(), vec![3])],
        "deleting the album must leave the track and artist rows"
    );

    db.close().await;
}
