//! Integration tests for calling a catalog plugin through the core trait.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{CatalogProvider, CatalogQuery};
use hearthsound_core::{EntityKind, ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{HELLO_CATALOG, fixture_plugin};

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

#[tokio::test]
async fn calls_the_hello_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host.install(&fixture_plugin(HELLO_CATALOG)).await.unwrap();

    let provider: Arc<dyn CatalogProvider> = host
        .catalog_provider(
            &installed,
            InstanceConfig::new(Slug::new("hello").unwrap())
                .with_server_config(r#"{"greeting":"Hi"}"#.to_owned()),
        )
        .unwrap();

    assert_eq!(provider.health().await.unwrap(), "hello-catalog ready");

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
    assert_eq!(results.tracks[0].title, "Hi, world");
    assert_eq!(
        results.tracks[0]
            .album
            .as_ref()
            .map(|album| album.id.as_str()),
        Some("hello-album-1")
    );
    assert_eq!(results.tracks[0].duration_ms, Some(180_000));
    assert!(results.albums.is_empty());
    assert!(results.artists.is_empty());

    assert_eq!(
        provider.track("missing").await,
        Err(ProviderError::NotFound)
    );

    let problems = provider.validate_config("not json").await.unwrap();
    assert!(
        !problems.is_empty(),
        "an unparsable configuration must be reported as a problem"
    );
}
