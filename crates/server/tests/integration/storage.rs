//! Full-stack tests for the storage lifecycle: managed files follow their pins
//! while the server runs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_core::{Availability, EntityId, EntityKind, StorageClass};
use hearthsound_library::{LibraryRepository, ManagedMediaFile, TrackInput};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::common::Fixture;

/// How long a pin change may take to reach the filesystem.
const PATIENCE: Duration = Duration::from_secs(5);

/// Stores a track with one managed file under the root of `storage`.
///
/// The track's availability follows its storage class: a cache file makes the
/// track `Cached`, any other file makes it `Local`.
pub(crate) async fn managed_track(
    fixture: &Fixture,
    storage: StorageClass,
    rel_path: &str,
) -> EntityId {
    let availability = match storage {
        StorageClass::Cache => Availability::Cached,
        StorageClass::Library | StorageClass::Folder => Availability::Local,
    };
    let library = LibraryRepository::new(fixture.services.database());
    let track = library
        .insert_track(&TrackInput::new("Song", availability), &[], &[])
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage,
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
    let path = fixture
        .services
        .storage_roots()
        .root_for(storage)
        .join(rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"flac").unwrap();
    track
}

/// Sends a request with the fixture's API key as a bearer credential.
async fn bearer(
    fixture: &Fixture,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key));
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    request.send().await.unwrap()
}

/// Unstarring a liked track moves its file from the library root into the
/// cache root without a manual step.
#[tokio::test]
async fn unstarring_a_liked_track_demotes_its_file_into_the_cache() {
    let fixture = Fixture::new().await;
    let rel_path = "Artist/Album/01 - Song.flac";
    let track = managed_track(&fixture, StorageClass::Library, rel_path).await;
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

/// A forced maintenance pass evicts the unpinned cache file and keeps the
/// pinned library file; the report reflects the final state of both roots.
#[tokio::test]
async fn forced_eviction_removes_only_unpinned_cache_files() {
    let fixture = Fixture::new().await;
    let roots = fixture.services.storage_roots().clone();
    let cache_rel = "Cached/Song.flac";
    let library_rel = "Pinned/Song.flac";
    let cached = managed_track(&fixture, StorageClass::Cache, cache_rel).await;
    let pinned = managed_track(&fixture, StorageClass::Library, library_rel).await;
    fixture
        .services
        .activity()
        .star(fixture.user_id, &[(EntityKind::Track, pinned.uuid())])
        .await
        .unwrap();

    let patch = bearer(
        &fixture,
        Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({
            "settings": {
                "storage.cache.max_bytes": 0,
                "storage.cache.min_free_bytes": 0,
            }
        })),
    )
    .await;
    assert_eq!(patch.status(), StatusCode::OK);

    let evict = bearer(&fixture, Method::POST, "/api/v1/admin/storage/evict", None).await;
    assert_eq!(evict.status(), StatusCode::OK);
    let report: Value = serde_json::from_str(&evict.text().await.unwrap()).unwrap();
    assert!(report["autopin"].is_object());
    assert!(report["relocation"].is_object());
    assert!(report["eviction"].is_object());

    let library = LibraryRepository::new(fixture.services.database());
    assert!(
        library
            .list_media_files_for_track(cached)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(!roots.cache.join(cache_rel).exists());
    assert!(roots.library.join(library_rel).is_file());
    assert_eq!(
        library
            .list_media_files_for_track(pinned)
            .await
            .unwrap()
            .len(),
        1
    );

    let storage = bearer(&fixture, Method::GET, "/api/v1/admin/storage", None).await;
    assert_eq!(storage.status(), StatusCode::OK);
    let storage: Value = serde_json::from_str(&storage.text().await.unwrap()).unwrap();
    assert_eq!(storage["cache"]["files"], 0);
    assert_eq!(storage["library"]["files"], 1);
    assert_eq!(storage["policy"]["cacheMaxBytes"], 0);
    fixture.stop().await;
}
