//! Integration tests for the administrative plugin lifecycle: pending install,
//! approval, the enable gate, and removal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::secret::SecretKey;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::events::{EventBus, ServerEvent};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    AdminError, InstanceService, InstanceSpec, PluginAdmin, PluginHost, PluginRepository,
    PluginSource, PluginState,
};
use hearthsound_test_support::{METADATA_FIXTURE, fixture_plugin};
use serde_json::json;

/// The id of the metadata fixture.
const METADATA_ID: &str = "com.example.metadata-fixture";

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// One test's host, repository, admin, and event bus over a temporary directory.
struct Env {
    /// Kept alive so the plugin directory outlives the host.
    dir: tempfile::TempDir,
    host: Arc<PluginHost>,
    repository: PluginRepository,
    admin: PluginAdmin,
    bus: Arc<EventBus>,
}

impl Env {
    /// Builds a host, a migrated database, and an admin service over them.
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
            .unwrap(),
        );
        let repository = PluginRepository::new(&database);
        let bus = Arc::new(EventBus::new());
        let admin = PluginAdmin::new(
            Arc::clone(&host),
            repository.clone(),
            Some(Arc::clone(&bus)),
        );
        Env {
            dir,
            host,
            repository,
            admin,
            bus,
        }
    }

    /// The directory plugin installs live under.
    fn plugins_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("plugins")
    }

    /// Installs a fixture as a sideload, pending approval, returning its id.
    async fn install_pending(&self, archive: &Path) -> String {
        self.admin
            .install_archive(archive, None, PluginSource::Sideload, None, false)
            .await
            .unwrap()
            .id
    }
}

/// The metadata fixture archive.
fn metadata_archive() -> std::path::PathBuf {
    fixture_plugin(METADATA_FIXTURE)
}

#[tokio::test]
async fn a_fresh_install_waits_for_approval() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;

    let row = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(row.state(false), PluginState::PendingApproval);
    assert!(!row.enabled);
    assert!(row.approved_permissions.is_none());
    assert!(env.plugins_dir().join(&id).join(&row.version).is_dir());
}

#[tokio::test]
async fn a_pending_install_is_kept_out_of_the_enabled_set() {
    let env = Env::new().await;
    env.install_pending(&metadata_archive()).await;

    let enabled: Vec<_> = env
        .repository
        .plugins()
        .await
        .unwrap()
        .into_iter()
        .filter(|row| row.enabled)
        .collect();
    assert!(enabled.is_empty(), "pending plugins must not be enabled");
}

#[tokio::test]
async fn approval_records_the_requested_permissions_and_enables() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;

    let approved = env.admin.approve(&id).await.unwrap();
    assert_eq!(approved.state(false), PluginState::Enabled);
    assert!(approved.enabled);
    assert!(approved.approved_permissions.is_some());
}

#[tokio::test]
async fn enabling_a_pending_plugin_is_refused_in_both_directions() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;

    let enable = env.admin.set_enabled(&id, true).await.unwrap_err();
    assert!(matches!(enable, AdminError::PendingApproval(_)), "{enable}");
    let disable = env.admin.set_enabled(&id, false).await.unwrap_err();
    assert!(
        matches!(disable, AdminError::PendingApproval(_)),
        "{disable}"
    );

    let row = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(row.state(false), PluginState::PendingApproval);
    assert!(!row.enabled);
}

#[tokio::test]
async fn approving_twice_is_refused_as_nothing_to_approve() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;
    env.admin.approve(&id).await.unwrap();

    let again = env.admin.approve(&id).await.unwrap_err();
    assert!(
        matches!(again, AdminError::NothingToApprove { .. }),
        "{again}"
    );
}

#[tokio::test]
async fn an_approved_plugin_can_be_disabled_and_enabled_again() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;
    env.admin.approve(&id).await.unwrap();

    env.admin.set_enabled(&id, false).await.unwrap();
    let disabled = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(disabled.state(false), PluginState::Disabled);

    env.admin.set_enabled(&id, true).await.unwrap();
    let enabled = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(enabled.state(false), PluginState::Enabled);
}

#[tokio::test]
async fn removal_cascades_rows_and_deletes_the_files() {
    let env = Env::new().await;
    let id = env.install_pending(&metadata_archive()).await;
    env.admin.approve(&id).await.unwrap();

    let service = InstanceService::new(
        Arc::clone(&env.host),
        env.repository.clone(),
        SecretKey::generate().secret_box(),
    );
    service
        .create(&InstanceSpec {
            slug: "meta".to_owned(),
            plugin_id: METADATA_ID.to_owned(),
            display_name: "meta".to_owned(),
            server_config: json!({}),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();

    env.admin.remove(&id).await.unwrap();

    assert!(env.repository.plugin(&id).await.unwrap().is_none());
    assert!(
        env.repository
            .instance_by_slug("meta")
            .await
            .unwrap()
            .is_none()
    );
    assert!(!env.plugins_dir().join(&id).exists());
}

#[tokio::test]
async fn removing_an_unknown_plugin_is_not_found() {
    let env = Env::new().await;
    let error = env.admin.remove("com.example.missing").await.unwrap_err();
    assert!(matches!(error, AdminError::NotFound(_)), "{error}");
}

#[tokio::test]
async fn lifecycle_writes_publish_plugin_status_events() {
    let env = Env::new().await;
    let mut events = env.bus.subscribe();

    let id = env.install_pending(&metadata_archive()).await;
    env.admin.approve(&id).await.unwrap();
    env.admin.remove(&id).await.unwrap();

    let mut states = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let ServerEvent::PluginStatus(status) = envelope.event {
            assert_eq!(status.plugin_id, id);
            states.push(status.state);
        }
    }
    assert_eq!(states, ["pending-approval", "enabled", "removed"]);
}

#[tokio::test]
async fn an_install_without_a_bus_still_records_the_plugin() {
    let env = Env::new().await;
    let admin = PluginAdmin::new(Arc::clone(&env.host), env.repository.clone(), None);
    let registration = admin
        .install_archive(
            &metadata_archive(),
            None,
            PluginSource::Sideload,
            None,
            false,
        )
        .await
        .unwrap();
    assert!(
        env.repository
            .plugin(&registration.id)
            .await
            .unwrap()
            .is_some()
    );
}
