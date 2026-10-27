//! Integration tests for protecting mandatory plugins.
//!
//! A plugin the server installs by itself cannot be removed, switched off, or
//! dropped from a layer its manifest provides; an administrator may still
//! reorder the layers it takes part in. The tests drive the admin and instance
//! services over the `http-catalog` and `metadata-fixture` fixtures, protecting
//! the catalog fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::secret::SecretKey;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::Layer;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    AdminError, InstanceError, InstanceService, InstanceSpec, PluginAdmin, PluginHost,
    PluginRepository, PluginSource, ProtectedPlugins,
};
use hearthsound_test_support::{HTTP_CATALOG, METADATA_FIXTURE, fixture_plugin};
use serde_json::{Value, json};

/// The id of the HTTP catalog fixture.
const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

/// The id of the metadata fixture.
const METADATA_ID: &str = "com.example.metadata-fixture";

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// One test's host, services and plugins, with the catalog fixture protected.
struct Env {
    /// Kept alive so the plugin directory outlives the host.
    _dir: tempfile::TempDir,
    repository: PluginRepository,
    admin: PluginAdmin,
    instances: InstanceService,
}

impl Env {
    /// Builds the services over a fresh temporary directory.
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let database = Arc::new(database);
        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings(),
                Arc::clone(&database),
            )
            .unwrap()
            .with_protected_plugins(ProtectedPlugins::new([HTTP_CATALOG_ID])),
        );
        let repository = PluginRepository::new(&database);
        let secrets = SecretKey::generate().secret_box();
        let admin = PluginAdmin::new(Arc::clone(&host), repository.clone(), None);
        let instances = InstanceService::new(Arc::clone(&host), repository.clone(), secrets);
        Env {
            _dir: dir,
            repository,
            admin,
            instances,
        }
    }

    /// Installs one fixture archive and returns its plugin id.
    async fn install(&self, archive: &Path) -> String {
        self.admin
            .install_archive(archive, None, PluginSource::Sideload, None)
            .await
            .unwrap()
            .id
    }

    /// Creates an instance of a plugin, naming the endpoint its configuration
    /// reaches.
    async fn instance(&self, slug: &str, plugin_id: &str) {
        self.instances
            .create(&InstanceSpec {
                slug: slug.to_owned(),
                plugin_id: plugin_id.to_owned(),
                display_name: format!("{slug} display"),
                server_config: json!({"url": "https://catalog.example/search"}),
                host_config: Value::Null,
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn a_protected_plugin_cannot_be_removed_or_switched_off() {
    let env = Env::new().await;
    let id = env.install(&fixture_plugin(HTTP_CATALOG)).await;
    assert_eq!(id, HTTP_CATALOG_ID);

    let error = env.admin.set_enabled(&id, false).await.unwrap_err();
    assert!(
        matches!(&error, AdminError::Required { plugin_id } if plugin_id == HTTP_CATALOG_ID),
        "{error}"
    );

    let error = env.admin.remove(&id).await.unwrap_err();
    assert!(
        matches!(&error, AdminError::Required { plugin_id } if plugin_id == HTTP_CATALOG_ID),
        "{error}"
    );

    let stored = env.repository.plugin(&id).await.unwrap().unwrap();
    assert!(stored.enabled, "the refused writes changed nothing");

    // A plugin that is not protected keeps the ordinary behavior.
    let metadata = env.install(&fixture_plugin(METADATA_FIXTURE)).await;
    assert_eq!(metadata, METADATA_ID);
    env.admin.set_enabled(&metadata, false).await.unwrap();
    let stored = env.repository.plugin(&metadata).await.unwrap().unwrap();
    assert!(!stored.enabled);
}

#[tokio::test]
async fn the_last_instance_of_a_protected_plugin_cannot_be_deleted_or_disabled() {
    let env = Env::new().await;
    env.install(&fixture_plugin(HTTP_CATALOG)).await;
    env.instance("primary", HTTP_CATALOG_ID).await;

    let error = env
        .instances
        .set_enabled("primary", false)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, InstanceError::Required { plugin_id } if plugin_id == HTTP_CATALOG_ID),
        "{error}"
    );
    let error = env.instances.delete("primary").await.unwrap_err();
    assert!(
        matches!(&error, InstanceError::Required { plugin_id } if plugin_id == HTTP_CATALOG_ID),
        "{error}"
    );
    assert!(env.instances.get("primary").await.unwrap().enabled);

    // With a second instance, the first may be switched off and deleted.
    env.instance("secondary", HTTP_CATALOG_ID).await;
    env.instances.set_enabled("primary", false).await.unwrap();
    env.instances.delete("primary").await.unwrap();
    assert!(env.instances.get("secondary").await.is_ok());
}

#[tokio::test]
async fn a_protected_plugin_stays_in_the_layers_it_provides() {
    let env = Env::new().await;
    env.install(&fixture_plugin(HTTP_CATALOG)).await;
    env.install(&fixture_plugin(METADATA_FIXTURE)).await;
    env.instance("primary", HTTP_CATALOG_ID).await;
    env.instance("mirror", METADATA_ID).await;

    env.instances
        .set_layer(Layer::Catalog, &["primary".to_owned()])
        .await
        .unwrap();

    // Dropping the protected instance from its layer is refused.
    let error = env
        .instances
        .set_layer(Layer::Catalog, &[])
        .await
        .unwrap_err();
    assert!(
        matches!(&error, InstanceError::Required { plugin_id } if plugin_id == HTTP_CATALOG_ID),
        "{error}"
    );
    let assignments = env.instances.assignments().await.unwrap();
    assert_eq!(
        assignments
            .iter()
            .filter(|assignment| assignment.layer == Layer::Catalog)
            .count(),
        1,
        "the refused write left the layer as it was"
    );

    // Reordering is allowed, as is adding another instance beside it.
    env.instances
        .set_layer(Layer::Catalog, &["primary".to_owned()])
        .await
        .unwrap();
}
