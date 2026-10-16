//! Tests for bookmarks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::Bookmarks;
use hearthsound_library::LibraryRepository;

use crate::common::{insert_album, insert_user, open_database, track_id};

/// A bookmark is saved, replaced, and removed, and a resave keeps one row.
#[tokio::test]
async fn a_bookmark_is_saved_replaced_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let bookmarks = Bookmarks::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let track = track_id(tracks[0]);

    assert!(bookmarks.list(user).await.unwrap().is_empty());

    let saved = bookmarks
        .upsert(user, track, 30_000, Some("chorus"))
        .await
        .unwrap();
    assert_eq!(saved.position_ms, 30_000);
    assert_eq!(saved.comment.as_deref(), Some("chorus"));

    let replaced = bookmarks.upsert(user, track, 45_000, None).await.unwrap();
    assert_eq!(replaced.position_ms, 45_000);
    assert_eq!(replaced.comment, None);
    assert!(replaced.updated_at >= saved.updated_at);

    let stored = bookmarks.list(user).await.unwrap();
    assert_eq!(stored.len(), 1, "one bookmark per user and track");
    assert_eq!(stored[0], replaced);

    assert!(bookmarks.delete(user, track).await.unwrap());
    assert!(!bookmarks.delete(user, track).await.unwrap());
    assert!(bookmarks.list(user).await.unwrap().is_empty());
}

/// Bookmarks are listed newest first and are per user.
#[tokio::test]
async fn bookmarks_are_listed_newest_first_and_per_user() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let bookmarks = Bookmarks::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;
    let (one, two) = (track_id(tracks[0]), track_id(tracks[1]));

    bookmarks.upsert(first, one, 1_000, None).await.unwrap();
    bookmarks.upsert(first, two, 2_000, None).await.unwrap();
    bookmarks.upsert(second, one, 3_000, None).await.unwrap();

    let listed = bookmarks.list(first).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|bookmark| bookmark.track_id)
            .collect::<Vec<_>>(),
        [two, one],
        "the most recently saved bookmark comes first"
    );
    assert_eq!(bookmarks.list(second).await.unwrap().len(), 1);
}
