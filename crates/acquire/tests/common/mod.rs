//! The harness the acquisition tests share.
//!
//! Every test builds a real plugin host, provider registry and database in a
//! temporary directory, installs the acquisition fixture as the instance of the
//! acquisition layer, and drives the orchestrator the way the API layer will.
//! Nothing here reaches the network: the fixture simulates its backend through
//! the host's key-value store and its file import.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_config::secret::SecretKey;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry};
use hearthsound_core::events::{EventBus, EventEnvelope, ServerEvent};
use hearthsound_core::{AcquisitionState, Availability, EntityId, Layer};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, Track, TrackInput};
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
};
use hearthsound_test_support::{ACQUISITION_FIXTURE, fixture_plugin};
use serde_json::{Value, json};
use uuid::Uuid;

/// The plugin id of the acquisition fixture.
pub const PLUGIN_ID: &str = "com.example.acquisition-fixture";

/// The slug of the fixture instance every test configures.
pub const INSTANCE: &str = "acq";

/// One test's database, plugin host, settings and orchestrator.
pub struct Env {
    /// Kept alive so the temporary tree outlives everything built over it.
    pub dir: tempfile::TempDir,
    /// The migrated database.
    pub database: Arc<Database>,
    /// The settings service the orchestrator reads.
    pub settings: Settings,
    /// The plugin host the fixture is installed into.
    pub host: Arc<PluginHost>,
    /// The provider registry the acquisition layer is read from.
    pub registry: Arc<ProviderRegistry>,
    /// The plugin tables.
    pub repository: PluginRepository,
    /// The event bus the orchestrator publishes on.
    pub events: Arc<EventBus>,
    /// The library tables the wanted tracks are read from.
    pub library: LibraryRepository,
    /// The instance's import root, where a completed download leaves files.
    pub downloads: PathBuf,
    /// The root of the staging tree, `<storage>/staging`.
    pub staging: PathBuf,
    /// The service under test.
    pub orchestrator: Arc<AcquisitionOrchestrator>,
}

impl Env {
    /// Builds an environment and configures the fixture instance.
    ///
    /// # Parameters
    ///
    /// - `server_config`: the fixture's server-scoped configuration, which says
    ///   what the simulated backend does.
    /// - `env`: extra environment variables for the settings layer, keyed by
    ///   their full `HEARTHSOUND__...` name. They are the highest-precedence
    ///   source, which is how a test shortens a timeout.
    pub async fn new(server_config: Value, env: &[(&str, &str)]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        let staging = dir.path().join("storage/staging");
        std::fs::create_dir_all(&staging).unwrap();

        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let database = Arc::new(database);

        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::new(
                env.iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect::<BTreeMap<_, _>>(),
                BTreeMap::new(),
            ),
        )
        .await
        .unwrap();

        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let secrets = SecretKey::generate().secret_box();
        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&host),
            repository.clone(),
            secrets.clone(),
        ));

        let installed = host
            .install(&fixture_plugin(ACQUISITION_FIXTURE))
            .await
            .unwrap();
        repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap();

        let instances = InstanceService::new(Arc::clone(&host), repository.clone(), secrets);
        instances
            .create(&InstanceSpec {
                slug: INSTANCE.to_owned(),
                plugin_id: PLUGIN_ID.to_owned(),
                display_name: "Acquisition fixture".to_owned(),
                server_config,
                host_config: host_config(&downloads),
            })
            .await
            .unwrap();
        instances
            .set_layer(Layer::Acquisition, &[INSTANCE.to_owned()])
            .await
            .unwrap();
        registry.rebuild().await.unwrap();

        let events = Arc::new(EventBus::new());
        let library = LibraryRepository::new(&database);
        let orchestrator = Arc::new(AcquisitionOrchestrator::new(
            Arc::clone(&database),
            settings.clone(),
            Arc::clone(&registry),
            Arc::new(FfmpegRunner::new(None, 8)),
            Arc::clone(&events),
            staging.clone(),
        ));

        Env {
            dir,
            database,
            settings,
            host,
            registry,
            repository,
            events,
            library,
            downloads,
            staging,
            orchestrator,
        }
    }

    /// Starts the scheduler.
    pub fn start(&self) {
        self.orchestrator.start();
    }

    /// Builds an environment whose instance runs a normal search for one file.
    ///
    /// # Parameters
    ///
    /// - `env`: extra settings environment variables.
    pub async fn normal(env: &[(&str, &str)]) -> Env {
        Env::new(candidate_config("downloads", ".flac"), env).await
    }

    /// Inserts an album and one track of it.
    ///
    /// # Parameters
    ///
    /// - `title`: the track title.
    /// - `artist`: the track and album artist.
    /// - `availability`: the track's availability.
    /// - `unavailable_until`: the cooldown end, for an `unavailable` track.
    pub async fn add_track(
        &self,
        title: &str,
        artist: &str,
        availability: Availability,
        unavailable_until: Option<i64>,
    ) -> EntityId {
        let album = self
            .library
            .upsert_album(
                &AlbumInput::new(format!("{title} album"), Origin::Catalog),
                &[ArtistCredit::new(artist, "main")],
            )
            .await
            .unwrap();
        let mut input = TrackInput::new(title, Origin::Catalog, availability);
        input.album_id = Some(album);
        input.display_artist = Some(artist.to_owned());
        input.disc_number = Some(1);
        input.track_number = Some(1);
        input.duration_ms = Some(180_000);
        input.isrc = Some("USHW12600001".to_owned());
        input.unavailable_until = unavailable_until;
        self.library
            .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
            .await
            .unwrap()
    }

    /// Inserts a track that is catalog-only, the usual starting point.
    pub async fn remote_track(&self, title: &str, artist: &str) -> EntityId {
        self.add_track(title, artist, Availability::Remote, None)
            .await
    }

    /// Reads a track row.
    pub async fn track(&self, id: EntityId) -> Track {
        self.library.get_track(id).await.unwrap().unwrap()
    }

    /// Reads an acquisition row.
    pub async fn acquisition(&self, id: Uuid) -> hearthsound_acquire::repo::AcquisitionRow {
        self.orchestrator
            .repository()
            .get(id)
            .await
            .unwrap()
            .unwrap()
    }

    /// Reads an acquisition's log.
    pub async fn log(&self, id: Uuid) -> Vec<hearthsound_acquire::repo::LogEntry> {
        self.orchestrator.repository().log(id).await.unwrap()
    }

    /// Counts the acquisition rows of a track.
    pub async fn acquisitions_for(&self, track_id: EntityId) -> i64 {
        sqlx::query_scalar!(
            "SELECT COUNT(*) FROM acquisitions WHERE track_id = ?",
            track_id.uuid().as_bytes().to_vec(),
        )
        .fetch_one(self.database.readers())
        .await
        .unwrap()
    }

    /// Waits until an acquisition reaches a state.
    ///
    /// # Panics
    ///
    /// Panics when the state is not reached within ten seconds, so a stuck
    /// pipeline fails the test instead of hanging it.
    pub async fn wait_for_state(&self, id: Uuid, state: AcquisitionState) {
        for _ in 0..500 {
            if self
                .orchestrator
                .repository()
                .get(id)
                .await
                .unwrap()
                .unwrap()
                .state
                == state
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = self.acquisition(id).await;
        panic!(
            "acquisition {id} stayed in {} instead of reaching {state}",
            row.state
        );
    }

    /// Waits until an acquisition reaches a terminal state.
    pub async fn wait_for_terminal(&self, id: Uuid) -> hearthsound_acquire::repo::AcquisitionRow {
        for _ in 0..1500 {
            let row = self.acquisition(id).await;
            if row.state.is_terminal() {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = self.acquisition(id).await;
        panic!("acquisition {id} stayed in {}", row.state);
    }
}

/// The host configuration that grants `downloads` as the instance's import root.
pub fn host_config(downloads: &Path) -> Value {
    json!({
        "importRoots": [
            {
                "id": "downloads",
                "path": downloads.to_string_lossy(),
                "mode": "move",
                "readOnly": false,
            }
        ],
        "maxConcurrent": 2,
        "maxConcurrentSearches": 2,
        "searchesPerHour": 120,
    })
}

/// The fixture configuration of a search that finds one matching candidate.
///
/// # Parameters
///
/// - `output_root`: the root the completed download lists, `staging` or the id
///   of an import root the instance was granted.
/// - `match_suffix`: the suffix an output file must have to be reported.
pub fn candidate_config(output_root: &str, match_suffix: &str) -> Value {
    json!({
        "candidates": [candidate("cand-1", "Hello Song", "peer/hello.flac")],
        "searchPolls": 1,
        "downloadQueuedPolls": 0,
        "downloadRunningPolls": 1,
        "downloadBytesPerPoll": 4000,
        "outputRoot": output_root,
        "matchSuffix": match_suffix,
    })
}

/// One fixture candidate that matches the `Hello Song` track of [`Env::add_track`].
pub fn candidate(token: &str, title: &str, path: &str) -> Value {
    json!({
        "token": token,
        "title": title,
        "path": path,
        "files": [
            {
                "name": "hello.flac",
                "size": 4000,
                "format": "flac",
                "bitrateKbps": 900,
                "sampleRate": 44100,
                "bitDepth": 16,
                "durationMs": 180_000,
                "trackIndex": 0,
            }
        ],
        "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
    })
}

/// Drains the events published since the receiver was created.
pub fn drain(events: &mut tokio::sync::broadcast::Receiver<EventEnvelope>) -> Vec<ServerEvent> {
    let mut drained = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        drained.push(envelope.event);
    }
    drained
}

/// Returns whether the events contain an `acquisition.updated` for `id`.
pub fn has_acquisition_event(events: &[ServerEvent], id: Uuid, state: AcquisitionState) -> bool {
    let client_id = EntityId::from_uuid(hearthsound_core::IdPrefix::Acquisition, id).to_string();
    events.iter().any(|event| match event {
        ServerEvent::AcquisitionUpdated(update) => {
            update.state == state && update.acquisition_id == client_id
        }
        _ => false,
    })
}

/// Returns whether the events contain a `track.availability` for `track`.
pub fn has_availability_event(
    events: &[ServerEvent],
    track: EntityId,
    availability: Availability,
) -> bool {
    events.iter().any(|event| match event {
        ServerEvent::TrackAvailability(update) => {
            update.availability == availability && update.track_id == track.to_string()
        }
        _ => false,
    })
}
