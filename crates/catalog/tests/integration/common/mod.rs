//! The test harness the catalog tests share.
//!
//! Every test builds a real plugin host, provider registry, instance service,
//! settings service, event bus and database in a temporary directory, installs
//! fixture plugins, and drives the entity layer the way the server does. Nothing
//! here reaches the network: the fixture plugins answer in process, and the
//! tests that need an outbound call point a fixture at a local wiremock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::CatalogService;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry};
use hearthsound_core::events::EventBus;
use hearthsound_core::providers::ExternalId;
use hearthsound_core::{EntityId, EntityKind, Layer};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::LibraryRepository;
use hearthsound_library::external_ids;
use hearthsound_library::index_queue;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{
    InstallError, InstanceService, InstanceSpec, InstanceView, PluginHost, PluginRepository,
    PluginSource, ProviderRegistry,
};
use hearthsound_test_support::fixture_plugin;
use serde_json::{Value, json};

/// The plugin id of the HTTP catalog fixture.
pub const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The plugin id of the discography catalog fixture.
pub const DISCOGRAPHY_CATALOG_ID: &str = "com.example.discography-catalog";

/// The plugin id of the metadata fixture.
pub const METADATA_ID: &str = "com.example.metadata-fixture";

/// The plugin id of the artwork fixture.
pub const ARTWORK_ID: &str = "com.example.artwork-fixture";

/// Opens a migrated temporary database for a test.
pub async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    database
        .migrate(&MIGRATOR, &dir.join("backups"))
        .await
        .unwrap();
    Arc::new(database)
}

/// One test's host, database, settings, event bus and provider registry.
pub struct Env {
    /// Kept alive so the plugin and artwork directories outlive the host.
    _dir: tempfile::TempDir,
    /// The artwork directory images are stored under.
    artwork: PathBuf,
    /// The plugin host the fixtures are installed into.
    pub host: Arc<PluginHost>,
    /// The plugin tables.
    pub repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    pub secrets: SecretBox,
    /// The settings service the entity layer reads.
    pub settings: Settings,
    /// The migrated database.
    pub database: Arc<Database>,
    /// The provider registry the layers are resolved from.
    pub registry: Arc<ProviderRegistry>,
    /// The bus `entity.updated` and `index.progress` are published on.
    pub events: Arc<EventBus>,
}

impl Env {
    /// Builds an environment over a fresh temporary directory.
    pub async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let database = database(dir.path()).await;
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
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
        let artwork = dir.path().join("artwork");
        let events = Arc::new(EventBus::new());
        Env {
            _dir: dir,
            artwork,
            host,
            repository,
            secrets,
            settings,
            database,
            registry,
            events,
        }
    }

    /// Builds the service under test.
    pub fn service(&self) -> Arc<CatalogService> {
        CatalogService::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.database),
            self.settings.clone(),
            Arc::clone(&self.events),
            self.artwork.clone(),
            Arc::new(FfmpegRunner::new(None, 2)),
        )
    }

    /// Returns the library repository over this environment's database.
    pub fn library(&self) -> LibraryRepository {
        LibraryRepository::new(&self.database)
    }

    /// Returns the directory images are stored under.
    pub fn artwork_dir(&self) -> &Path {
        &self.artwork
    }

    /// Lists the files under the artwork directory, recursively.
    pub fn artwork_files(&self) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else if let Ok(relative) = path.strip_prefix(root) {
                    out.push(relative.to_string_lossy().into_owned());
                }
            }
        }
        let mut files = Vec::new();
        walk(&self.artwork, &self.artwork, &mut files);
        files.sort();
        files
    }

    /// Builds the instance service over this environment.
    pub fn instances(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Installs a fixture plugin, returning its plugin id.
    ///
    /// Installing a plugin that is already registered is a no-op, so an
    /// environment can hold two instances of one fixture.
    pub async fn install(&self, archive: &str) -> String {
        match self.host.install(&fixture_plugin(archive)).await {
            Ok(installed) => {
                self.repository
                    .register(&installed, PluginSource::Sideload)
                    .await
                    .unwrap()
                    .id
            }
            Err(InstallError::AlreadyInstalled { id, .. }) => id,
            Err(error) => panic!("installing {archive} failed: {error}"),
        }
    }

    /// Installs a fixture plugin and configures one instance of it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    /// - `archive`: the fixture archive file name.
    /// - `plugin_id`: the fixture's plugin id.
    /// - `config`: the instance's server-scoped configuration.
    pub async fn instance(
        &self,
        slug: &str,
        archive: &str,
        plugin_id: &str,
        config: Value,
    ) -> InstanceView {
        self.install(archive).await;
        self.instances()
            .create(&InstanceSpec {
                slug: slug.to_owned(),
                plugin_id: plugin_id.to_owned(),
                display_name: format!("{slug} display"),
                server_config: config,
                host_config: Value::Null,
            })
            .await
            .unwrap()
    }

    /// Assigns the given instances to a layer, in order, and rebuilds.
    pub async fn assign(&self, layer: Layer, slugs: &[&str]) {
        let slugs: Vec<String> = slugs.iter().map(|slug| (*slug).to_owned()).collect();
        self.instances().set_layer(layer, &slugs).await.unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Writes a setting and republishes the snapshot.
    pub async fn set(&self, key: &str, value: SettingValue) {
        self.settings.set(key, value, None).await.unwrap();
    }

    /// Inserts an account row and returns its id.
    pub async fn user(&self) -> uuid::Uuid {
        let id = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
             VALUES (?, ?, 'Listener', 'x', 0, 0)",
        )
        .bind(id.as_bytes().as_slice())
        .bind(format!("user-{id}"))
        .execute(self.database.writer())
        .await
        .unwrap();
        id
    }

    /// Saves an entity into an account's library.
    pub async fn save(&self, user: uuid::Uuid, kind: EntityKind, id: EntityId) {
        sqlx::query(
            "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
             VALUES (?, ?, ?, 'saved', 0)",
        )
        .bind(user.as_bytes().as_slice())
        .bind(kind_literal(kind))
        .bind(id.uuid().as_bytes().as_slice())
        .execute(self.database.writer())
        .await
        .unwrap();
    }

    /// Recomputes an entity through the stored sources.
    pub async fn recompute(
        &self,
        kind: EntityKind,
        id: EntityId,
        precedence: hearthsound_catalog::Precedence,
        order: &hearthsound_catalog::LayerOrder,
    ) {
        let mut connection = self.database.writer().acquire().await.unwrap();
        hearthsound_catalog::fields::recompute(&mut connection, kind, id.uuid(), precedence, order)
            .await
            .unwrap();
    }

    /// Counts the rows of one table.
    pub async fn count_rows(&self, table: &str) -> i64 {
        let statement = match table {
            "tracks" => "SELECT COUNT(*) FROM tracks",
            "albums" => "SELECT COUNT(*) FROM albums",
            "artists" => "SELECT COUNT(*) FROM artists",
            "external_ids" => "SELECT COUNT(*) FROM external_ids",
            "entity_sources" => "SELECT COUNT(*) FROM entity_sources",
            "entity_images" => "SELECT COUNT(*) FROM entity_images",
            "entity_redirects" => "SELECT COUNT(*) FROM entity_redirects",
            "index_queue" => "SELECT COUNT(*) FROM index_queue",
            "answer_cache" => "SELECT COUNT(*) FROM answer_cache",
            "album_artists" => "SELECT COUNT(*) FROM album_artists",
            "media_files" => "SELECT COUNT(*) FROM media_files",
            other => panic!("the harness does not know the table `{other}`"),
        };
        sqlx::query_scalar(statement)
            .fetch_one(self.database.writer())
            .await
            .unwrap()
    }

    /// Reads the identifiers stored for one entity.
    pub async fn ids_of(&self, kind: EntityKind, id: EntityId) -> Vec<ExternalId> {
        let mut connection = self.database.writer().acquire().await.unwrap();
        external_ids::ids_of(&mut connection, kind, id.uuid())
            .await
            .unwrap()
    }

    /// Attaches identifiers to one entity.
    pub async fn attach(&self, kind: EntityKind, id: EntityId, ids: &[ExternalId]) {
        let mut connection = self.database.writer().acquire().await.unwrap();
        external_ids::attach(&mut connection, kind, id.uuid(), ids)
            .await
            .unwrap();
    }

    /// Queues an entity for indexing.
    pub async fn enqueue(
        &self,
        kind: EntityKind,
        id: EntityId,
        depth: index_queue::Depth,
        priority: i64,
    ) {
        let mut connection = self.database.writer().acquire().await.unwrap();
        index_queue::enqueue(&mut connection, kind, id.uuid(), depth, priority)
            .await
            .unwrap();
    }

    /// Returns the queue's queued and failed counts.
    pub async fn queue_counts(&self) -> (i64, i64) {
        let mut connection = self.database.writer().acquire().await.unwrap();
        index_queue::counts(&mut connection).await.unwrap()
    }

    /// Polls the queue until it drains, or panics after `timeout`.
    pub async fn wait_for_queue(&self, timeout: Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (queued, failed) = self.queue_counts().await;
            if queued == 0 && failed == 0 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the index queue did not drain: {queued} queued, {failed} failed"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

/// Returns the literal an entity kind has in the `entity_kind` column.
pub fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}

/// Returns an external identifier.
pub fn external_id(namespace: &str, value: &str) -> ExternalId {
    ExternalId {
        namespace: namespace.to_owned(),
        value: value.to_owned(),
    }
}

/// Builds the server configuration of an `http-catalog` instance.
pub fn http_config(url: &str) -> Value {
    json!({ "url": url })
}

/// Builds the server configuration of a metadata fixture instance.
pub fn metadata_config(artists: Value) -> Value {
    json!({ "recordings": [], "artists": artists })
}

/// Builds the server configuration of a metadata fixture instance whose release
/// answers wait `delay_ms` milliseconds.
pub fn delayed_metadata_config(recordings: Value, artists: Value, delay_ms: i64) -> Value {
    json!({ "recordings": recordings, "artists": artists, "delayMs": delay_ms })
}

/// Builds the server configuration of an artwork fixture instance.
pub fn artwork_config(image_url: &str, albums: Value, artists: Value) -> Value {
    json!({ "imageUrl": image_url, "albums": albums, "artists": artists })
}
