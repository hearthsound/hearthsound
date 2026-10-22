//! Full-stack tests of prefetch inference.
//!
//! The server is built by the composition root, so the prefetch scheduler is the
//! one the composition root built over the running acquisition layer, and the
//! `/rest` endpoints are the ones that feed it. A `getAlbum` of a catalog album
//! followed by a `stream` of its first track therefore runs the whole inference:
//! the listing is recorded, the streamed track is noted, and the tracks the
//! client is about to reach are requested from the acquisition layer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_acquire::repo::{AcquisitionRow, ListFilter};
use hearthsound_catalog::CatalogService;
use hearthsound_core::{CatalogId, EntityId, EntityKind, Layer, Priority, Slug};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use hearthsound_test_support::{ACQUISITION_FIXTURE, ALBUM_CATALOG, fixture_plugin};
use reqwest::StatusCode;
use serde_json::json;

/// The catalog fixture's manifest id.
const ALBUM_ID: &str = "com.example.album-catalog";

/// The acquisition fixture's manifest id.
const ACQ_ID: &str = "com.example.acquisition-fixture";

/// The provider id of the fixture's album.
const ALBUM_PROVIDER_ID: &str = "album-catalog-album-1";

/// How many tracks the fixture's album holds.
const ALBUM_TRACKS: u8 = 5;

/// How long the acquisition fixture answers `running` before a search completes.
///
/// The streamed track has to be acquired and imported before the held stream
/// can be served, so the fixture's search completes on its first poll and the
/// test does not wait on a simulated backend that keeps searching.
const SEARCH_POLLS: u32 = 0;

/// The name of the file the fixture's download produces.
const FIRST_FILE: &str = "01 - Fixture Song 1.flac";

/// How long the generated audio lasts, in seconds.
///
/// The album catalog's tracks claim 180 seconds, and verification refuses a
/// file whose duration is more than five seconds away from the track's.
const AUDIO_SECONDS: f64 = 180.0;

/// Installs the album catalog fixture and assigns it to the catalog layer.
async fn install_catalog(services: &Arc<hearthsound_server::Services>) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ALBUM_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, ALBUM_ID);
    services
        .instances()
        .create(&InstanceSpec {
            slug: "album".to_owned(),
            plugin_id: ALBUM_ID.to_owned(),
            display_name: "Album catalog".to_owned(),
            server_config: json!({}),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Catalog, &["album".to_owned()])
        .await
        .unwrap();
}

/// Installs the acquisition fixture and assigns it to the acquisition layer.
async fn install_acquisition(services: &Arc<hearthsound_server::Services>, downloads: &Path) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ACQUISITION_FIXTURE))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, ACQ_ID);
    services
        .instances()
        .create(&InstanceSpec {
            slug: "acq".to_owned(),
            plugin_id: ACQ_ID.to_owned(),
            display_name: "Acquisition fixture".to_owned(),
            server_config: json!({
                "candidates": [{
                    "token": "cand-1",
                    "title": "Fixture Song 1",
                    "path": "Fixture Artist/Fixture Album/01 - Fixture Song 1.flac",
                    "files": [{
                        "name": "01 - Fixture Song 1.flac",
                        "size": 4000,
                        "format": "flac",
                        "bitrateKbps": 900,
                        "sampleRate": 44100,
                        "bitDepth": 16,
                        "durationMs": 180_000,
                        "trackIndex": 0,
                    }],
                    "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
                }],
                "scenario": "normal",
                "searchPolls": SEARCH_POLLS,
                "downloadQueuedPolls": 0,
                "downloadRunningPolls": 1,
                "downloadBytesPerPoll": 4000,
                "outputRoot": "downloads",
                "matchSuffix": ".flac",
            }),
            host_config: json!({
                "importRoots": [{
                    "id": "downloads",
                    "path": downloads.to_string_lossy(),
                    "mode": "move",
                    "readOnly": false,
                }],
                "maxConcurrent": 2,
                "maxConcurrentSearches": 2,
                "searchesPerHour": 120,
            }),
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Acquisition, &["acq".to_owned()])
        .await
        .unwrap();
}

/// Writes the audio the simulated backend serves into its import root.
///
/// The held stream of the first track is only served once its acquisition has
/// imported a file, so the import root holds the real audio the fixture's
/// download resolves and imports.
///
/// # Parameters
///
/// - `downloads`: the instance's import root.
async fn place_audio(downloads: &Path) {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(AUDIO_SECONDS)
        .file_name(FIRST_FILE.to_owned())
        .write_into(downloads)
        .await
        .unwrap();
}

/// Returns the catalog id of one track of the fixture's album.
///
/// # Parameters
///
/// - `number`: the track number within the album, one-based.
fn catalog_track(number: u8) -> CatalogId {
    CatalogId::new(
        Slug::new("album").unwrap(),
        EntityKind::Track,
        format!("album-catalog-track-{number}"),
    )
    .unwrap()
}

/// Returns the catalog id of the fixture's album.
fn catalog_album() -> CatalogId {
    CatalogId::new(
        Slug::new("album").unwrap(),
        EntityKind::Album,
        ALBUM_PROVIDER_ID,
    )
    .unwrap()
}

/// Resolves one track of the fixture's album to its stored row.
///
/// # Parameters
///
/// - `catalog`: the running catalog layer.
/// - `number`: the track number within the album, one-based.
async fn stored_track(catalog: &CatalogService, number: u8) -> EntityId {
    catalog
        .materialize_track(&catalog_track(number))
        .await
        .unwrap()
}

/// Returns the ledger rows of one track.
///
/// # Parameters
///
/// - `orchestrator`: the running acquisition layer.
/// - `track`: the track to read.
async fn ledger_rows(
    orchestrator: &AcquisitionOrchestrator,
    track: EntityId,
) -> Vec<AcquisitionRow> {
    let listed = orchestrator.list(&ListFilter::default()).await.unwrap();
    let mut rows = Vec::new();
    for summary in listed.into_iter().filter(|row| row.track_id == Some(track)) {
        rows.push(
            orchestrator
                .repository()
                .get(summary.id)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    rows
}

/// Waits until a condition holds, up to ten seconds.
///
/// The prefetch runs off the request path, so the test has to give the spawned
/// note a moment; a condition that never holds fails rather than hangs.
///
/// # Parameters
///
/// - `condition`: the condition to poll.
///
/// # Panics
///
/// Panics when the condition is still false after ten seconds.
async fn eventually<F, Fut>(mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..500 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the condition never held");
}

/// Playing track 1 of a catalog album prefetches tracks 2 to 4.
#[tokio::test]
async fn a_catalog_stream_prefetches_the_tracks_after_it() {
    let fixture = crate::common::Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = fixture.database_path().parent().unwrap().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    install_catalog(&services).await;
    install_acquisition(&services, &downloads).await;
    services.registry().rebuild().await.unwrap();
    place_audio(&downloads).await;

    let api_key = &fixture.api_key;
    let album = catalog_album();

    // The client reads the album, which is the listing the queue is inferred
    // from, and then plays its first track.
    let listed = fixture
        .get(&format!(
            "/rest/getAlbum?id={album}&c=symfonium&apiKey={api_key}&f=json"
        ))
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    let body = listed.text().await.unwrap();
    assert!(
        body.contains(&catalog_track(1).to_string()),
        "the listing names the album's tracks: {body}"
    );

    let streamed = fixture
        .get(&format!(
            "/rest/stream?id={}&c=symfonium&apiKey={api_key}",
            catalog_track(1)
        ))
        .await;
    assert_eq!(streamed.status(), StatusCode::OK);
    drop(streamed);

    let tracks: Vec<EntityId> = {
        let catalog = services.catalog();
        let mut tracks = Vec::new();
        for number in 1..=ALBUM_TRACKS {
            tracks.push(stored_track(catalog, number).await);
        }
        tracks
    };

    let orchestrator = Arc::clone(services.orchestrator());
    let prefetched = tracks[1..4].to_vec();
    eventually(|| {
        let orchestrator = Arc::clone(&orchestrator);
        let prefetched = prefetched.clone();
        async move {
            for track in prefetched {
                let rows = ledger_rows(&orchestrator, track).await;
                if !rows.iter().any(|row| row.reason == "prefetch") {
                    return false;
                }
            }
            true
        }
    })
    .await;

    for (index, track) in tracks[1..4].iter().enumerate() {
        let rows = ledger_rows(&orchestrator, *track).await;
        let prefetch = rows
            .iter()
            .find(|row| row.reason == "prefetch")
            .unwrap_or_else(|| panic!("{track} is prefetched"));
        let expected = if index == 0 {
            Priority::Next
        } else {
            Priority::Queue
        };
        assert_eq!(prefetch.priority, expected, "the priority of {track}");
        assert_eq!(prefetch.requested_by, Some(fixture.user_id));
        assert_eq!(prefetch.track_id, Some(*track));
    }

    assert!(
        ledger_rows(&orchestrator, tracks[4]).await.is_empty(),
        "the look-ahead stops before the fifth track"
    );

    let played = ledger_rows(&orchestrator, tracks[0]).await;
    assert_eq!(
        played.len(),
        1,
        "the streamed track has its own acquisition"
    );
    assert_eq!(
        played[0].reason, "stream",
        "the streamed track is acquired by the stream, not by the prefetch"
    );
    assert_eq!(played[0].priority, Priority::Play);
}
