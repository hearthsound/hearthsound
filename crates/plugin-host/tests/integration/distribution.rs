//! Integration tests for how plugins are distributed: installing them from the
//! official index, from a repository an administrator registered, and from a
//! repository URL that is registered on the way in; and the background check
//! that looks for newer versions of what is installed.
//!
//! Every request goes to a mock HTTP server, so the tests exercise the real
//! download, signature-verification and integrity paths without a network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::{Layer, Priority, new_uuid_v7};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{Job, JobHandler, JobKind, JobState};
use hearthsound_plugin_host::manifest::{ApiVersion, NEWEST_API};
use hearthsound_plugin_host::{
    IndexClient, InstallError, InstanceService, InstanceSpec, PluginAdmin, PluginHost,
    PluginInstaller, PluginRepository, PluginSource, PluginState, ProviderRegistry, RepositoryRef,
    UPDATE_CHECK_JOB_KIND, Unavailable, UpdateCheckJobHandler, UpdateChecker,
};
use hearthsound_test_support::{LEGACY_CATALOG, METADATA_FIXTURE, fixture_plugin};
use minisign::KeyPair;
use minisign_verify::PublicKey;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The id of the metadata fixture.
const METADATA_ID: &str = "com.example.metadata-fixture";

/// The id of the catalog fixture built for the previous plugin API.
const LEGACY_ID: &str = "com.example.legacy-catalog";

/// The path the official index is served at in these tests.
const OFFICIAL_PATH: &str = "/v1/index.json";

/// The path a third-party repository's index is served at in these tests.
const REPOSITORY_PATH: &str = "/repo/index.json";

/// The path the metadata fixture's archive is served at in these tests.
const ARCHIVE_PATH: &str = "/download/metadata.hsplugin";

/// The path the metadata fixture's newer archive is served at in these tests.
const NEW_ARCHIVE_PATH: &str = "/download/metadata-0.2.0.hsplugin";

/// One signing key pair the tests publish an index with.
struct Signer {
    /// The key pair itself.
    pair: KeyPair,
}

impl Signer {
    /// Generates a fresh key pair.
    fn new() -> Signer {
        Signer {
            pair: KeyPair::generate_unencrypted_keypair().unwrap(),
        }
    }

    /// Returns the base64 public key of this signer.
    fn public_key(&self) -> String {
        self.pair.pk.to_base64()
    }

    /// Returns the public key as the verifier uses it.
    fn verify_key(&self) -> PublicKey {
        PublicKey::from_base64(&self.public_key()).unwrap()
    }

    /// Signs one document and returns the `.minisig` text.
    fn sign(&self, document: &str) -> String {
        minisign::sign(
            Some(&self.pair.pk),
            &self.pair.sk,
            document.as_bytes(),
            None,
            None,
        )
        .unwrap()
        .to_string()
    }
}

/// One test's services: a mock index server, a plugin host over a temporary
/// directory, the repository rows, the admin write path, and the installer.
struct Env {
    /// Keeps the plugin directory and the test archives alive.
    dir: tempfile::TempDir,
    /// The server the indexes and archives are served from.
    server: MockServer,
    /// The key the official index is signed with.
    signer: Signer,
    /// The plugin host the instances are built through.
    host: Arc<PluginHost>,
    /// The key material instance secrets are encrypted with.
    secrets: SecretBox,
    /// The repository rows.
    repository: PluginRepository,
    /// The admin write path every install goes through.
    admin: Arc<PluginAdmin>,
    /// The installer under test.
    installer: Arc<PluginInstaller>,
}

impl Env {
    /// Builds the services with the default settings.
    async fn new() -> Env {
        Env::with_settings(&[]).await
    }

    /// Builds the services with `plugins.*` settings overridden.
    ///
    /// # Parameters
    ///
    /// - `settings`: `HEARTHSOUND__` environment variable names and values.
    async fn with_settings(env: &[(&str, &str)]) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let signer = Signer::new();
        let settings = settings(env);
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let database = Arc::new(database);
        let host = Arc::new(
            PluginHost::new(dir.path().join("plugins"), settings, Arc::clone(&database)).unwrap(),
        );
        let repository = PluginRepository::new(&database);
        let secrets = SecretKey::generate().secret_box();
        let admin = Arc::new(PluginAdmin::new(
            Arc::clone(&host),
            repository.clone(),
            None,
        ));
        let index = IndexClient::new(http(), official_url(&server), signer.verify_key());
        let installer = Arc::new(PluginInstaller::new(
            Arc::clone(&admin),
            repository.clone(),
            index,
            dir.path().join("plugins"),
        ));
        Env {
            dir,
            server,
            signer,
            host,
            secrets,
            repository,
            admin,
            installer,
        }
    }

    /// The directory plugin installs live under.
    fn plugins_dir(&self) -> PathBuf {
        self.dir.path().join("plugins")
    }

    /// The URL of the third-party repository's index.
    fn repository_url(&self) -> String {
        format!("{}{REPOSITORY_PATH}", self.server.uri())
    }

    /// The URL the metadata fixture's archive is served at.
    fn archive_url(&self) -> String {
        format!("{}{ARCHIVE_PATH}", self.server.uri())
    }

    /// The URL the metadata fixture's newer archive is served at.
    fn new_archive_url(&self) -> String {
        format!("{}{NEW_ARCHIVE_PATH}", self.server.uri())
    }

    /// The update checker over this environment's installer.
    fn checker(&self) -> Arc<UpdateChecker> {
        Arc::new(UpdateChecker::new(
            Arc::clone(&self.installer),
            self.repository.clone(),
        ))
    }

    /// The instance service over this environment.
    fn service(&self) -> InstanceService {
        InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// An empty provider registry over this environment.
    fn registry(&self) -> ProviderRegistry {
        ProviderRegistry::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        )
    }

    /// Serves the official index and its signature.
    async fn serve_official(&self, document: &str) {
        serve_text(&self.server, OFFICIAL_PATH, document).await;
        serve_text(
            &self.server,
            &format!("{OFFICIAL_PATH}.minisig"),
            &self.signer.sign(document),
        )
        .await;
    }

    /// Serves a repository's index document, unsigned.
    async fn serve_repository(&self, document: &str) {
        serve_text(&self.server, REPOSITORY_PATH, document).await;
    }
}

/// Builds the HTTP client the tests fetch with.
fn http() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::new()
}

/// Returns the official index URL the mock server serves.
fn official_url(server: &MockServer) -> String {
    format!("{}{OFFICIAL_PATH}", server.uri())
}

/// Resolves the settings, with `plugins.*` overrides given as environment
/// variable names and values.
fn settings(env: &[(&str, &str)]) -> Arc<SettingsStore> {
    let env = env
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect::<BTreeMap<_, _>>();
    Arc::new(
        SettingsStore::resolve(
            &SettingsRegistry::builtin(),
            &SettingSources::new(env, BTreeMap::new()),
        )
        .unwrap(),
    )
}

/// Serves one 200 response with a text body.
async fn serve_text(server: &MockServer, document_path: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(document_path))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

/// Serves one 200 response with a binary body.
async fn serve_bytes(server: &MockServer, document_path: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(document_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

/// Builds one index document.
fn document(plugins: &[Value]) -> String {
    json!({
        "formatVersion": 1,
        "name": "Test index",
        "generatedAt": "2026-01-01T00:00:00Z",
        "plugins": plugins,
    })
    .to_string()
}

/// Builds one plugin entry.
fn plugin(id: &str, official: bool, versions: &[Value]) -> Value {
    json!({
        "id": id,
        "name": "Metadata fixture",
        "description": "A metadata plugin.",
        "provides": ["metadata"],
        "homepage": "https://example.com/metadata",
        "license": "MIT",
        "official": official,
        "versions": versions,
    })
}

/// Builds one version entry for an archive.
fn version(version: &str, api: &str, url: &str, bytes: &[u8], permissions: Value) -> Value {
    json!({
        "version": version,
        "api": api,
        "url": url,
        "sha256": hex(&Sha256::digest(bytes)),
        "size": bytes.len(),
        "publishedAt": "2026-01-01T00:00:00Z",
        "changelog": "A release.",
        "permissions": permissions,
    })
}

/// Renders bytes as lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Reads one entry out of an archive.
fn read_entry(archive: &Path, name: &str) -> Vec<u8> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
    let mut buffer = Vec::new();
    zip.by_name(name).unwrap().read_to_end(&mut buffer).unwrap();
    buffer
}

/// Writes an archive holding a manifest and one component.
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

/// The bytes of the metadata fixture archive.
fn metadata_bytes() -> Vec<u8> {
    std::fs::read(fixture_plugin(METADATA_FIXTURE)).unwrap()
}

/// The bytes of the catalog fixture built for the previous plugin API.
fn legacy_bytes() -> Vec<u8> {
    std::fs::read(fixture_plugin(LEGACY_CATALOG)).unwrap()
}

/// The bytes of the metadata fixture repacked at `version` with `permissions`.
///
/// The component is copied unchanged, so the archive installs exactly like the
/// fixture and only its manifest differs.
fn metadata_bytes_at(dir: &Path, version: &str, permissions: Value) -> Vec<u8> {
    let source = fixture_plugin(METADATA_FIXTURE);
    let mut manifest: Value =
        serde_json::from_slice(&read_entry(&source, "manifest.json")).unwrap();
    manifest["version"] = json!(version);
    manifest["permissions"] = permissions;
    let wasm = read_entry(&source, "plugin.wasm");
    let target = dir.join(format!("metadata-{version}.hsplugin"));
    write_archive(&target, &serde_json::to_vec(&manifest).unwrap(), &wasm);
    std::fs::read(&target).unwrap()
}

/// Asserts the plugins directory holds no leftover download.
fn assert_no_downloads(env: &Env) {
    let leftovers: Vec<String> = std::fs::read_dir(env.plugins_dir())
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".download-"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "the download was not removed: {leftovers:?}"
    );
}

/// Asserts nothing of the plugin was written or recorded.
async fn assert_untouched(env: &Env) {
    assert!(env.repository.plugins().await.unwrap().is_empty());
    assert!(!env.plugins_dir().join(METADATA_ID).exists());
    assert_no_downloads(env);
}

#[tokio::test]
async fn an_install_from_a_signed_index_is_enabled() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    let document = document(&[plugin(
        METADATA_ID,
        true,
        &[version(
            "0.1.0",
            NEWEST_API,
            &env.archive_url(),
            &bytes,
            json!({}),
        )],
    )]);
    env.serve_official(&document).await;

    let registration = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap();

    assert_eq!(registration.id, METADATA_ID);
    assert_eq!(registration.version, "0.1.0");
    assert_eq!(registration.api, NEWEST_API);
    assert_eq!(registration.source, PluginSource::Official);

    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.state(false), PluginState::Enabled);
    assert_eq!(row.source, "official");
    assert_eq!(row.repository_id, None);
    assert_eq!(row.sha256, Sha256::digest(&bytes).to_vec());
    assert!(env.plugins_dir().join(METADATA_ID).join("0.1.0").is_dir());
    assert_no_downloads(&env);
}

#[tokio::test]
async fn a_plugin_built_for_the_previous_api_installs_from_the_index() {
    let env = Env::new().await;
    let bytes = legacy_bytes();
    let legacy_path = "/download/legacy.hsplugin";
    serve_bytes(&env.server, legacy_path, bytes.clone()).await;
    let archive_url = format!("{}{legacy_path}", env.server.uri());
    env.serve_official(&document(&[plugin(
        LEGACY_ID,
        true,
        &[version(
            "0.1.0",
            "hearthsound:plugin@0.3.0",
            &archive_url,
            &bytes,
            json!({}),
        )],
    )]))
    .await;

    let registration = env
        .installer
        .install_from_index(LEGACY_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap();

    assert_eq!(registration.id, LEGACY_ID);
    assert_eq!(registration.api, "hearthsound:plugin@0.3.0");
    let installed = env
        .host
        .load(LEGACY_ID, &semver::Version::new(0, 1, 0))
        .await
        .unwrap();
    assert_eq!(installed.api(), ApiVersion::V0_3);
    assert_no_downloads(&env);
}

#[tokio::test]
async fn a_digest_mismatch_leaves_no_row_and_no_files() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    let mut entry = version("0.1.0", NEWEST_API, &env.archive_url(), &bytes, json!({}));
    entry["sha256"] = json!("00".repeat(32));
    env.serve_official(&document(&[plugin(METADATA_ID, true, &[entry])]))
        .await;

    let error = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap_err();

    assert!(
        matches!(error, InstallError::IntegrityFailed { .. }),
        "expected an integrity failure, got {error:?}"
    );
    assert_untouched(&env).await;
}

#[tokio::test]
async fn a_size_mismatch_leaves_no_row_and_no_files() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    let mut entry = version("0.1.0", NEWEST_API, &env.archive_url(), &bytes, json!({}));
    entry["size"] = json!(bytes.len() + 1);
    env.serve_official(&document(&[plugin(METADATA_ID, true, &[entry])]))
        .await;

    let error = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap_err();

    assert!(
        matches!(
            error,
            InstallError::SizeMismatch {
                declared,
                received,
            } if declared == bytes.len() as u64 + 1 && received == bytes.len() as u64
        ),
        "expected a size mismatch, got {error:?}"
    );
    assert_untouched(&env).await;
}

#[tokio::test]
async fn the_newest_compatible_version_is_installed() {
    let env = Env::new().await;
    let old = metadata_bytes();
    let new = metadata_bytes_at(env.dir.path(), "0.2.0", json!({}));
    serve_bytes(&env.server, ARCHIVE_PATH, new.clone()).await;
    env.serve_official(&document(&[plugin(
        METADATA_ID,
        true,
        &[
            version(
                "0.1.0",
                "hearthsound:plugin@9.9.9",
                &env.archive_url(),
                &old,
                json!({}),
            ),
            version("0.2.0", NEWEST_API, &env.archive_url(), &new, json!({})),
        ],
    )]))
    .await;

    let registration = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap();

    assert_eq!(registration.version, "0.2.0");
    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.version, "0.2.0");
    assert_eq!(row.sha256, Sha256::digest(&new).to_vec());
    assert_no_downloads(&env);
}

#[tokio::test]
async fn a_requested_version_must_exist_and_be_compatible() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    env.serve_official(&document(&[plugin(
        METADATA_ID,
        true,
        &[
            version(
                "0.1.0",
                "hearthsound:plugin@9.9.9",
                &env.archive_url(),
                &bytes,
                json!({}),
            ),
            version("0.2.0", NEWEST_API, &env.archive_url(), &bytes, json!({})),
        ],
    )]))
    .await;

    let missing = env
        .installer
        .install_from_index(METADATA_ID, Some("9.9.9"), Some(RepositoryRef::Official))
        .await
        .unwrap_err();
    assert!(
        matches!(
            missing,
            InstallError::Unavailable(Unavailable::NoSuchVersion { ref requested, .. })
                if requested == "9.9.9"
        ),
        "expected a missing version, got {missing:?}"
    );

    let incompatible = env
        .installer
        .install_from_index(METADATA_ID, Some("0.1.0"), Some(RepositoryRef::Official))
        .await
        .unwrap_err();
    assert!(
        matches!(
            incompatible,
            InstallError::Unavailable(Unavailable::NoCompatibleVersion { .. })
        ),
        "expected an incompatible version, got {incompatible:?}"
    );
    assert_untouched(&env).await;
}

#[tokio::test]
async fn an_incompatible_only_plugin_is_reported_unavailable() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    env.serve_official(&document(&[plugin(
        METADATA_ID,
        true,
        &[version(
            "0.1.0",
            "hearthsound:plugin@0.2.0",
            &env.archive_url(),
            &bytes,
            json!({}),
        )],
    )]))
    .await;

    let error = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Official))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            InstallError::Unavailable(Unavailable::NoCompatibleVersion { .. })
        ),
        "expected an unavailable plugin, got {error:?}"
    );

    let unknown = env
        .installer
        .install_from_index("com.example.absent", None, Some(RepositoryRef::Official))
        .await
        .unwrap_err();
    assert!(
        matches!(unknown, InstallError::UnknownPlugin { ref id, .. } if id == "com.example.absent"),
        "expected an unknown plugin, got {unknown:?}"
    );

    let report = env
        .installer
        .available(Some(RepositoryRef::Official))
        .await
        .unwrap();
    assert!(report.errors.is_empty());
    assert_eq!(report.plugins.len(), 1);
    let offered = &report.plugins[0];
    assert_eq!(offered.id, METADATA_ID);
    assert_eq!(offered.version, None);
    assert!(offered.official);
    assert!(offered.verified);
    assert_eq!(offered.repository_id, None);
    assert!(matches!(
        offered.unavailable,
        Some(Unavailable::NoCompatibleVersion { .. })
    ));
}

#[tokio::test]
async fn the_listing_merges_sources_and_reports_the_failed_one() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    env.serve_official(&document(&[plugin(
        METADATA_ID,
        true,
        &[version(
            "0.1.0",
            NEWEST_API,
            &env.archive_url(),
            &bytes,
            json!({"http": {"hosts": ["api.example.com"]}, "kv": {"maxBytes": 2097152}}),
        )],
    )]))
    .await;
    env.serve_repository(&document(&[plugin(
        "com.example.other",
        false,
        &[version(
            "1.0.0",
            NEWEST_API,
            &env.archive_url(),
            &bytes,
            json!({}),
        )],
    )]))
    .await;
    let registered = env
        .repository
        .insert_repository(&env.repository_url(), Some("Example"), None, true)
        .await
        .unwrap();
    let broken_url = format!("{}/broken/index.json", env.server.uri());
    let broken = env
        .repository
        .insert_repository(&broken_url, None, None, true)
        .await
        .unwrap();

    let report = env.installer.available(None).await.unwrap();

    let listed: Vec<&str> = report
        .plugins
        .iter()
        .map(|plugin| plugin.id.as_str())
        .collect();
    assert_eq!(listed, [METADATA_ID, "com.example.other"]);

    let official = &report.plugins[0];
    assert!(official.official && official.verified);
    assert_eq!(official.version.as_deref(), Some("0.1.0"));
    let described: Vec<(&str, &str, &str)> = official
        .permissions
        .iter()
        .map(|permission| {
            (
                permission.scope,
                permission.risk.as_str(),
                permission.detail.as_str(),
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            ("http.hosts", "low", "api.example.com"),
            ("kv.maxBytes", "low", "2097152 bytes"),
        ]
    );
    let third_party = &report.plugins[1];
    assert!(!third_party.official);
    assert!(!third_party.verified);
    assert_eq!(third_party.repository_id, Some(registered.id));

    assert_eq!(report.errors.len(), 1);
    assert_eq!(report.errors[0].source, broken_url);

    let fetched = env
        .repository
        .repository(registered.id)
        .await
        .unwrap()
        .unwrap();
    assert!(fetched.last_fetched_at.is_some());
    assert_eq!(fetched.last_error, None);
    let failed = env.repository.repository(broken.id).await.unwrap().unwrap();
    assert!(failed.last_fetched_at.is_some());
    assert!(failed.last_error.is_some());

    // Listing one repository covers that repository alone.
    let only = env
        .installer
        .available(Some(RepositoryRef::Id(registered.id)))
        .await
        .unwrap();
    assert_eq!(only.plugins.len(), 1);
    assert_eq!(only.plugins[0].id, "com.example.other");
}

#[tokio::test]
async fn a_url_install_registers_the_repository_once() {
    let env = Env::new().await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    env.serve_repository(&document(&[plugin(
        METADATA_ID,
        false,
        &[version(
            "0.1.0",
            NEWEST_API,
            &env.archive_url(),
            &bytes,
            json!({}),
        )],
    )]))
    .await;

    let registration = env
        .installer
        .install_from_url(METADATA_ID, &env.repository_url(), None)
        .await
        .unwrap();

    assert_eq!(registration.source, PluginSource::Repository);
    let rows = env.repository.repositories().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].url, env.repository_url());
    assert!(rows[0].enabled);
    assert!(rows[0].last_fetched_at.is_some());
    assert_eq!(rows[0].last_error, None);

    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.source, "repository");
    assert_eq!(row.repository_id, Some(rows[0].id));
    assert_eq!(row.state(false), PluginState::Enabled);
    assert_no_downloads(&env);

    // A second install of the same version uses the registered repository
    // rather than adding another row.
    let again = env
        .installer
        .install_from_url(METADATA_ID, &env.repository_url(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(again, InstallError::AlreadyInstalled { .. }),
        "expected an already-installed error, got {again:?}"
    );
    assert_eq!(env.repository.repositories().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_third_party_gate_refuses_an_unregistered_url() {
    let env = Env::with_settings(&[(
        "HEARTHSOUND__PLUGINS__ALLOW_THIRD_PARTY_REPOSITORIES",
        "false",
    )])
    .await;
    let bytes = metadata_bytes();
    serve_bytes(&env.server, ARCHIVE_PATH, bytes.clone()).await;
    env.serve_repository(&document(&[plugin(
        METADATA_ID,
        false,
        &[version(
            "0.1.0",
            NEWEST_API,
            &env.archive_url(),
            &bytes,
            json!({}),
        )],
    )]))
    .await;

    let error = env
        .installer
        .install_from_index(
            METADATA_ID,
            None,
            Some(RepositoryRef::Url(env.repository_url())),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, InstallError::RepositoriesDisabled),
        "expected the gate to refuse the URL, got {error:?}"
    );
    assert!(env.repository.repositories().await.unwrap().is_empty());
    assert_untouched(&env).await;

    // A repository an administrator registered while the gate was on stays
    // usable, and one switched off is refused.
    let registered = env
        .repository
        .insert_repository(&env.repository_url(), None, None, true)
        .await
        .unwrap();
    let installed = env
        .installer
        .install_from_url(METADATA_ID, &env.repository_url(), None)
        .await
        .unwrap();
    assert_eq!(installed.source, PluginSource::Repository);

    env.admin.remove(METADATA_ID).await.unwrap();
    env.repository
        .update_repository(registered.id, None, None, false)
        .await
        .unwrap();
    let disabled = env
        .installer
        .install_from_index(METADATA_ID, None, Some(RepositoryRef::Id(registered.id)))
        .await
        .unwrap_err();
    assert!(
        matches!(disabled, InstallError::RepositoryDisabled { .. }),
        "expected a disabled repository error, got {disabled:?}"
    );
}

/// Serves the official index offering 0.1.0 and 0.2.0 of the metadata fixture.
///
/// The 0.2.0 archive is repacked with `permissions`, so an upgrade can be made
/// to ask for nothing new or for something new.
///
/// # Returns
///
/// The bytes of the 0.2.0 archive.
async fn serve_two_versions(env: &Env, permissions: Value) -> Vec<u8> {
    let old = metadata_bytes();
    let new = metadata_bytes_at(env.dir.path(), "0.2.0", permissions.clone());
    serve_bytes(&env.server, ARCHIVE_PATH, old.clone()).await;
    serve_bytes(&env.server, NEW_ARCHIVE_PATH, new.clone()).await;
    let document = document(&[plugin(
        METADATA_ID,
        true,
        &[
            version("0.1.0", NEWEST_API, &env.archive_url(), &old, json!({})),
            version(
                "0.2.0",
                NEWEST_API,
                &env.new_archive_url(),
                &new,
                permissions,
            ),
        ],
    )]);
    env.serve_official(&document).await;
    new
}

/// Installs the metadata fixture at 0.1.0 from the official index.
async fn install_v1(env: &Env) {
    env.installer
        .install_from_index(METADATA_ID, Some("0.1.0"), Some(RepositoryRef::Official))
        .await
        .unwrap();
}

/// Builds the job the update-check handler is registered under.
fn update_job() -> Job {
    Job {
        id: new_uuid_v7(),
        kind: JobKind::new(UPDATE_CHECK_JOB_KIND),
        payload: None,
        state: JobState::Running,
        priority: Priority::Queue,
        attempts: 1,
        run_at: 0,
        locked_until: None,
        last_error: None,
        created_at: 0,
        updated_at: 0,
    }
}

#[tokio::test]
async fn a_newer_compatible_version_is_listed() {
    let env = Env::new().await;
    serve_two_versions(&env, json!({})).await;
    env.installer
        .install_from_index(METADATA_ID, Some("0.1.0"), Some(RepositoryRef::Official))
        .await
        .unwrap();

    let updates = env.checker().check().await;

    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(update.plugin_id, METADATA_ID);
    assert_eq!(update.current, "0.1.0");
    assert_eq!(update.version, "0.2.0");
    assert_eq!(update.source, PluginSource::Official);
    assert_eq!(update.repository_id, None);
}

#[tokio::test]
async fn a_bundled_plugin_is_checked_against_the_official_index() {
    let env = Env::new().await;
    serve_two_versions(&env, json!({})).await;
    env.admin
        .install_archive(
            &fixture_plugin(METADATA_FIXTURE),
            None,
            PluginSource::Bundled,
            None,
        )
        .await
        .unwrap();

    let updates = env.checker().check().await;

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].current, "0.1.0");
    assert_eq!(updates[0].version, "0.2.0");
    assert_eq!(updates[0].source, PluginSource::Bundled);
}

#[tokio::test]
async fn a_sideloaded_plugin_is_never_updated() {
    let env = Env::new().await;
    serve_two_versions(&env, json!({})).await;
    env.admin
        .install_archive(
            &fixture_plugin(METADATA_FIXTURE),
            None,
            PluginSource::Sideload,
            None,
        )
        .await
        .unwrap();

    // The official index publishes a newer version of the same plugin, and the
    // sideloaded install is still not checked against it.
    assert!(env.checker().check().await.is_empty());
}

#[tokio::test]
async fn a_repository_plugin_is_checked_against_its_repository() {
    let env = Env::new().await;
    let old = metadata_bytes();
    let new = metadata_bytes_at(env.dir.path(), "0.2.0", json!({}));
    serve_bytes(&env.server, ARCHIVE_PATH, old.clone()).await;
    serve_bytes(&env.server, NEW_ARCHIVE_PATH, new.clone()).await;
    env.serve_repository(&document(&[plugin(
        METADATA_ID,
        false,
        &[
            version("0.1.0", NEWEST_API, &env.archive_url(), &old, json!({})),
            version("0.2.0", NEWEST_API, &env.new_archive_url(), &new, json!({})),
        ],
    )]))
    .await;
    // The official index publishes a much newer version of the same plugin,
    // which a plugin installed from a repository is not checked against.
    env.serve_official(&document(&[plugin(
        METADATA_ID,
        true,
        &[version(
            "9.0.0",
            NEWEST_API,
            &env.new_archive_url(),
            &new,
            json!({}),
        )],
    )]))
    .await;

    env.installer
        .install_from_url(METADATA_ID, &env.repository_url(), Some("0.1.0"))
        .await
        .unwrap();

    let updates = env.checker().check().await;

    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].version, "0.2.0");
    assert_eq!(updates[0].source, PluginSource::Repository);
    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert!(row.repository_id.is_some());
    assert_eq!(updates[0].repository_id, row.repository_id);
}

#[tokio::test]
async fn auto_update_applies_an_upgrade_that_asks_for_nothing_new() {
    let env = Env::with_settings(&[("HEARTHSOUND__PLUGINS__AUTO_UPDATE", "true")]).await;
    serve_two_versions(&env, json!({})).await;
    install_v1(&env).await;

    let checker = env.checker();
    let updates = checker.check().await;
    assert_eq!(updates.len(), 1);

    let applied = checker.apply_auto_updates(&updates).await;

    assert_eq!(applied, updates);
    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.version, "0.2.0");
    assert!(row.enabled);
    assert_eq!(row.state(false), PluginState::Enabled);
    assert!(env.repository.upgrade(METADATA_ID).await.unwrap().is_none());
    assert!(env.plugins_dir().join(METADATA_ID).join("0.2.0").is_dir());
    assert!(!env.plugins_dir().join(METADATA_ID).join("0.1.0").exists());
    assert_no_downloads(&env);

    // The plugin now serves the newest version, so nothing is left to install.
    assert!(checker.check().await.is_empty());
}

#[tokio::test]
async fn an_upgrade_that_asks_for_more_waits_for_review() {
    let env = Env::with_settings(&[("HEARTHSOUND__PLUGINS__AUTO_UPDATE", "true")]).await;
    let permissions = json!({"http": {"hosts": ["api.example.com"]}});
    serve_two_versions(&env, permissions).await;
    install_v1(&env).await;

    let checker = env.checker();
    let updates = checker.check().await;
    assert_eq!(updates.len(), 1);

    let applied = checker.apply_auto_updates(&updates).await;

    assert_eq!(applied, updates);
    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.version, "0.1.0");
    assert_eq!(row.state(true), PluginState::UpgradePendingApproval);
    let staged = env.repository.upgrade(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(staged.version, "0.2.0");
    assert!(env.plugins_dir().join(METADATA_ID).join("0.1.0").is_dir());
    assert!(env.plugins_dir().join(METADATA_ID).join("0.2.0").is_dir());

    // The staged version is not installed a second time.
    assert!(checker.apply_auto_updates(&updates).await.is_empty());

    // Applying it switches the plugin over and prunes the version it replaced.
    let applied = env.admin.apply_upgrade(METADATA_ID).await.unwrap();
    assert_eq!(applied.version, "0.2.0");
    assert!(!env.plugins_dir().join(METADATA_ID).join("0.1.0").exists());
}

#[tokio::test]
async fn an_automatic_update_is_served_without_a_restart() {
    let env = Env::with_settings(&[("HEARTHSOUND__PLUGINS__AUTO_UPDATE", "true")]).await;
    serve_two_versions(&env, json!({})).await;
    install_v1(&env).await;

    let service = env.service();
    service
        .create(&InstanceSpec {
            slug: "meta1".to_owned(),
            plugin_id: METADATA_ID.to_owned(),
            display_name: "meta1 display".to_owned(),
            server_config: json!({}),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    service
        .set_layer(Layer::Metadata, &["meta1".to_owned()])
        .await
        .unwrap();

    let registry = Arc::new(env.registry());
    registry.rebuild().await.unwrap();
    assert_eq!(
        registry.instance("meta1").unwrap().plugin_version(),
        "0.1.0"
    );

    UpdateCheckJobHandler::new(env.checker())
        .with_registry(Arc::clone(&registry))
        .check_if_due()
        .await
        .unwrap();

    assert_eq!(
        registry.instance("meta1").unwrap().plugin_version(),
        "0.2.0"
    );
}

#[tokio::test]
async fn the_first_check_is_due_and_the_next_waits_for_the_interval() {
    // Automatic updates are switched off here so the test observes only the
    // check schedule.
    let env = Env::with_settings(&[("HEARTHSOUND__PLUGINS__AUTO_UPDATE", "false")]).await;
    serve_two_versions(&env, json!({})).await;
    env.installer
        .install_from_index(METADATA_ID, Some("0.1.0"), Some(RepositoryRef::Official))
        .await
        .unwrap();

    let handler = UpdateCheckJobHandler::new(env.checker());
    assert!(handler.last_check().is_none());

    let report = handler.check_if_due().await.unwrap();
    assert_eq!(report.updates.len(), 1);
    assert_eq!(report.updates[0].version, "0.2.0");
    assert!(report.checked_at > 0);
    assert_eq!(handler.last_check().unwrap(), report);

    // The interval has not passed, and the scheduled job does nothing either.
    assert!(handler.check_if_due().await.is_none());
    handler.handle(update_job()).await.unwrap();
    assert_eq!(handler.last_check().unwrap(), report);

    // Automatic updates are off here, so the plugin still serves 0.1.0 and has
    // nothing staged.
    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.version, "0.1.0");
    assert!(env.repository.upgrade(METADATA_ID).await.unwrap().is_none());
}

#[tokio::test]
async fn the_default_check_installs_an_update() {
    let env = Env::new().await;
    serve_two_versions(&env, json!({})).await;
    install_v1(&env).await;

    UpdateCheckJobHandler::new(env.checker())
        .check_if_due()
        .await
        .unwrap();

    let row = env.repository.plugin(METADATA_ID).await.unwrap().unwrap();
    assert_eq!(row.version, "0.2.0");
    assert!(row.enabled);
    assert!(env.repository.upgrade(METADATA_ID).await.unwrap().is_none());
}

#[tokio::test]
async fn a_zero_hour_interval_switches_the_check_off() {
    let env = Env::with_settings(&[("HEARTHSOUND__PLUGINS__UPDATE_CHECK_HOURS", "0")]).await;
    serve_two_versions(&env, json!({})).await;
    env.installer
        .install_from_index(METADATA_ID, Some("0.1.0"), Some(RepositoryRef::Official))
        .await
        .unwrap();

    let handler = UpdateCheckJobHandler::new(env.checker());

    assert!(handler.check_if_due().await.is_none());
    handler.handle(update_job()).await.unwrap();
    assert!(handler.last_check().is_none());
}
