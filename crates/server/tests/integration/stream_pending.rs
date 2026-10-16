//! Full-stack tests of the pending-stream path.
//!
//! The server is built by the composition root with a real acquisition
//! orchestrator and a real catalog layer, both backed by the fixture plugins, so
//! `/rest/stream` of an unmaterialized catalog id runs the whole path: the
//! catalog materializes the track, the acquisition layer is asked for it, and
//! `stream.pending.mode` decides what the client receives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{CatalogId, EntityKind, Layer, Slug};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::{ACQUISITION_FIXTURE, HELLO_CATALOG, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use serde_json::json;

/// The catalog fixture's manifest id.
const HELLO_ID: &str = "com.example.hello-catalog";

/// The acquisition fixture's manifest id.
const ACQ_ID: &str = "com.example.acquisition-fixture";

/// The track provider id the catalog fixture answers with.
const HELLO_TRACK: &str = "hello-track-1";

/// How long the acquisition fixture answers `running` before a search completes.
///
/// A pending stream's job is to decide what to answer while an acquisition
/// runs, so the search is kept alive far longer than the test.
const SEARCH_POLLS: u32 = 60;

/// Installs the catalog fixture and assigns it to the catalog layer.
async fn install_catalog(services: &Arc<hearthsound_server::Services>) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register_installed(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, HELLO_ID);
    services
        .instances()
        .create(&InstanceSpec {
            slug: "hello".to_owned(),
            plugin_id: HELLO_ID.to_owned(),
            display_name: "Hello catalog".to_owned(),
            server_config: json!({ "greeting": "Hello" }),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Catalog, &["hello".to_owned()])
        .await
        .unwrap();
}

/// Installs the acquisition fixture and assigns it to the acquisition layer.
///
/// The instance simulates a backend that finds one lossless candidate and keeps
/// searching, so an acquisition stays running for the whole test.
async fn install_acquisition(services: &Arc<hearthsound_server::Services>, downloads: &Path) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ACQUISITION_FIXTURE))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register_installed(&installed, PluginSource::Sideload)
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
                    "title": "Hello Song",
                    "path": "Hello Artist/Hello Song album/01 - Hello Song.flac",
                    "files": [{
                        "name": "01 - Hello Song.flac",
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

/// A catalog stream is held until its file is ready, and `fail` answers 503.
#[tokio::test]
async fn a_catalog_stream_is_held_and_then_answers_503_in_fail_mode() {
    let fixture = crate::common::Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    let downloads = fixture.database_path().parent().unwrap().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    install_catalog(&services).await;
    install_acquisition(&services, &downloads).await;
    services.registry().rebuild().await.unwrap();

    let id = CatalogId::new(Slug::new("hello").unwrap(), EntityKind::Track, HELLO_TRACK).unwrap();
    let path = format!("/rest/stream?id={id}&apiKey={}", fixture.api_key);

    // The wait bound is shortened so the held request ends as a `503` instead
    // of holding for the documented default.
    services
        .settings()
        .set(
            keys::STREAM_PENDING_MAX_WAIT_SECS.name(),
            SettingValue::Integer(2),
            None,
        )
        .await
        .unwrap();

    // The default mode is `hold`: nothing is sent while the acquisition runs, so
    // no response arrives within the window the test watches.
    assert!(
        tokio::time::timeout(Duration::from_millis(500), fixture.get(&path))
            .await
            .is_err(),
        "hold answered the request before the acquisition produced a file"
    );

    // The held request gives up with a retry hint once its wait elapses, while
    // the acquisition keeps running in the background.
    let response = fixture.get(&path).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("10")
    );

    // `fail` answers at once with the same hint.
    services
        .settings()
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("fail".to_owned()),
            None,
        )
        .await
        .unwrap();

    let response = fixture.get(&path).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("10")
    );
}
