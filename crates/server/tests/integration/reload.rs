//! Full-stack tests of the reload path.
//!
//! A command-line command writes the database through its own connection while
//! the server runs. These tests simulate that by opening a second connection to
//! the same file and asserting that the running server picks the change up
//! within about a second, which is the promise `08-operations-and-release.md`
//! section 4 makes about the first-run commands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_acquire::orchestrator::AcquireRequest;
use hearthsound_acquire::repo::AcquisitionRepository;
use hearthsound_config::secret::SecretKey;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::{AcquisitionState, Availability, EntityId, Layer, Priority};
use hearthsound_db::Database;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use hearthsound_plugin_host::{InstanceService, InstanceSpec, PluginRepository, PluginSource};
use hearthsound_server::Services;
use hearthsound_test_support::{ACQUISITION_FIXTURE, HELLO_CATALOG, fixture_plugin};

/// The fixture plugin's manifest id.
const HELLO_ID: &str = "com.example.hello-catalog";

/// How long a change made by another connection may take to be visible.
const RELOAD_BUDGET: Duration = Duration::from_secs(2);

/// A setting written by another connection reaches the running server.
#[tokio::test]
async fn a_setting_written_by_another_connection_is_picked_up() {
    let fixture = crate::common::Fixture::new().await;
    assert_eq!(
        fixture
            .services
            .settings()
            .current()
            .get(keys::STREAM_PENDING_MODE)
            .unwrap(),
        "hold"
    );

    let other = Arc::new(Database::open(fixture.database_path()).await.unwrap());
    let settings = Settings::load(
        other.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();

    settings
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("fail".to_owned()),
            None,
        )
        .await
        .unwrap();
    settings
        .set(
            keys::STREAM_MAX_CONCURRENT_TRANSCODES.name(),
            SettingValue::Integer(3),
            None,
        )
        .await
        .unwrap();

    let started = Instant::now();
    loop {
        let mode = fixture
            .services
            .settings()
            .current()
            .get(keys::STREAM_PENDING_MODE)
            .unwrap();
        if mode == "fail" && fixture.services.ffmpeg().max_concurrent() == 3 {
            break;
        }
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "after {:?} the server still reports mode {mode} and cap {}",
            started.elapsed(),
            fixture.services.ffmpeg().max_concurrent()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A setting written through the server updates the cached ffmpeg cap.
#[tokio::test]
async fn a_setting_written_through_the_server_updates_the_cached_cap() {
    let fixture = crate::common::Fixture::new().await;

    fixture
        .services
        .settings()
        .set(
            keys::STREAM_MAX_CONCURRENT_TRANSCODES.name(),
            SettingValue::Integer(5),
            None,
        )
        .await
        .unwrap();

    let started = Instant::now();
    while fixture.services.ffmpeg().max_concurrent() != 5 {
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "the ffmpeg cap is still {}",
            fixture.services.ffmpeg().max_concurrent()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A layer assignment written by another connection rebuilds the registry.
///
/// The fixture plugin is installed and configured through the running server's
/// own handles, then the layer is assigned the way `hearthsound layers set`
/// does: through a second connection to the same database file.
#[tokio::test]
async fn a_layer_assignment_written_by_another_connection_rebuilds_the_registry() {
    let fixture = crate::common::Fixture::new().await;
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(fixture.services.database())
        .register_installed(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, HELLO_ID);
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "hello".to_owned(),
            plugin_id: HELLO_ID.to_owned(),
            display_name: "Hello".to_owned(),
            server_config: serde_json::json!({}),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();
    assert!(
        fixture
            .services
            .registry()
            .layers(Layer::Catalog)
            .is_empty()
    );

    let other = Arc::new(Database::open(fixture.database_path()).await.unwrap());
    let key = SecretKey::load(
        &fixture.services.config().secrets.key_file,
        &BTreeMap::new(),
    )
    .unwrap();
    let instances = InstanceService::new(
        Arc::clone(fixture.services.plugins()),
        PluginRepository::new(&other),
        key.secret_box(),
    );
    instances
        .set_layer(Layer::Catalog, &["hello".to_owned()])
        .await
        .unwrap();

    let started = Instant::now();
    loop {
        let assigned = fixture.services.registry().layers(Layer::Catalog);
        if assigned
            .iter()
            .any(|instance| instance.slug().as_str() == "hello")
        {
            break;
        }
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "the catalog layer still holds {} instances after {:?}",
            assigned.len(),
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The acquisition fixture's manifest id.
const ACQ_ID: &str = "com.example.acquisition-fixture";

/// How long the acquisition fixture answers `running` before a search completes.
///
/// The acquisition has to be alive while the test cancels it, so the search is
/// kept running far longer than the test.
const SEARCH_POLLS: u32 = 60;

/// Installs the acquisition fixture and assigns it to the acquisition layer.
///
/// The instance simulates a backend that finds one lossless candidate and keeps
/// searching, so an acquisition stays alive for the whole test.
///
/// # Parameters
///
/// - `services`: the running server's services.
/// - `downloads`: the directory granted as the instance's import root.
async fn install_acquisition(services: &Arc<Services>, downloads: &std::path::Path) {
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
            server_config: serde_json::json!({
                "candidates": [{
                    "token": "cand-1",
                    "title": "Reload Song",
                    "path": "Reload Artist/Reload Album/01 - Reload Song.flac",
                    "files": [{
                        "name": "01 - Reload Song.flac",
                        "size": 4000,
                        "format": "flac",
                        "bitrateKbps": 900,
                        "sampleRate": 44100,
                        "bitDepth": 16,
                        "durationMs": 200,
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
            host_config: serde_json::json!({
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

/// Inserts one catalog-only track and returns its id.
///
/// # Parameters
///
/// - `library`: the library tables of the running server's database.
async fn insert_remote_track(library: &LibraryRepository) -> EntityId {
    let mut album_input = AlbumInput::new("Reload Album", Origin::Catalog);
    album_input.year = Some(1993);
    album_input.display_artist = Some("Reload Artist".to_owned());
    let album = library
        .upsert_album(&album_input, &[ArtistCredit::new("Reload Artist", "main")])
        .await
        .unwrap();
    let mut input = TrackInput::new("Reload Song", Origin::Catalog, Availability::Remote);
    input.album_id = Some(album);
    input.display_artist = Some("Reload Artist".to_owned());
    input.track_number = Some(1);
    input.duration_ms = Some(200);
    library
        .insert_track(&input, &[ArtistCredit::new("Reload Artist", "main")], &[])
        .await
        .unwrap()
}

/// Cancels an acquisition through a second connection.
///
/// This is what `hearthsound acquisitions cancel` does: it opens the database
/// file itself and writes the terminal row through its own connection while the
/// running server keeps writing to the same file. The cancellation's transaction
/// reads the row's state and then writes it, and it starts as a write, so a
/// commit the server made in between cannot fail it with a busy snapshot.
///
/// # Parameters
///
/// - `path`: the database file the command line opens.
/// - `id`: the acquisition to cancel.
///
/// # Returns
///
/// `true` when the cancellation was written.
///
/// # Panics
///
/// Panics when the database cannot be opened or the cancellation cannot be
/// written, both of which a server writing to the same file used to cause.
async fn cancel_through_another_connection(path: &std::path::Path, id: uuid::Uuid) -> bool {
    let other = Arc::new(Database::open(path).await.unwrap());
    let repository = AcquisitionRepository::new(&other);
    repository
        .cancel(id, Some("cancelled from the command line"))
        .await
        .expect("the cancellation is written while the server runs")
}

/// A cancellation written by another connection stops the server's worker.
///
/// `hearthsound acquisitions cancel` writes the terminal row through its own
/// connection and cannot reach the running process, so the reload path is what
/// undoes the acquisition: the worker is woken, the staging directory is
/// removed, and the track goes back to catalog-only availability.
#[tokio::test]
async fn a_cancellation_written_by_another_connection_is_reconciled() {
    let fixture = crate::common::Fixture::new().await;
    let downloads = fixture.database_path().parent().unwrap().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    install_acquisition(&fixture.services, &downloads).await;
    fixture.services.registry().rebuild().await.unwrap();

    let library = LibraryRepository::new(fixture.services.database());
    let track = insert_remote_track(&library).await;
    let orchestrator = Arc::clone(fixture.services.orchestrator());
    let id = orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "test"))
        .await
        .unwrap()
        .acquisition_id()
        .unwrap();
    let staging = orchestrator.staging_dir(id);
    assert!(
        staging.is_dir(),
        "an admitted acquisition has its staging directory"
    );

    // The command line cancels through its own connection to the same file.
    assert!(cancel_through_another_connection(fixture.database_path(), id).await);

    let started = Instant::now();
    loop {
        let row = library.get_track(track).await.unwrap().unwrap();
        if row.availability == Availability::Remote && !staging.exists() {
            break;
        }
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "the server did not reconcile the cancellation: availability {}, staging {}",
            row.availability.as_str(),
            staging.exists()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let row = orchestrator.repository().get(id).await.unwrap().unwrap();
    assert_eq!(row.state, AcquisitionState::Cancelled);
    assert_eq!(
        row.error_detail.as_deref(),
        Some("cancelled from the command line"),
        "the command line's detail is kept"
    );
}
