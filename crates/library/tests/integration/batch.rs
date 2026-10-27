//! Tests for the batch loaders: personal track membership and the preferred
//! media file of many tracks at once, including reads past the chunk boundary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::path::Path;

use hearthsound_core::{Availability, EntityId, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::{MAX_IDS_PER_QUERY, personal_track_ids};
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, TrackInput,
    media_files::preferred_media_files,
};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

async fn insert_user(db: &Database, name: &str) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("{name}-{id}"))
    .bind(name)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts a catalog album and returns its id.
async fn make_album(library: &LibraryRepository) -> EntityId {
    let album = AlbumInput::new("Batch Album");
    library
        .upsert_album(&album, &[ArtistCredit::new("Batch Artist", "main")])
        .await
        .unwrap()
}

/// Inserts a track in `album` and returns its id.
async fn make_track(library: &LibraryRepository, album: EntityId, number: i64) -> EntityId {
    let mut input = TrackInput::new(format!("Track {number}"), Availability::Remote);
    input.album_id = Some(album);
    input.track_number = Some(number);
    library.insert_track(&input, &[], &[]).await.unwrap()
}

/// Attaches a managed library file to a track.
async fn attach_managed(library: &LibraryRepository, track: EntityId, rel_path: &str) {
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: rel_path.to_owned(),
            size: 1_000,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(123_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
}

/// Records a like on a track.
async fn like(db: &Database, user: uuid::Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) VALUES (?, 'track', ?, 'like', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Saves a track into a user's library.
async fn save(db: &Database, user: uuid::Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Creates a playlist owned by `user` and returns its id.
async fn playlist(db: &Database, user: uuid::Uuid, temporary: bool) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_temporary, created_at, updated_at) \
         VALUES (?, ?, 'mix', ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(temporary)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Adds a track to a playlist.
async fn add_to_playlist(db: &Database, playlist: uuid::Uuid, track: EntityId, position: i64) {
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) VALUES (?, ?, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(position)
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Reads the personal track membership of `ids` on a reader connection.
async fn personal(db: &Database, user: uuid::Uuid, ids: &[EntityId]) -> HashSet<EntityId> {
    let mut conn = db.readers().acquire().await.unwrap();
    personal_track_ids(&mut conn, user, ids).await.unwrap()
}

/// A liked, saved, and permanent-playlist track is in the personal library;
/// a temporary playlist track and another user's track are not.
#[tokio::test]
async fn personal_membership_follows_the_scope_rule() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let owner = insert_user(&db, "owner").await;
    let stranger = insert_user(&db, "stranger").await;
    let album = make_album(&library).await;

    let liked = make_track(&library, album, 1).await;
    let saved = make_track(&library, album, 2).await;
    let kept = make_track(&library, album, 3).await;
    let temporary = make_track(&library, album, 4).await;
    let theirs = make_track(&library, album, 5).await;
    let untouched = make_track(&library, album, 6).await;

    like(&db, owner, liked).await;
    save(&db, owner, saved).await;
    let permanent = playlist(&db, owner, false).await;
    add_to_playlist(&db, permanent, kept, 0).await;
    let scratch = playlist(&db, owner, true).await;
    add_to_playlist(&db, scratch, temporary, 0).await;
    like(&db, stranger, theirs).await;

    let all = [liked, saved, kept, temporary, theirs, untouched];
    let found = personal(&db, owner, &all).await;
    assert_eq!(found, HashSet::from([liked, saved, kept]));
    assert!(personal(&db, owner, &[]).await.is_empty());
}

/// More than one chunk of ids is read in full: members on both sides of the
/// chunk boundary are found and non-members are not.
#[tokio::test]
async fn personal_membership_reads_past_the_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let owner = insert_user(&db, "owner").await;
    let album = make_album(&library).await;

    let count = MAX_IDS_PER_QUERY + 201;
    let mut tracks = Vec::with_capacity(count);
    for number in 0..count {
        tracks.push(make_track(&library, album, number as i64 + 1).await);
    }
    let members = [0, MAX_IDS_PER_QUERY - 1, MAX_IDS_PER_QUERY, count - 1];
    for &index in &members {
        save(&db, owner, tracks[index]).await;
    }

    let found = personal(&db, owner, &tracks).await;
    let expected: HashSet<EntityId> = members.iter().map(|&i| tracks[i]).collect();
    assert_eq!(found, expected);
}

/// A preferred file is returned per track; tracks with no file are absent, and
/// a track with two files returns the one flagged preferred.
#[tokio::test]
async fn preferred_files_are_returned_per_track() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = make_album(&library).await;

    let with_file = make_track(&library, album, 1).await;
    let without = make_track(&library, album, 2).await;
    let with_two = make_track(&library, album, 3).await;
    attach_managed(&library, with_file, "a.flac").await;
    attach_managed(&library, with_two, "b.flac").await;
    attach_managed(&library, with_two, "c.flac").await;

    let mut conn = db.readers().acquire().await.unwrap();
    let files = preferred_media_files(&mut conn, &[with_file, without, with_two])
        .await
        .unwrap();
    assert_eq!(files.len(), 2);
    assert!(!files.contains_key(&without));
    assert_eq!(files[&with_file].rel_path, "a.flac");
    assert!(files[&with_file].is_preferred);
    assert!(files[&with_two].is_preferred);
    assert_eq!(files[&with_two].track_id, with_two);
}

/// Preferred files are found on both sides of the chunk boundary.
#[tokio::test]
async fn preferred_files_read_past_the_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = make_album(&library).await;

    let count = MAX_IDS_PER_QUERY + 201;
    let mut tracks = Vec::with_capacity(count);
    for number in 0..count {
        tracks.push(make_track(&library, album, number as i64 + 1).await);
    }
    let members = [0, MAX_IDS_PER_QUERY, count - 1];
    for &index in &members {
        attach_managed(&library, tracks[index], &format!("{index}.flac")).await;
    }

    let mut conn = db.readers().acquire().await.unwrap();
    let files = preferred_media_files(&mut conn, &tracks).await.unwrap();
    let found: HashSet<EntityId> = files.keys().copied().collect();
    let expected: HashSet<EntityId> = members.iter().map(|&i| tracks[i]).collect();
    assert_eq!(found, expected);
    assert_eq!(
        files[&tracks[count - 1]].rel_path,
        format!("{}.flac", count - 1)
    );
}
