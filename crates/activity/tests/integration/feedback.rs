//! Tests for likes and the personal-library pin they keep in step.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::ActivityService;
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::common::{insert_album, insert_user, is_pinned, open_database};

/// Returns how many artist likes one user holds.
async fn artist_likes(database: &Database, user: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM user_feedback \
         WHERE user_id = ? AND entity_kind = 'artist' AND value = 'like'",
    )
    .bind(user.as_bytes().as_slice())
    .fetch_one(database.readers())
    .await
    .unwrap()
}

/// A like pins the track, and unliking it releases the pin.
#[tokio::test]
async fn a_like_pins_a_track_until_it_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    assert!(!is_pinned(&library, tracks[0]).await);
    activity
        .star(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(is_pinned(&library, tracks[0]).await);
    assert!(
        !is_pinned(&library, tracks[1]).await,
        "only the liked track is pinned"
    );

    activity
        .unstar(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(!is_pinned(&library, tracks[0]).await);
}

/// A like of an album pins every track of it.
#[tokio::test]
async fn an_album_like_pins_its_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;

    activity
        .star(user, &[(EntityKind::Album, album)])
        .await
        .unwrap();
    for track in &tracks {
        assert!(is_pinned(&library, *track).await);
    }

    activity
        .unstar(user, &[(EntityKind::Album, album)])
        .await
        .unwrap();
    for track in &tracks {
        assert!(!is_pinned(&library, *track).await);
    }
}

/// A second user's like keeps the pin when the first one unlikes.
#[tokio::test]
async fn one_users_like_keeps_another_users_pin() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;

    activity
        .star(first, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    activity
        .star(second, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    activity
        .unstar(first, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(
        is_pinned(&library, tracks[0]).await,
        "the other user still likes the track"
    );

    activity
        .unstar(second, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(!is_pinned(&library, tracks[0]).await);
}

/// An artist like is recorded and pins nothing.
#[tokio::test]
async fn an_artist_like_pins_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let artist = library.list_artists().await.unwrap()[0].id.uuid();

    activity
        .star(user, &[(EntityKind::Artist, artist)])
        .await
        .unwrap();

    assert_eq!(
        artist_likes(&database, user).await,
        1,
        "the like is recorded"
    );
    for track in &tracks {
        assert!(!is_pinned(&library, *track).await);
    }

    activity
        .unstar(user, &[(EntityKind::Artist, artist)])
        .await
        .unwrap();
    assert_eq!(artist_likes(&database, user).await, 0);
}

/// A `user_library` row pins a track through the same refresh.
#[tokio::test]
async fn unliking_keeps_a_saved_tracks_pin() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(tracks[0].as_bytes().as_slice())
    .execute(database.writer())
    .await
    .unwrap();

    activity
        .star(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    activity
        .unstar(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    assert!(
        is_pinned(&library, tracks[0]).await,
        "the saved row still pins the track"
    );
}
