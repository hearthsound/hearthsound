//! Tests for the batch dislike lookup: only the requesting user's dislikes of
//! the requested kind are reported, and reads past the chunk boundary are whole.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use hearthsound_activity::{MAX_IDS_PER_QUERY, disliked_ids};
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use uuid::Uuid;

use crate::common::{insert_user, open_database};

/// Records a feedback row of any value for an entity.
async fn feedback(database: &Database, user: Uuid, kind: &str, entity: Uuid, value: &str) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) VALUES (?, ?, ?, ?, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind)
    .bind(entity.as_bytes().as_slice())
    .bind(value)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Reads the disliked subset of `ids` on a reader connection.
async fn load(database: &Database, user: Uuid, kind: EntityKind, ids: &[Uuid]) -> HashSet<Uuid> {
    let mut conn = database.readers().acquire().await.unwrap();
    disliked_ids(&mut conn, user, kind, ids).await.unwrap()
}

/// A dislike is reported for its owner and kind only; likes, other users'
/// dislikes, and dislikes of another kind are not.
#[tokio::test]
async fn only_the_users_own_dislikes_of_the_kind_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let user = insert_user(&database).await;
    let other = insert_user(&database).await;

    let disliked = Uuid::now_v7();
    let liked = Uuid::now_v7();
    let theirs = Uuid::now_v7();
    let artist_disliked = Uuid::now_v7();
    feedback(&database, user, "track", disliked, "dislike").await;
    feedback(&database, user, "track", liked, "like").await;
    feedback(&database, other, "track", theirs, "dislike").await;
    feedback(&database, user, "artist", artist_disliked, "dislike").await;

    let ids = [disliked, liked, theirs, artist_disliked, Uuid::now_v7()];
    let found = load(&database, user, EntityKind::Track, &ids).await;
    assert_eq!(found, HashSet::from([disliked]));
    assert_eq!(
        load(&database, user, EntityKind::Artist, &ids).await,
        HashSet::from([artist_disliked])
    );
    assert!(
        load(&database, user, EntityKind::Track, &[])
            .await
            .is_empty()
    );
}

/// Dislikes on both sides of the chunk boundary are found in one call.
#[tokio::test]
async fn dislikes_are_found_past_the_chunk_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let user = insert_user(&database).await;

    let count = MAX_IDS_PER_QUERY + 201;
    let ids: Vec<Uuid> = (0..count).map(|_| Uuid::now_v7()).collect();
    let members = [0, MAX_IDS_PER_QUERY - 1, MAX_IDS_PER_QUERY, count - 1];
    for &index in &members {
        feedback(&database, user, "track", ids[index], "dislike").await;
    }

    let found = load(&database, user, EntityKind::Track, &ids).await;
    let expected: HashSet<Uuid> = members.iter().map(|&i| ids[i]).collect();
    assert_eq!(found, expected);
}
