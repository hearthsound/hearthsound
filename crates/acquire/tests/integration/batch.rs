//! Tests for the batch acquisition lookups: the active row of each track, and
//! the latest row of each track in any state, including reads past the chunk
//! boundary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::Path;

use hearthsound_acquire::repo::{self, AcquisitionRow, NewAcquisition};
use hearthsound_core::{Availability, EntityId, FailureReason, Granularity, Priority};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::MAX_IDS_PER_QUERY;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, TrackInput};
use uuid::Uuid;

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(db.writer()).await.unwrap();
    db
}

/// Inserts a catalog track in `album` and returns its id.
async fn make_track(library: &LibraryRepository, album: EntityId, number: i64) -> EntityId {
    let mut input = TrackInput::new(format!("Track {number}"), Availability::Remote);
    input.album_id = Some(album);
    input.track_number = Some(number);
    library
        .insert_track(&input, &[ArtistCredit::new("Batch Artist", "main")], &[])
        .await
        .unwrap()
}

/// Inserts a queued acquisition row for `track` and returns its id.
async fn request(db: &Database, track: EntityId) -> Uuid {
    let id = Uuid::now_v7();
    let mut conn = db.writer().acquire().await.unwrap();
    repo::insert(
        &mut conn,
        &NewAcquisition {
            id,
            track_id: Some(track),
            album_id: None,
            granularity: Granularity::Track,
            priority: Priority::Queue,
            reason: "batch test".to_owned(),
            requested_by: None,
        },
    )
    .await
    .unwrap();
    id
}

/// Moves a queued row to `failed`.
async fn fail(db: &Database, id: Uuid) {
    let mut conn = db.writer().acquire().await.unwrap();
    repo::fail(&mut conn, id, FailureReason::NoCandidates, None)
        .await
        .unwrap();
}

/// Moves a queued row to `cancelled`.
async fn cancel(db: &Database, id: Uuid) {
    let mut conn = db.writer().acquire().await.unwrap();
    assert!(repo::cancel(&mut conn, id, None).await.unwrap());
}

/// Reads the active rows of `tracks` on a reader connection.
async fn active(db: &Database, tracks: &[EntityId]) -> HashMap<EntityId, AcquisitionRow> {
    let mut conn = db.readers().acquire().await.unwrap();
    repo::active_for_tracks(&mut conn, tracks).await.unwrap()
}

/// Reads the latest row of `tracks` on a reader connection.
async fn latest(db: &Database, tracks: &[EntityId]) -> HashMap<EntityId, AcquisitionRow> {
    let mut conn = db.readers().acquire().await.unwrap();
    repo::latest_for_tracks(&mut conn, tracks).await.unwrap()
}

/// The active lookup keeps only non-terminal rows, while the latest lookup
/// keeps the newest row of each track whatever its state.
#[tokio::test]
async fn active_keeps_only_non_terminal_rows_and_latest_keeps_any_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = library
        .upsert_album(
            &AlbumInput::new("Batch Album"),
            &[ArtistCredit::new("Batch Artist", "main")],
        )
        .await
        .unwrap();
    let queued = make_track(&library, album, 1).await;
    let cancelled = make_track(&library, album, 2).await;
    let failed = make_track(&library, album, 3).await;
    let untouched = make_track(&library, album, 4).await;

    let queued_row = request(&db, queued).await;
    let cancelled_row = request(&db, cancelled).await;
    cancel(&db, cancelled_row).await;
    let failed_row = request(&db, failed).await;
    fail(&db, failed_row).await;

    let tracks = [queued, cancelled, failed, untouched];

    let active_rows = active(&db, &tracks).await;
    assert_eq!(active_rows.len(), 1);
    assert_eq!(active_rows[&queued].id, queued_row);

    let latest_rows = latest(&db, &tracks).await;
    assert_eq!(latest_rows.len(), 3);
    assert_eq!(latest_rows[&queued].id, queued_row);
    assert_eq!(latest_rows[&cancelled].id, cancelled_row);
    assert_eq!(latest_rows[&failed].id, failed_row);
    assert!(!latest_rows.contains_key(&untouched));
}

/// A newer terminal row hides an older active one from the latest lookup but
/// not from the active lookup, which still answers with the active row.
#[tokio::test]
async fn an_older_active_row_survives_a_newer_terminal_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = library
        .upsert_album(
            &AlbumInput::new("Batch Album"),
            &[ArtistCredit::new("Batch Artist", "main")],
        )
        .await
        .unwrap();
    let track = make_track(&library, album, 1).await;

    let older = request(&db, track).await;
    let newer = request(&db, track).await;
    cancel(&db, newer).await;

    let active_rows = active(&db, &[track]).await;
    assert_eq!(active_rows[&track].id, older);
    let latest_rows = latest(&db, &[track]).await;
    assert_eq!(latest_rows[&track].id, newer);
}

/// Active rows on both sides of the chunk boundary are found in one call, and
/// the latest lookup agrees on the same tracks.
#[tokio::test]
async fn batch_lookups_read_past_the_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = library
        .upsert_album(
            &AlbumInput::new("Batch Album"),
            &[ArtistCredit::new("Batch Artist", "main")],
        )
        .await
        .unwrap();

    let count = MAX_IDS_PER_QUERY + 201;
    let mut tracks = Vec::with_capacity(count);
    for number in 0..count {
        tracks.push(make_track(&library, album, number as i64 + 1).await);
    }
    let members = [0, MAX_IDS_PER_QUERY - 1, MAX_IDS_PER_QUERY, count - 1];
    let mut expected = HashMap::new();
    for &index in &members {
        expected.insert(tracks[index], request(&db, tracks[index]).await);
    }

    let active_rows = active(&db, &tracks).await;
    assert_eq!(active_rows.len(), members.len());
    for (track, id) in &expected {
        assert_eq!(active_rows[track].id, *id);
    }

    let latest_rows = latest(&db, &tracks).await;
    assert_eq!(latest_rows.len(), members.len());
    for (track, id) in &expected {
        assert_eq!(latest_rows[track].id, *id);
    }
}
