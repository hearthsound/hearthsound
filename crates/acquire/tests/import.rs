//! The import stage: where an acquired file lands, what it carries, what the
//! rows and events say, and what happens to the file's import root.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use common::{Env, InstanceLimits, config, has_availability_event, one_candidate};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle};
use hearthsound_core::events::{LibraryChange, ServerEvent};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, PinReason, Priority, StorageClass,
};
use hearthsound_media::{Container, probe};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};

/// Admits one acquisition and returns its handle.
async fn request(env: &Env, track: EntityId, priority: Priority) -> AcquisitionHandle {
    env.orchestrator
        .request(AcquireRequest::new(track, priority, "test request"))
        .await
        .unwrap()
        .handle()
        .expect("the request must be admitted")
        .clone()
}

/// Writes a generated FLAC into the instance's import root.
async fn place(env: &Env, name: &str) -> PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.2)
        .file_name(name.to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap()
}

/// The relative path the harness's track renders to.
const RELATIVE: &str = "Hello Artist/Hello Song album (1993)/01 - Hello Song.flac";

#[tokio::test]
async fn an_unpinned_track_lands_in_the_cache_with_its_tags() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let mut events = env.events.subscribe();
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = env.cache_root.join(RELATIVE);
    assert!(destination.is_file(), "the file landed in the cache root");
    assert!(!env.library_root.join(RELATIVE).exists());
    assert!(!source.exists(), "a move import root drops the source file");
    assert!(
        !env.staging.join(id.to_string()).exists(),
        "the staging directory is removed"
    );

    let probed = probe(&destination).await.unwrap();
    assert_eq!(probed.container, Container::Flac);
    assert!(probed.lossless);
    assert_eq!(probed.tags.title.as_deref(), Some("Hello Song"));
    assert_eq!(probed.tags.album.as_deref(), Some("Hello Song album"));
    assert_eq!(probed.tags.album_artist.as_deref(), Some("Hello Artist"));
    assert_eq!(probed.tags.artists, ["Hello Artist"]);
    assert_eq!(probed.tags.track_number, Some(1));
    assert_eq!(probed.tags.isrc.as_deref(), Some("USHW12600001"));
    assert_eq!(probed.tags.date.as_deref(), Some("1993"));

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert!(row.media_file_id.is_some());
    assert_eq!(
        env.track(track).await.availability,
        Availability::Cached,
        "an unpinned track is cached"
    );

    let stored = sqlx::query!(
        "SELECT storage, rel_path, quality_tier, acquisition_id FROM media_files WHERE track_id = ?",
        track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(stored.storage, "cache");
    assert_eq!(stored.rel_path, RELATIVE);
    assert_eq!(stored.quality_tier.as_deref(), Some("lossless"));
    assert_eq!(
        stored.acquisition_id,
        Some(id.as_bytes().to_vec()),
        "the media file names the acquisition that produced it"
    );
    let preferred = env
        .library
        .preferred_media_file(track)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preferred.storage, StorageClass::Cache);
    assert_eq!(preferred.rel_path, RELATIVE);

    let published = common::drain(&mut events);
    assert!(
        has_availability_event(&published, track, Availability::Cached),
        "the availability change is published"
    );
    assert!(
        published.iter().any(|event| matches!(
            event,
            ServerEvent::LibraryChanged(change)
                if change.entity_id == track.to_string() && change.change == LibraryChange::Added
        )),
        "the library change is published"
    );
}

#[tokio::test]
async fn a_pinned_track_lands_in_the_library() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.library
        .add_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert!(env.library_root.join(RELATIVE).is_file());
    assert!(!env.cache_root.join(RELATIVE).exists());
    assert_eq!(env.track(track).await.availability, Availability::Local);
    let stored = sqlx::query_scalar!(
        "SELECT storage FROM media_files WHERE track_id = ?",
        track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(stored, "library");
}

#[tokio::test]
async fn a_copy_import_root_keeps_the_source() {
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "copy",
        false,
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert!(source.is_file(), "a copy import root leaves the source");
    assert!(env.cache_root.join(RELATIVE).is_file());
}

#[tokio::test]
async fn a_link_import_root_keeps_the_source() {
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "link",
        false,
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let original = std::fs::read(&source).unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert!(source.is_file(), "a link import root leaves the original");
    assert_eq!(
        std::fs::read(&source).unwrap(),
        original,
        "the original is never written to"
    );
    assert!(env.cache_root.join(RELATIVE).is_file());
}

#[tokio::test]
async fn a_read_only_import_root_is_never_cleaned_up() {
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "move",
        true,
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert!(
        source.is_file(),
        "a read-only import root is never written or cleaned up"
    );
    assert!(env.cache_root.join(RELATIVE).is_file());
}

#[tokio::test]
async fn a_name_collision_does_not_overwrite_another_file() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    let occupant = env.cache_root.join(RELATIVE);
    std::fs::create_dir_all(occupant.parent().unwrap()).unwrap();
    std::fs::write(&occupant, b"another track's file").unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert_eq!(
        std::fs::read(&occupant).unwrap(),
        b"another track's file",
        "the file that already held the name is untouched"
    );
    let second = env
        .cache_root
        .join("Hello Artist/Hello Song album (1993)/01 - Hello Song (2).flac");
    assert!(second.is_file(), "the import took the next free name");
    let stored = sqlx::query_scalar!(
        "SELECT rel_path FROM media_files WHERE track_id = ?",
        track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(
        stored,
        "Hello Artist/Hello Song album (1993)/01 - Hello Song (2).flac"
    );
}
