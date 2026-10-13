//! Integration tests for fetching catalog artwork under an instance's HTTP
//! permissions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::Layer;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::http::HttpError;
use hearthsound_plugin_host::manifest::ImageCachePolicy;
use hearthsound_plugin_host::{
    FetchError, InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource,
    ProviderRegistry,
};
use hearthsound_test_support::{HTTP_CATALOG, METADATA_FIXTURE, fixture_plugin};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The id of the HTTP catalog fixture.
const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The id of the metadata fixture.
const METADATA_ID: &str = "com.example.metadata-fixture";

/// The bytes a test image consists of.
const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// One test's host, database, and the services over them.
struct Env {
    /// Kept alive so the plugin directory outlives the host.
    _dir: tempfile::TempDir,
    host: Arc<PluginHost>,
    repository: PluginRepository,
    secrets: SecretBox,
}

impl Env {
    /// Builds a host over a fresh temporary directory.
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let database = database(dir.path()).await;
        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let secrets = SecretKey::generate().secret_box();
        let repository = PluginRepository::new(&database);
        Env {
            _dir: dir,
            host,
            repository,
            secrets,
        }
    }

    /// Builds the instance service over this environment.
    fn service(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Installs a fixture and records it.
    async fn install(&self, archive: &str) {
        let installed = self.host.install(&fixture_plugin(archive)).await.unwrap();
        self.repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap();
    }

    /// Builds a rebuilt registry over this environment.
    async fn registry(&self) -> ProviderRegistry {
        let registry = ProviderRegistry::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        registry.rebuild().await.unwrap();
        registry
    }
}

/// The specification of an instance with default host settings.
fn spec(slug: &str, plugin_id: &str, server_config: Value) -> InstanceSpec {
    InstanceSpec {
        slug: slug.to_owned(),
        plugin_id: plugin_id.to_owned(),
        display_name: format!("{slug} display"),
        server_config,
        host_config: Value::Null,
    }
}

#[tokio::test]
async fn fetches_an_image_under_the_instances_permissions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(IMAGE),
        )
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.install(HTTP_CATALOG).await;
    let service = env.service();
    service
        .create(&spec("httpcat", HTTP_CATALOG_ID, json!({"url": endpoint})))
        .await
        .unwrap();
    service
        .set_layer(Layer::Catalog, &["httpcat".to_owned()])
        .await
        .unwrap();

    let registry = env.registry().await;
    let image = registry
        .fetch_image("httpcat", &format!("{}/cover.png", server.uri()))
        .await
        .unwrap();

    assert_eq!(image.status, 200);
    assert_eq!(image.content_type.as_deref(), Some("image/png"));
    assert_eq!(image.bytes, IMAGE);
    // The fixture's manifest forbids caching its images, and the policy travels
    // with the fetch so the caller cannot store one by accident.
    assert_eq!(image.cache_policy, ImageCachePolicy::NoStore);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn denies_a_url_outside_the_instances_allowlist() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(IMAGE))
        .mount(&server)
        .await;
    let endpoint = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.install(HTTP_CATALOG).await;
    let service = env.service();
    service
        .create(&spec("httpcat", HTTP_CATALOG_ID, json!({"url": endpoint})))
        .await
        .unwrap();
    service
        .set_layer(Layer::Catalog, &["httpcat".to_owned()])
        .await
        .unwrap();

    let registry = env.registry().await;
    let error = registry
        .fetch_image("httpcat", "https://art.example.com/cover.png")
        .await
        .unwrap_err();
    assert!(
        matches!(error, FetchError::Http(HttpError::Denied(_))),
        "{error}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn an_instance_without_an_http_permission_reaches_nothing() {
    let server = MockServer::start().await;
    let env = Env::new().await;
    env.install(METADATA_FIXTURE).await;
    let service = env.service();
    service
        .create(&spec("metafix", METADATA_ID, json!({})))
        .await
        .unwrap();
    service
        .set_layer(Layer::Metadata, &["metafix".to_owned()])
        .await
        .unwrap();

    let registry = env.registry().await;
    let error = registry
        .fetch_image("metafix", &format!("{}/cover.png", server.uri()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, FetchError::Http(HttpError::Denied(_))),
        "{error}"
    );

    let error = registry
        .fetch_image("absent", &format!("{}/cover.png", server.uri()))
        .await
        .unwrap_err();
    assert!(matches!(error, FetchError::UnknownInstance(_)), "{error}");
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
