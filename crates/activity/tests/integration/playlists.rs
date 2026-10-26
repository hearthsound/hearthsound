//! Tests for creating, editing, and reading playlists.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{
    ActivityError, EXPIRE_JOB_KIND, ExpireJobHandler, PlaylistUpdate, Playlists,
};
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

/// A temporary playlist is listed like any other and pins none of its tracks;
/// making it permanent pins them again.
#[tokio::test]
async fn a_temporary_playlist_pins_nothing_until_it_is_permanent() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let ids = entities(&tracks);
    let temporary = playlist
        .create(user, "Synced", None, false, &ids)
        .await
        .unwrap();
    assert!(!temporary.is_temporary);
    assert_eq!(temporary.expires_at, None);

    let temporary = playlist
        .set_lifetime(temporary.id, user, true, Some(4_000))
        .await
        .unwrap();
    assert!(temporary.is_temporary);
    assert_eq!(temporary.expires_at, Some(4_000));
    assert_eq!(order(&temporary, &ids), [0, 1]);
    for track in &ids {
        assert!(
            !is_pinned(&library, track.uuid()).await,
            "a temporary playlist pins nothing"
        );
    }
    let listed = playlist.list(user, None).await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|playlist| (playlist.name.as_str(), playlist.is_temporary))
            .collect::<Vec<_>>(),
        [("Synced", true)],
        "a temporary playlist is listed like any other"
    );

    let permanent = playlist
        .set_lifetime(temporary.id, user, false, Some(4_000))
        .await
        .unwrap();
    assert!(!permanent.is_temporary);
    assert_eq!(
        permanent.expires_at, None,
        "a permanent playlist never expires"
    );
    for track in &ids {
        assert!(is_pinned(&library, track.uuid()).await);
    }
}

/// Converting a playlist is the owner's decision only.
#[tokio::test]
async fn only_the_owner_may_convert_a_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let (owner, other) = (insert_user(&database).await, insert_user(&database).await);
    let (_, tracks) = insert_album(&library).await;
    let created = playlist
        .create(owner, "Synced", None, false, &entities(&tracks))
        .await
        .unwrap();

    let refused = playlist
        .set_lifetime(created.id, other, true, None)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, ActivityError::NotOwner { .. }),
        "{refused:?}"
    );
    assert!(
        is_pinned(&library, tracks[0]).await,
        "the refused conversion pinned nothing"
    );

    let unknown = playlist
        .set_lifetime(
            EntityId::new(hearthsound_core::IdPrefix::Playlist),
            owner,
            true,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(unknown, ActivityError::UnknownPlaylist { .. }),
        "{unknown:?}"
    );
}

/// The sweeper deletes a temporary playlist whose expiry has passed, and leaves
/// a permanent one and a temporary one without an expiry alone.
#[tokio::test]
async fn the_sweeper_deletes_only_expired_temporary_playlists() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let ids = entities(&tracks);

    let expired = playlist
        .create(user, "Expired", None, false, &ids)
        .await
        .unwrap();
    playlist
        .set_lifetime(expired.id, user, true, Some(1_000))
        .await
        .unwrap();
    let later = playlist
        .create(user, "Later", None, false, &ids)
        .await
        .unwrap();
    playlist
        .set_lifetime(later.id, user, true, Some(9_000))
        .await
        .unwrap();
    let no_expiry = playlist
        .create(user, "Never", None, false, &ids)
        .await
        .unwrap();
    playlist
        .set_lifetime(no_expiry.id, user, true, None)
        .await
        .unwrap();
    let permanent = playlist
        .create(user, "Permanent", None, false, &ids)
        .await
        .unwrap();

    assert_eq!(playlist.delete_expired(5_000).await.unwrap(), 1);
    assert!(playlist.get(expired.id, user).await.unwrap().is_none());
    for kept in [later.id, no_expiry.id, permanent.id] {
        assert!(playlist.get(kept, user).await.unwrap().is_some());
    }
    assert_eq!(
        playlist.delete_expired(5_000).await.unwrap(),
        0,
        "a second sweep finds nothing"
    );
    // Deleting a temporary playlist releases nothing by itself: the tracks it
    // held are still pinned by the permanent playlist that also holds them.
    for track in &ids {
        assert!(is_pinned(&library, track.uuid()).await);
    }
}

/// The expiry job runs the same sweep the repository offers.
#[tokio::test]
async fn the_expiry_job_deletes_what_has_passed() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let created = playlist
        .create(user, "Synced", None, false, &entities(&tracks))
        .await
        .unwrap();
    playlist
        .set_lifetime(created.id, user, true, Some(0))
        .await
        .unwrap();

    let handler = ExpireJobHandler::new(playlist.clone());
    let job = hearthsound_jobs::Job {
        id: Uuid::now_v7(),
        kind: EXPIRE_JOB_KIND.into(),
        payload: None,
        state: hearthsound_jobs::JobState::Running,
        priority: hearthsound_core::Priority::Background,
        attempts: 1,
        run_at: 0,
        locked_until: None,
        last_error: None,
        created_at: 0,
        updated_at: 0,
    };
    hearthsound_jobs::JobHandler::handle(&handler, job)
        .await
        .unwrap();
    assert!(playlist.get(created.id, user).await.unwrap().is_none());
}

/// A reorder writes every position in the requested order, and a list that is
/// not a permutation of the playlist's positions is refused without writing.
#[tokio::test]
async fn reordering_rewrites_the_positions_and_refuses_a_bad_order() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let (owner, other) = (insert_user(&database).await, insert_user(&database).await);
    let (album, tracks) = insert_album(&library).await;
    let third = insert_track(&library, album, "Third", 3).await;
    let ids = entities(&[tracks[0], tracks[1], third]);
    let created = playlist
        .create(owner, "Evening", None, false, &ids)
        .await
        .unwrap();
    let added_at: Vec<i64> = created.entries.iter().map(|entry| entry.added_at).collect();

    let reordered = playlist
        .reorder(created.id, owner, &[2, 0, 1])
        .await
        .unwrap();
    assert_eq!(order(&reordered, &ids), [2, 0, 1]);
    assert_eq!(
        reordered
            .entries
            .iter()
            .map(|entry| entry.position)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        reordered
            .entries
            .iter()
            .map(|entry| entry.added_at)
            .collect::<Vec<_>>(),
        [added_at[2], added_at[0], added_at[1]],
        "a reorder keeps the instant each entry was added"
    );

    let stored = playlist.get(created.id, owner).await.unwrap().unwrap();
    assert_eq!(order(&stored, &ids), [2, 0, 1], "the rows were rewritten");

    // Every entry stays pinned; a reorder changes no membership.
    for track in &ids {
        assert!(is_pinned(&library, track.uuid()).await);
    }

    for bad in [vec![0, 0, 1], vec![0, 1], vec![0, 1, 3]] {
        assert!(matches!(
            playlist.reorder(created.id, owner, &bad).await.unwrap_err(),
            ActivityError::InvalidOrder { len: 3 }
        ));
    }
    assert!(matches!(
        playlist
            .reorder(created.id, other, &[0, 1, 2])
            .await
            .unwrap_err(),
        ActivityError::NotOwner { .. }
    ));
    assert_eq!(
        order(
            &playlist.get(created.id, owner).await.unwrap().unwrap(),
            &ids
        ),
        [2, 0, 1],
        "a refused reorder leaves the playlist as it was"
    );
}

/// Expiring a playlist releases the playlist pin of its tracks, and a track
/// another permanent playlist still holds keeps it.
#[tokio::test]
async fn expiring_a_playlist_releases_its_playlist_pins() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlist = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let (reverted, kept) = (track_id(tracks[0]), track_id(tracks[1]));

    // One playlist holds both tracks and is forced back to temporary with an
    // expiry that has passed, which is the row the old sync left behind: its
    // pins are stale, because a temporary playlist pins nothing.
    let expired = playlist
        .create(user, "Reverted", None, false, &[reverted, kept])
        .await
        .unwrap();
    sqlx::query("UPDATE playlists SET is_temporary = 1, expires_at = 1 WHERE id = ?")
        .bind(expired.id.uuid().as_bytes().as_slice())
        .execute(database.writer())
        .await
        .unwrap();
    // A second, permanent playlist still holds one of them.
    playlist
        .create(user, "Kept", None, false, &[kept])
        .await
        .unwrap();

    assert_eq!(playlist.delete_expired(5_000).await.unwrap(), 1);

    let mut conn = database.readers().acquire().await.unwrap();
    let released = hearthsound_library::retention::pin_reasons(&mut conn, reverted)
        .await
        .unwrap();
    assert!(
        !released.contains(&hearthsound_core::PinReason::Playlist),
        "the deleted playlist still pins its track: {released:?}"
    );
    let retained = hearthsound_library::retention::pin_reasons(&mut conn, kept)
        .await
        .unwrap();
    assert!(
        retained.contains(&hearthsound_core::PinReason::Playlist),
        "a track another permanent playlist holds lost its pin: {retained:?}"
    );
}
