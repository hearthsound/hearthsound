//! Integration tests for registering plugins, configuring provider instances,
//! and the provider registry built from the plugin tables.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::events::{EventBus, EventEnvelope, ServerEvent};
use hearthsound_core::providers::CatalogQuery;
use hearthsound_core::{EntityKind, Layer, ProviderError};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::manifest::{ImageCachePolicy, Permissions};
use hearthsound_plugin_host::{
    InstanceError, InstanceService, InstanceSpec, InstanceView, PluginHost, PluginRepository,
    PluginSource, ProviderRegistry, Risk, SECRET_PLACEHOLDER,
};
use hearthsound_test_support::{HTTP_CATALOG, METADATA_FIXTURE, SPIN_CATALOG, fixture_plugin};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The id of the HTTP catalog fixture.
const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The id of the metadata fixture.
const METADATA_ID: &str = "com.example.metadata-fixture";

/// The id of the spin catalog fixture.
const SPIN_ID: &str = "com.example.spin-catalog";

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

    /// Builds an empty provider registry over this environment.
    fn registry(&self) -> ProviderRegistry {
        ProviderRegistry::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Installs a fixture and records it, returning its plugin id.
    async fn install(&self, archive: &str) -> String {
        let installed = self.host.install(&fixture_plugin(archive)).await.unwrap();
        let registration = self
            .repository
            .register(&installed, PluginSource::Sideload)
            .await
            .unwrap();
        registration.id
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

/// A catalog search for one text.
fn query(text: &str) -> CatalogQuery {
    CatalogQuery {
        text: text.to_owned(),
        kinds: vec![EntityKind::Track],
        limit: 10,
        offset: 0,
    }
}

/// Reads one instance's row, which must exist.
async fn row(env: &Env, slug: &str) -> hearthsound_plugin_host::InstanceRow {
    env.repository
        .instance_by_slug(slug)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn registering_an_installed_plugin_records_its_manifest_and_permissions() {
    let env = Env::new().await;
    let installed = env
        .host
        .install(&fixture_plugin(HTTP_CATALOG))
        .await
        .unwrap();

    let registration = env
        .repository
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();

    assert_eq!(registration.id, HTTP_CATALOG_ID);
    assert_eq!(registration.version, "0.1.0");
    assert_eq!(registration.api, "hearthsound:plugin@0.2.0");
    assert_eq!(registration.source, PluginSource::Sideload);
    assert_eq!(registration.provides, vec![Layer::Catalog]);

    // The fixture reaches whatever hosts the administrator configured (its API
    // URL and its cover URL): one medium-risk permission listing both entries.
    assert_eq!(registration.requested.len(), 1);
    let permission = &registration.requested[0];
    assert_eq!(permission.scope, "http.hosts");
    assert_eq!(permission.detail, "$config:url, $config:coverUrl");
    assert_eq!(permission.risk, Risk::Medium);

    let stored = env
        .repository
        .plugin(HTTP_CATALOG_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.enabled, "a registered plugin is enabled");
    assert_eq!(stored.source, "sideload");
    assert_eq!(stored.version, registration.version);
    assert_eq!(stored.api, registration.api);
    assert_eq!(stored.sha256, installed.sha256().to_vec());
    assert_eq!(stored.manifest, installed.manifest_json());

    let approved: Permissions =
        serde_json::from_str(stored.approved_permissions.as_deref().unwrap()).unwrap();
    assert_eq!(approved, installed.manifest().permissions);
}

#[tokio::test]
async fn an_instance_whose_configuration_breaks_the_schema_is_rejected() {
    let env = Env::new().await;
    env.install(HTTP_CATALOG).await;
    let service = env.service();

    let wrong_type = service
        .create(&spec("httpcat", HTTP_CATALOG_ID, json!({"url": 42})))
        .await
        .unwrap_err();
    assert!(
        matches!(wrong_type, InstanceError::Schema(_)),
        "{wrong_type}"
    );

    let missing = service
        .create(&spec("httpcat", HTTP_CATALOG_ID, json!({})))
        .await
        .unwrap_err();
    assert!(matches!(missing, InstanceError::Schema(_)), "{missing}");

    assert!(
        service.list().await.unwrap().is_empty(),
        "a rejected configuration must not be stored"
    );
}

#[tokio::test]
async fn a_secret_is_sealed_at_rest_and_delivered_to_the_plugin_in_the_clear() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"]}]"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.install(HTTP_CATALOG).await;
    let service = env.service();

    let view = service
        .create(&spec(
            "httpcat",
            HTTP_CATALOG_ID,
            json!({ "url": endpoint.clone() }),
        ))
        .await
        .unwrap();
    assert_eq!(view.server_config["url"], json!(SECRET_PLACEHOLDER));
    assert!(!format!("{view:?}").contains(&endpoint));

    let stored = row(&env, "httpcat").await.server_config.unwrap();
    assert!(stored.contains("$secret"), "{stored}");
    assert!(
        !stored.contains(&endpoint),
        "the plaintext must not be stored: {stored}"
    );

    service
        .set_layer(Layer::Catalog, &["httpcat".to_owned()])
        .await
        .unwrap();

    // The plugin can only reach the endpoint if the host decrypted the field:
    // both the `$config:url` allowlist entry and the URL itself come from the
    // configuration the plugin is handed.
    let registry = env.registry();
    registry.rebuild().await.unwrap();
    let instance = registry.layers(Layer::Catalog).into_iter().next().unwrap();
    let provider = instance.catalog().unwrap().clone();

    let results = match provider.search(&query("hello")).await {
        Ok(results) => results,
        Err(error) => panic!("the plugin could not reach the endpoint: {error:?}"),
    };
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].title, "Remote Song");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_configuration_the_plugin_rejects_is_not_stored() {
    let env = Env::new().await;
    env.install(METADATA_FIXTURE).await;
    let service = env.service();

    // The schema allows any object in `recordings`, but the fixture's own
    // configuration type requires every recording to carry a title.
    let error = service
        .create(&spec(
            "metafix",
            METADATA_ID,
            json!({"recordings": [{"mbid": "6a1a"}]}),
        ))
        .await
        .unwrap_err();

    let InstanceError::ValidateConfig(problems) = error else {
        panic!("unexpected error: {error}");
    };
    assert!(
        problems.iter().any(|problem| problem.contains("title")),
        "unexpected problems: {problems:?}"
    );
    assert!(service.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_layer_only_takes_instances_whose_plugin_provides_it() {
    let env = Env::new().await;
    env.install(METADATA_FIXTURE).await;
    let service = env.service();
    service
        .create(&spec("meta1", METADATA_ID, json!({})))
        .await
        .unwrap();
    service
        .create(&spec("meta2", METADATA_ID, json!({})))
        .await
        .unwrap();

    service
        .set_layer(Layer::Metadata, &["meta1".to_owned(), "meta2".to_owned()])
        .await
        .unwrap();

    let assignments = service.assignments().await.unwrap();
    assert_eq!(assignments.len(), 2);
    assert_eq!(assignments[0].layer, Layer::Metadata);
    assert_eq!(assignments[0].position, 0);
    assert_eq!(assignments[1].position, 1);
    assert_eq!(assignments[0].instance_id, row(&env, "meta1").await.id);

    let wrong_layer = service
        .set_layer(Layer::Catalog, &["meta1".to_owned()])
        .await
        .unwrap_err();
    assert!(
        matches!(
            wrong_layer,
            InstanceError::LayerNotProvided { ref plugin, layer: Layer::Catalog }
                if plugin == METADATA_ID
        ),
        "{wrong_layer}"
    );

    let unknown = service
        .set_layer(Layer::Metadata, &["nope".to_owned()])
        .await
        .unwrap_err();
    assert!(
        matches!(unknown, InstanceError::UnknownInstance(_)),
        "{unknown}"
    );

    let duplicate = service
        .set_layer(Layer::Metadata, &["meta1".to_owned(), "meta1".to_owned()])
        .await
        .unwrap_err();
    assert!(
        matches!(duplicate, InstanceError::DuplicateInstance(_)),
        "{duplicate}"
    );

    // A rejected assignment leaves the previous order in place.
    let assignments = service.assignments().await.unwrap();
    assert_eq!(assignments.len(), 2);
}

#[tokio::test]
async fn the_registry_serves_enabled_instances_in_the_assigned_order() {
    let env = Env::new().await;
    env.install(METADATA_FIXTURE).await;
    let service = env.service();
    for slug in ["meta1", "meta2", "meta3"] {
        service
            .create(&spec(slug, METADATA_ID, json!({})))
            .await
            .unwrap();
    }
    service
        .set_layer(
            Layer::Metadata,
            &["meta2".to_owned(), "meta1".to_owned(), "meta3".to_owned()],
        )
        .await
        .unwrap();

    let registry = env.registry();
    let report = registry.rebuild().await.unwrap();
    assert_eq!(report.instances, 3);
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let slugs: Vec<String> = registry
        .layers(Layer::Metadata)
        .iter()
        .map(|instance| instance.slug().as_str().to_owned())
        .collect();
    assert_eq!(slugs, ["meta2", "meta1", "meta3"]);

    let first = registry.layers(Layer::Metadata).remove(0);
    assert_eq!(first.plugin_id(), METADATA_ID);
    assert_eq!(first.plugin_version(), "0.1.0");
    assert_eq!(first.id(), row(&env, "meta2").await.id);
    assert!(first.metadata().is_some());
    assert!(first.catalog().is_none());
    assert_eq!(first.image_cache_policy(), ImageCachePolicy::Cache);
    assert!(registry.layers(Layer::Catalog).is_empty());

    // A disabled instance keeps its assignment but is not served.
    service.set_enabled("meta1", false).await.unwrap();
    registry.rebuild().await.unwrap();
    let slugs: Vec<String> = registry
        .layers(Layer::Metadata)
        .iter()
        .map(|instance| instance.slug().as_str().to_owned())
        .collect();
    assert_eq!(slugs, ["meta2", "meta3"]);

    // Disabling the plugin takes every instance out of service.
    env.repository
        .set_plugin_enabled(METADATA_ID, false)
        .await
        .unwrap();
    let report = registry.rebuild().await.unwrap();
    assert_eq!(report.instances, 0);
    assert!(registry.layers(Layer::Metadata).is_empty());

    // Re-enabling both brings them back.
    env.repository
        .set_plugin_enabled(METADATA_ID, true)
        .await
        .unwrap();
    service.set_enabled("meta1", true).await.unwrap();
    let report = registry.rebuild().await.unwrap();
    assert_eq!(report.instances, 3);
}

#[tokio::test]
async fn an_instance_is_enabled_on_creation_and_defaults_are_filled_in() {
    let env = Env::new().await;
    env.install(SPIN_CATALOG).await;
    let service = env.service();

    let view: InstanceView = service
        .create(&spec("spin", SPIN_ID, json!({})))
        .await
        .unwrap();

    assert!(view.enabled);
    assert_eq!(view.slug, "spin");
    assert_eq!(view.server_config["greeting"], json!("Hello"));
    assert_eq!(view.host_config.max_concurrent, 2);
    assert_eq!(view.host_config.max_concurrent_searches, 2);
    assert_eq!(view.host_config.searches_per_hour, 120);

    let created = service.get("spin").await.unwrap();
    assert_eq!(created, view);

    let updated = service
        .update("spin", &spec("spin", SPIN_ID, json!({"greeting": "Hey"})))
        .await
        .unwrap();
    assert_eq!(updated.server_config["greeting"], json!("Hey"));
    assert_eq!(updated.created_at, view.created_at);

    service.delete("spin").await.unwrap();
    assert!(matches!(
        service.get("spin").await.unwrap_err(),
        InstanceError::UnknownInstance(_)
    ));
}

#[tokio::test]
async fn repeated_traps_disable_the_instance_in_the_database_and_publish_an_event() {
    let env = Env::new().await;
    env.install(SPIN_CATALOG).await;
    let service = env.service();
    service
        .create(&spec("spin", SPIN_ID, json!({})))
        .await
        .unwrap();
    service
        .set_layer(Layer::Catalog, &["spin".to_owned()])
        .await
        .unwrap();

    let mut events = env.host.events().subscribe();

    let registry = env.registry();
    registry.rebuild().await.unwrap();
    let instance = registry.layers(Layer::Catalog).into_iter().next().unwrap();
    let provider = instance.catalog().unwrap().clone();

    for attempt in 0..5 {
        assert_eq!(
            provider.search(&query("anything")).await,
            Err(ProviderError::Internal),
            "call {attempt} should have trapped"
        );
    }
    assert_eq!(
        provider.search(&query("anything")).await,
        Err(ProviderError::Unavailable)
    );

    let envelope = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let envelope: EventEnvelope = events.recv().await.unwrap();
            if let ServerEvent::PluginStatus(status) = envelope.event {
                return status;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(envelope.plugin_id, SPIN_ID);
    assert_eq!(envelope.slug.as_deref(), Some("spin"));
    assert_eq!(envelope.state, "disabled");

    let disabled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !row(&env, "spin").await.enabled {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        disabled.is_ok(),
        "the instance was not disabled in the database"
    );
}

/// The event bus is reachable from the host and can be replaced.
#[tokio::test]
async fn a_host_can_publish_on_the_servers_bus() {
    let bus = Arc::new(EventBus::new());
    let dir = tempfile::tempdir().unwrap();
    let database = database(dir.path()).await;
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        Arc::clone(&database),
    )
    .unwrap()
    .with_event_bus(Arc::clone(&bus));
    assert!(Arc::ptr_eq(host.events(), &bus));
}

/// An update that echoes the sealed placeholder back keeps the stored secret,
/// so the plugin is handed the secret the instance was configured with.
#[tokio::test]
async fn an_update_that_echoes_the_placeholder_keeps_the_secret() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"]}]"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.install(HTTP_CATALOG).await;
    let service = env.service();
    let created = service
        .create(&spec(
            "httpcat",
            HTTP_CATALOG_ID,
            json!({ "url": endpoint }),
        ))
        .await
        .unwrap();
    assert_eq!(created.server_config["url"], json!(SECRET_PLACEHOLDER));

    // The configuration an administrator read and sent back carries the
    // placeholder where the secret belongs.
    let echoed = service
        .update(
            "httpcat",
            &spec(
                "httpcat",
                HTTP_CATALOG_ID,
                json!({ "url": SECRET_PLACEHOLDER }),
            ),
        )
        .await
        .unwrap();
    assert_eq!(echoed.server_config["url"], json!(SECRET_PLACEHOLDER));
    let stored = row(&env, "httpcat").await.server_config.unwrap();
    assert!(stored.contains("$secret"), "{stored}");
    assert!(!stored.contains(SECRET_PLACEHOLDER), "{stored}");

    // The plugin is handed the secret rather than the placeholder: the search
    // can only reach the endpoint the configuration names, and the endpoint is
    // the one the update echoed back.
    service
        .set_layer(Layer::Catalog, &["httpcat".to_owned()])
        .await
        .unwrap();
    let registry = env.registry();
    registry.rebuild().await.unwrap();
    let instance = registry.layers(Layer::Catalog).into_iter().next().unwrap();
    let provider = instance.catalog().unwrap().clone();
    drop(instance);
    let results = provider.search(&query("hello")).await.unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].title, "Remote Song");
}
