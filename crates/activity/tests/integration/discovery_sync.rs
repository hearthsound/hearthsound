//! Tests for writing a generated playlist into its slot.
//!
//! The discovery sync writes the playlists its services generated through
//! [`Playlists::upsert_generated`]. These tests cover the storage rules the
//! sync relies on: the slot is identified by owner, instance and reference, a
//! second write replaces the tracks in place, and a generated playlist pins
//! nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{Playlists, SyncedPlaylist};
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::common::{insert_album, insert_track, insert_user, is_pinned, open_database, track_id};

/// Inserts a plugin and one enabled instance of it, returning the instance id.
///
/// A generated playlist references the discovery instance that produced it, so
/// the tests need a real `provider_instances` row for the foreign key.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `display_name`: the name the instance reports.
async fn insert_instance(database: &Database, display_name: &str) -> Uuid {
    let plugin_id = "com.example.discovery-test".to_owned();
    sqlx::query(
        "INSERT INTO plugins (id, version, api, manifest, sha256, source, enabled, installed_at) \
         VALUES (?, '0.1.0', 'hearthsound:plugin@0.3.0', '{}', x'00', 'sideload', 1, 0)",
    )
    .bind(&plugin_id)
    .execute(database.writer())
    .await
    .unwrap();
    let id = Uuid::now_v7();
    let slug = format!("discovery{}", id.simple());
    sqlx::query(
        "INSERT INTO provider_instances \
             (id, plugin_id, slug, display_name, enabled, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 1, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(&plugin_id)
    .bind(&slug)
    .bind(display_name)
    .execute(database.writer())
    .await
    .unwrap();
    id
}

/// A generated playlist is stored as the listener's temporary playlist, with
/// the instance and reference a refresh matches.
#[tokio::test]
async fn a_generated_playlist_is_written_into_its_slot() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let user = insert_user(&database).await;
    let instance = insert_instance(&database, "Weekly Jams Service").await;
    let (album, tracks) = insert_album(&library).await;
    let ids = vec![track_id(tracks[0]), track_id(tracks[1])];
    let write = SyncedPlaylist {
        owner_user_id: user,
        source_instance_id: instance,
        source_ref: "weekly-jams",
        name: "Weekly Jams",
        comment: Some("A week of listening"),
        expires_at: 4_000,
        tracks: &ids,
    };

    let written = Playlists::new(&database)
        .upsert_generated(&write, 1_000)
        .await
        .unwrap();

    assert_eq!(written.id.kind(), hearthsound_core::IdPrefix::Playlist);
    assert_eq!(written.owner_user_id, user);
    assert!(written.is_temporary);
    assert!(!written.is_public);
    assert_eq!(written.expires_at, Some(4_000));
    assert_eq!(written.source_instance_id, Some(instance));
    assert_eq!(written.source_ref.as_deref(), Some("weekly-jams"));
    assert_eq!(written.created_at, 1_000);
    assert_eq!(written.updated_at, 1_000);
    assert_eq!(
        written
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        ids
    );

    // The slot and the temporary flag are the stored row's, not the value the
    // write happened to return.
    let stored = Playlists::new(&database)
        .get(written.id, user)
        .await
        .unwrap()
        .expect("the playlist was stored");
    assert_eq!(stored.source_instance_id, Some(instance));
    assert_eq!(stored.source_ref.as_deref(), Some("weekly-jams"));
    assert_eq!(
        stored.source_instance_name.as_deref(),
        Some("Weekly Jams Service")
    );
    assert!(stored.is_temporary);

    // A generated playlist pins nothing: its tracks stay evictable.
    for track in &tracks {
        assert!(!is_pinned(&library, *track).await);
    }
    let _ = album;
}

/// A second sync of the same slot keeps the playlist id and replaces its
/// tracks.
#[tokio::test]
async fn a_second_sync_replaces_the_tracks_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlists = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let third = insert_track(&library, album, "Third", 3).await;
    let instance = insert_instance(&database, "Weekly Jams Service").await;

    let first = vec![track_id(tracks[0]), track_id(tracks[1])];
    let created = playlists
        .upsert_generated(
            &SyncedPlaylist {
                owner_user_id: user,
                source_instance_id: instance,
                source_ref: "weekly-jams",
                name: "Weekly Jams",
                comment: None,
                expires_at: 4_000,
                tracks: &first,
            },
            1_000,
        )
        .await
        .unwrap();

    let second = vec![track_id(tracks[1]), track_id(third)];
    let refreshed = playlists
        .upsert_generated(
            &SyncedPlaylist {
                owner_user_id: user,
                source_instance_id: instance,
                source_ref: "weekly-jams",
                name: "Weekly Jams 2",
                comment: Some("Refreshed"),
                expires_at: 8_000,
                tracks: &second,
            },
            2_000,
        )
        .await
        .unwrap();

    assert_eq!(refreshed.id, created.id, "the slot keeps its playlist");
    assert_eq!(refreshed.name, "Weekly Jams 2");
    assert_eq!(refreshed.comment.as_deref(), Some("Refreshed"));
    assert_eq!(refreshed.expires_at, Some(8_000));
    assert_eq!(refreshed.created_at, 1_000);
    assert_eq!(refreshed.updated_at, 2_000);
    assert_eq!(
        refreshed
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        second
    );

    // The replacement is what a reader sees, too.
    let stored = playlists
        .get(created.id, user)
        .await
        .unwrap()
        .expect("the playlist is still stored");
    assert_eq!(
        stored
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>(),
        second
    );
}

/// A different reference is a different playlist.
#[tokio::test]
async fn a_new_reference_is_a_new_playlist() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let playlists = Playlists::new(&database);
    let user = insert_user(&database).await;
    let (_album, tracks) = insert_album(&library).await;
    let ids = vec![track_id(tracks[0])];
    let instance = insert_instance(&database, "Weekly Jams Service").await;

    let first = playlists
        .upsert_generated(
            &SyncedPlaylist {
                owner_user_id: user,
                source_instance_id: instance,
                source_ref: "weekly-jams",
                name: "Weekly Jams",
                comment: None,
                expires_at: 4_000,
                tracks: &ids,
            },
            1_000,
        )
        .await
        .unwrap();
    let second = playlists
        .upsert_generated(
            &SyncedPlaylist {
                owner_user_id: user,
                source_instance_id: instance,
                source_ref: "daily-mix",
                name: "Daily Mix",
                comment: None,
                expires_at: 4_000,
                tracks: &ids,
            },
            1_000,
        )
        .await
        .unwrap();

    assert_ne!(first.id, second.id);
    assert_eq!(playlists.list(user, Some(user)).await.unwrap().len(), 2);
}
