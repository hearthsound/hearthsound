//! Tests for saving and unsaving items in a personal library.
//!
//! A save writes a `user_library` row of its own and keeps the personal-library
//! pin of every track it affects in step, so a saved track and the tracks of a
//! saved album are pinned; an unsave removes only what a save wrote and releases
//! the pins nothing else holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::time::Duration;

use hearthsound_activity::ActivityService;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use hearthsound_library::query::{personal_album_ids, personal_track_ids};
use uuid::Uuid;

use crate::common::{insert_album, insert_user, is_pinned, open_database, track_id};

/// Returns the client id of an album.
fn album_id(album: Uuid) -> EntityId {
    EntityId::from_uuid(IdPrefix::Album, album)
}

/// Reads the personal track membership of `ids` for one user.
async fn personal_tracks(database: &Database, user: Uuid, ids: &[EntityId]) -> HashSet<EntityId> {
    let mut conn = database.readers().acquire().await.unwrap();
    personal_track_ids(&mut conn, user, ids).await.unwrap()
}

/// Counts the rows a save wrote for one entity.
async fn saved_rows(database: &Database, user: Uuid, kind: &str, entity: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM user_library \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND origin = 'saved'",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind)
    .bind(entity.as_bytes().as_slice())
    .fetch_one(database.readers())
    .await
    .unwrap()
}

/// Saving a track pins it, puts it in the personal scope, and wakes the
/// maintenance worker; unsaving releases both and wakes it again.
#[tokio::test]
async fn saving_a_track_pins_it_and_unsaving_releases_those_pins() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let signal = activity.maintenance_signal().clone();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let first = track_id(tracks[0]);

    activity
        .save(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(is_pinned(&library, tracks[0]).await, "the save pinned it");
    assert_eq!(saved_rows(&database, user, "track", tracks[0]).await, 1);
    assert_eq!(
        personal_tracks(&database, user, &[first]).await,
        HashSet::from([first]),
        "the personal scope picks the saved track up"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), signal.notified())
            .await
            .is_ok(),
        "the save woke the maintenance worker"
    );

    activity
        .unsave(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(
        !is_pinned(&library, tracks[0]).await,
        "the unsave released it"
    );
    assert_eq!(saved_rows(&database, user, "track", tracks[0]).await, 0);
    assert!(personal_tracks(&database, user, &[first]).await.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), signal.notified())
            .await
            .is_ok(),
        "the unsave woke the maintenance worker"
    );
}

/// Saving an album pins every track of it and puts the album in the personal
/// scope; unsaving releases the member tracks again.
#[tokio::test]
async fn saving_an_album_pins_its_member_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;

    activity
        .save(user, &[(EntityKind::Album, album)])
        .await
        .unwrap();
    for track in &tracks {
        assert!(
            is_pinned(&library, *track).await,
            "the album save pinned its member tracks"
        );
    }
    let mut conn = database.readers().acquire().await.unwrap();
    assert_eq!(
        personal_album_ids(&mut conn, user, &[album_id(album)])
            .await
            .unwrap(),
        HashSet::from([album_id(album)])
    );

    activity
        .unsave(user, &[(EntityKind::Album, album)])
        .await
        .unwrap();
    for track in &tracks {
        assert!(
            !is_pinned(&library, *track).await,
            "the unsave released the member tracks"
        );
    }
    assert_eq!(saved_rows(&database, user, "album", album).await, 0);
}

/// An unsave removes only the rows a save wrote: an upload's row and the pin it
/// holds stay.
#[tokio::test]
async fn unsaving_keeps_an_uploaded_tracks_row_and_pin() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'upload', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(tracks[0].as_bytes().as_slice())
    .execute(database.writer())
    .await
    .unwrap();

    activity
        .save(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert_eq!(saved_rows(&database, user, "track", tracks[0]).await, 1);
    activity
        .unsave(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();

    assert_eq!(saved_rows(&database, user, "track", tracks[0]).await, 0);
    assert!(
        is_pinned(&library, tracks[0]).await,
        "the upload still pins the track"
    );

    // The negative direction of the same rule: an unsave removes nothing else.
    activity
        .unsave(user, &[(EntityKind::Album, Uuid::now_v7())])
        .await
        .unwrap();
    assert!(is_pinned(&library, tracks[0]).await);
}

/// Saving two tracks writes both rows in one transaction.
#[tokio::test]
async fn saving_several_entities_writes_every_row() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;

    activity
        .save(
            user,
            &[(EntityKind::Track, tracks[0]), (EntityKind::Album, album)],
        )
        .await
        .unwrap();
    assert_eq!(saved_rows(&database, user, "track", tracks[0]).await, 1);
    assert_eq!(saved_rows(&database, user, "album", album).await, 1);
    for track in &tracks {
        assert!(is_pinned(&library, *track).await);
    }
}
