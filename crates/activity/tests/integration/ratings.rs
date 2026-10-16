//! Tests for per-user ratings.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{ActivityError, ActivityService, MAX_RATING};
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::common::{insert_album, insert_user, open_database};

/// Returns the stored rating of one user's track, which is empty when the user
/// has rated nothing.
async fn stored_rating(database: &Database, user: Uuid, track: Uuid) -> Vec<i64> {
    sqlx::query_scalar("SELECT rating FROM user_ratings WHERE user_id = ? AND entity_id = ?")
        .bind(user.as_bytes().as_slice())
        .bind(track.as_bytes().as_slice())
        .fetch_all(database.readers())
        .await
        .unwrap()
}

/// A rating is stored, replaced, and cleared, and an out-of-range one is
/// refused without touching the stored row.
#[tokio::test]
async fn a_rating_is_stored_replaced_and_cleared() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    assert!(stored_rating(&database, user, tracks[0]).await.is_empty());

    activity
        .set_rating(user, EntityKind::Track, tracks[0], 4)
        .await
        .unwrap();
    assert_eq!(stored_rating(&database, user, tracks[0]).await, vec![4]);

    activity
        .set_rating(user, EntityKind::Track, tracks[0], MAX_RATING)
        .await
        .unwrap();
    assert_eq!(
        stored_rating(&database, user, tracks[0]).await,
        vec![i64::from(MAX_RATING)]
    );

    activity
        .set_rating(user, EntityKind::Track, tracks[0], 0)
        .await
        .unwrap();
    assert!(stored_rating(&database, user, tracks[0]).await.is_empty());

    let refused = activity
        .set_rating(user, EntityKind::Track, tracks[0], MAX_RATING + 1)
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            ActivityError::InvalidRating {
                rating: 6,
                max: MAX_RATING
            }
        ),
        "{refused:?}"
    );
    assert!(stored_rating(&database, user, tracks[0]).await.is_empty());
}

/// Two users rate the same track independently, and an album rating is its own
/// row.
#[tokio::test]
async fn ratings_are_per_user_and_per_entity() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);
    let (album, tracks) = insert_album(&library).await;

    activity
        .set_rating(first, EntityKind::Track, tracks[0], 5)
        .await
        .unwrap();
    activity
        .set_rating(second, EntityKind::Track, tracks[0], 1)
        .await
        .unwrap();
    activity
        .set_rating(first, EntityKind::Album, album, 3)
        .await
        .unwrap();

    let rows =
        sqlx::query!("SELECT user_id, entity_kind, rating FROM user_ratings ORDER BY rating",)
            .fetch_all(database.readers())
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].user_id, second.as_bytes().to_vec());
    assert_eq!(rows[0].entity_kind, "track");
    assert_eq!(rows[0].rating, 1);
    assert_eq!(rows[1].user_id, first.as_bytes().to_vec());
    assert_eq!(rows[1].entity_kind, "album");
    assert_eq!(rows[1].rating, 3);
    assert_eq!(rows[2].user_id, first.as_bytes().to_vec());
    assert_eq!(rows[2].entity_kind, "track");
    assert_eq!(rows[2].rating, 5);
}

/// A like and a rating are different rows, so clearing one leaves the other.
#[tokio::test]
async fn a_rating_and_a_like_are_separate() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;

    activity
        .star(user, &[(EntityKind::Track, tracks[0])])
        .await
        .unwrap();
    activity
        .set_rating(user, EntityKind::Track, tracks[0], 5)
        .await
        .unwrap();
    activity
        .set_rating(user, EntityKind::Track, tracks[0], 0)
        .await
        .unwrap();

    let liked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_feedback WHERE user_id = ? AND value = 'like'",
    )
    .bind(user.as_bytes().as_slice())
    .fetch_one(database.readers())
    .await
    .unwrap();
    assert_eq!(liked, 1, "clearing the rating leaves the like");
}
