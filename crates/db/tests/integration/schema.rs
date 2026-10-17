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
         VALUES ('app.hearthsound.deezer', '0.1.0', 'hearthsound:plugin@0.2.0', '{}', x'00',
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

/// The activity tables exist and their constraints hold.
#[tokio::test]
async fn activity_tables_exist_and_reject_invalid_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let pool = db.writer();

    for table in ["bookmarks", "now_playing", "play_queues", "user_ratings"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "the {table} table exists");
    }

    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (x'01', 'echo', 'Echo', 'x', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    insert_track(pool, &[2], "Bookmarked", "Nobody").await;

    sqlx::query(
        "INSERT INTO bookmarks (user_id, track_id, position_ms, comment, updated_at)
         VALUES (x'01', x'02', 1000, 'chapter', 5)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO now_playing (user_id, client, track_id, started_at, position_ms, state)
         VALUES (x'01', 'web', x'02', 5, 0, 'playing')",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO play_queues (user_id, client, current_index, position_ms, track_ids, updated_at)
         VALUES (x'01', 'web', 0, 0, '[\"tr_one\"]', 5)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at)
         VALUES (x'01', 'track', x'02', 5, 5)",
    )
    .execute(pool)
    .await
    .unwrap();

    for rating in [0, 6] {
        let rejected = sqlx::query(
            "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at)
             VALUES (x'01', 'album', x'03', ?, 5)",
        )
        .bind(rating)
        .execute(pool)
        .await;
        assert!(rejected.is_err(), "a rating of {rating} is rejected");
    }

    let unknown_kind = sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at)
         VALUES (x'01', 'playlist', x'03', 3, 5)",
    )
    .execute(pool)
    .await;
    assert!(unknown_kind.is_err(), "an unknown entity kind is rejected");

    db.close().await;
}

/// The session and quota tables exist, and both follow their account.
#[tokio::test]
async fn session_and_quota_rows_follow_their_account() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let pool = db.writer();

    for table in ["sessions", "user_quotas"] {
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(count, 1, "the {table} table exists");
    }

    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (x'01', 'echo', 'Echo', 'x', 0, 0)",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at,
                               last_seen_at, user_agent)
         VALUES (x'11', x'01', x'aa', 'csrf-one', 10, 99, 10, 'a client')",
    )
    .execute(pool)
    .await
    .unwrap();

    let duplicate = sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, csrf_token, created_at, expires_at,
                               last_seen_at, user_agent)
         VALUES (x'12', x'01', x'aa', 'csrf-two', 10, 99, 10, NULL)",
    )
    .execute(pool)
    .await;
    assert!(duplicate.is_err(), "a token hash is unique");

    sqlx::query(
        "INSERT INTO user_quotas (user_id, acquisitions_per_day, acquisition_bytes_per_day)
         VALUES (x'01', 5, NULL)",
    )
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("DELETE FROM users WHERE id = x'01'")
        .execute(pool)
        .await
        .unwrap();

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0, "deleting the account removes its sessions");
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM user_quotas")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0, "deleting the account removes its quota row");

    db.close().await;
}

/// The staged upgrade table exists, rejects unknown sources, and follows its
/// plugin and repository.
#[tokio::test]
async fn plugin_upgrade_rows_follow_their_plugin_and_repository() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let pool = db.writer();

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'plugin_upgrades'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "the plugin_upgrades table exists");

    sqlx::query(
        "INSERT INTO plugin_repositories (id, url, name, public_key, enabled)
         VALUES (x'01', 'https://plugins.example/v1/index.json', 'Example', NULL, 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO plugins (id, version, api, manifest, sha256, source, repository_id,
                              approved_permissions, enabled, installed_at)
         VALUES ('app.hearthsound.deezer', '0.1.0', 'hearthsound:plugin@0.2.0', '{}', x'00',
                 'repository', x'01', '{}', 1, 0)",
    )
    .execute(pool)
    .await
    .unwrap();

    let unknown_source = sqlx::query(
        "INSERT INTO plugin_upgrades (plugin_id, version, api, manifest, sha256, source,
                                      repository_id, staged_at)
         VALUES ('app.hearthsound.deezer', '0.2.0', 'hearthsound:plugin@0.2.0', '{}', x'00',
                 'elsewhere', x'01', 0)",
    )
    .execute(pool)
    .await;
    assert!(unknown_source.is_err(), "the source must be a known source");

    sqlx::query(
        "INSERT INTO plugin_upgrades (plugin_id, version, api, manifest, sha256, source,
                                      repository_id, staged_at)
         VALUES ('app.hearthsound.deezer', '0.2.0', 'hearthsound:plugin@0.2.0', '{}', x'00',
                 'repository', x'01', 0)",
    )
    .execute(pool)
    .await
    .unwrap();

    let second = sqlx::query(
        "INSERT INTO plugin_upgrades (plugin_id, version, api, manifest, sha256, source,
                                      repository_id, staged_at)
         VALUES ('app.hearthsound.deezer', '0.3.0', 'hearthsound:plugin@0.2.0', '{}', x'00',
                 'repository', x'01', 0)",
    )
    .execute(pool)
    .await;
    assert!(second.is_err(), "a plugin holds at most one staged upgrade");

    sqlx::query("DELETE FROM plugin_repositories WHERE id = x'01'")
        .execute(pool)
        .await
        .unwrap();
    let repository: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT repository_id FROM plugin_upgrades")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        repository, None,
        "deleting the repository clears the upgrade's link"
    );

    sqlx::query("DELETE FROM plugins WHERE id = 'app.hearthsound.deezer'")
        .execute(pool)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM plugin_upgrades")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(
        remaining, 0,
        "deleting the plugin removes its staged upgrade"
    );

    db.close().await;
}
