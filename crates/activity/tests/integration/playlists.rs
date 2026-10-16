//! Tests for creating, editing, and reading playlists.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{ActivityError, PlaylistUpdate, Playlists};
use hearthsound_core::EntityId;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::common::{insert_album, insert_track, insert_user, is_pinned, open_database, track_id};

/// Returns the client ids of tracks a test created.
fn entities(tracks: &[Uuid]) -> Vec<EntityId> {
    tracks.iter().copied().map(track_id).collect()
}

/// Returns which of `tracks` a playlist holds, in playing order.
fn order(playlist: &hearthsound_activity::Playlist, tracks: &[EntityId]) -> Vec<usize> {
    playlist
        .entries
        .iter()
        .map(|entry| {
            tracks
                .iter()
                .position(|id| *id == entry.track_id)
                .expect("the entry is one of the tracks the test created")
        })
        .collect()
}

/// A playlist keeps its entries in order and renumbers them from zero.
#[tokio::test]
async fn a_playlist_keeps_its_entries_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let third = insert_track(&library, album, "Third", 3).await;
    let ids = entities(&[tracks[0], tracks[1], third]);

    let created = playlist
        .create(user, "Evening", Some("Slow"), false, &ids)
        .await
        .unwrap();
    assert_eq!(order(&created, &ids), [0, 1, 2]);
    assert_eq!(
        created
            .entries
            .iter()
            .map(|entry| entry.position)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(created.name, "Evening");
    assert_eq!(created.comment.as_deref(), Some("Slow"));

    // Every entry is pinned while the playlist holds it.
    for track in &ids {
        assert!(is_pinned(&library, track.uuid()).await);
    }
}

/// A removal is read against the order the client saw, and the rest is
/// renumbered.
#[tokio::test]
async fn removing_an_entry_renumbers_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let third = insert_track(&library, album, "Third", 3).await;
    let ids = entities(&[tracks[0], tracks[1], third]);
    let created = playlist
        .create(user, "Evening", None, false, &ids)
        .await
        .unwrap();

    let edited = playlist
        .update(
            created.id,
            user,
            &PlaylistUpdate {
                name: Some("Night".to_owned()),
                remove_indexes: vec![0, 2],
                ..PlaylistUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.name, "Night");
    assert_eq!(order(&edited, &ids), [1]);
    assert_eq!(edited.entries[0].position, 0);
    assert!(
        !is_pinned(&library, ids[0].uuid()).await,
        "the first was removed"
    );
    assert!(is_pinned(&library, ids[1].uuid()).await);
    assert!(
        !is_pinned(&library, ids[2].uuid()).await,
        "the last was removed"
    );
}

/// An addition appends, and a replacement writes the whole list.
#[tokio::test]
async fn adding_appends_and_replacing_writes_the_whole_list() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let third = insert_track(&library, album, "Third", 3).await;
    let ids = entities(&[tracks[0], tracks[1], third]);

    let created = playlist
        .create(user, "Evening", None, false, &ids[..2])
        .await
        .unwrap();
    let appended = playlist
        .update(
            created.id,
            user,
            &PlaylistUpdate {
                add: vec![ids[2]],
                ..PlaylistUpdate::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(order(&appended, &ids), [0, 1, 2]);

    let replaced = playlist
        .replace_tracks(created.id, user, &[ids[2], ids[0]])
        .await
        .unwrap();
    assert_eq!(order(&replaced, &ids), [2, 0]);
    assert_eq!(replaced.entries[0].position, 0);
    assert!(
        !is_pinned(&library, ids[1].uuid()).await,
        "the replaced-away track lost its pin"
    );
    assert!(is_pinned(&library, ids[2].uuid()).await);
}

/// Deleting a playlist releases the pins it held.
#[tokio::test]
async fn deleting_a_playlist_releases_its_pins() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let ids = entities(&tracks);

    let created = playlist
        .create(user, "Evening", None, false, &ids)
        .await
        .unwrap();
    assert!(playlist.delete(created.id, user).await.unwrap());
    for track in &ids {
        assert!(!is_pinned(&library, track.uuid()).await);
    }
    assert!(playlist.get(created.id, user).await.unwrap().is_none());
    assert!(matches!(
        playlist.delete(created.id, user).await.unwrap_err(),
        ActivityError::UnknownPlaylist { .. }
    ));
}

/// Only the owner may edit or delete, and only the owner or a public playlist
/// may be read.
#[tokio::test]
async fn only_the_owner_may_edit_and_others_see_public_playlists() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let (owner, other) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;
    let ids = entities(&tracks);

    let private = playlist
        .create(owner, "Private", None, false, &ids)
        .await
        .unwrap();
    let public = playlist
        .create(owner, "Public", None, true, &ids)
        .await
        .unwrap();

    let refused = playlist
        .update(
            private.id,
            other,
            &PlaylistUpdate {
                name: Some("Stolen".to_owned()),
                ..PlaylistUpdate::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ActivityError::NotOwner { .. }),
        "{refused:?}"
    );
    assert!(matches!(
        playlist.delete(private.id, other).await.unwrap_err(),
        ActivityError::NotOwner { .. }
    ));

    assert!(playlist.get(private.id, other).await.unwrap().is_none());
    assert!(playlist.get(public.id, other).await.unwrap().is_some());

    let seen = playlist.list(other, None).await.unwrap();
    assert_eq!(
        seen.iter()
            .map(|playlist| playlist.name.as_str())
            .collect::<Vec<_>>(),
        ["Public"]
    );
    let owner_only = playlist.list(other, Some(owner)).await.unwrap();
    assert_eq!(
        owner_only
            .iter()
            .map(|playlist| playlist.name.as_str())
            .collect::<Vec<_>>(),
        ["Public"],
        "the filter narrows the result instead of widening it"
    );
    assert_eq!(playlist.list(owner, Some(owner)).await.unwrap().len(), 2);
}

/// An empty name is refused.
#[tokio::test]
async fn an_empty_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;

    let refused = playlist
        .create(user, "   ", None, false, &[])
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ActivityError::InvalidName { .. }),
        "{refused:?}"
    );
}
