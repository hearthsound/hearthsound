//! The index queue: a claim leases a row, so a second worker cannot take it, and
//! a library change queues an album before the tracks it answers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{Availability, EntityKind};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::index_queue::{
    self, CLAIM_LEASE_MS, Depth, PRIORITY_LIBRARY, PRIORITY_TRACK,
};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, TrackInput};
use uuid::Uuid;

/// Opens a migrated database in `dir`.
async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// A claim pushes the row's `run_at` past the lease, so the next claim neither
/// sees it nor waits for it.
///
/// The queue is worked by several walks at once and the claim has no row lock:
/// the lease is the only thing that stops two workers walking one entity twice.
#[tokio::test]
async fn a_claim_puts_the_row_out_of_reach() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let mut connection = db.writer().acquire().await.unwrap();
    let entities: Vec<Uuid> = (0..3).map(|_| Uuid::now_v7()).collect();
    for entity in &entities {
        index_queue::enqueue(
            &mut connection,
            EntityKind::Album,
            *entity,
            Depth::Page,
            PRIORITY_LIBRARY,
        )
        .await
        .unwrap();
    }

    let first = index_queue::claim_many(&mut connection, 2).await.unwrap();
    assert_eq!(first.len(), 2, "two rows are due");
    let second = index_queue::claim_many(&mut connection, 2).await.unwrap();
    assert_eq!(second.len(), 1, "the leased rows are out of reach");
    assert!(
        !first
            .iter()
            .any(|claimed| claimed.entity == second[0].entity),
        "a claimed row is never handed out twice"
    );
    assert!(
        index_queue::claim_many(&mut connection, 2)
            .await
            .unwrap()
            .is_empty(),
        "every row is leased, so nothing is due"
    );

    // The lease is what keeps them out of reach, and it expires rather than
    // losing them: a worker that dies costs one lease, not the row.
    let lease: i64 = sqlx::query_scalar("SELECT MIN(run_at) FROM index_queue")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    assert!(
        lease > now + CLAIM_LEASE_MS - 60_000,
        "a claimed row is leased for CLAIM_LEASE_MS, got {lease} against {now}"
    );
    sqlx::query("UPDATE index_queue SET run_at = 0")
        .execute(&mut *connection)
        .await
        .unwrap();
    assert_eq!(
        index_queue::claim_many(&mut connection, 3)
            .await
            .unwrap()
            .len(),
        3,
        "an expired lease hands the rows back"
    );
}

/// A library track's album is queued ahead of the track.
///
/// The album's page carries the tracklist - titles, numbers, recordings, ISRCs
/// and credits - so walking it answers the track's own row without a provider
/// request per track. The track keeps its own row for the artists it credits, at
/// a lower urgency than any album page.
#[tokio::test]
async fn a_library_track_queues_its_album_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = library
        .upsert_album(
            &AlbumInput::new("First Light"),
            &[ArtistCredit::new("Dawn", "main")],
        )
        .await
        .unwrap();
    let track = library
        .insert_track(
            &TrackInput {
                album_id: Some(album),
                ..TrackInput::new("Dawn", Availability::Local)
            },
            &[ArtistCredit::new("Dawn", "main")],
            &[],
        )
        .await
        .unwrap();

    let mut connection = db.writer().acquire().await.unwrap();
    index_queue::enqueue_library(&mut connection, EntityKind::Track, track.uuid())
        .await
        .unwrap();

    let (album_priority, album_depth) =
        queued(&mut connection, EntityKind::Album, album.uuid()).await;
    assert_eq!(album_priority, PRIORITY_LIBRARY);
    assert_eq!(album_depth, Depth::Deep);
    let (track_priority, track_depth) =
        queued(&mut connection, EntityKind::Track, track.uuid()).await;
    assert_eq!(track_priority, PRIORITY_TRACK);
    assert_eq!(track_depth, Depth::Deep);
    assert!(
        album_priority < track_priority,
        "the album is walked before the track it answers"
    );

    let order: Vec<(String, i64)> = sqlx::query_as(
        "SELECT entity_kind, priority FROM index_queue ORDER BY priority, enqueued_at",
    )
    .fetch_all(&mut *connection)
    .await
    .unwrap();
    assert_eq!(
        order.first().map(|row| row.0.as_str()),
        Some("album"),
        "the claim order puts the album first, got {order:?}"
    );
}

/// Reads one queue row's priority and depth.
async fn queued(conn: &mut sqlx::SqliteConnection, kind: EntityKind, entity: Uuid) -> (i64, Depth) {
    let (priority, depth): (i64, String) = sqlx::query_as(
        "SELECT priority, depth FROM index_queue WHERE entity_kind = ? AND entity_id = ?",
    )
    .bind(match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    })
    .bind(entity.as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    (priority, Depth::parse(&depth).unwrap())
}
