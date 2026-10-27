//! Integration test for a plugin built for the previous plugin API.
//!
//! The host loads `hearthsound:plugin@0.5.0` and `hearthsound:plugin@0.4.0` at
//! the same time. This test drives the frozen 0.4.0 path end to end: the plugin
//! installs, the registry builds it into a catalog provider, and a search
//! crosses the boundary through the `v4` adapters.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::secret::SecretKey;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::CatalogQuery;
use hearthsound_core::{EntityKind, Layer};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::manifest::ApiVersion;
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
};
use hearthsound_test_support::{LEGACY_CATALOG, fixture_plugin};
use serde_json::json;

/// The id the legacy catalog fixture declares.
const LEGACY_ID: &str = "com.example.legacy-catalog";

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// Opens a migrated temporary database.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

#[tokio::test]
async fn a_legacy_plugin_serves_a_catalog_search() {
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
    let repository = PluginRepository::new(&database);
    let secrets = SecretKey::generate().secret_box();

    let installed = host.install(&fixture_plugin(LEGACY_CATALOG)).await.unwrap();
    assert_eq!(
        installed.api(),
        ApiVersion::V0_4,
        "the fixture is built for the frozen API"
    );
    assert_eq!(installed.manifest().id, LEGACY_ID);

    let registration = repository
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.api, "hearthsound:plugin@0.4.0");

    let service = InstanceService::new(Arc::clone(&host), repository.clone(), secrets.clone());
    service
        .create(&InstanceSpec {
            slug: "legacy".to_owned(),
            plugin_id: LEGACY_ID.to_owned(),
            display_name: "Legacy catalog".to_owned(),
            server_config: json!({}),
            host_config: json!(null),
        })
        .await
        .unwrap();
    service
        .set_layer(Layer::Catalog, &["legacy".to_owned()])
        .await
        .unwrap();

    let registry = ProviderRegistry::new(Arc::clone(&host), repository, secrets);
    registry.rebuild().await.unwrap();
    let instance = registry.layers(Layer::Catalog).into_iter().next().unwrap();
    let provider = instance.catalog().unwrap().clone();

    let results = provider
        .search(&CatalogQuery {
            text: "world".to_owned(),
            kinds: vec![EntityKind::Track],
            limit: 10,
            offset: 0,
        })
        .await
        .unwrap();

    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].title, "Hello, world");
    assert_eq!(
        results.tracks[0]
            .album
            .as_ref()
            .map(|album| album.id.as_str()),
        Some("hello-album-1")
    );
    // A 0.3.0 plugin cannot report an album count.
    assert_eq!(results.artists.first().and_then(|a| a.album_count), None);
}
