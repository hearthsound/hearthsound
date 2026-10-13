//! Integration tests for the catalog service over the fixture plugins.
//!
//! Every test builds a real plugin host, registry, instance service and
//! database in a temporary directory, installs fixture plugins, and drives the
//! service the way the API layer will. Nothing here reaches the network: the
//! `hello-catalog` fixture answers in process, and `http-catalog` talks to a
//! local wiremock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_catalog::{CatalogError, CatalogService};
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::providers::CatalogQuery;
use hearthsound_core::{CatalogId, EntityKind, Layer, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG, SPIN_CATALOG, fixture_plugin};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The plugin id of the hello catalog fixture.
const HELLO_CATALOG_ID: &str = "com.example.hello-catalog";

/// The plugin id of the HTTP catalog fixture.
const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The plugin id of the spin catalog fixture.
const SPIN_CATALOG_ID: &str = "com.example.spin-catalog";

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// One test's host, database, settings and provider registry.
struct Env {
    /// Kept alive so the plugin directory outlives the host.
    _dir: tempfile::TempDir,
    /// The plugin host the fixtures are installed into.
    host: Arc<PluginHost>,
    /// The plugin tables.
    repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    secrets: SecretBox,
    /// The settings service the service under test reads.
    settings: Settings,
    /// The migrated database.
    database: Arc<Database>,
    /// The provider registry the service resolves instances through.
    registry: Arc<ProviderRegistry>,
}

impl Env {
    /// Builds an environment over a fresh temporary directory.
    async fn new() -> Env {
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
    fn service(&self) -> CatalogService {
        CatalogService::new(
            Arc::clone(&self.registry),
            Arc::clone(&self.database),
            self.settings.clone(),
        )
    }

    /// Builds the instance service over this environment.
    fn instances(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Installs a fixture plugin, returning its plugin id.
    async fn install(&self, archive: &str) -> String {
        let installed = self.host.install(&fixture_plugin(archive)).await.unwrap();
        self.repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap()
            .id
    }

    /// Installs a fixture plugin and configures one instance of it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    /// - `archive`: the fixture archive file name.
    /// - `plugin_id`: the fixture's plugin id.
    /// - `config`: the instance's server-scoped configuration.
    async fn instance(&self, slug: &str, archive: &str, plugin_id: &str, config: Value) {
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
    async fn assign(&self, layer: Layer, slugs: &[&str]) {
        let slugs: Vec<String> = slugs.iter().map(|slug| (*slug).to_owned()).collect();
        self.instances().set_layer(layer, &slugs).await.unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Writes a setting and republishes the snapshot.
    async fn set(&self, key: &str, value: SettingValue) {
        self.settings.set(key, value, None).await.unwrap();
    }
}

/// Builds a catalog search query for one text.
fn query(text: &str) -> CatalogQuery {
    CatalogQuery {
        text: text.to_owned(),
        kinds: vec![EntityKind::Track, EntityKind::Album, EntityKind::Artist],
        limit: 10,
        offset: 0,
    }
}

/// Builds a catalog id for a slug and provider id.
fn catalog_id(slug: &str, kind: EntityKind, provider_id: &str) -> CatalogId {
    CatalogId::new(Slug::new(slug).unwrap(), kind, provider_id).unwrap()
}

/// Serves one JSON search response from a wiremock endpoint.
///
/// # Returns
///
/// The server, kept alive by the caller, and the URL to configure an instance
/// with.
async fn catalog_endpoint(
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

#[tokio::test]
async fn search_returns_the_primary_instances_results_with_their_ids() {
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    let track = &results.tracks[0];
    assert_eq!(track.id.slug().as_str(), "hello");
    assert_eq!(track.id.kind(), EntityKind::Track);
    assert_eq!(track.id.provider_id(), "hello-track-1");
    assert_eq!(track.track.title, "Hi, world");
    assert_eq!(track.track.duration_ms, Some(180_000));
    assert_eq!(results.albums.len(), 1);
    assert_eq!(results.albums[0].id.provider_id(), "hello-album-1");
    assert_eq!(results.albums[0].album.title, "Hello Album");
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.artists[0].id.provider_id(), "hello-artist-1");
    assert_eq!(results.artists[0].artist.name, "Hello Artist");
}

#[tokio::test]
async fn search_is_empty_when_catalog_search_is_disabled() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    env.set(
        keys::SEARCH_CATALOG_ENABLED.name(),
        SettingValue::Bool(false),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert_eq!(results.source, None);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a disabled catalog search must not contact an instance"
    );
}

#[tokio::test]
async fn search_asks_nothing_when_the_result_cap_is_zero() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    env.set(
        keys::SEARCH_CATALOG_MAX_RESULTS.name(),
        SettingValue::Integer(0),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a zero result cap must not contact an instance"
    );
}

#[tokio::test]
async fn search_without_a_catalog_instance_is_an_empty_search() {
    let env = Env::new().await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert_eq!(results.source, None);
}

#[tokio::test]
async fn lookups_route_by_the_slug_the_id_carries() {
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let track = service
        .track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();
    assert_eq!(track.track.title, "Hi, world");
    assert_eq!(track.id.provider_id(), "hello-track-1");

    let album = service
        .album(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap();
    assert_eq!(album.album.title, "Hello Album");
    assert_eq!(album.album.tracks.len(), 1);
    assert_eq!(album.album.tracks[0].id, "hello-track-1");

    let artist = service
        .artist(&catalog_id("hello", EntityKind::Artist, "hello-artist-1"))
        .await
        .unwrap();
    assert_eq!(artist.artist.name, "Hello Artist");

    let albums = service.artist_albums(&artist.id, 10, 0).await.unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].id.provider_id(), "hello-album-1");

    let top = service.artist_top_tracks(&artist.id, 5).await.unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].id.provider_id(), "hello-track-1");

    assert!(
        service
            .artist_albums(&artist.id, 10, 1)
            .await
            .unwrap()
            .is_empty(),
        "the offset must reach the provider"
    );
}

#[tokio::test]
async fn an_unknown_slug_or_a_wrong_kind_is_not_found() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let unknown = service
        .track(&catalog_id("nowhere", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap_err();
    assert!(matches!(unknown, CatalogError::NotFound), "{unknown}");

    let mixed = service
        .track(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap_err();
    assert!(matches!(mixed, CatalogError::NotFound), "{mixed}");

    let missing = service
        .track(&catalog_id("hello", EntityKind::Track, "nope"))
        .await
        .unwrap_err();
    assert!(matches!(missing, CatalogError::NotFound), "{missing}");
}

#[tokio::test]
async fn an_instance_that_is_not_assigned_serves_no_lookup() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    let service = env.service();

    let error = service
        .track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap_err();
    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn search_falls_back_when_the_primary_errors() {
    let (server, url) = catalog_endpoint(500, "", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat", "hello"]).await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.title, "Hi, world");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn search_falls_back_when_the_primary_exceeds_its_deadline() {
    let (server, url) = catalog_endpoint(200, "[]", Some(Duration::from_secs(2))).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat", "hello"]).await;
    env.set(
        keys::SEARCH_CATALOG_TIMEOUT_MS.name(),
        SettingValue::Integer(100),
    )
    .await;

    let started = Instant::now();
    let results = env.service().search(&query("world")).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    assert!(
        elapsed < Duration::from_secs(1),
        "the fallback must not wait for the slow primary: {elapsed:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_rate_limited_instance_is_reported_with_its_retry_delay() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "2"))
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;

    let error = env.service().search(&query("world")).await.unwrap_err();

    assert_eq!(error.retry_after_ms(), Some(2_000), "{error}");
    assert_eq!(error.instance(), Some("httpcat"));
    assert!(error.is_transient());
}

#[tokio::test]
async fn a_failing_primary_is_unavailable_when_no_fallback_answers() {
    let (_server, url) = catalog_endpoint(503, "", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;

    let error = env.service().search(&query("world")).await.unwrap_err();

    assert!(
        matches!(&error, CatalogError::Unavailable { instance } if instance == "httpcat"),
        "{error}"
    );
}

#[tokio::test]
async fn lookup_isrc_uses_the_primary_and_never_merges() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello", "httpcat"]).await;

    let tracks = env.service().lookup_isrc("USRC17607839").await.unwrap();

    assert!(tracks.is_empty());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a later instance is consulted only when the primary does not answer"
    );
}

#[tokio::test]
async fn lookup_isrc_reports_the_failure_of_the_last_instance() {
    let env = Env::new().await;
    env.instance("spin", SPIN_CATALOG, SPIN_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["spin"]).await;

    let error = env.service().lookup_isrc("USRC17607839").await.unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}
