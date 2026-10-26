//! Helpers shared by the integration tests.
//!
//! Every test builds a temporary database with the server schema, one account
//! with an API key and an app password, and the shared state; the request
//! helpers send requests through the real router.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response as HttpResponse;
use hearthsound_acquire::orchestrator::{AcquisitionOrchestrator, AcquisitionStorage};
use hearthsound_acquire::prefetch::PrefetchScheduler;
use hearthsound_activity::ActivityService;
use hearthsound_api_subsonic::{PendingSource, SubsonicState};
use hearthsound_auth::{ApiKeys, AppPasswords, Authenticator, NewUser, Users};
use hearthsound_catalog::{CatalogService, DiscoveryService, LinkedUserContexts};
use hearthsound_config::secret::SecretBox;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::events::EventBus;
use hearthsound_core::{Availability, EntityId, IdPrefix, Layer};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{JobQueue, QueueOptions};
use hearthsound_library::scan::LibraryScanner;
use hearthsound_library::thumbnails::ThumbnailCache;
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, Origin, ScannedMediaFile, StorageRoots, TrackInput,
};
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
    UserConfigService,
};
use hearthsound_test_support::{
    ALBUM_CATALOG, ARTWORK_FIXTURE, ARTWORK_NOSTORE_FIXTURE, DISCOGRAPHY_CATALOG, HELLO_CATALOG,
    METADATA_FIXTURE, SCROBBLE_FIXTURE, fixture_plugin,
};
use tempfile::TempDir;
use tower::ServiceExt;

/// The bytes the artwork layer's mock server serves.
pub const ARTWORK_IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nartwork layer image";

/// The base URL the portrait fixture describes its images from.
///
/// The artist-information endpoint returns the URL without fetching it, so no
/// server has to answer it.
pub const PORTRAIT_BASE: &str = "http://images.example";

/// The login name of the test account.
pub const USERNAME: &str = "echo";

/// The account password of the test account.
pub const ACCOUNT_PASSWORD: &str = "hunter22";

/// The filters that make a seeded response stable across runs.
///
/// A seeded row's `created` timestamp is the moment the test inserted it, so it
/// differs from run to run; the filter replaces it with a fixed marker.
/// Everything else in a snapshot is exactly what the endpoint computed, because
/// the seeded client ids are replaced by name through
/// [`seed::Seeded::stabilize`].
pub const SNAPSHOT_FILTERS: [(&str, &str); 1] = [(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})",
    "TIMESTAMP",
)];

pub mod artwork;
pub mod seed;

/// A migrated temporary database with one account and its credentials.
pub struct Fixture {
    /// Keeps the temporary directory alive.
    _dir: TempDir,
    /// The open database.
    pub database: Arc<Database>,
    /// The settings service, which a test changes to arrange its case.
    pub settings: Settings,
    /// The test account's client id, `us_<...>`.
    pub user_id: EntityId,
    /// An API key of the test account.
    pub api_key: String,
    /// An app password of the test account.
    pub app_password: String,
    /// The shared state the router is built with.
    pub state: SubsonicState,
    /// The catalog layer the state and the acquisition layer share.
    pub catalog: Arc<CatalogService>,
    /// The discovery layer the suggestion endpoints ask.
    pub discovery: Arc<DiscoveryService>,
    /// The event bus the acquisition layer publishes on.
    pub events: Arc<EventBus>,
    /// The managed storage roots the state resolves stored files against.
    pub roots: StorageRoots,
    /// The ffmpeg runner the state transcodes and verifies through.
    ffmpeg: Arc<FfmpegRunner>,
    /// The artwork cache directory catalog cover art is written to.
    artwork: PathBuf,
    /// The scaled local cover cache `/rest/getCoverArt` serves sizes from.
    thumbnails: Arc<ThumbnailCache>,
    /// The plugin host a fixture plugin is installed into.
    host: Arc<PluginHost>,
    /// The plugin tables an instance is configured in.
    repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    secrets: SecretBox,
    /// The stored per-user provider links.
    user_configs: UserConfigService,
    /// The provider registry the catalog layer resolves instances through.
    registry: Arc<ProviderRegistry>,
    /// The library scanner the scan endpoints report.
    pub scanner: Arc<LibraryScanner>,
    /// The job queue a scan is enqueued on.
    pub jobs: Arc<JobQueue>,
}

impl Fixture {
    /// Builds the fixture without an acquisition layer.
    pub async fn new() -> Fixture {
        Fixture::build(None).await
    }

    /// Builds a fixture whose stream service answers pending tracks with the
    /// given acquisition source.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer behind `/rest/stream` for a track the
    ///   server has not stored.
    pub async fn with_pending(pending: Arc<dyn PendingSource>) -> Fixture {
        Fixture::build(Some(pending)).await
    }

    /// Builds the fixture.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer the shared state is built with, or
    ///   `None` for the `404` behavior of a process without one.
    async fn build(pending: Option<Arc<dyn PendingSource>>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let database = Arc::new(
            Database::open(&dir.path().join("hearthsound.db"))
                .await
                .unwrap(),
        );
        MIGRATOR.run(database.writer()).await.unwrap();
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
        )
        .await
        .unwrap();

        let user = Users::new(&database)
            .create(
                &settings.current(),
                &NewUser::new(USERNAME, "Echo", ACCOUNT_PASSWORD, true),
            )
            .await
            .unwrap();
        let secret_box = hearthsound_config::secret::SecretKey::generate().secret_box();
        let api_key = ApiKeys::new(&database)
            .generate(user.id, "test")
            .await
            .unwrap()
            .plaintext;
        let app_password = AppPasswords::new(&database, secret_box.clone())
            .generate(user.id, "test")
            .await
            .unwrap()
            .plaintext;
        let authenticator = Arc::new(Authenticator::new(&database, secret_box.clone(), 10));

        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&host),
            repository.clone(),
            secret_box.clone(),
        ));
        let artwork = dir.path().join("artwork");
        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            artwork.clone(),
        ));
        let user_configs = UserConfigService::new(&database, secret_box.clone());
        let discovery = Arc::new(DiscoveryService::new(
            Arc::clone(&registry),
            Arc::clone(&catalog),
            Arc::new(LinkedUserContexts::new(user_configs.clone())),
        ));
        let activity = Arc::new(ActivityService::new(&database));
        let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));
        std::fs::create_dir_all(roots.library.as_path()).unwrap();
        std::fs::create_dir_all(roots.cache.as_path()).unwrap();
        std::fs::create_dir_all(dir.path().join("artwork")).unwrap();
        let ffmpeg = Arc::new(FfmpegRunner::new(None, 2));
        let events = Arc::new(EventBus::new());
        let state = SubsonicState::new(
            Arc::clone(&database),
            hearthsound_auth::Sessions::new(&database),
            settings.clone(),
            authenticator,
            Arc::clone(&catalog),
            activity,
            Arc::clone(&ffmpeg),
            roots.clone(),
        )
        // The registry the catalog layer resolves instances through, so the
        // vendor endpoints report the layers this fixture enabled.
        .with_registry(Arc::clone(&registry))
        .with_discovery(Arc::clone(&discovery));
        let state = match pending {
            Some(pending) => state.with_pending_source(pending),
            None => state,
        };
        // The scanner the scan endpoints report and the queue they enqueue on.
        let scanner = Arc::new(LibraryScanner::new(
            &database,
            Arc::clone(&events),
            settings.clone(),
        ));
        let jobs = Arc::new(JobQueue::new(&database, QueueOptions::default()));
        let state = state.with_scanner(Arc::clone(&scanner), Arc::clone(&jobs));
        // Local covers are scaled through the same runner, over the thumbs
        // directory inside the artwork directory, exactly as the server wires
        // them.
        let thumbnails = Arc::new(ThumbnailCache::new(
            artwork.join("thumbs"),
            Arc::clone(&ffmpeg),
        ));
        let state = state.with_thumbnails(Arc::clone(&thumbnails));

        Fixture {
            _dir: dir,
            database,
            settings,
            user_id: EntityId::from_uuid(IdPrefix::User, user.id),
            api_key,
            app_password,
            state,
            catalog,
            discovery,
            events,
            roots,
            ffmpeg,
            artwork,
            thumbnails,
            host,
            repository,
            secrets: secret_box,
            user_configs,
            registry,
            scanner,
            jobs,
        }
    }

    /// Installs the `hello-catalog` fixture and assigns it to the catalog layer.
    ///
    /// The instance answers every non-empty search with one greeting track,
    /// album, and artist, so a search response's catalog side is exactly those
    /// three entities under the `hello` slug.
    pub async fn install_hello_catalog(&self) {
        self.install_hello_catalog_with(serde_json::json!({ "greeting": "Hello" }))
            .await;
    }

    /// Installs the `hello-catalog` fixture with the server configuration a test
    /// writes, and assigns it to the catalog layer.
    ///
    /// # Parameters
    ///
    /// - `config`: the instance's server configuration, for example a `coverUrl`
    ///   or an `albumCount`.
    pub async fn install_hello_catalog_with(&self, config: serde_json::Value) {
        let installed = self
            .host
            .install(&fixture_plugin(HELLO_CATALOG))
            .await
            .unwrap();
        let plugin_id = self
            .repository
            .register(&installed, PluginSource::Sideload)
            .await
            .unwrap()
            .id;
        let instances = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        instances
            .create(&InstanceSpec {
                slug: "hello".to_owned(),
                plugin_id,
                display_name: "Hello catalog".to_owned(),
                server_config: config,
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        instances
            .set_layer(Layer::Catalog, &["hello".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the album catalog fixture as the catalog layer.
    ///
    /// # Parameters
    ///
    /// - `config`: the server configuration, whose `searchResults` table makes
    ///   a query answer another track of the fixture's album.
    pub async fn install_album_catalog(&self, config: serde_json::Value) {
        let plugin_id = self
            .install(ALBUM_CATALOG, "com.example.album-catalog")
            .await;
        self.instance_service()
            .create(&InstanceSpec {
                slug: "album".to_owned(),
                plugin_id,
                display_name: "Album catalog".to_owned(),
                server_config: config,
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        self.instance_service()
            .set_layer(Layer::Catalog, &["album".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the discography catalog fixture as the catalog layer.
    ///
    /// The instance's artist released three albums, one of them a remaster whose
    /// title and track titles carry an edition suffix. The provider ids are
    /// stable, so a test can build an `x_disco_a_...` id from them.
    pub async fn install_discography_catalog(&self) {
        let plugin_id = self
            .install(DISCOGRAPHY_CATALOG, "com.example.discography-catalog")
            .await;
        let service = self.instance_service();
        service
            .create(&InstanceSpec {
                slug: "disco".to_owned(),
                plugin_id,
                display_name: "Discography catalog".to_owned(),
                server_config: serde_json::json!({}),
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        service
            .set_layer(Layer::Catalog, &["disco".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the scrobble fixture as the discovery layer.
    ///
    /// # Parameters
    ///
    /// - `instances`: one `(slug, server config)` per instance, in layer order.
    ///   Each config carries the fixture's discovery tables, so a test states
    ///   the exact suggestions the layer answers.
    pub async fn install_discovery_layer(&self, instances: &[(&str, serde_json::Value)]) {
        let plugin_id = self
            .install(SCROBBLE_FIXTURE, "com.example.scrobble-fixture")
            .await;
        let service = self.instance_service();
        let mut slugs = Vec::with_capacity(instances.len());
        for (slug, config) in instances {
            service
                .create(&InstanceSpec {
                    slug: (*slug).to_owned(),
                    plugin_id: plugin_id.clone(),
                    display_name: format!("Discovery {slug}"),
                    server_config: discovery_config(config.clone()),
                    host_config: serde_json::Value::Null,
                })
                .await
                .unwrap();
            slugs.push((*slug).to_owned());
        }
        service.set_layer(Layer::Discovery, &slugs).await.unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Reconfigures one discovery instance and rebuilds the registry.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance to reconfigure.
    /// - `config`: the new server configuration.
    pub async fn update_discovery_config(&self, slug: &str, config: serde_json::Value) {
        self.instance_service()
            .update(
                slug,
                &InstanceSpec {
                    slug: slug.to_owned(),
                    plugin_id: "com.example.scrobble-fixture".to_owned(),
                    display_name: format!("Discovery {slug}"),
                    server_config: discovery_config(config),
                    host_config: serde_json::Value::Null,
                },
            )
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the metadata fixture once per configuration and assigns the
    /// instances to the metadata layer in that order.
    ///
    /// # Parameters
    ///
    /// - `configs`: one server configuration per instance; the first is asked
    ///   first.
    pub async fn install_metadata_layer(&self, configs: &[serde_json::Value]) {
        let plugin_id = self
            .install(METADATA_FIXTURE, "com.example.metadata-fixture")
            .await;
        let service = self.instance_service();
        let mut slugs = Vec::with_capacity(configs.len());
        for (index, config) in configs.iter().enumerate() {
            let slug = format!("metadata{}", index + 1);
            service
                .create(&InstanceSpec {
                    slug: slug.clone(),
                    plugin_id: plugin_id.clone(),
                    display_name: format!("Metadata {}", index + 1),
                    server_config: config.clone(),
                    host_config: serde_json::Value::Null,
                })
                .await
                .unwrap();
            slugs.push(slug);
        }
        service.set_layer(Layer::Metadata, &slugs).await.unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the artwork fixture with portrait renditions for some artists.
    ///
    /// The URLs are not fetched: the artist-information endpoint describes them
    /// to the client, so the fixture only has to name them.
    ///
    /// # Parameters
    ///
    /// - `artists`: the artist names the fixture describes a portrait for.
    pub async fn install_artist_portraits(&self, artists: &[&str]) {
        let plugin_id = self
            .install(ARTWORK_FIXTURE, "com.example.artwork-fixture")
            .await;
        self.instance_service()
            .create(&InstanceSpec {
                slug: "artwork".to_owned(),
                plugin_id,
                display_name: "Artwork".to_owned(),
                server_config: serde_json::json!({
                    "imageUrl": PORTRAIT_BASE,
                    "artists": artists
                        .iter()
                        .map(|name| serde_json::json!({
                            "title": name,
                            "images": [{ "path": "/portrait.jpg", "width": 500 }],
                        }))
                        .collect::<Vec<_>>(),
                }),
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        self.instance_service()
            .set_layer(Layer::Artwork, &["artwork".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs a fixture plugin, returning its plugin id.
    ///
    /// Installing a plugin that is already registered returns the registered
    /// id, so one fixture can hold several instances of one plugin.
    ///
    /// # Parameters
    ///
    /// - `archive`: the fixture archive file name.
    /// - `plugin_id`: the fixture's plugin id.
    async fn install(&self, archive: &str, plugin_id: &str) -> String {
        let installed = self.host.install(&fixture_plugin(archive)).await.unwrap();
        match self
            .repository
            .register(&installed, PluginSource::Sideload)
            .await
        {
            Ok(registration) => registration.id,
            Err(error) => panic!("registering {plugin_id} failed: {error}"),
        }
    }

    /// Returns the instance service over this fixture's host and tables.
    fn instance_service(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Stores one album and one track with a folder file the account owns.
    ///
    /// The track is visible in the account's personal scope, which is what the
    /// suggestion endpoints search to decide whether a suggestion is stored.
    ///
    /// # Parameters
    ///
    /// - `album`: the album title.
    /// - `artist`: the artist name.
    /// - `title`: the track title.
    /// - `duration_ms`: the track duration in milliseconds.
    ///
    /// # Returns
    ///
    /// The stored track, album and artist ids.
    pub async fn seed_track(
        &self,
        album: &str,
        artist: &str,
        title: &str,
        duration_ms: i64,
    ) -> (EntityId, EntityId, EntityId) {
        let library = LibraryRepository::new(&self.database);
        let folder = library
            .add_library_folder(&self.subdir("suggestions"), Some(self.user_id.uuid()))
            .await
            .unwrap();
        let album_id = library
            .upsert_album(
                &AlbumInput::new(album, Origin::Local),
                &[ArtistCredit::new(artist, "main")],
            )
            .await
            .unwrap();
        let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
        let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
        input.album_id = Some(album_id);
        input.display_artist = Some(artist.to_owned());
        input.track_number = Some(1);
        input.disc_number = Some(1);
        input.duration_ms = Some(duration_ms);
        let track_id = library
            .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
            .await
            .unwrap();
        library
            .upsert_folder_media_file(
                track_id,
                &ScannedMediaFile {
                    folder_id: folder.id,
                    rel_path: format!("{title}.mp3"),
                    size: 1_024,
                    mtime_ms: 0,
                    format: Some("mp3".to_owned()),
                    codec: Some("mp3".to_owned()),
                    bitrate_kbps: Some(320),
                    sample_rate: Some(44_100),
                    bit_depth: None,
                    channels: Some(2),
                    duration_ms: Some(duration_ms),
                    lossless: Some(false),
                    replaygain: None,
                    has_embedded_art: Some(false),
                },
            )
            .await
            .unwrap();
        (track_id, album_id, artist_id)
    }

    /// Stores an album and one track in a folder no account owns.
    ///
    /// The track is stored server-wide but outside every personal library,
    /// which is the row a search can return to an administrator and a
    /// non-administrator cannot see.
    ///
    /// # Parameters
    ///
    /// - `album`: the album title.
    /// - `artist`: the artist name.
    /// - `title`: the track title.
    /// - `duration_ms`: the track duration in milliseconds.
    ///
    /// # Returns
    ///
    /// The track, album and artist ids.
    pub async fn seed_server_track(
        &self,
        album: &str,
        artist: &str,
        title: &str,
        duration_ms: i64,
    ) -> (EntityId, EntityId, EntityId) {
        let library = LibraryRepository::new(&self.database);
        let folder = library
            .add_library_folder(&self.subdir("server"), None)
            .await
            .unwrap();
        let album_id = library
            .upsert_album(
                &AlbumInput::new(album, Origin::Local),
                &[ArtistCredit::new(artist, "main")],
            )
            .await
            .unwrap();
        let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
        let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
        input.album_id = Some(album_id);
        input.display_artist = Some(artist.to_owned());
        input.track_number = Some(1);
        input.disc_number = Some(1);
        input.duration_ms = Some(duration_ms);
        let track_id = library
            .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
            .await
            .unwrap();
        library
            .upsert_folder_media_file(
                track_id,
                &ScannedMediaFile {
                    folder_id: folder.id,
                    rel_path: format!("{title}.mp3"),
                    size: 1_024,
                    mtime_ms: 0,
                    format: Some("mp3".to_owned()),
                    codec: Some("mp3".to_owned()),
                    bitrate_kbps: Some(320),
                    sample_rate: Some(44_100),
                    bit_depth: None,
                    channels: Some(2),
                    duration_ms: Some(duration_ms),
                    lossless: Some(false),
                    replaygain: None,
                    has_embedded_art: Some(false),
                },
            )
            .await
            .unwrap();
        (track_id, album_id, artist_id)
    }

    /// Stores an album and a track in a folder no account owns, leaving the
    /// album itself uncredited.
    ///
    /// This is the shape a materialization used to leave behind when a
    /// provider named an album with no artists: the album was uncredited while
    /// its tracks credited the artist.
    ///
    /// # Parameters
    ///
    /// - `album`: the album title.
    /// - `artist`: the artist the track credits.
    /// - `title`: the track title.
    /// - `duration_ms`: the track duration in milliseconds.
    ///
    /// # Returns
    ///
    /// The track, album and artist ids.
    pub async fn seed_server_track_with_bare_album(
        &self,
        album: &str,
        artist: &str,
        title: &str,
        duration_ms: i64,
    ) -> (EntityId, EntityId, EntityId) {
        let library = LibraryRepository::new(&self.database);
        let folder = library
            .add_library_folder(&self.subdir("server"), None)
            .await
            .unwrap();
        let album_id = library
            .upsert_album(&AlbumInput::new(album, Origin::Local), &[])
            .await
            .unwrap();
        let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
        input.album_id = Some(album_id);
        input.display_artist = Some(artist.to_owned());
        input.track_number = Some(1);
        input.disc_number = Some(1);
        input.duration_ms = Some(duration_ms);
        let track_id = library
            .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
            .await
            .unwrap();
        let artist_id = library.list_track_credits(track_id).await.unwrap()[0].artist_id;
        library
            .upsert_folder_media_file(
                track_id,
                &ScannedMediaFile {
                    folder_id: folder.id,
                    rel_path: format!("{title}.mp3"),
                    size: 1_024,
                    mtime_ms: 0,
                    format: Some("mp3".to_owned()),
                    codec: Some("mp3".to_owned()),
                    bitrate_kbps: Some(320),
                    sample_rate: Some(44_100),
                    bit_depth: None,
                    channels: Some(2),
                    duration_ms: Some(duration_ms),
                    lossless: Some(false),
                    replaygain: None,
                    has_embedded_art: Some(false),
                },
            )
            .await
            .unwrap();
        (track_id, album_id, artist_id)
    }

    /// Installs the artwork fixture as the artwork layer and serves one image.
    ///
    /// The fixture answers a portrait for any artist whose name is `Hello
    /// Artist` and a cover for `Hello Album`, from the path the caller names,
    /// which a mock server serves.
    ///
    /// # Parameters
    ///
    /// - `path`: the image path the fixture configures.
    /// - `artists`: the artist names the fixture answers a portrait for.
    ///
    /// # Returns
    ///
    /// The bytes the mock server serves as the image.
    pub async fn install_artwork_layer(&self, path: &str, artists: &[&str]) -> Vec<u8> {
        let server = artwork_server(path, 200).await;
        let served = self.install_artwork_layer_at(&server, path, artists).await;
        // The server lives as long as the fixture keeps its own runtime alive;
        // leaking it here is what a test's mock server does in this harness.
        std::mem::forget(server);
        served
    }

    /// Installs the artwork fixture as the artwork layer, served by a mock
    /// server the caller owns.
    ///
    /// The caller owns the server so it can answer with a refusal and assert
    /// how often it was asked. The fixture plugin is installed only once per
    /// fixture: a second call updates nothing and fails.
    ///
    /// # Parameters
    ///
    /// - `server`: the mock server that answers the image path.
    /// - `path`: the image path the fixture configures.
    /// - `artists`: the artist names the fixture answers a portrait for.
    ///
    /// # Returns
    ///
    /// The bytes [`ARTWORK_IMAGE`] holds, which is what a 200 answer serves.
    pub async fn install_artwork_layer_at(
        &self,
        server: &wiremock::MockServer,
        path: &str,
        artists: &[&str],
    ) -> Vec<u8> {
        self.install_artwork_layer_from(ARTWORK_FIXTURE, server, path, artists)
            .await
    }

    /// Installs the artwork fixture whose manifest allows no caching as the
    /// artwork layer.
    ///
    /// The mock server that answers the image lives as long as the fixture's
    /// runtime, exactly as [`Fixture::install_artwork_layer`]'s does.
    ///
    /// # Parameters
    ///
    /// - `path`: the image path the fixture configures.
    /// - `artists`: the artist names the fixture answers a portrait for.
    ///
    /// # Returns
    ///
    /// The bytes [`ARTWORK_IMAGE`] holds, which is what a 200 answer serves.
    pub async fn install_no_store_artwork_layer(&self, path: &str, artists: &[&str]) -> Vec<u8> {
        let server = artwork_server(path, 200).await;
        let served = self
            .install_artwork_layer_from(ARTWORK_NOSTORE_FIXTURE, &server, path, artists)
            .await;
        std::mem::forget(server);
        served
    }

    /// Installs one artwork fixture archive as the artwork layer.
    ///
    /// # Parameters
    ///
    /// - `archive`: the fixture archive to install.
    /// - `server`: the mock server that answers the image path.
    /// - `path`: the image path the fixture configures.
    /// - `artists`: the artist names the fixture answers a portrait for.
    ///
    /// # Returns
    ///
    /// The bytes [`ARTWORK_IMAGE`] holds, which is what a 200 answer serves.
    async fn install_artwork_layer_from(
        &self,
        archive: &str,
        server: &wiremock::MockServer,
        path: &str,
        artists: &[&str],
    ) -> Vec<u8> {
        let installed = self.host.install(&fixture_plugin(archive)).await.unwrap();
        let plugin_id = self
            .repository
            .register(&installed, PluginSource::Sideload)
            .await
            .unwrap()
            .id;
        let instances = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        instances
            .create(&InstanceSpec {
                slug: "artwork".to_owned(),
                plugin_id,
                display_name: "Artwork".to_owned(),
                server_config: serde_json::json!({
                    "imageUrl": server.uri(),
                    "albums": [{
                        "title": "Hello Album",
                        "images": [{ "path": path, "width": 500 }],
                    }],
                    "artists": artists
                        .iter()
                        .map(|name| serde_json::json!({
                            "title": name,
                            "images": [{ "path": path, "width": 500 }],
                        }))
                        .collect::<Vec<_>>(),
                }),
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        instances
            .set_layer(Layer::Artwork, &["artwork".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
        ARTWORK_IMAGE.to_vec()
    }

    /// Returns the stored per-user provider links.
    pub fn user_configs(&self) -> &UserConfigService {
        &self.user_configs
    }

    /// Returns the id of one configured provider instance.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    ///
    /// # Panics
    ///
    /// Panics when no built instance carries the slug.
    /// Primes the catalog cache for one entity, as a background enrichment run
    /// does.
    ///
    /// A vendor page read answers from the library and the cached catalog data
    /// only, so a test that asserts the merged page primes the cache first and
    /// then reads. A cold read is asserted on its own, without this call.
    ///
    /// # Parameters
    ///
    /// - `id`: the stored or catalog id of the entity.
    pub async fn enrich(&self, id: &hearthsound_core::ClientId) {
        self.catalog.enrich(id).await.unwrap();
    }

    pub fn instance_id(&self, slug: &str) -> uuid::Uuid {
        self.registry
            .instance(slug)
            .unwrap_or_else(|| panic!("no instance is configured as {slug}"))
            .id()
    }

    /// Links the test account to one discovery instance.
    ///
    /// The recommendation and playlist queries only ask an instance the
    /// listener linked, so a test that exercises them links the instance here.
    ///
    /// # Parameters
    ///
    /// - `slug`: the discovery instance the account links.
    pub async fn link_discovery(&self, slug: &str) {
        let instance = self
            .registry
            .instance(slug)
            .unwrap_or_else(|| panic!("no instance is configured as {slug}"));
        self.user_configs
            .put(
                instance.id(),
                self.user_id.uuid(),
                Some("fixture-user".to_owned()),
                &serde_json::json!({ "token": "good" }),
            )
            .await
            .unwrap();
    }

    /// Returns the managed storage roots the state resolves files against.
    pub fn storage_roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Returns the ffmpeg runner the state transcodes and verifies through.
    ///
    /// A test reads its in-flight count to assert that a request started (or
    /// did not start) an ffmpeg process.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.ffmpeg
    }

    /// Returns the managed library root a `library` file resolves against.
    ///
    /// The directory exists from [`Fixture::new`], so a test can write a file
    /// into it and insert the matching row.
    pub fn library_root(&self) -> PathBuf {
        self.roots.library.clone()
    }

    /// Returns the artwork cache directory, which exists from
    /// [`Fixture::new`].
    pub fn artwork_dir(&self) -> PathBuf {
        self.artwork.clone()
    }

    /// Returns the directory scaled local covers are written to.
    pub fn thumbs_dir(&self) -> PathBuf {
        self.artwork.join("thumbs")
    }

    /// Returns the `/rest` router under test.
    pub fn app(&self) -> Router {
        hearthsound_api_subsonic::router(self.state.clone())
    }

    /// Returns the `/rest` router with a different acquisition layer.
    ///
    /// A test that needs the acquisition layer to write to the fixture's own
    /// database builds the source after the fixture and installs it here.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer behind `/rest/stream`.
    pub fn app_with(&self, pending: Arc<dyn PendingSource>) -> Router {
        hearthsound_api_subsonic::router(self.state.clone().with_pending_source(pending))
    }

    /// Creates a directory inside the fixture's temporary directory.
    ///
    /// A test that seeds library folders needs a path that exists and is a
    /// directory, because adding a folder canonicalizes it.
    ///
    /// # Parameters
    ///
    /// - `name`: the directory's name.
    ///
    /// # Returns
    ///
    /// The absolute path of the created directory.
    pub fn subdir(&self, name: &str) -> PathBuf {
        let path = self._dir.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Creates a second account with the given permissions and returns its id
    /// and an API key for it.
    ///
    /// The permissions are written to `users.default_permissions` first, which
    /// is where a new account's permission flags come from.
    ///
    /// # Parameters
    ///
    /// - `permissions`: the permission names, for example `acquire`.
    ///
    /// # Returns
    ///
    /// The account's id and a freshly generated API key.
    pub async fn account(&self, permissions: &[&str]) -> (EntityId, String) {
        self.settings
            .set(
                keys::USERS_DEFAULT_PERMISSIONS.name(),
                SettingValue::StringList(permissions.iter().map(|name| name.to_string()).collect()),
                None,
            )
            .await
            .unwrap();
        let user = Users::new(&self.database)
            .create(
                &self.settings.current(),
                &NewUser::new("limited", "Limited", ACCOUNT_PASSWORD, false),
            )
            .await
            .unwrap();
        let key = ApiKeys::new(&self.database)
            .generate(user.id, "limited")
            .await
            .unwrap()
            .plaintext;
        (EntityId::from_uuid(IdPrefix::User, user.id), key)
    }

    /// Creates an administrator account with every permission.
    ///
    /// # Returns
    ///
    /// The account's id and a freshly generated API key.
    pub async fn admin_account(&self) -> (EntityId, String) {
        let user = Users::new(&self.database)
            .create(
                &self.settings.current(),
                &NewUser::new("root", "Root", ACCOUNT_PASSWORD, true),
            )
            .await
            .unwrap();
        let key = ApiKeys::new(&self.database)
            .generate(user.id, "root")
            .await
            .unwrap()
            .plaintext;
        (EntityId::from_uuid(IdPrefix::User, user.id), key)
    }

    /// Creates a second account with the given permissions and returns an API
    /// key for it.
    ///
    /// # Parameters
    ///
    /// - `permissions`: the permission names, for example `acquire`.
    pub async fn account_with(&self, permissions: &[&str]) -> String {
        self.account(permissions).await.1
    }

    /// Adds the acquisition layer and the prefetch scheduler over the fixture's
    /// own database.
    ///
    /// The layer is not started, so an acquisition a test asks for stays in the
    /// state its admission wrote until the test walks it on, which is what makes
    /// a snapshot of one stable.
    ///
    /// # Returns
    ///
    /// The orchestrator the state now answers from.
    pub async fn add_acquisition_layer(&mut self) -> Arc<AcquisitionOrchestrator> {
        let staging = self._dir.path().join("storage/staging");
        std::fs::create_dir_all(&staging).unwrap();
        let orchestrator = Arc::new(AcquisitionOrchestrator::new(
            Arc::clone(&self.database),
            self.settings.clone(),
            Arc::clone(&self.registry),
            Arc::clone(&self.catalog),
            Arc::clone(&self.ffmpeg),
            Arc::clone(&self.events),
            AcquisitionStorage::new(staging, self.roots.clone()),
        ));
        let prefetch = Arc::new(PrefetchScheduler::new(Arc::clone(&orchestrator)));
        self.state = self
            .state
            .clone()
            .with_acquisitions(Arc::clone(&orchestrator))
            .with_prefetch(prefetch)
            .with_registry(Arc::clone(&self.registry));
        orchestrator
    }

    /// Inserts a catalog-only album with the given number of tracks.
    ///
    /// The tracks carry no file, so each one's availability is `remote`: they
    /// are what a client asks the acquisition layer for.
    ///
    /// # Parameters
    ///
    /// - `title`: the album title; its tracks are titled `<title> <number>`.
    /// - `tracks`: how many tracks to insert.
    ///
    /// # Returns
    ///
    /// The album's id and its track ids, in playing order.
    pub async fn remote_album(&self, title: &str, tracks: usize) -> (EntityId, Vec<EntityId>) {
        let library = LibraryRepository::new(&self.database);
        let artist = "The Faraway";
        let mut input = AlbumInput::new(title, Origin::Catalog);
        input.display_artist = Some(artist.to_owned());
        let album = library
            .upsert_album(&input, &[ArtistCredit::new(artist, "main")])
            .await
            .unwrap();
        let mut ids = Vec::with_capacity(tracks);
        for number in 1..=tracks {
            let mut track = TrackInput::new(
                format!("{title} {number}"),
                Origin::Catalog,
                Availability::Remote,
            );
            track.album_id = Some(album);
            track.display_artist = Some(artist.to_owned());
            track.track_number = Some(i64::try_from(number).unwrap());
            track.duration_ms = Some(200_000);
            ids.push(
                library
                    .insert_track(&track, &[ArtistCredit::new(artist, "main")], &[])
                    .await
                    .unwrap(),
            );
        }
        (album, ids)
    }
}

/// One response, read into memory.
pub struct Probe {
    /// The HTTP status.
    pub status: StatusCode,
    /// The response's head.
    pub headers: HeaderMap,
    /// The response's body, decoded as UTF-8 with invalid sequences replaced.
    ///
    /// An envelope is always UTF-8; a binary body is read through
    /// [`Probe::bytes`] instead, because decoding it as text would lose bytes.
    pub body: String,
    /// The response's body, exactly as it was written.
    pub bytes: Vec<u8>,
}

impl Probe {
    /// Reads a response into a probe.
    async fn read(response: HttpResponse) -> Probe {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Probe {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    /// Returns the response's content type.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
    }
}

/// Adds the mock endpoint the scrobble fixture requires when a test config
/// omits it.
///
/// The discovery queries never use the endpoint, but the fixture's schema
/// requires it, so a suggestion-only instance still has to name one.
///
/// # Parameters
///
/// - `config`: the server configuration a test wrote.
///
/// # Returns
///
/// The configuration with an `endpoint` member.
fn discovery_config(mut config: serde_json::Value) -> serde_json::Value {
    if let Some(object) = config.as_object_mut() {
        object
            .entry("endpoint")
            .or_insert_with(|| serde_json::Value::String("http://127.0.0.1:9".to_owned()));
    }
    config
}

/// Sends a GET request through a router.
pub async fn get(app: &Router, uri: &str) -> Probe {
    request(app, Method::GET, uri, &[]).await
}

/// Starts a mock server that answers one image path with a status.
///
/// # Parameters
///
/// - `path`: the image path the server answers.
/// - `status`: the HTTP status it answers with; 200 carries
///   [`ARTWORK_IMAGE`] and any other status carries no body.
///
/// # Returns
///
/// The running server, with that one mock mounted.
pub async fn artwork_server(path: &str, status: u16) -> wiremock::MockServer {
    let server = wiremock::MockServer::start().await;
    let body: &[u8] = if status == 200 { ARTWORK_IMAGE } else { &[] };
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path(path.to_owned()))
        .respond_with(
            wiremock::ResponseTemplate::new(status)
                .insert_header("content-type", "image/png")
                .set_body_bytes(body.to_vec()),
        )
        .mount(&server)
        .await;
    server
}

/// Sends a request with explicit method and headers through a router.
///
/// # Parameters
///
/// - `app`: the router under test.
/// - `method`: the request method, `GET` or `HEAD`.
/// - `uri`: the request target, including its query string.
/// - `headers`: the request headers, for example `Range` or `If-None-Match`.
pub async fn request(app: &Router, method: Method, uri: &str, headers: &[(&str, &str)]) -> Probe {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).unwrap();
    Probe::read(app.clone().oneshot(request).await.unwrap()).await
}

/// Sends a POST request with an `application/x-www-form-urlencoded` body.
pub async fn post_form(app: &Router, uri: &str, form: &str) -> Probe {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    Probe::read(app.clone().oneshot(request).await.unwrap()).await
}

/// Asserts that a response is a failed envelope carrying an error code.
///
/// The code is looked for in the JSON member and in the XML attribute, so a
/// test can assert either encoding.
pub fn assert_error(probe: &Probe, code: i32) {
    assert_eq!(probe.status, StatusCode::OK, "body: {}", probe.body);
    assert!(
        probe.body.contains(&format!("\"code\":{code}"))
            || probe.body.contains(&format!("code=\"{code}\"")),
        "expected error {code} in: {}",
        probe.body
    );
}

/// Asserts that a response is a successful envelope.
pub fn assert_ok(probe: &Probe) {
    assert_eq!(probe.status, StatusCode::OK, "body: {}", probe.body);
    assert!(
        probe.body.contains("\"status\":\"ok\"") || probe.body.contains("status=\"ok\""),
        "expected a successful envelope in: {}",
        probe.body
    );
}
