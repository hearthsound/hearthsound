//! Integration tests for the administrative plugin lifecycle: pending install,
//! approval, the enable gate, upgrade parking, and removal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use hearthsound_config::secret::SecretKey;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::events::{EventBus, ServerEvent};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{
    AdminError, InstallError, InstanceService, InstanceSpec, PluginAdmin, PluginHost,
    PluginRepository, PluginRow, PluginSource, PluginState,
};
use hearthsound_test_support::{METADATA_FIXTURE, fixture_plugin};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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

/// Reads one entry out of an archive.
fn read_entry(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut buffer = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut buffer).unwrap();
    buffer
}

/// Writes an archive holding a manifest and the fixture's component.
fn write_archive(archive: &Path, manifest: &[u8], wasm: &[u8]) {
    let file = std::fs::File::create(archive).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    writer.start_file("manifest.json", options).unwrap();
    writer.write_all(manifest).unwrap();
    writer.start_file("plugin.wasm", options).unwrap();
    writer.write_all(wasm).unwrap();
    writer.finish().unwrap();
}

/// Repacks the metadata fixture with an edited manifest into `dir`.
///
/// The component is copied unchanged, so the archive installs exactly like the
/// fixture and only its manifest differs.
fn repacked_metadata(
    dir: &Path,
    file_name: &str,
    edit: impl FnOnce(&mut Value),
) -> std::path::PathBuf {
    let source = metadata_archive();
    let mut manifest: Value =
        serde_json::from_slice(&read_entry(&source, "manifest.json")).unwrap();
    edit(&mut manifest);
    let manifest = serde_json::to_vec(&manifest).unwrap();
    let wasm = read_entry(&source, "plugin.wasm");
    let target = dir.join(file_name);
    write_archive(&target, &manifest, &wasm);
    target
}

/// Repacks the metadata fixture at `version`, requesting `permissions`.
fn metadata_version(dir: &Path, version: &str, permissions: Value) -> std::path::PathBuf {
    repacked_metadata(dir, &format!("metadata-{version}.hsplugin"), |manifest| {
        manifest["version"] = json!(version);
        manifest["permissions"] = permissions;
    })
}

/// The manifest of an archive, parsed.
fn archive_manifest(archive: &Path) -> Value {
    serde_json::from_slice(&read_entry(archive, "manifest.json")).unwrap()
}

/// The SHA-256 of an archive file.
fn archive_sha256(archive: &Path) -> Vec<u8> {
    Sha256::digest(std::fs::read(archive).unwrap()).to_vec()
}

/// The permission set an approved row holds.
fn approved_permissions(row: &PluginRow) -> Value {
    serde_json::from_str(row.approved_permissions.as_deref().unwrap()).unwrap()
}

/// A permission set reaching one public host.
fn one_host(host: &str) -> Value {
    json!({"http": {"hosts": [host]}})
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

/// Installs an approved metadata plugin at `0.1.0` requesting `permissions`.
async fn approved_metadata(env: &Env, permissions: Value) -> String {
    let archive = metadata_version(env.dir.path(), "0.1.0", permissions);
    let id = env.install_pending(&archive).await;
    env.admin.approve(&id).await.unwrap();
    id
}

#[tokio::test]
async fn an_upgrade_covered_by_the_installed_permissions_switches_immediately() {
    let env = Env::new().await;
    let id = approved_metadata(&env, one_host("api.example.com")).await;
    let before = env.repository.plugin(&id).await.unwrap().unwrap();

    let archive = metadata_version(
        env.dir.path(),
        "0.2.0",
        json!({
            "http": {"hosts": ["api.example.com"]},
            "kv": {"maxBytes": 1048576}
        }),
    );
    env.admin
        .install_archive(&archive, None, PluginSource::Repository, None, false)
        .await
        .unwrap();

    let after = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(after.version, "0.2.0");
    assert_eq!(after.api, "hearthsound:plugin@0.2.0");
    assert_eq!(
        serde_json::from_str::<Value>(&after.manifest).unwrap(),
        archive_manifest(&archive)
    );
    assert_eq!(
        serde_json::from_str::<Value>(&before.manifest).unwrap()["version"],
        "0.1.0"
    );
    assert_eq!(after.sha256, archive_sha256(&archive));
    assert_eq!(after.source, "repository");
    assert_eq!(before.source, "sideload");
    assert_eq!(
        approved_permissions(&after),
        json!({
            "http": {"hosts": ["api.example.com"], "rateLimit": null, "maxBodyBytes": null},
            "kv": {"maxBytes": 1048576},
            "files": null,
            "process": null
        })
    );
    assert!(approved_permissions(&before)["kv"].is_null());
    assert!(after.enabled);
    assert_eq!(after.state(false), PluginState::Enabled);
    assert!(env.repository.upgrade(&id).await.unwrap().is_none());
    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());
    assert!(!env.plugins_dir().join(&id).join("0.1.0").exists());
}

#[tokio::test]
async fn an_upgrade_requesting_a_new_permission_waits_for_approval() {
    let env = Env::new().await;
    let id = approved_metadata(&env, json!({})).await;
    let before = env.repository.plugin(&id).await.unwrap().unwrap();

    let archive = metadata_version(env.dir.path(), "0.2.0", one_host("api.example.com"));
    let registration = env
        .admin
        .install_archive(&archive, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();
    assert_eq!(registration.version, "0.2.0");
    assert_eq!(registration.requested.len(), 1);
    assert_eq!(registration.requested[0].detail, "api.example.com");

    let upgrade = env.repository.upgrade(&id).await.unwrap().unwrap();
    assert_eq!(upgrade.plugin_id, id);
    assert_eq!(upgrade.version, "0.2.0");
    assert_eq!(upgrade.source, "sideload");
    assert_eq!(
        serde_json::from_str::<Value>(&upgrade.manifest).unwrap(),
        archive_manifest(&archive)
    );
    assert_eq!(upgrade.sha256, archive_sha256(&archive));

    let after = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(after.version, before.version);
    assert_eq!(after.version, "0.1.0");
    assert_eq!(after.manifest, before.manifest);
    assert_eq!(after.sha256, before.sha256);
    assert_eq!(after.approved_permissions, before.approved_permissions);
    assert!(after.enabled);
    assert_eq!(after.state(true), PluginState::UpgradePendingApproval);

    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());
    assert!(env.plugins_dir().join(&id).join("0.1.0").is_dir());
}

#[tokio::test]
async fn an_equal_version_is_already_installed_and_an_older_one_is_a_downgrade() {
    let env = Env::new().await;
    let id = approved_metadata(&env, json!({})).await;

    let archive = metadata_version(env.dir.path(), "0.2.0", json!({}));
    env.admin
        .install_archive(&archive, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();
    assert_eq!(
        env.repository.plugin(&id).await.unwrap().unwrap().version,
        "0.2.0"
    );

    let same = env
        .admin
        .install_archive(&archive, None, PluginSource::Sideload, None, false)
        .await
        .unwrap_err();
    assert!(
        matches!(same, InstallError::AlreadyInstalled { .. }),
        "{same}"
    );

    let older = metadata_version(env.dir.path(), "0.1.0", json!({}));
    let rejected = env
        .admin
        .install_archive(&older, None, PluginSource::Sideload, None, false)
        .await
        .unwrap_err();
    match rejected {
        InstallError::Downgrade {
            installed,
            requested,
            ..
        } => {
            assert_eq!(installed, "0.2.0");
            assert_eq!(requested, "0.1.0");
        }
        other => panic!("expected a downgrade, got {other}"),
    }

    let row = env.repository.plugin(&id).await.unwrap().unwrap();
    assert_eq!(row.version, "0.2.0");
    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());
    assert!(!env.plugins_dir().join(&id).join("0.1.0").exists());
}

#[tokio::test]
async fn approving_a_staged_upgrade_switches_the_version_and_prunes_the_old_one() {
    let env = Env::new().await;
    let id = approved_metadata(&env, json!({})).await;

    let archive = metadata_version(env.dir.path(), "0.2.0", one_host("api.example.com"));
    env.admin
        .install_archive(&archive, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();

    let applied = env.admin.approve(&id).await.unwrap();
    assert_eq!(applied.version, "0.2.0");
    assert_eq!(applied.source, "sideload");
    assert_eq!(applied.sha256, archive_sha256(&archive));
    assert_eq!(
        serde_json::from_str::<Value>(&applied.manifest).unwrap(),
        archive_manifest(&archive)
    );
    assert_eq!(
        approved_permissions(&applied),
        json!({
            "http": {"hosts": ["api.example.com"], "rateLimit": null, "maxBodyBytes": null},
            "kv": null,
            "files": null,
            "process": null
        })
    );
    assert!(applied.enabled);
    assert_eq!(applied.state(false), PluginState::Enabled);

    assert!(env.repository.upgrade(&id).await.unwrap().is_none());
    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());
    assert!(!env.plugins_dir().join(&id).join("0.1.0").exists());
}

#[tokio::test]
async fn declining_a_staged_upgrade_keeps_the_installed_version() {
    let env = Env::new().await;
    let id = approved_metadata(&env, one_host("api.example.com")).await;
    let before = env.repository.plugin(&id).await.unwrap().unwrap();

    let refused = env.admin.decline_upgrade(&id).await.unwrap_err();
    assert!(
        matches!(refused, AdminError::NoUpgradeStaged(_)),
        "{refused}"
    );

    let archive = metadata_version(env.dir.path(), "0.2.0", one_host("cdn.example.com"));
    env.admin
        .install_archive(&archive, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();
    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());

    let kept = env.admin.decline_upgrade(&id).await.unwrap();
    assert_eq!(kept.version, "0.1.0");
    assert_eq!(kept.manifest, before.manifest);
    assert_eq!(kept.sha256, before.sha256);
    assert_eq!(kept.approved_permissions, before.approved_permissions);
    assert!(kept.enabled);
    assert_eq!(kept.state(false), PluginState::Enabled);

    assert!(env.repository.upgrade(&id).await.unwrap().is_none());
    assert!(env.plugins_dir().join(&id).join("0.1.0").is_dir());
    assert!(!env.plugins_dir().join(&id).join("0.2.0").exists());
}

#[tokio::test]
async fn a_second_staged_upgrade_replaces_the_first() {
    let env = Env::new().await;
    let id = approved_metadata(&env, json!({})).await;

    let first = metadata_version(env.dir.path(), "0.2.0", one_host("api.example.com"));
    env.admin
        .install_archive(&first, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();
    assert!(env.plugins_dir().join(&id).join("0.2.0").is_dir());

    let second = metadata_version(env.dir.path(), "0.3.0", one_host("cdn.example.com"));
    env.admin
        .install_archive(&second, None, PluginSource::Sideload, None, false)
        .await
        .unwrap();

    let upgrade = env.repository.upgrade(&id).await.unwrap().unwrap();
    assert_eq!(upgrade.version, "0.3.0");
    assert_eq!(
        serde_json::from_str::<Value>(&upgrade.manifest).unwrap(),
        archive_manifest(&second)
    );
    assert_eq!(upgrade.sha256, archive_sha256(&second));

    assert_eq!(
        env.repository.plugin(&id).await.unwrap().unwrap().version,
        "0.1.0"
    );
    assert!(env.plugins_dir().join(&id).join("0.3.0").is_dir());
    assert!(env.plugins_dir().join(&id).join("0.1.0").is_dir());
    assert!(!env.plugins_dir().join(&id).join("0.2.0").exists());
}
