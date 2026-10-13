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

use hearthsound_acquire::orchestrator::{AcquisitionOrchestrator, AcquisitionStorage};
use hearthsound_acquire::repo::{self, NewAcquisition};
use hearthsound_catalog::CatalogService;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry};
use hearthsound_core::events::{EventBus, EventEnvelope, ServerEvent};
use hearthsound_core::{AcquisitionState, Availability, EntityId, Granularity, Layer, Priority};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, Origin, StorageRoots, Track, TrackInput, tracks,
};
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use hearthsound_test_support::{ACQUISITION_FIXTURE, METADATA_FIXTURE, fixture_plugin};
use serde_json::{Value, json};
use uuid::Uuid;

/// The plugin id of the acquisition fixture.
pub const PLUGIN_ID: &str = "com.example.acquisition-fixture";

/// The plugin id of the metadata fixture.
pub const METADATA_PLUGIN_ID: &str = "com.example.metadata-fixture";

/// The slug of the fixture instance every test configures first.
pub const INSTANCE: &str = "acq";

/// The concurrency and rate limits an instance is configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceLimits {
    /// How many downloads the instance may run at once.
    pub max_concurrent: u32,
    /// How many searches the instance may run at once.
    pub max_concurrent_searches: u32,
    /// How many searches the instance may start per hour.
    pub searches_per_hour: u32,
}

impl Default for InstanceLimits {
    /// Returns the defaults the plugin host documents.
    fn default() -> InstanceLimits {
        InstanceLimits {
            max_concurrent: 2,
            max_concurrent_searches: 2,
            searches_per_hour: 120,
        }
    }
}

/// One test's database, plugin host, settings and orchestrator.
pub struct Env {
    /// Kept alive so the temporary tree outlives everything built over it.
    ///
    /// A reopened environment takes a directory its caller already owns, so it
    /// holds no `TempDir` and this is `None`.
    pub dir: Option<tempfile::TempDir>,
    /// The directory the database, the plugin tree and the storage live in.
    pub root: PathBuf,
    /// The migrated database.
    pub database: Arc<Database>,
    /// The settings service the orchestrator reads.
    pub settings: Settings,
    /// The plugin host the fixture is installed into.
    pub host: Arc<PluginHost>,
    /// The provider registry the acquisition layer is read from.
    pub registry: Arc<ProviderRegistry>,
    /// The catalog service identity resolution goes through.
    pub catalog: Arc<CatalogService>,
    /// The plugin tables.
    pub repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    pub secrets: SecretBox,
    /// The event bus the orchestrator publishes on.
    pub events: Arc<EventBus>,
    /// The library tables the wanted tracks are read from.
    pub library: LibraryRepository,
    /// The instance's import root, where a completed download leaves files.
    pub downloads: PathBuf,
    /// The root of the staging tree, `<storage>/staging`.
    pub staging: PathBuf,
    /// The managed permanent storage root, `<storage>/library`.
    pub library_root: PathBuf,
    /// The managed evictable storage root, `<storage>/cache`.
    pub cache_root: PathBuf,
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
    /// - `limits`: the instance's concurrency and rate limits.
    /// - `env`: extra environment variables for the settings layer, keyed by
    ///   their full `HEARTHSOUND__...` name. They are the highest-precedence
    ///   source, which is how a test shortens a timeout or lowers a bound.
    pub async fn new(server_config: Value, limits: InstanceLimits, env: &[(&str, &str)]) -> Env {
        Env::build(server_config, limits, env, "move", false).await
    }

    /// Opens an environment over a directory a previous environment built.
    ///
    /// This is what a server restart looks like to a test: the database, the
    /// installed plugin, the secret key and the storage roots are the ones the
    /// previous process left, and only the host, the registry, the services and
    /// the orchestrator are new. Nothing is installed and no instance is
    /// configured, because those rows are already there. The caller keeps the
    /// directory alive, because the returned environment does not own it.
    ///
    /// # Parameters
    ///
    /// - `root`: the directory a previous [`Env::new`] was built in.
    /// - `env`: extra settings environment variables, the same ones the previous
    ///   environment was built with.
    pub async fn reopen(root: &Path, env: &[(&str, &str)]) -> Env {
        let environment = Env::open(root, env).await;
        environment.registry.rebuild().await.unwrap();
        environment
    }

    /// Returns the temporary directory the database and the storage live in.
    ///
    /// A test that needs a second connection - the one a CLI command would
    /// open - builds its own [`Database`](hearthsound_db::Database) from
    /// `<directory>/hearthsound.db`.
    pub fn directory(&self) -> PathBuf {
        self.root.clone()
    }

    /// Builds an environment whose import root has a non-default mode.
    ///
    /// # Parameters
    ///
    /// - `server_config`: the fixture's server-scoped configuration.
    /// - `limits`: the instance's concurrency and rate limits.
    /// - `env`: extra environment variables for the settings layer.
    /// - `mode`: the import root's post-import mode, `move`, `copy` or `link`.
    /// - `read_only`: whether the import root is read-only.
    pub async fn with_import_mode(
        server_config: Value,
        limits: InstanceLimits,
        env: &[(&str, &str)],
        mode: &str,
        read_only: bool,
    ) -> Env {
        Env::build(server_config, limits, env, mode, read_only).await
    }

    /// Builds a fresh environment with the given import-root mode.
    async fn build(
        server_config: Value,
        limits: InstanceLimits,
        env: &[(&str, &str)],
        mode: &str,
        read_only: bool,
    ) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for directory in [
            "downloads",
            "storage/staging",
            "storage/library",
            "storage/cache",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let mut environment = Env::open(&root, env).await;
        environment.dir = Some(dir);

        let installed = environment
            .host
            .install(&fixture_plugin(ACQUISITION_FIXTURE))
            .await
            .unwrap();
        environment
            .repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap();

        let service = InstanceService::new(
            Arc::clone(&environment.host),
            environment.repository.clone(),
            environment.secrets.clone(),
        );
        service
            .create(&InstanceSpec {
                slug: INSTANCE.to_owned(),
                plugin_id: PLUGIN_ID.to_owned(),
                display_name: "Acquisition fixture".to_owned(),
                server_config,
                host_config: host_config_with(&environment.downloads, limits, mode, read_only),
            })
            .await
            .unwrap();
        service
            .set_layer(Layer::Acquisition, &[INSTANCE.to_owned()])
            .await
            .unwrap();
        environment.registry.rebuild().await.unwrap();

        environment
    }

    /// Opens the database, the plugin tree and the services of a directory.
    ///
    /// Everything the environment owns is rebuilt from the directory, which is
    /// what makes a reopened environment independent of the process that built
    /// the tree: the secret key is read from `<root>/secret.key` rather than
    /// generated, so instance configurations sealed by the previous process can
    /// still be opened.
    ///
    /// # Parameters
    ///
    /// - `root`: the directory the database and the storage live in.
    /// - `env`: extra settings environment variables.
    async fn open(root: &Path, env: &[(&str, &str)]) -> Env {
        let database = Database::open(&root.join("hearthsound.db")).await.unwrap();
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
                root.join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let secrets = SecretKey::load(&root.join("secret.key"), &BTreeMap::new())
            .unwrap()
            .secret_box();
        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&host),
            repository.clone(),
            secrets.clone(),
        ));

        let events = Arc::new(EventBus::new());
        let library = LibraryRepository::new(&database);
        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            root.join("artwork"),
        ));
        let downloads = root.join("downloads");
        let staging = root.join("storage/staging");
        let library_root = root.join("storage/library");
        let cache_root = root.join("storage/cache");
        let orchestrator = Arc::new(AcquisitionOrchestrator::new(
            Arc::clone(&database),
            settings.clone(),
            Arc::clone(&registry),
            Arc::clone(&catalog),
            Arc::new(FfmpegRunner::new(None, 8)),
            Arc::clone(&events),
            AcquisitionStorage::new(
                staging.clone(),
                StorageRoots::new(library_root.clone(), cache_root.clone()),
            ),
        ));

        Env {
            dir: None,
            root: root.to_path_buf(),
            database,
            settings,
            host,
            registry,
            catalog,
            repository,
            secrets,
            events,
            library,
            downloads,
            staging,
            library_root,
            cache_root,
            orchestrator,
        }
    }

    /// Builds an environment whose instance finds and downloads one lossless
    /// candidate.
    ///
    /// The generated audio file the simulated backend "downloads" is already in
    /// the instance's import root, so an acquisition admitted here runs the
    /// whole pipeline to `verifying`.
    ///
    /// # Parameters
    ///
    /// - `env`: extra settings environment variables.
    pub async fn normal(env: &[(&str, &str)]) -> Env {
        let env = Env::new(
            config(one_candidate(), "normal", 0, "downloads", ".flac"),
            InstanceLimits::default(),
            env,
        )
        .await;
        env.place_download("01 - Hello Song.flac").await;
        env
    }

    /// Writes a generated audio file into the instance's import root.
    ///
    /// The fixture reports every file with the configured suffix it finds there
    /// when a download completes, which is how the simulated backend produces
    /// its output.
    ///
    /// # Parameters
    ///
    /// - `name`: the file name to write.
    ///
    /// # Returns
    ///
    /// The path of the written file.
    pub async fn place_download(&self, name: &str) -> PathBuf {
        AudioFixture::new(AudioFormat::Flac)
            .seconds(0.2)
            .file_name(name.to_owned())
            .write_into(&self.downloads)
            .await
            .unwrap()
    }

    /// Adds a second instance to the acquisition layer, after the first.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    /// - `server_config`: the fixture configuration of the new instance.
    ///
    /// # Returns
    ///
    /// The instance id, which is the key its key-value rows use.
    pub async fn add_instance(&self, slug: &str, server_config: Value) -> Uuid {
        let service = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        let view = service
            .create(&InstanceSpec {
                slug: slug.to_owned(),
                plugin_id: PLUGIN_ID.to_owned(),
                display_name: format!("{slug} display"),
                server_config,
                host_config: host_config(&self.downloads, InstanceLimits::default()),
            })
            .await
            .unwrap();
        service
            .set_layer(Layer::Acquisition, &[INSTANCE.to_owned(), slug.to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
        view.id
    }

    /// Installs the metadata fixture and makes it the metadata layer.
    ///
    /// # Parameters
    ///
    /// - `server_config`: the fixture configuration, which lists the recordings
    ///   it knows.
    ///
    /// # Returns
    ///
    /// The instance id.
    pub async fn add_metadata_instance(&self, server_config: Value) -> Uuid {
        let installed = self
            .host
            .install(&fixture_plugin(METADATA_FIXTURE))
            .await
            .unwrap();
        self.repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap();
        let service = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        let view = service
            .create(&InstanceSpec {
                slug: "meta".to_owned(),
                plugin_id: METADATA_PLUGIN_ID.to_owned(),
                display_name: "Metadata fixture".to_owned(),
                server_config,
                host_config: host_config(&self.downloads, InstanceLimits::default()),
            })
            .await
            .unwrap();
        service
            .set_layer(Layer::Metadata, &["meta".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
        view.id
    }

    /// Inserts an acquisition that stopped with its download finished.
    ///
    /// The row is written the way a process that stopped during verification or
    /// import leaves it: inserted `queued` and then walked to `state` through the
    /// legal transitions of the state machine, with the instance, the candidate
    /// and the download token the download stage recorded. The candidate JSON
    /// comes from a real completed acquisition, so it carries the files that
    /// download resolved.
    ///
    /// # Parameters
    ///
    /// - `track`: the track the acquisition targets.
    /// - `state`: the stage the process stopped in, `verifying` or `importing`.
    /// - `candidate`: the `candidate` column of a completed acquisition.
    ///
    /// # Returns
    ///
    /// The new acquisition's id.
    ///
    /// # Panics
    ///
    /// Panics when `state` is not one of the two stages a download can stop in.
    pub async fn stopped_after_download(
        &self,
        track: EntityId,
        state: AcquisitionState,
        candidate: &str,
    ) -> Uuid {
        assert!(
            matches!(
                state,
                AcquisitionState::Verifying | AcquisitionState::Importing
            ),
            "only the stages after a download can be stopped in"
        );
        let instance_id = self.registry.instance(INSTANCE).unwrap().id();
        let id = Uuid::now_v7();
        let mut tx = self.database.writer().begin().await.unwrap();
        repo::insert(
            &mut tx,
            &NewAcquisition {
                id,
                track_id: Some(track),
                album_id: None,
                granularity: Granularity::Track,
                priority: Priority::Play,
                reason: "stopped before the import".to_owned(),
                requested_by: None,
            },
        )
        .await
        .unwrap();
        tracks::set_track_availability(&mut tx, track, Availability::Acquiring, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let repository = self.orchestrator.repository();
        assert!(repository.claim(id).await.unwrap());
        for step in [
            AcquisitionState::Searching,
            AcquisitionState::Downloading,
            AcquisitionState::Verifying,
            AcquisitionState::Importing,
        ] {
            repository
                .transition(id, step, Some("stopped before the import"))
                .await
                .unwrap();
            if step == state {
                break;
            }
        }
        repository
            .set_download(id, Some(instance_id), Some(candidate), Some("download-1"))
            .await
            .unwrap();
        id
    }

    /// Starts the scheduler.
    pub fn start(&self) {
        self.orchestrator.start();
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
        let mut album_input = AlbumInput::new(format!("{title} album"), Origin::Catalog);
        album_input.year = Some(1993);
        album_input.display_artist = Some(artist.to_owned());
        let album = self
            .library
            .upsert_album(&album_input, &[ArtistCredit::new(artist, "main")])
            .await
            .unwrap();
        let mut input = TrackInput::new(title, Origin::Catalog, availability);
        input.album_id = Some(album);
        input.display_artist = Some(artist.to_owned());
        input.disc_number = Some(1);
        input.track_number = Some(1);
        input.duration_ms = Some(200);
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

    /// Inserts a user row and returns its id.
    ///
    /// The ledger's `requested_by` column is a foreign key, so a test that
    /// requests an acquisition on a user's behalf needs the row to exist. The
    /// row holds the bare minimum the table requires; nothing here checks a
    /// password, because no test authenticates one.
    ///
    /// # Returns
    ///
    /// The new user's id.
    pub async fn add_user(&self) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
             VALUES (?, ?, ?, 'not-a-hash', 0, 0)",
        )
        .bind(id.as_bytes().as_slice())
        .bind(format!("user-{id}"))
        .bind("Test User")
        .execute(self.database.writer())
        .await
        .unwrap();
        id
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

    /// Counts how many searches the instance with `slug` has started.
    ///
    /// The fixture counts its own searches through the host's key-value store,
    /// which makes the searches a test can observe exactly the searches the
    /// plugin was asked for.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    pub async fn searches_started(&self, slug: &str) -> u64 {
        let instance = self.registry.instance(slug).unwrap();
        let instance_id = instance.id().as_bytes().to_vec();
        let stored: Option<Vec<u8>> = sqlx::query_scalar!(
            "SELECT value FROM plugin_kv WHERE instance_id = ? AND key = 'counter:search'",
            instance_id,
        )
        .fetch_optional(self.database.readers())
        .await
        .unwrap();
        stored
            .map(|value| serde_json::from_slice::<u64>(&value).unwrap())
            .unwrap_or(0)
    }

    /// Counts how many downloads the instance with `slug` has started.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    pub async fn downloads_started(&self, slug: &str) -> u64 {
        let instance = self.registry.instance(slug).unwrap();
        let instance_id = instance.id().as_bytes().to_vec();
        let stored: Option<Vec<u8>> = sqlx::query_scalar!(
            "SELECT value FROM plugin_kv WHERE instance_id = ? AND key = 'counter:download'",
            instance_id,
        )
        .fetch_optional(self.database.readers())
        .await
        .unwrap();
        stored
            .map(|value| serde_json::from_slice::<u64>(&value).unwrap())
            .unwrap_or(0)
    }

    /// Waits until an acquisition reaches a state.
    ///
    /// # Panics
    ///
    /// Panics when the state is not reached within ten seconds, so a stuck
    /// pipeline fails the test instead of hanging it.
    pub async fn wait_for_state(&self, id: Uuid, state: AcquisitionState) {
        for _ in 0..1000 {
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
        for _ in 0..3000 {
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

/// The host configuration of the fixture instance.
///
/// # Parameters
///
/// - `downloads`: the directory granted as the instance's `downloads` import
///   root, in `move` mode.
/// - `limits`: the instance's concurrency and rate limits.
pub fn host_config(downloads: &Path, limits: InstanceLimits) -> Value {
    host_config_with(downloads, limits, "move", false)
}

/// The host configuration of the fixture instance with a chosen import mode.
///
/// # Parameters
///
/// - `downloads`: the directory granted as the instance's `downloads` import
///   root.
/// - `limits`: the instance's concurrency and rate limits.
/// - `mode`: the import root's post-import mode, `move`, `copy` or `link`.
/// - `read_only`: whether the import root is read-only.
pub fn host_config_with(
    downloads: &Path,
    limits: InstanceLimits,
    mode: &str,
    read_only: bool,
) -> Value {
    json!({
        "importRoots": [
            {
                "id": "downloads",
                "path": downloads.to_string_lossy(),
                "mode": mode,
                "readOnly": read_only,
            }
        ],
        "maxConcurrent": limits.max_concurrent,
        "maxConcurrentSearches": limits.max_concurrent_searches,
        "searchesPerHour": limits.searches_per_hour,
    })
}

/// The fixture configuration of a simulated backend.
///
/// # Parameters
///
/// - `candidates`: the candidates a search returns.
/// - `scenario`: `normal`, `no-candidates`, `download-failure` or `stalled`.
/// - `search_polls`: how many polls answer `running` before a search completes.
/// - `output_root`: the root a completed download lists, `staging` or the id of
///   an import root the instance was granted.
/// - `match_suffix`: the suffix an output file must have to be reported.
pub fn config(
    candidates: Value,
    scenario: &str,
    search_polls: u32,
    output_root: &str,
    match_suffix: &str,
) -> Value {
    json!({
        "candidates": candidates,
        "scenario": scenario,
        "searchPolls": search_polls,
        "downloadQueuedPolls": 0,
        "downloadRunningPolls": 1,
        "downloadBytesPerPoll": 4000,
        "outputRoot": output_root,
        "matchSuffix": match_suffix,
    })
}

/// The list holding one candidate that matches the harness's tracks.
pub fn one_candidate() -> Value {
    json!([candidate("cand-1", "flac", Some(900))])
}

/// One fixture candidate that matches the tracks [`Env::add_track`] inserts.
///
/// The path carries the artist, the album and the track number, so the core's
/// scoring recovers all three from it the way it would from a real backend's
/// path.
///
/// # Parameters
///
/// - `token`: the token that addresses the candidate in a download.
/// - `format`: the container format of the file.
/// - `bitrate_kbps`: the file's bitrate, which decides the tier of a lossy file.
pub fn candidate(token: &str, format: &str, bitrate_kbps: Option<u32>) -> Value {
    json!({
        "token": token,
        "title": "Hello Song",
        "path": "Hello Artist/Hello Song album/01 - Hello Song.flac",
        "files": [
            {
                "name": "01 - Hello Song.flac",
                "size": 4000,
                "format": format,
                "bitrateKbps": bitrate_kbps,
                "sampleRate": 44100,
                "bitDepth": 16,
                "durationMs": 200,
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
