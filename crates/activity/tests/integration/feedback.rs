//! Tests for likes, dislikes, and the personal-library pin they keep in step.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_activity::{ActivityService, Feedback};
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

/// Returns the feedback value one user holds about an entity.
async fn feedback_value(
    database: &Database,
    user: Uuid,
    kind: &str,
    entity: Uuid,
) -> Option<String> {
    sqlx::query_scalar(
        "SELECT value FROM user_feedback WHERE user_id = ? AND entity_kind = ? AND entity_id = ?",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind)
    .bind(entity.as_bytes().as_slice())
    .fetch_optional(database.readers())
    .await
    .unwrap()
}

/// A like set through the feedback operation pins the track, like a star does.
#[tokio::test]
async fn a_like_feedback_pins_a_track() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let signal = activity.maintenance_signal().clone();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    activity
        .set_feedback(user, EntityKind::Track, tracks[0], Feedback::Like)
        .await
        .unwrap();

    assert_eq!(
        feedback_value(&database, user, "track", tracks[0])
            .await
            .as_deref(),
        Some("like")
    );
    assert!(is_pinned(&library, tracks[0]).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), signal.notified())
            .await
            .is_ok(),
        "the change woke the maintenance worker"
    );
}

/// A dislike replaces a like and releases the pin the like held; on its own it
/// pins nothing.
#[tokio::test]
async fn a_dislike_replaces_a_like_and_unpins() {
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
        .set_feedback(user, EntityKind::Album, album, Feedback::Dislike)
        .await
        .unwrap();

    assert_eq!(
        feedback_value(&database, user, "album", album)
            .await
            .as_deref(),
        Some("dislike")
    );
    for track in &tracks {
        assert!(!is_pinned(&library, *track).await, "a dislike pins nothing");
    }
}

/// Clearing removes whichever feedback the user held.
#[tokio::test]
async fn clearing_removes_a_like_or_a_dislike() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    activity
        .set_feedback(user, EntityKind::Track, tracks[0], Feedback::Like)
        .await
        .unwrap();
    activity
        .set_feedback(user, EntityKind::Track, tracks[0], Feedback::None)
        .await
        .unwrap();
    assert_eq!(
        feedback_value(&database, user, "track", tracks[0]).await,
        None
    );
    assert!(
        !is_pinned(&library, tracks[0]).await,
        "clearing the like released the pin"
    );

    activity
        .set_feedback(user, EntityKind::Track, tracks[0], Feedback::Dislike)
        .await
        .unwrap();
    activity
        .set_feedback(user, EntityKind::Track, tracks[0], Feedback::None)
        .await
        .unwrap();
    assert_eq!(
        feedback_value(&database, user, "track", tracks[0]).await,
        None
    );

    // Clearing feedback that was never recorded is not an error.
    activity
        .set_feedback(user, EntityKind::Track, tracks[1], Feedback::None)
        .await
        .unwrap();
    assert_eq!(
        feedback_value(&database, user, "track", tracks[1]).await,
        None
    );
}

/// A dislike of one user does not touch another user's like or pin.
#[tokio::test]
async fn one_users_dislike_keeps_another_users_like() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;

    activity
        .set_feedback(first, EntityKind::Track, tracks[0], Feedback::Like)
        .await
        .unwrap();
    activity
        .set_feedback(second, EntityKind::Track, tracks[0], Feedback::Dislike)
        .await
        .unwrap();

    assert_eq!(
        feedback_value(&database, first, "track", tracks[0])
            .await
            .as_deref(),
        Some("like")
    );
    assert_eq!(
        feedback_value(&database, second, "track", tracks[0])
            .await
            .as_deref(),
        Some("dislike")
    );
    assert!(
        is_pinned(&library, tracks[0]).await,
        "the other user still likes the track"
    );
}
