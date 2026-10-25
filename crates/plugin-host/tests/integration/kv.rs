//! Integration test for how a plugin's key-value read failure is reported.
//!
//! A 0.4.0 plugin reads through `host-kv`, whose `get` returns
//! `result<option<list<u8>>, string>`: a storage failure is an error the guest
//! sees, and the fixture turns that error into a provider error. A 0.3.0 plugin
//! keeps the frozen silent miss, because its interface has no error channel.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{CatalogProvider, CatalogQuery};
use hearthsound_core::{EntityKind, ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{KV_FIXTURE, LEGACY_CATALOG, fixture_plugin};
use tracing_subscriber::fmt::MakeWriter;

/// A writer that appends everything written to a shared buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

/// Locks the capture buffer, ignoring poisoning.
fn buffer(capture: &Capture) -> MutexGuard<'_, Vec<u8>> {
    match capture.0.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        buffer(self).extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

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

/// A search for one track.
fn query() -> CatalogQuery {
    CatalogQuery {
        text: "probe".to_owned(),
        kinds: vec![EntityKind::Track],
        limit: 10,
        offset: 0,
    }
}

#[tokio::test]
async fn a_broken_store_reaches_a_0_4_0_guest_as_an_error() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().unwrap();
    let database = database(dir.path()).await;
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        Arc::clone(&database),
    )
    .unwrap();
    let kv = host.install(&fixture_plugin(KV_FIXTURE)).await.unwrap();
    let legacy = host.install(&fixture_plugin(LEGACY_CATALOG)).await.unwrap();

    let kv_provider: Arc<dyn CatalogProvider> = host
        .catalog_provider(
            &kv,
            InstanceConfig::new(Slug::new("kv").unwrap()).with_server_config("{}".to_owned()),
        )
        .unwrap();
    let legacy_provider: Arc<dyn CatalogProvider> = host
        .catalog_provider(
            &legacy,
            InstanceConfig::new(Slug::new("legacy").unwrap()).with_server_config("{}".to_owned()),
        )
        .unwrap();

    // An intact store with no row is a miss, not an error.
    let results = kv_provider.search(&query()).await.unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].title, "no value");

    sqlx::query("DROP TABLE plugin_kv")
        .execute(database.writer())
        .await
        .unwrap();

    assert_eq!(
        kv_provider.search(&query()).await,
        Err(ProviderError::Internal)
    );
    let logged = String::from_utf8(buffer(&capture).clone()).unwrap();
    assert!(
        logged.contains("the plugin's storage is unavailable"),
        "expected the guest's reason in the log, got:\n{logged}"
    );

    // The 0.3.0 guest reports the same failure as a miss.
    let results = legacy_provider.search(&query()).await.unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].title, "Hello, probe");
}
