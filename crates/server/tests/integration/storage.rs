//! Full-stack tests for the storage lifecycle: managed files follow their pins
//! while the server runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_core::{Availability, EntityId, EntityKind, StorageClass};
use hearthsound_library::{LibraryRepository, ManagedMediaFile, Origin, TrackInput};
use reqwest::StatusCode;

use crate::common::Fixture;

/// How long a pin change may take to reach the filesystem.
const PATIENCE: Duration = Duration::from_secs(5);

/// Stores a track with one managed file under `<storage>/library`.
async fn stored_track(fixture: &Fixture, rel_path: &str) -> EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let track = library
        .insert_track(
            &TrackInput::new("Song", Origin::Local, Availability::Local),
            &[],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: rel_path.to_owned(),
            size: 4,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    let path = fixture.services.storage_roots().library.join(rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"flac").unwrap();
    track
}

/// Unstarring a liked track moves its file from the library root into the
/// cache root without a manual step.
#[tokio::test]
async fn unstarring_a_liked_track_demotes_its_file_into_the_cache() {
    let fixture = Fixture::new().await;
    let rel_path = "Artist/Album/01 - Song.flac";
    let track = stored_track(&fixture, rel_path).await;
    fixture
        .services
        .activity()
        .star(fixture.user_id, &[(EntityKind::Track, track.uuid())])
        .await
        .unwrap();
    let roots = fixture.services.storage_roots().clone();

    let response = fixture
        .get(&format!(
            "/rest/unstar?id={track}&apiKey={}&f=json",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("\"status\":\"ok\""));

    // The file moves first and its row follows, so the demotion is complete
    // once the row says the file is in the cache.
    let library = LibraryRepository::new(fixture.services.database());
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let files = library.list_media_files_for_track(track).await.unwrap();
        if files.iter().any(|file| file.storage == StorageClass::Cache) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the file was not demoted within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(roots.cache.join(rel_path).is_file());
    assert!(!roots.library.join(rel_path).exists());
    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(stored.availability, Availability::Cached);
    fixture.stop().await;
}
