//! Tests for loading a user's annotations for a page of entities.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{ActivityService, Annotation, load_annotations};
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::common::{insert_album, insert_user, open_database, track_id};

/// Loads the annotations of one chunk of ids.
async fn load(
    database: &Database,
    user: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
) -> std::collections::HashMap<Uuid, Annotation> {
    let mut conn = database.readers().acquire().await.unwrap();
    load_annotations(&mut conn, user, kind, ids).await.unwrap()
}

/// A like, a rating, and plays are read for one page of tracks.
#[tokio::test]
async fn a_page_of_tracks_carries_its_stars_ratings_and_plays() {
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
        .set_rating(user, EntityKind::Track, tracks[0], 4)
        .await
        .unwrap();
    for _ in 0..3 {
        activity
            .plays()
            .record_play(
                user,
                tracks[1],
                Some("dsub"),
                1_700_000_000_000,
                180_000,
                true,
            )
            .await
            .unwrap();
    }

    let loaded = load(&database, user, EntityKind::Track, &tracks).await;
    let first = loaded.get(&tracks[0]).copied().unwrap_or_default();
    assert!(first.starred_at.is_some());
    assert_eq!(first.rating, Some(4));
    assert_eq!(first.play_count, 0);

    let second = loaded.get(&tracks[1]).copied().unwrap_or_default();
    assert_eq!(second.starred_at, None);
    assert_eq!(second.rating, None);
    assert_eq!(second.play_count, 3);
    assert_eq!(second.last_played, Some(1_700_000_000_000));

    // An id the user has done nothing with is absent, and every track is asked
    // about in one page.
    let untouched = load(&database, user, EntityKind::Track, &tracks[..1]).await;
    assert_eq!(untouched.len(), 1);
}

/// An album's plays are the sum over its tracks, and an artist's are zero.
#[tokio::test]
async fn an_album_sums_its_tracks_and_an_artist_has_no_plays() {
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
    activity
        .set_rating(user, EntityKind::Album, album, 5)
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            tracks[0],
            Some("dsub"),
            1_700_000_000_000,
            180_000,
            true,
        )
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            user,
            tracks[1],
            Some("dsub"),
            1_700_000_100_000,
            180_000,
            true,
        )
        .await
        .unwrap();

    let loaded = load(&database, user, EntityKind::Album, &[album]).await;
    let listed = loaded.get(&album).copied().unwrap_or_default();
    assert!(listed.starred_at.is_some());
    assert_eq!(listed.rating, Some(5));
    assert_eq!(listed.play_count, 2, "both tracks were played");
    assert_eq!(listed.last_played, Some(1_700_000_100_000));

    let artist = library.list_artists().await.unwrap()[0].id.uuid();
    let loaded = load(&database, user, EntityKind::Artist, &[artist]).await;
    let listed = loaded.get(&artist).copied().unwrap_or_default();
    assert_eq!(listed.play_count, 0);
    assert_eq!(listed.last_played, None);
}

/// Another user's activity is not this user's.
#[tokio::test]
async fn annotations_are_per_user() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;

    activity
        .set_rating(first, EntityKind::Track, tracks[0], 3)
        .await
        .unwrap();
    activity
        .plays()
        .record_play(
            second,
            tracks[0],
            Some("dsub"),
            1_700_000_000_000,
            180_000,
            true,
        )
        .await
        .unwrap();

    let loaded = load(
        &database,
        first,
        EntityKind::Track,
        &[track_id(tracks[0]).uuid()],
    )
    .await;
    let listed = loaded.get(&tracks[0]).copied().unwrap_or_default();
    assert_eq!(listed.rating, Some(3));
    assert_eq!(
        listed.play_count, 0,
        "the other user's play is not this user's"
    );
}

/// A page longer than one statement's id list is read in chunks.
#[tokio::test]
async fn a_long_page_is_read_in_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;

    // Two past the chunk size, so the last chunk is partial.
    let count = hearthsound_activity::MAX_IDS_PER_QUERY + 2;
    let mut ids = vec![album];
    for index in 0..count {
        ids.push(
            crate::common::insert_track(
                &library,
                album,
                &format!("Filler {index}"),
                index as i64 + 3,
            )
            .await,
        );
    }
    let Some(last) = ids.last().copied() else {
        panic!("the page holds ids");
    };
    activity
        .set_rating(user, EntityKind::Track, last, 2)
        .await
        .unwrap();

    let loaded = load(&database, user, EntityKind::Track, &ids[1..]).await;
    assert_eq!(
        loaded.get(&last).copied().unwrap_or_default().rating,
        Some(2)
    );
    assert_eq!(
        loaded.len(),
        1,
        "only the rated track has something to report"
    );
    assert_eq!(tracks.len(), 2);
}
