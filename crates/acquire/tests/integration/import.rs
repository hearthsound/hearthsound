//! The import stage: where an acquired file lands, what it carries, what the
//! rows and events say, and what happens to the file's import root.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::common::{Env, InstanceLimits, config, has_availability_event, one_candidate};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle};
use hearthsound_core::events::{LibraryChange, ServerEvent};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, FailureReason, PinReason, Priority, StorageClass,
};
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_media::{Container, probe};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use uuid::Uuid;

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
    // The source file and the staging directory are cleaned up after the
    // completed row is written, so they are waited for rather than asserted on
    // sight.
    crate::common::wait_for_removal(&source, "a move import root drops the source file").await;
    crate::common::wait_for_removal(
        &env.staging.join(id.to_string()),
        "the staging directory is removed",
    )
    .await;

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

    let published = crate::common::wait_for_events(
        &mut events,
        "the availability and the library change are published",
        |published| {
            has_availability_event(published, track, Availability::Cached)
                && published.iter().any(|event| matches!(
                    event,
                    ServerEvent::LibraryChanged(change)
                        if change.entity_id == track.to_string() && change.change == LibraryChange::Added
                ))
        },
    )
    .await;
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
    let source_meta = std::fs::metadata(&source).unwrap();
    let copied_meta = std::fs::metadata(env.cache_root.join(RELATIVE)).unwrap();
    assert_ne!(
        (copied_meta.ino(), copied_meta.dev()),
        (source_meta.ino(), source_meta.dev()),
        "a copy import root copies rather than links"
    );
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
    let before = std::fs::metadata(&source).unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = env.cache_root.join(RELATIVE);
    assert!(destination.is_file());
    assert!(source.is_file(), "a link import root leaves the original");
    let after = std::fs::metadata(&source).unwrap();
    assert_eq!(
        std::fs::read(&source).unwrap(),
        original,
        "the original is never written to"
    );
    assert_eq!(
        (before.ino(), before.dev()),
        (after.ino(), after.dev()),
        "the source is the file that was linked"
    );
    assert_eq!(
        before.modified().unwrap(),
        after.modified().unwrap(),
        "the source keeps its modification time"
    );
    let linked = std::fs::metadata(&destination).unwrap();
    assert_eq!(
        (linked.ino(), linked.dev()),
        (after.ino(), after.dev()),
        "the managed file is the source inode"
    );
    let probed = probe(&destination).await.unwrap();
    assert_eq!(
        probed.tags.title, None,
        "a linked file is byte-identical and therefore untagged"
    );
    assert!(probed.tags.artists.is_empty());
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        original,
        "the managed file is byte-identical to the source"
    );
}

#[tokio::test]
async fn a_failed_hardlink_falls_back_to_a_tagged_copy() {
    let env = Env::with_hardlink(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "link",
        std::sync::Arc::new(|_: &std::path::Path, _: &std::path::Path| {
            Err(std::io::Error::other("injected hardlink failure"))
        }),
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let original = std::fs::read(&source).unwrap();
    let before = std::fs::metadata(&source).unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = env.cache_root.join(RELATIVE);
    assert!(destination.is_file(), "the copy fallback lands the file");
    assert!(
        source.is_file(),
        "the source of a link root is never removed"
    );
    let after = std::fs::metadata(&source).unwrap();
    assert_eq!(
        std::fs::read(&source).unwrap(),
        original,
        "the source is never written to"
    );
    assert_eq!(
        before.modified().unwrap(),
        after.modified().unwrap(),
        "the fallback does not touch the source"
    );
    let copied = std::fs::metadata(&destination).unwrap();
    assert_ne!(
        (copied.ino(), copied.dev()),
        (after.ino(), after.dev()),
        "the fallback is a copy, not the source inode"
    );
    let probed = probe(&destination).await.unwrap();
    assert_eq!(
        probed.tags.title.as_deref(),
        Some("Hello Song"),
        "the fallback writes the tags the linked file cannot carry"
    );
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

/// The number the catalog fixture's album holds.
const CATALOG_ALBUM_TRACKS: u8 = 5;

/// The candidate the acquisition fixture serves for the fixture's third track.
fn catalog_candidate() -> serde_json::Value {
    serde_json::json!([{
        "token": "cand-1",
        "title": "Fixture Song 3",
        "path": "Fixture Artist/Fixture Album/03 - Fixture Song 3.flac",
        "files": [{
            "name": "03 - Fixture Song 3.flac",
            "size": 4000,
            "format": "flac",
            "bitrateKbps": 900,
            "sampleRate": 44100,
            "bitDepth": 16,
            "durationMs": 180_000,
            "trackIndex": 0,
        }],
        "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
    }])
}

/// A lone catalog track is tagged with its album's real totals.
///
/// The third track of the fixture's five-track album is stored on its own,
/// so the album row holds one track of five; the import has to read the album's
/// tracklist from the instance that issued it rather than count the stored rows,
/// or the file would claim to be the only track of its album.
#[tokio::test]
async fn a_lone_catalog_track_is_tagged_with_its_album_total() {
    let env = Env::new(
        config(catalog_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    env.add_catalog_instance().await;
    AudioFixture::new(AudioFormat::Flac)
        .seconds(180.0)
        .file_name("03 - Fixture Song 3.flac".to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap();
    let track = env.resolved_catalog_track(3).await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let stored: String = sqlx::query_scalar!(
        "SELECT rel_path FROM media_files WHERE track_id = ?",
        track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(
        stored, "Fixture Artist/Fixture Album (2026)/03 - Fixture Song 3.flac",
        "a single-disc album renders no disc prefix"
    );
    let probed = probe(&env.cache_root.join(&stored)).await.unwrap();
    assert_eq!(probed.tags.track_number, Some(3));
    assert_eq!(
        probed.tags.track_total,
        Some(u32::from(CATALOG_ALBUM_TRACKS)),
        "the file carries the album's own track count, not the stored rows'"
    );
    assert_eq!(probed.tags.disc_number, Some(1));
    assert_eq!(probed.tags.disc_total, None, "a single-disc album has none");
    assert_eq!(probed.tags.album_artist.as_deref(), Some("Fixture Artist"));
    assert_eq!(probed.tags.album.as_deref(), Some("Fixture Album"));
}

/// An album nobody credited is named and tagged with the track's own artist.
///
/// This is the live case: a track stored from an album reference that
/// carries no artist leaves an album with no album artist, and the imported file
/// still has to land under an artist's directory and carry an `albumartist` tag.
#[tokio::test]
async fn an_album_without_an_artist_takes_the_track_artist() {
    let env = Env::new(
        config(
            serde_json::json!([{
                "token": "cand-1",
                "title": "Solo Song",
                "path": "Solo Artist/Solo Song album/01 - Solo Song.flac",
                "files": [{
                    "name": "01 - Solo Song.flac",
                    "size": 4000,
                    "format": "flac",
                    "bitrateKbps": 900,
                    "sampleRate": 44100,
                    "bitDepth": 16,
                    "durationMs": 200,
                    "trackIndex": 0,
                }],
                "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
            }]),
            "normal",
            0,
            "downloads",
            ".flac",
        ),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Solo Song.flac").await;
    let track = env
        .add_track_without_album_artist("Solo Song", "Solo Artist")
        .await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let stored: String = sqlx::query_scalar!(
        "SELECT rel_path FROM media_files WHERE track_id = ?",
        track.uuid().as_bytes().to_vec(),
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap();
    assert_eq!(
        stored, "Solo Artist/Solo Song album/01 - Solo Song.flac",
        "the album artist directory comes from the track's artist"
    );
    let probed = probe(&env.cache_root.join(&stored)).await.unwrap();
    assert_eq!(probed.tags.album_artist.as_deref(), Some("Solo Artist"));
    assert_eq!(probed.tags.artists, ["Solo Artist"]);
}

/// How many imports the concurrency test runs at once.
const CONCURRENT_IMPORTS: usize = 8;

/// Concurrent imports that render one name all land on their own file.
///
/// The naming template renders the same relative path for every track here, and
/// the whole batch starts together, so a filesystem check alone hands several
/// imports the same name: all of them see it free before any of them has
/// written a file. The import that loses the race then hits the unique index on
/// `media_files (storage, rel_path)` and, before this was fixed, its acquisition
/// was left in `importing` with nothing left to move it. Each import reserves
/// its destination, so the batch ends with one file and one row per track.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_imports_of_one_name_all_land() {
    // A `copy` import root, because the fixture serves one file in the root to
    // every download: a `move` root would have the first import delete it under
    // the others' copies, which is a different failure than the one under test.
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits {
            max_concurrent: CONCURRENT_IMPORTS as u32,
            max_concurrent_searches: CONCURRENT_IMPORTS as u32,
            searches_per_hour: 1000,
        },
        &[("HEARTHSOUND__ACQUISITION__MAX_CONCURRENT", "8")],
        "copy",
        false,
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;

    // Every track renders `Hello Artist/Hello Song album (1993)/01 - Hello
    // Song.flac`, and every acquisition is admitted before the first worker
    // starts, so all of them reach the import stage together.
    let mut tracks = Vec::with_capacity(CONCURRENT_IMPORTS);
    let mut ids = Vec::with_capacity(CONCURRENT_IMPORTS);
    for _ in 0..CONCURRENT_IMPORTS {
        let track = env.remote_track("Hello Song", "Hello Artist").await;
        ids.push(request(&env, track, Priority::Play).await.id);
        tracks.push(track);
    }

    env.start();
    for id in &ids {
        let row = env.wait_for_terminal(*id).await;
        assert_eq!(row.state, AcquisitionState::Completed, "acquisition {id}");
        assert!(
            row.media_file_id.is_some(),
            "acquisition {id} imported its file"
        );
    }

    let mut paths = HashSet::new();
    for track in &tracks {
        let stored: String = sqlx::query_scalar!(
            "SELECT rel_path FROM media_files WHERE track_id = ?",
            track.uuid().as_bytes().to_vec(),
        )
        .fetch_one(env.database.readers())
        .await
        .unwrap();
        assert_eq!(
            env.track(*track).await.availability,
            Availability::Cached,
            "the track of `{stored}` is cached"
        );
        assert!(
            env.cache_root.join(&stored).is_file(),
            "`{stored}` holds the file of its track"
        );
        assert!(paths.insert(stored.clone()), "`{stored}` was taken twice");
    }
    assert_eq!(paths.len(), CONCURRENT_IMPORTS, "one file per import");
}

/// An import that cannot write its destination fails the acquisition.
///
/// The import stage returns its error to the pipeline, which has no way to move
/// a row in `importing`: only a restart picks one up again. The import therefore
/// ends the acquisition through the normal failure path, so the row is terminal,
/// the track cools down and the staging directory is gone.
#[tokio::test]
async fn a_failed_import_fails_the_acquisition() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    // A cache root that is a file rather than a directory, so no import can
    // create its destination under it.
    std::fs::remove_dir_all(&env.cache_root).unwrap();
    std::fs::write(&env.cache_root, b"not a directory").unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    let row = env.wait_for_terminal(id).await;

    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::AllCandidatesFailed));
    assert!(
        row.error_detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("the import failed:")),
        "the detail carries the import error: {:?}",
        row.error_detail
    );
    assert_eq!(
        env.track(track).await.availability,
        Availability::Unavailable,
        "a failed import puts the track into cooldown"
    );
    // The staging directory is removed after the failed row is written.
    crate::common::wait_for_removal(
        &env.orchestrator.staging_dir(id),
        "the staging directory is removed",
    )
    .await;
}

#[tokio::test]
async fn a_copied_import_never_replaces_a_file_that_appeared() {
    let env = Env::with_hardlink(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "move",
        std::sync::Arc::new(
            |_source: &std::path::Path, _destination: &std::path::Path| {
                Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices))
            },
        ),
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    // The name the naming template renders is already taken by a file the import
    // did not write.
    let occupied = env.cache_root.join(RELATIVE);
    std::fs::create_dir_all(occupied.parent().unwrap()).unwrap();
    std::fs::write(&occupied, b"a file the import must not replace").unwrap();

    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;
    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert_eq!(
        std::fs::read(&occupied).unwrap(),
        b"a file the import must not replace",
        "the file that held the name is untouched"
    );
    let rel_path: String =
        sqlx::query_scalar("SELECT rel_path FROM media_files WHERE track_id = ?")
            .bind(track.uuid().as_bytes().to_vec())
            .fetch_one(env.database.readers())
            .await
            .unwrap();
    assert_ne!(
        rel_path, RELATIVE,
        "the import takes the next collision suffix"
    );
    assert!(env.cache_root.join(&rel_path).is_file());
    assert_eq!(env.media_files_for(track).await, 1);
}

/// Admits an acquisition a user's save requested, so the save location applies.
///
/// # Parameters
///
/// - `env`: the test environment.
/// - `track`: the track to acquire.
/// - `user`: the account whose save location decides where the file goes.
async fn request_save(env: &Env, track: EntityId, user: Uuid) -> AcquisitionHandle {
    env.orchestrator
        .request(
            AcquireRequest::new(
                track,
                Priority::Play,
                hearthsound_acquire::adopt::SAVE_REASON,
            )
            .with_requested_by(Some(user)),
        )
        .await
        .unwrap()
        .handle()
        .expect("the request must be admitted")
        .clone()
}

/// The media rows a track has.
async fn media_rows(env: &Env, track: EntityId) -> Vec<(String, Option<Vec<u8>>, String)> {
    sqlx::query_as("SELECT storage, folder_id, rel_path FROM media_files WHERE track_id = ?")
        .bind(track.uuid().as_bytes().to_vec())
        .fetch_all(env.database.readers())
        .await
        .unwrap()
}

/// The pin reasons a track carries.
async fn pins(env: &Env, track: EntityId) -> Vec<PinReason> {
    let mut conn = env.database.readers().acquire().await.unwrap();
    hearthsound_library::retention::pin_reasons(&mut conn, track)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_import_for_a_save_is_written_into_the_owners_folder() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    let user = env.add_user().await;
    let folder = env.save_folder(user, "music").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request_save(&env, track, user).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = folder.path.join(RELATIVE);
    assert!(
        destination.is_file(),
        "the file is written into the owner's folder"
    );
    assert!(!env.cache_root.join(RELATIVE).exists());
    assert!(!env.library_root.join(RELATIVE).exists());
    let rows = media_rows(&env, track).await;
    assert_eq!(rows.len(), 1, "one file, and so one row");
    assert_eq!(rows[0].0, "folder");
    assert_eq!(
        rows[0].1.as_deref(),
        Some(folder.id.as_bytes().as_slice()),
        "the row names the folder it is in"
    );
    assert_eq!(rows[0].2, RELATIVE);
    assert_eq!(env.track(track).await.availability, Availability::Local);
    assert!(
        pins(&env, track).await.contains(&PinReason::Folder),
        "a folder file is pinned with the folder reason"
    );
    let probed = probe(&destination).await.unwrap();
    assert_eq!(probed.tags.title.as_deref(), Some("Hello Song"));
    assert_eq!(probed.tags.album.as_deref(), Some("Hello Song album"));
    assert_eq!(probed.tags.album_artist.as_deref(), Some("Hello Artist"));
    assert_eq!(probed.tags.track_number, Some(1));
}

#[tokio::test]
async fn a_link_mode_import_copies_when_the_target_is_a_folder() {
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
        "link",
        false,
    )
    .await;
    let source = place(&env, "01 - Hello Song.flac").await;
    let source_bytes = std::fs::read(&source).unwrap();
    let user = env.add_user().await;
    let folder = env.save_folder(user, "music").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request_save(&env, track, user).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = folder.path.join(RELATIVE);
    assert!(destination.is_file());
    assert_eq!(
        std::fs::read(&source).unwrap(),
        source_bytes,
        "the backend's own file keeps its bytes"
    );
    assert_ne!(
        std::fs::metadata(&source).unwrap().ino(),
        std::fs::metadata(&destination).unwrap().ino(),
        "the folder file must not share the backend's inode"
    );
    assert_eq!(std::fs::metadata(&destination).unwrap().nlink(), 1);
}

#[tokio::test]
async fn a_saved_import_writes_the_musicbrainz_ids_of_its_rows() {
    let env = catalog_env().await;
    place_catalog_download(&env).await;
    let user = env.add_user().await;
    let folder = env.save_folder(user, "music").await;
    let track = env.resolved_catalog_track(3).await;

    let id = request_save(&env, track, user).await.id;
    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(
        row.state,
        AcquisitionState::Completed,
        "log: {:?}",
        env.log(id).await
    );

    let destination = folder.path.join(CATALOG_RELATIVE);
    assert!(
        destination.is_file(),
        "the file is written into the owner's folder"
    );
    let probed = probe(&destination).await.unwrap();
    assert_eq!(
        probed.tags.mb_artist_ids,
        ["fixture-artist-mb"],
        "the artist row's MusicBrainz id is written"
    );
    assert_eq!(
        probed.tags.mb_album_artist_ids,
        ["fixture-artist-mb"],
        "and so is the album artist's"
    );
    assert_eq!(
        probed.tags.mb_release_id.as_deref(),
        Some("fixture-release-1"),
        "the album row's release id is written"
    );
    assert_eq!(
        probed.tags.mb_recording_id.as_deref(),
        Some("fixture-recording-1")
    );
}

#[tokio::test]
async fn a_rescan_of_a_saved_file_keeps_one_artist_and_one_album_row() {
    let env = catalog_env().await;
    place_catalog_download(&env).await;
    let user = env.add_user().await;
    let folder = env.save_folder(user, "music").await;
    let track = env.resolved_catalog_track(3).await;

    let id = request_save(&env, track, user).await.id;
    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let destination = folder.path.join(CATALOG_RELATIVE);
    let credited = credited_artists(&env, track).await;
    assert_eq!(credited.len(), 1);
    let album = env.track(track).await.album_id;

    // A rescan only reads a file whose size or mtime changed, so the saved file
    // is rewritten with its own bytes: the import wrote tags that agree with the
    // rows, and the scan must keep the track on the artist and album it has.
    let bytes = std::fs::read(&destination).unwrap();
    std::fs::write(&destination, bytes).unwrap();

    let scanner = LibraryScanner::new(
        &env.database,
        Arc::clone(&env.events),
        env.settings.clone(),
        Arc::new(FolderLocks::new()),
    );
    scanner.scan_folder(folder.id).await.unwrap();

    assert_eq!(
        naming_rows(&env, "artists", "Fixture Artist").await,
        1,
        "one act has one row after the rescan"
    );
    assert_eq!(
        naming_rows(&env, "albums", "Fixture Album").await,
        1,
        "one album has one row after the rescan"
    );
    assert_eq!(
        naming_rows(&env, "tracks", "Fixture Song 3").await,
        1,
        "the saved file is still one track"
    );
    assert_eq!(
        credited_artists(&env, track).await,
        credited,
        "the track is credited to the artist row it had"
    );
    assert_eq!(
        env.track(track).await.album_id,
        album,
        "the track still belongs to the album row it had"
    );
}

/// The relative path the album fixture's third track renders to.
const CATALOG_RELATIVE: &str = "Fixture Artist/Fixture Album (2026)/03 - Fixture Song 3.flac";

/// The recording the metadata fixture knows for the album fixture's third track.
///
/// It is the fixture's whole server configuration, because the instance service
/// stores a JSON object scoped to the server.
const CATALOG_RECORDING: &str = r#"{
  "recordings": [
    {
      "title": "Fixture Song 3",
      "artists": ["Fixture Artist"],
      "artistMbid": "fixture-artist-mb",
      "mbid": "fixture-recording-1",
      "score": 0.98,
      "releases": [
        {"mbid": "fixture-release-1", "title": "Fixture Album", "date": "2026-02-01"}
      ]
    }
  ]
}"#;

/// Builds an environment whose catalog reports only its own ids, the way a
/// service such as Deezer does, plus the metadata instance that resolves them.
///
/// # Returns
///
/// The environment, with the catalog and metadata layers assigned.
async fn catalog_env() -> Env {
    let env = Env::new(
        config(catalog_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    env.add_catalog_instance_with(serde_json::json!({ "canonicalIds": false }))
        .await;
    env.add_metadata_instance(serde_json::from_str(CATALOG_RECORDING).unwrap())
        .await;
    env
}

/// Writes the file the acquisition fixture serves for the catalog candidate.
///
/// The fixture's tracks are three minutes long and the verification stage
/// compares the file's duration with the track's, so the generated audio is as
/// long as the row says.
async fn place_catalog_download(env: &Env) {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(180.0)
        .file_name("03 - Fixture Song 3.flac".to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap();
}

/// The artist ids a track credits, in credit order.
///
/// # Parameters
///
/// - `env`: the test environment.
/// - `track`: the track to read.
///
/// # Returns
///
/// The credited artists' raw primary keys.
async fn credited_artists(env: &Env, track: EntityId) -> Vec<Vec<u8>> {
    sqlx::query_scalar("SELECT artist_id FROM track_artists WHERE track_id = ? ORDER BY position")
        .bind(track.uuid().as_bytes().to_vec())
        .fetch_all(env.database.readers())
        .await
        .unwrap()
}

/// Counts the rows of one table whose name or title is the given text.
///
/// # Parameters
///
/// - `env`: the test environment.
/// - `table`: `artists`, `albums` or `tracks`; the statement per table is a
///   literal, because a query string sqlx accepts has to be one.
/// - `text`: the name or title to match.
async fn naming_rows(env: &Env, table: &str, text: &str) -> i64 {
    let statement = match table {
        "artists" => "SELECT COUNT(*) FROM artists WHERE name = ?",
        "albums" => "SELECT COUNT(*) FROM albums WHERE title = ?",
        "tracks" => "SELECT COUNT(*) FROM tracks WHERE title = ?",
        other => panic!("the harness knows no table {other}"),
    };
    sqlx::query_scalar(statement)
        .bind(text)
        .fetch_one(env.database.readers())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_saved_import_falls_back_to_managed_storage_when_the_folder_is_read_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, "01 - Hello Song.flac").await;
    let user = env.add_user().await;
    let folder = env.save_folder(user, "music").await;
    let original = std::fs::metadata(&folder.path).unwrap().permissions();
    std::fs::set_permissions(&folder.path, std::fs::Permissions::from_mode(0o555)).unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request_save(&env, track, user).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;
    std::fs::set_permissions(&folder.path, original).unwrap();

    assert!(
        env.cache_root.join(RELATIVE).is_file(),
        "the file is kept in the managed library instead"
    );
    let rows = media_rows(&env, track).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "cache");
    assert_eq!(rows[0].1, None, "the row is a managed one");
    assert_eq!(env.track(track).await.availability, Availability::Cached);
}
