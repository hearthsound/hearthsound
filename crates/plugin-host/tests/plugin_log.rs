//! Confirms that a plugin's log lines reach the server's tracing subscriber.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{CatalogProvider, CatalogQuery};
use hearthsound_core::{EntityKind, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{HELLO_CATALOG, fixture_plugin};

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

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

#[tokio::test]
async fn a_search_logs_its_query() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = tempfile::tempdir().unwrap();
    let settings = Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    );
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings,
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host.install(&fixture_plugin(HELLO_CATALOG)).await.unwrap();

    let provider: Arc<dyn CatalogProvider> = host
        .catalog_provider(
            &installed,
            InstanceConfig::new(Slug::new("hello").unwrap()).with_server_config("{}".to_owned()),
        )
        .unwrap();

    provider
        .search(&CatalogQuery {
            text: "world".to_owned(),
            kinds: vec![EntityKind::Track],
            limit: 10,
            offset: 0,
        })
        .await
        .unwrap();

    let output = String::from_utf8(buffer(&capture).clone()).unwrap();
    assert!(
        output.contains("com.example.hello-catalog"),
        "the plugin field is missing from:\n{output}"
    );
    assert!(
        output.contains("hello"),
        "the instance field is missing from:\n{output}"
    );
    assert!(
        output.contains("search: world"),
        "the message is missing from:\n{output}"
    );
}
