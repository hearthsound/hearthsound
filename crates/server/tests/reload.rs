//! Full-stack tests of the reload path.
//!
//! A command-line command writes the database through its own connection while
//! the server runs. These tests simulate that by opening a second connection to
//! the same file and asserting that the running server picks the change up
//! within about a second, which is the promise `08-operations-and-release.md`
//! section 4 makes about the first-run commands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::secret::SecretKey;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::Layer;
use hearthsound_db::Database;
use hearthsound_plugin_host::{InstanceService, InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::{HELLO_CATALOG, fixture_plugin};

/// The fixture plugin's manifest id.
const HELLO_ID: &str = "com.example.hello-catalog";

/// How long a change made by another connection may take to be visible.
const RELOAD_BUDGET: Duration = Duration::from_secs(2);

/// A setting written by another connection reaches the running server.
#[tokio::test]
async fn a_setting_written_by_another_connection_is_picked_up() {
    let fixture = common::Fixture::new().await;
    assert_eq!(
        fixture
            .services
            .settings()
            .current()
            .get(keys::STREAM_PENDING_MODE)
            .unwrap(),
        "pad"
    );

    let other = Arc::new(Database::open(fixture.database_path()).await.unwrap());
    let settings = Settings::load(
        other.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();

    settings
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("hold".to_owned()),
            None,
        )
        .await
        .unwrap();
    settings
        .set(
            keys::STREAM_MAX_CONCURRENT_TRANSCODES.name(),
            SettingValue::Integer(3),
            None,
        )
        .await
        .unwrap();

    let started = Instant::now();
    loop {
        let mode = fixture
            .services
            .settings()
            .current()
            .get(keys::STREAM_PENDING_MODE)
            .unwrap();
        if mode == "hold" && fixture.services.ffmpeg().max_concurrent() == 3 {
            break;
        }
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "after {:?} the server still reports mode {mode} and cap {}",
            started.elapsed(),
            fixture.services.ffmpeg().max_concurrent()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A setting written through the server updates the cached ffmpeg cap.
#[tokio::test]
async fn a_setting_written_through_the_server_updates_the_cached_cap() {
    let fixture = common::Fixture::new().await;

    fixture
        .services
        .settings()
        .set(
            keys::STREAM_MAX_CONCURRENT_TRANSCODES.name(),
            SettingValue::Integer(5),
            None,
        )
        .await
        .unwrap();

    let started = Instant::now();
    while fixture.services.ffmpeg().max_concurrent() != 5 {
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "the ffmpeg cap is still {}",
            fixture.services.ffmpeg().max_concurrent()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A layer assignment written by another connection rebuilds the registry.
///
/// The fixture plugin is installed and configured through the running server's
/// own handles, then the layer is assigned the way `hearthsound layers set`
/// does: through a second connection to the same database file.
#[tokio::test]
async fn a_layer_assignment_written_by_another_connection_rebuilds_the_registry() {
    let fixture = common::Fixture::new().await;
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(fixture.services.database())
        .register_installed(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, HELLO_ID);
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "hello".to_owned(),
            plugin_id: HELLO_ID.to_owned(),
            display_name: "Hello".to_owned(),
            server_config: serde_json::json!({}),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();
    assert!(
        fixture
            .services
            .registry()
            .layers(Layer::Catalog)
            .is_empty()
    );

    let other = Arc::new(Database::open(fixture.database_path()).await.unwrap());
    let key = SecretKey::load(
        &fixture.services.config().secrets.key_file,
        &BTreeMap::new(),
    )
    .unwrap();
    let instances = InstanceService::new(
        Arc::clone(fixture.services.plugins()),
        PluginRepository::new(&other),
        key.secret_box(),
    );
    instances
        .set_layer(Layer::Catalog, &["hello".to_owned()])
        .await
        .unwrap();

    let started = Instant::now();
    loop {
        let assigned = fixture.services.registry().layers(Layer::Catalog);
        if assigned
            .iter()
            .any(|instance| instance.slug().as_str() == "hello")
        {
            break;
        }
        assert!(
            started.elapsed() < RELOAD_BUDGET,
            "the catalog layer still holds {} instances after {:?}",
            assigned.len(),
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
