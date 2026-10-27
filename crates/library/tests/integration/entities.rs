//! Tests for merging duplicate entity rows: the survivor a merge keeps, the
//! redirect that keeps the merged-away id resolving, and the credits both rows
//! brought to the one that is left.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{EntityKind, IdPrefix};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::entities::{canonical, merge};
use hearthsound_library::{LibraryRepository, sort_key};
use uuid::Uuid;

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Inserts an artist row and returns its raw primary key.
async fn insert_artist(db: &Database, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO artists (id, name, sort_name, created_at, updated_at) \
         VALUES (?, ?, ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(name)
    .bind(sort_key(name))
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts a remote track row and returns its raw primary key.
async fn insert_track(db: &Database, id: Uuid, title: &str) {
    sqlx::query(
        "INSERT INTO tracks (id, title, sort_title, availability, created_at, updated_at) \
         VALUES (?, ?, ?, 'remote', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(title)
    .bind(sort_key(title))
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records one credit of a track.
async fn credit(db: &Database, track: Uuid, artist: Uuid, position: i64, role: &str) {
    sqlx::query(
        "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
    )
    .bind(track.as_bytes().as_slice())
    .bind(artist.as_bytes().as_slice())
    .bind(position)
    .bind(role)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Merging two credited tracks leaves one row, one credit list, and an artist
/// that was credited on both listed once.
///
/// The two rows carry overlapping positions: the survivor's credits hold their
/// positions, the loser's credit of an artist the survivor already has is
/// dropped, and the loser's other credit is appended after the survivor's last
/// position rather than colliding with it.
#[tokio::test]
async fn merging_two_credited_tracks_keeps_one_credit_per_position() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let main = insert_artist(&db, "Energy 52").await;
    let guest = insert_artist(&db, "Guest Voice").await;
    let other = insert_artist(&db, "Other Guest").await;

    // The survivor is the older row, so hand the merge two known ids instead of
    // relying on the order they were created in.
    let mut ids = [Uuid::now_v7(), Uuid::now_v7()];
    ids.sort();
    let [survivor, loser] = ids;
    insert_track(&db, survivor, "Cafe del Mar").await;
    insert_track(&db, loser, "Cafe del Mar (Remix)").await;

    credit(&db, survivor, main, 0, "main").await;
    credit(&db, survivor, guest, 1, "featured").await;
    credit(&db, loser, main, 0, "main").await;
    credit(&db, loser, other, 1, "featured").await;

    let mut tx = db.writer().begin().await.unwrap();
    let merged = merge(&mut tx, EntityKind::Track, loser, survivor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(merged, survivor, "the older row survives");
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(remaining, 1, "one track row is left");
    assert!(library.get_track(loser_id(loser)).await.unwrap().is_none());

    let credits: Vec<(Vec<u8>, i64, String)> =
        sqlx::query_as("SELECT artist_id, position, role FROM track_artists ORDER BY position")
            .fetch_all(db.readers())
            .await
            .unwrap();
    assert_eq!(
        credits
            .iter()
            .map(|(_, position, _)| *position)
            .collect::<Vec<_>>(),
        [0, 1, 2],
        "one credit per position"
    );
    assert_eq!(
        credits
            .iter()
            .filter(|(artist, _, _)| artist.as_slice() == main.as_bytes().as_slice())
            .count(),
        1,
        "the artist both rows credited is listed once"
    );
    assert!(credits.iter().any(|(artist, _, role)| artist.as_slice()
        == guest.as_bytes().as_slice()
        && role == "featured"));
    assert!(credits.iter().any(|(artist, _, role)| artist.as_slice()
        == other.as_bytes().as_slice()
        && role == "featured"));

    // The merged-away id keeps resolving to the survivor.
    let mut read = db.readers().acquire().await.unwrap();
    assert_eq!(
        canonical(&mut read, EntityKind::Track, loser)
            .await
            .unwrap(),
        Some(survivor)
    );
    assert_eq!(
        canonical(&mut read, EntityKind::Track, survivor)
            .await
            .unwrap(),
        Some(survivor)
    );
}

/// Builds the client id of a raw track key.
fn loser_id(id: Uuid) -> hearthsound_core::EntityId {
    hearthsound_core::EntityId::from_uuid(IdPrefix::Track, id)
}
