//! Confirms that a plugin that never returns is stopped by the deadline, and
//! that enough traps disable the instance.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{CatalogProvider, CatalogQuery};
use hearthsound_core::{ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{SPIN_CATALOG, fixture_plugin};

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

fn instance(slug: &str) -> InstanceConfig {
    InstanceConfig::new(Slug::new(slug).unwrap()).with_server_config("{}".to_owned())
}

#[tokio::test]
async fn repeated_traps_disable_the_instance() {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host.install(&fixture_plugin(SPIN_CATALOG)).await.unwrap();
    let provider: Arc<dyn CatalogProvider> =
        host.catalog_provider(&installed, instance("spin")).unwrap();

    let query = CatalogQuery {
        text: "anything".to_owned(),
        kinds: vec![],
        limit: 10,
        offset: 0,
    };

    for attempt in 0..5 {
        let started = Instant::now();
        let result = provider.search(&query).await;
        assert_eq!(
            result,
            Err(ProviderError::Internal),
            "call {attempt} should have trapped"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "call {attempt} took {:?}",
            started.elapsed()
        );
    }

    let started = Instant::now();
    let result = provider.search(&query).await;
    assert_eq!(result, Err(ProviderError::Unavailable));
    assert!(
        started.elapsed() < Duration::from_millis(100),
        "a disabled instance must fail fast, took {:?}",
        started.elapsed()
    );

    let fresh: Arc<dyn CatalogProvider> = host
        .catalog_provider(&installed, instance("spin2"))
        .unwrap();
    assert_eq!(fresh.health().await.unwrap(), "spin-catalog ready");
}
