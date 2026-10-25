//! Integration tests for installing and loading `.hsplugin` archives.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::manifest::{ApiVersion, SUPPORTED_APIS_LIST};
use hearthsound_plugin_host::{InstallError, PluginHost};
use hearthsound_test_support::{
    ACQUISITION_FIXTURE, ALBUM_CATALOG, ARTWORK_FIXTURE, DISCOGRAPHY_CATALOG, HELLO_CATALOG,
    HTTP_CATALOG, LEGACY_CATALOG, METADATA_FIXTURE, PROCESS_FIXTURE, SCROBBLE_FIXTURE,
    SPIN_CATALOG, VERIFICATION_FIXTURE, fixture_plugin, workspace_root,
};

/// The id the hello fixture declares.
const HELLO_ID: &str = "com.example.hello-catalog";

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

fn hello_manifest_path() -> std::path::PathBuf {
    workspace_root().join("crates/test-support/fixtures/plugins/hello-catalog/manifest.json")
}

/// Builds a host over a fresh temp directory.
async fn host(dir: &Path) -> PluginHost {
    PluginHost::new(dir.join("plugins"), settings(), database(dir).await).unwrap()
}

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// Reads an entry out of an archive.
fn read_entry(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut buffer = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut buffer).unwrap();
    buffer
}

/// Writes an archive holding a manifest and, optionally, a component.
fn write_archive(archive: &Path, manifest: &[u8], wasm: Option<&[u8]>) {
    let file = std::fs::File::create(archive).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    writer.start_file("manifest.json", options).unwrap();
    writer.write_all(manifest).unwrap();
    if let Some(wasm) = wasm {
        writer.start_file("plugin.wasm", options).unwrap();
        writer.write_all(wasm).unwrap();
    }
    writer.finish().unwrap();
}

/// Rewrites one fixture's manifest and component into a new archive.
fn archive_with_source(source: &Path, archive: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&read_entry(source, "manifest.json")).unwrap();
    edit(&mut manifest);
    let manifest = serde_json::to_vec_pretty(&manifest).unwrap();
    let wasm = read_entry(source, "plugin.wasm");
    write_archive(archive, &manifest, Some(&wasm));
}

/// Rewrites the hello fixture's manifest and component into a new archive.
fn archive_with_manifest(archive: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    archive_with_source(&fixture_plugin(HELLO_CATALOG), archive, edit);
}

#[tokio::test]
async fn installs_the_hello_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;

    let installed = host.install(&fixture_plugin(HELLO_CATALOG)).await.unwrap();

    assert_eq!(installed.manifest().id, HELLO_ID);
    assert!(installed.dir().join("manifest.json").is_file());
    assert!(installed.dir().join("plugin.wasm").is_file());
    assert!(installed.dir().join("plugin.cwasm").is_file());
}

#[tokio::test]
async fn a_second_install_of_the_same_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    host.install(&fixture_plugin(HELLO_CATALOG)).await.unwrap();

    let error = host
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap_err();

    assert!(
        matches!(error, InstallError::AlreadyInstalled { .. }),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn an_archive_without_a_component_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let manifest = std::fs::read(hello_manifest_path()).unwrap();
    let archive = dir.path().join("no-wasm.hsplugin");
    write_archive(&archive, &manifest, None);

    let error = host.install(&archive).await.unwrap_err();

    assert!(
        matches!(error, InstallError::MissingEntry("plugin.wasm")),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_manifest_promising_metadata_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let archive = dir.path().join("metadata.hsplugin");
    archive_with_manifest(&archive, |manifest| {
        manifest["provides"] = serde_json::json!(["metadata"]);
        manifest.as_object_mut().unwrap().remove("catalog");
    });

    let error = host.install(&archive).await.unwrap_err();

    match error {
        InstallError::MissingExport { interface } => {
            assert_eq!(interface, "hearthsound:plugin/metadata@0.4.0");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn an_account_link_export_needs_a_scrobble_or_discovery_layer() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let archive = dir.path().join("catalog-account-link.hsplugin");
    archive_with_source(&fixture_plugin(SCROBBLE_FIXTURE), &archive, |manifest| {
        manifest["provides"] = serde_json::json!(["catalog"]);
    });

    let error = host.install(&archive).await.unwrap_err();

    assert!(
        matches!(error, InstallError::UndeclaredExport(ref interface)
            if interface == "hearthsound:plugin/account-link@0.4.0"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn an_unsupported_api_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let archive = dir.path().join("api.hsplugin");
    archive_with_manifest(&archive, |manifest| {
        manifest["api"] = serde_json::json!("hearthsound:plugin@0.2.0");
    });

    let error = host.install(&archive).await.unwrap_err();

    assert!(
        matches!(error, InstallError::UnsupportedApi { ref api } if api == "hearthsound:plugin@0.2.0"),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("hearthsound:plugin@0.4.0")
            && message.contains("hearthsound:plugin@0.3.0"),
        "expected both hosted versions in the message, got: {message}"
    );
    assert_eq!(
        SUPPORTED_APIS_LIST,
        "hearthsound:plugin@0.4.0 and hearthsound:plugin@0.3.0"
    );
}

#[tokio::test]
async fn every_fixture_installs_and_reports_its_own_api() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;

    for fixture in [
        HELLO_CATALOG,
        ALBUM_CATALOG,
        DISCOGRAPHY_CATALOG,
        SPIN_CATALOG,
        HTTP_CATALOG,
        METADATA_FIXTURE,
        ARTWORK_FIXTURE,
        ACQUISITION_FIXTURE,
        PROCESS_FIXTURE,
        SCROBBLE_FIXTURE,
        VERIFICATION_FIXTURE,
    ] {
        let installed = host.install(&fixture_plugin(fixture)).await.unwrap();
        assert_eq!(
            installed.api(),
            ApiVersion::V0_4,
            "{fixture} should be built for the newest API"
        );
    }

    let legacy = host.install(&fixture_plugin(LEGACY_CATALOG)).await.unwrap();
    assert_eq!(legacy.api(), ApiVersion::V0_3);
}

#[tokio::test]
async fn a_manifest_newer_than_its_component_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let archive = dir.path().join("newer-manifest.hsplugin");
    archive_with_source(&fixture_plugin(LEGACY_CATALOG), &archive, |manifest| {
        manifest["api"] = serde_json::json!("hearthsound:plugin@0.4.0");
    });

    let error = host.install(&archive).await.unwrap_err();

    assert!(
        matches!(
            error,
            InstallError::ApiMismatch { ref declared, ref component }
                if declared == "hearthsound:plugin@0.4.0"
                    && component == "hearthsound:plugin@0.3.0"
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_manifest_older_than_its_component_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let archive = dir.path().join("older-manifest.hsplugin");
    archive_with_manifest(&archive, |manifest| {
        manifest["api"] = serde_json::json!("hearthsound:plugin@0.3.0");
    });

    let error = host.install(&archive).await.unwrap_err();

    assert!(
        matches!(
            error,
            InstallError::ApiMismatch { ref declared, ref component }
                if declared == "hearthsound:plugin@0.3.0"
                    && component == "hearthsound:plugin@0.4.0"
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn a_corrupt_precompiled_artifact_is_recompiled() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path()).await;
    let installed = host.install(&fixture_plugin(HELLO_CATALOG)).await.unwrap();
    let plugin_dir = installed.dir().to_path_buf();
    drop(installed);

    std::fs::write(plugin_dir.join("plugin.cwasm"), b"garbage").unwrap();

    let reloaded = host
        .load(HELLO_ID, &semver::Version::new(0, 1, 0))
        .await
        .unwrap();
    assert_eq!(reloaded.manifest().id, HELLO_ID);

    let rewritten = std::fs::read(plugin_dir.join("plugin.cwasm")).unwrap();
    assert_ne!(rewritten, b"garbage");
}
