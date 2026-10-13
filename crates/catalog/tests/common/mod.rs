//! The test harness the catalog tests share.
//!
//! Every test builds a real plugin host, provider registry, instance service,
//! settings service and database in a temporary directory, installs fixture
//! plugins, and drives the service under test the way the API layer will.
//! Nothing here reaches the network: `hello-catalog` and `spin-catalog` answer
//! in process, and `http-catalog` talks to a local wiremock server.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::CatalogService;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry};
use hearthsound_core::providers::CatalogQuery;
use hearthsound_core::{CatalogId, EntityKind, Layer, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    InstallError, InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource,
    ProviderRegistry,
};
use hearthsound_test_support::fixture_plugin;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The plugin id of the hello catalog fixture.
pub const HELLO_CATALOG_ID: &str = "com.example.hello-catalog";

/// The plugin id of the HTTP catalog fixture.
pub const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The plugin id of the spin catalog fixture.
pub const SPIN_CATALOG_ID: &str = "com.example.spin-catalog";

/// The plugin id of the metadata fixture.
pub const METADATA_ID: &str = "com.example.metadata-fixture";

/// Opens a migrated temporary database for a test.
pub async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// One test's host, database, settings and provider registry.
pub struct Env {
    /// Kept alive so the plugin directory outlives the host.
    _dir: tempfile::TempDir,
    /// The plugin host the fixtures are installed into.
    pub host: Arc<PluginHost>,
    /// The plugin tables.
    pub repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    pub secrets: SecretBox,
    /// The settings service the service under test reads.
    pub settings: Settings,
    /// The migrated database.
    pub database: Arc<Database>,
    /// The provider registry the service resolves instances through.
    pub registry: Arc<ProviderRegistry>,
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
        Env {
            _dir: dir,
            host,
            repository,
            secrets,
            settings,
            database,
            registry,
        }
    }

    /// Builds the service under test.
    pub fn service(&self) -> CatalogService {
        CatalogService::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.database),
            self.settings.clone(),
        )
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
                    .register_installed(&installed, PluginSource::Sideload)
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
    pub async fn instance(&self, slug: &str, archive: &str, plugin_id: &str, config: Value) {
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
            .unwrap();
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

    /// Returns the id of a configured instance.
    pub fn instance_id(&self, slug: &str) -> uuid::Uuid {
        self.registry.instance(slug).unwrap().id()
    }
}

/// Builds a catalog search query for one text.
pub fn query(text: &str) -> CatalogQuery {
    CatalogQuery {
        text: text.to_owned(),
        kinds: vec![EntityKind::Track, EntityKind::Album, EntityKind::Artist],
        limit: 10,
        offset: 0,
    }
}

/// Builds a catalog id for a slug and provider id.
pub fn catalog_id(slug: &str, kind: EntityKind, provider_id: &str) -> CatalogId {
    CatalogId::new(Slug::new(slug).unwrap(), kind, provider_id).unwrap()
}

/// Serves one JSON search response from a wiremock endpoint.
///
/// # Parameters
///
/// - `status`: the HTTP status to answer with.
/// - `body`: the body to answer with, for a success status.
/// - `delay`: how long the endpoint waits before answering.
///
/// # Returns
///
/// The server, kept alive by the caller, and the URL to configure an instance
/// with.
pub async fn catalog_endpoint(
    status: u16,
    body: &str,
    delay: Option<Duration>,
) -> (MockServer, String) {
    let server = MockServer::start().await;
    let mut template = ResponseTemplate::new(status);
    if status < 300 {
        template = template.set_body_string(body);
    }
    if let Some(delay) = delay {
        template = template.set_delay(delay);
    }
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(template)
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());
    (server, url)
}

/// Builds the server configuration of an `http-catalog` instance.
///
/// # Parameters
///
/// - `url`: the endpoint the fixture searches.
pub fn http_config(url: &str) -> Value {
    json!({ "url": url })
}

/// Builds the server configuration of a metadata fixture instance.
///
/// # Parameters
///
/// - `recordings`: the recordings the fixture knows, as JSON objects with
///   `title`, `mbid`, an optional `isrc`, an optional `score`, and optional
///   `releases`.
pub fn metadata_config(recordings: Value) -> Value {
    json!({ "recordings": recordings })
}

/// Writes one row of the catalog cache directly.
///
/// # Parameters
///
/// - `database`: the database to write.
/// - `instance`: the instance id part of the key.
/// - `request_key`: the canonical request key.
/// - `response`: the stored response JSON.
/// - `expires_at`: the instant the row stops being fresh, in unix milliseconds.
pub async fn write_cache_row(
    database: &Database,
    instance: uuid::Uuid,
    request_key: &str,
    response: &str,
    expires_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO catalog_cache (instance_id, request_key, response, fetched_at, expires_at) \
         VALUES (?, ?, ?, 0, ?)",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(request_key)
    .bind(response)
    .bind(expires_at)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Reads the lifetime a stored cache row was given.
///
/// # Returns
///
/// The row's `expires_at - fetched_at` in milliseconds, or `None` when the row
/// does not exist or has no expiry.
pub async fn cache_row_lifetime(
    database: &Database,
    instance: uuid::Uuid,
    request_key: &str,
) -> Option<i64> {
    let row: (i64, Option<i64>) = sqlx::query_as(
        "SELECT fetched_at, expires_at FROM catalog_cache \
         WHERE instance_id = ? AND request_key = ?",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(request_key)
    .fetch_optional(database.readers())
    .await
    .unwrap()?;
    Some(row.1? - row.0)
}

/// Counts the rows of the catalog cache.
pub async fn count_cache_rows(database: &Database) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM catalog_cache")
        .fetch_one(database.readers())
        .await
        .unwrap()
}

/// Returns the current time in unix milliseconds.
pub fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

/// The setting keys a test writes.
pub use hearthsound_config::keys as setting_keys;
