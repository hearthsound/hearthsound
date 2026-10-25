//! Full-stack tests of the plugin platform from a client's side.
//!
//! Every test builds a server over a temporary data directory, serves the plugin
//! index and the archive it publishes from a mock HTTP server, and installs the
//! plugin the way a client does: through `POST /api/v1/admin/plugins`, through
//! the command line's `plugins` command, or through the multipart upload route.
//! Installing records the permissions the manifest requests and enables the
//! plugin, so the whole path from an index to a plugin the provider registry
//! serves is walked once from the API and once from the command line. The suite makes no network
//! request: every index document and every archive download points at the mock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use hearthsound_core::Layer;
use hearthsound_test_support::{
    HELLO_CATALOG, HTTP_CATALOG, LEGACY_CATALOG, METADATA_FIXTURE, fixture_plugin,
};
use minisign::KeyPair;
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Fixture;

/// The id of the metadata fixture the tests install.
const PLUGIN_ID: &str = "com.example.metadata-fixture";

/// The version the tests publish.
const VERSION: &str = "0.1.0";

/// The plugin API version this server hosts.
const API: &str = "hearthsound:plugin@0.4.0";

/// The path an index document is served at.
const INDEX_PATH: &str = "/v1/index.json";

/// The path the fixture archive is served at.
const ARCHIVE_PATH: &str = "/download/metadata.hsplugin";

/// The boundary of the multipart bodies the tests send.
const BOUNDARY: &str = "----hearthsound-test-boundary";

/// Sends one administrative request and returns the response.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `method`: the HTTP method.
/// - `path`: the path and query to request, starting with `/`.
/// - `body`: the JSON body, or `None` for a request without one.
async fn admin(
    fixture: &Fixture,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key));
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    request.send().await.unwrap()
}

/// Reads a response body as JSON.
///
/// # Parameters
///
/// - `response`: the response to read.
async fn json(response: reqwest::Response) -> Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

/// Asserts a response is a problem document with the given status and code.
///
/// # Parameters
///
/// - `response`: the response to check.
/// - `status`: the status the problem must carry.
/// - `code`: the problem code the problem must carry.
async fn assert_problem(response: reqwest::Response, status: StatusCode, code: &str) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
    let body = json(response).await;
    assert_eq!(body["code"], code, "{body}");
    body
}

/// The bytes of the metadata fixture archive.
fn fixture_bytes() -> Vec<u8> {
    std::fs::read(fixture_plugin(METADATA_FIXTURE)).unwrap()
}

/// Renders bytes as lowercase hexadecimal.
///
/// # Parameters
///
/// - `bytes`: the bytes to render.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Returns the directory the installed plugins live under.
///
/// # Parameters
///
/// - `fixture`: the running server.
fn plugins_dir(fixture: &Fixture) -> &Path {
    fixture.services.plugins().plugins_dir()
}

/// Returns the config file the fixture's server was built from.
///
/// A command line process is pointed at this file, which is what makes it work
/// on the same data directory, database and plugin tree as the running server.
///
/// # Parameters
///
/// - `fixture`: the running server.
fn config_path(fixture: &Fixture) -> PathBuf {
    let data = fixture.database_path().parent().unwrap();
    data.parent().unwrap().join("hearthsound.toml")
}

/// Serves one 200 response with a binary body.
///
/// # Parameters
///
/// - `server`: the mock server.
/// - `document_path`: the path the body is served at.
/// - `body`: the body.
async fn serve_bytes(server: &MockServer, document_path: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(document_path))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(server)
        .await;
}

/// Serves the fixture archive and returns its URL with its bytes.
///
/// # Parameters
///
/// - `server`: the mock server.
async fn serve_archive(server: &MockServer) -> (String, Vec<u8>) {
    let bytes = fixture_bytes();
    serve_bytes(server, ARCHIVE_PATH, bytes.clone()).await;
    (format!("{}{ARCHIVE_PATH}", server.uri()), bytes)
}

/// Serves one index document, with its signature when a signer is given.
///
/// # Parameters
///
/// - `server`: the mock server.
/// - `index`: the document.
/// - `signer`: the key pair that signs the document, or `None` for an unsigned
///   index.
///
/// # Returns
///
/// The URL of the index document.
async fn serve_index(server: &MockServer, index: &str, signer: Option<&KeyPair>) -> String {
    Mock::given(method("GET"))
        .and(path(INDEX_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(index))
        .mount(server)
        .await;
    if let Some(pair) = signer {
        let signature = minisign::sign(Some(&pair.pk), &pair.sk, index.as_bytes(), None, None)
            .unwrap()
            .to_string();
        Mock::given(method("GET"))
            .and(path(format!("{INDEX_PATH}.minisig")))
            .respond_with(ResponseTemplate::new(200).set_body_string(signature))
            .mount(server)
            .await;
    }
    format!("{}{INDEX_PATH}", server.uri())
}

/// Builds an index that publishes the metadata fixture.
///
/// # Parameters
///
/// - `archive_url`: the URL the archive is served at.
/// - `sha256`: the digest the index declares for the archive, lowercase
///   hexadecimal.
/// - `size`: the size the index declares for the archive.
fn index_document(archive_url: &str, sha256: &str, size: u64) -> String {
    json!({
        "formatVersion": 1,
        "name": "Test index",
        "generatedAt": "2026-01-01T00:00:00Z",
        "plugins": [{
            "id": PLUGIN_ID,
            "name": "Metadata fixture",
            "description": "A metadata plugin.",
            "provides": ["metadata"],
            "homepage": "https://example.com/metadata",
            "license": "MIT",
            "official": false,
            "versions": [{
                "version": VERSION,
                "api": API,
                "url": archive_url,
                "sha256": sha256,
                "size": size,
                "publishedAt": "2026-01-01T00:00:00Z",
                "changelog": "A release.",
                "permissions": { "http": { "hosts": ["api.example.com"] } },
            }],
        }],
    })
    .to_string()
}

/// Registers a repository through the API and returns its client id.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `url`: the URL of the repository's index document.
/// - `public_key`: the minisign public key to verify the index with, or `None`
///   for an index that is read unsigned.
async fn register_repository(fixture: &Fixture, url: &str, public_key: Option<&str>) -> String {
    let response = admin(
        fixture,
        reqwest::Method::POST,
        "/api/v1/admin/plugin-repositories",
        Some(json!({ "url": url, "name": "test", "publicKey": public_key })),
    )
    .await;
    let status = response.status();
    let row = json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{row}");
    row["id"].as_str().unwrap().to_owned()
}

/// Installs one plugin from a repository through the API and returns its view.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `repository`: the repository's client id.
async fn install(fixture: &Fixture, repository: &str) -> reqwest::Response {
    admin(
        fixture,
        reqwest::Method::POST,
        "/api/v1/admin/plugins",
        Some(json!({
            "source": "index",
            "pluginId": PLUGIN_ID,
            "repositoryId": repository,
        })),
    )
    .await
}

/// Returns the slugs the registry serves for one layer, in order.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `layer`: the layer to read.
fn served(fixture: &Fixture, layer: Layer) -> Vec<String> {
    fixture
        .services
        .registry()
        .layers(layer)
        .iter()
        .map(|instance| instance.slug().to_string())
        .collect()
}

/// Configures an instance from the metadata fixture and assigns it to its layer.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `slug`: the instance slug.
async fn configure_metadata(fixture: &Fixture, slug: &str) {
    let created = admin(
        fixture,
        reqwest::Method::POST,
        "/api/v1/admin/providers",
        Some(json!({ "slug": slug, "pluginId": PLUGIN_ID })),
    )
    .await;
    let status = created.status();
    let instance = json(created).await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");

    let assigned = admin(
        fixture,
        reqwest::Method::PUT,
        "/api/v1/admin/layers/metadata",
        Some(json!([slug])),
    )
    .await;
    let status = assigned.status();
    let layers = json(assigned).await;
    assert_eq!(status, StatusCode::OK, "{layers}");
}

/// Asserts that nothing of one plugin was recorded or written.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `id`: the plugin id that must not be there.
async fn assert_untouched(fixture: &Fixture, id: &str) {
    let listed =
        json(admin(fixture, reqwest::Method::GET, "/api/v1/admin/plugins", None).await).await;
    assert!(
        listed["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .all(|plugin| plugin["id"] != id),
        "the plugin was recorded: {listed}"
    );
    assert!(!plugins_dir(fixture).join(id).exists());
    let leftovers: Vec<String> = std::fs::read_dir(plugins_dir(fixture))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(".download-") || name.starts_with(".upload-"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leftovers.is_empty(),
        "a temporary file was left behind: {leftovers:?}"
    );
}

/// Sends a multipart upload to one plugin route.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `endpoint`: the route to post to, after `/api/v1/admin/plugins`.
/// - `bytes`: the archive.
async fn multipart_upload(fixture: &Fixture, endpoint: &str, bytes: &[u8]) -> reqwest::Response {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"plugin.hsplugin\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    fixture
        .client
        .post(format!(
            "{}/api/v1/admin/plugins/{endpoint}",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .unwrap()
}

/// Sends a multipart upload of one `.hsplugin` file.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `bytes`: the archive.
async fn upload(fixture: &Fixture, bytes: &[u8]) -> reqwest::Response {
    multipart_upload(fixture, "upload", bytes).await
}

/// Sends a multipart file to the inspection route.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `bytes`: the file to read.
async fn inspect(fixture: &Fixture, bytes: &[u8]) -> reqwest::Response {
    multipart_upload(fixture, "inspect", bytes).await
}

/// Returns the cargo arguments that build the command line binary.
///
/// The profile is the one this test binary was built with, so the build reuses
/// the dependency graph the test run already compiled instead of rebuilding it
/// under another profile.
fn cargo_arguments() -> Vec<String> {
    let profile = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .and_then(|dir| dir.parent())
                .map(Path::to_owned)
        })
        .and_then(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "debug".to_owned());
    let mut arguments = vec![
        "build".to_owned(),
        "-p".to_owned(),
        "hearthsound".to_owned(),
        "--message-format=json".to_owned(),
    ];
    match profile.as_str() {
        // A test run's binaries are built with the test profile, which writes
        // beside the development profile's artifacts.
        "debug" => arguments.extend(["--profile".to_owned(), "test".to_owned()]),
        "release" => arguments.push("--release".to_owned()),
        other => arguments.extend(["--profile".to_owned(), other.to_owned()]),
    }
    arguments
}

/// Returns the built command line binary.
///
/// This package does not depend on the command line crate, so a test run of this
/// suite does not build it. The binary is built once per test process, which is
/// what lets a test drive the real command against the running server's data
/// directory.
///
/// # Panics
///
/// Panics when the binary cannot be built or cargo reports no binary for it.
fn cli_binary() -> &'static Path {
    static BINARY: LazyLock<PathBuf> = LazyLock::new(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let output = Command::new(cargo)
            .args(cargo_arguments())
            .output()
            .expect("cargo could not be run");
        assert!(
            output.status.success(),
            "the command line could not be built:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|message| {
                let artifact = message["reason"] == "compiler-artifact"
                    && message["target"]["name"] == "hearthsound";
                match message["executable"].as_str() {
                    Some(executable) if artifact => Some(PathBuf::from(executable)),
                    _ => None,
                }
            })
            .expect("cargo reported no command line binary")
    });
    &BINARY
}

/// Runs one command against the fixture's data directory and returns its
/// standard output.
///
/// # Parameters
///
/// - `fixture`: the running server whose data directory the command works on.
/// - `args`: the arguments after the global `--config`.
///
/// # Panics
///
/// Panics when the command fails.
fn run_cli(fixture: &Fixture, args: &[&str]) -> String {
    let config = config_path(fixture);
    let output = Command::new(cli_binary())
        .arg("--config")
        .arg(&config)
        .args(args)
        .env_clear()
        .env("HOME", config.parent().unwrap())
        .output()
        .expect("the command line could not be run");
    assert!(
        output.status.success(),
        "`hearthsound {}` failed with {:?}:\n{}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// An index URL installs a plugin over the API, which enables it.
///
/// The plugin is installed with the permissions its manifest requests and
/// enabled, so an instance can be configured from it and the registry serves it
/// at once. Applying an upgrade when none is staged is refused.
#[tokio::test]
async fn an_index_url_installs_over_the_api_and_is_enabled() {
    let fixture = Fixture::new().await;
    let server = MockServer::start().await;
    let (archive, bytes) = serve_archive(&server).await;
    let index = index_document(&archive, &hex(&Sha256::digest(&bytes)), bytes.len() as u64);
    let index_url = serve_index(&server, &index, None).await;

    let response = admin(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/plugins",
        Some(json!({ "source": "url", "pluginId": PLUGIN_ID, "url": index_url })),
    )
    .await;
    let status = response.status();
    let view = json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["id"], PLUGIN_ID);
    assert_eq!(view["name"], "Metadata fixture");
    assert_eq!(view["isDefault"], false);
    assert_eq!(
        view["config"]["schema"]["properties"]["recordings"]["type"],
        "array"
    );
    assert_eq!(view["config"]["uiSchema"]["type"], "VerticalLayout");
    assert_eq!(view["version"], VERSION);
    assert_eq!(view["source"], "repository");
    assert_eq!(view["state"], "enabled");
    assert_eq!(view["enabled"], true);
    let repository = view["repositoryId"].as_str().unwrap().to_owned();

    // The index URL was registered on the way in, unsigned and so unverified.
    let listed = json(
        admin(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/plugin-repositories",
            None,
        )
        .await,
    )
    .await;
    let row = listed["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == repository.as_str())
        .expect("the repository was registered")
        .clone();
    assert_eq!(row["url"], index_url);
    assert_eq!(row["verified"], false);
    assert!(row["lastFetchedAt"].is_i64(), "{row}");

    // Nothing is staged, so there is no upgrade to apply.
    let refused = admin(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
        None,
    )
    .await;
    let refused = assert_problem(refused, StatusCode::CONFLICT, "conflict").await;
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("has no staged upgrade"),
        "{refused}"
    );
    assert!(served(&fixture, Layer::Metadata).is_empty());

    configure_metadata(&fixture, "meta").await;
    assert_eq!(served(&fixture, Layer::Metadata), vec!["meta".to_owned()]);
}

/// The command line installs a plugin from an index URL, and it is enabled.
///
/// The command writes the rows and the files of the running server's data
/// directory directly, which is the other side of the platform the API's own
/// install walks.
#[tokio::test]
async fn an_index_url_installs_over_the_command_line_and_is_enabled() {
    let fixture = Fixture::new().await;
    let server = MockServer::start().await;
    let (archive, bytes) = serve_archive(&server).await;
    let index = index_document(&archive, &hex(&Sha256::digest(&bytes)), bytes.len() as u64);
    let index_url = serve_index(&server, &index, None).await;

    // The listing names the fixture's version, its layers, its source, and
    // that nothing is installed yet.
    let listed = run_cli(
        &fixture,
        &[
            "plugins",
            "available",
            "--source",
            "url",
            "--url",
            &index_url,
        ],
    );
    assert!(
        listed.contains(&format!("{PLUGIN_ID} {VERSION} (metadata)")),
        "{listed}"
    );
    assert!(
        listed.contains(&format!("  source: {index_url}")),
        "{listed}"
    );
    assert!(listed.contains("(unverified)"), "{listed}");
    assert!(listed.contains("  not installed"), "{listed}");

    let installed = run_cli(
        &fixture,
        &[
            "plugins",
            "install",
            "--source",
            "url",
            PLUGIN_ID,
            "--url",
            &index_url,
            "--version",
            VERSION,
        ],
    );
    assert!(
        installed.contains(&format!("installed {PLUGIN_ID} {VERSION} (repository)")),
        "{installed}"
    );
    assert!(installed.contains("api: "), "{installed}");
    assert!(installed.contains("provides: metadata"), "{installed}");
    assert!(installed.contains("state: enabled"), "{installed}");
    assert!(!installed.contains("approve it with"), "{installed}");

    let listed = run_cli(
        &fixture,
        &[
            "plugins",
            "available",
            "--source",
            "url",
            "--url",
            &index_url,
        ],
    );
    assert!(listed.contains("  installed"), "{listed}");

    // The running server reads the row the command wrote.
    let installed = json(
        admin(
            &fixture,
            reqwest::Method::GET,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(installed["state"], "enabled");
    assert_eq!(installed["enabled"], true);
    assert_eq!(installed["source"], "repository");

    configure_metadata(&fixture, "meta").await;
    assert_eq!(served(&fixture, Layer::Metadata), vec!["meta".to_owned()]);
}

/// An index whose signature does not verify fails closed and leaves nothing.
#[tokio::test]
async fn a_bad_signature_blocks_the_install_and_leaves_nothing() {
    let fixture = Fixture::new().await;
    let server = MockServer::start().await;
    let (archive, bytes) = serve_archive(&server).await;
    let index = index_document(&archive, &hex(&Sha256::digest(&bytes)), bytes.len() as u64);
    // The index is signed with one key while the repository is registered with
    // another's public key, so the document cannot be trusted.
    let signer = KeyPair::generate_unencrypted_keypair().unwrap();
    let index_url = serve_index(&server, &index, Some(&signer)).await;
    let other = KeyPair::generate_unencrypted_keypair().unwrap();
    let repository = register_repository(&fixture, &index_url, Some(&other.pk.to_base64())).await;

    let refused = install(&fixture, &repository).await;
    assert_problem(refused, StatusCode::BAD_GATEWAY, "signature-invalid").await;
    assert_untouched(&fixture, PLUGIN_ID).await;

    // The failed fetch is recorded on the repository, which is what an
    // administrator reads to find out why nothing installed.
    let row = json(
        admin(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/plugin-repositories",
            None,
        )
        .await,
    )
    .await;
    let row = row["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == repository.as_str())
        .unwrap()
        .clone();
    assert!(
        row["lastError"]
            .as_str()
            .unwrap()
            .contains("signature of the index"),
        "{row}"
    );
}

/// An archive whose digest is not the one the index declares fails closed and
/// leaves nothing.
#[tokio::test]
async fn a_digest_mismatch_blocks_the_install_and_leaves_nothing() {
    let fixture = Fixture::new().await;
    let server = MockServer::start().await;
    let (archive, bytes) = serve_archive(&server).await;
    // The index declares the digest of another archive for the bytes it serves.
    let index = index_document(
        &archive,
        &hex(&Sha256::digest(b"not the archive")),
        bytes.len() as u64,
    );
    let index_url = serve_index(&server, &index, None).await;
    let repository = register_repository(&fixture, &index_url, None).await;

    let refused = install(&fixture, &repository).await;
    assert_problem(refused, StatusCode::BAD_GATEWAY, "integrity-failed").await;
    assert_untouched(&fixture, PLUGIN_ID).await;
}

/// The multipart route installs a sideloaded plugin, and the gate refuses it.
#[tokio::test]
async fn an_upload_installs_and_the_sideload_gate_refuses_it() {
    let fixture = Fixture::new().await;

    let response = upload(&fixture, &fixture_bytes()).await;
    let status = response.status();
    let view = json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["id"], PLUGIN_ID);
    assert_eq!(view["source"], "sideload");
    assert_eq!(view["state"], "enabled");
    assert_eq!(view["enabled"], true);
    assert!(view["repositoryId"].is_null(), "{view}");

    configure_metadata(&fixture, "meta").await;
    assert_eq!(served(&fixture, Layer::Metadata), vec!["meta".to_owned()]);

    // With sideloading off the route refuses another file and installs nothing.
    let changed = admin(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({ "settings": { "plugins.allow_sideload": false } })),
    )
    .await;
    let status = changed.status();
    let listing = json(changed).await;
    assert_eq!(status, StatusCode::OK, "{listing}");

    let other = std::fs::read(fixture_plugin(HELLO_CATALOG)).unwrap();
    let refused = upload(&fixture, &other).await;
    let problem = assert_problem(refused, StatusCode::FORBIDDEN, "sideload-disabled").await;
    assert!(
        problem["detail"].as_str().unwrap().contains("turned off"),
        "{problem}"
    );
    assert_untouched(&fixture, "com.example.hello-catalog").await;
    assert!(plugins_dir(&fixture).join(PLUGIN_ID).exists());
}

/// The upload route installs a plugin built for the previous plugin API.
///
/// The host loads both hosted versions, so an archive whose manifest declares
/// `hearthsound:plugin@0.3.0` installs, reports its own version, and is served.
#[tokio::test]
async fn an_upload_installs_a_plugin_built_for_the_previous_api() {
    let fixture = Fixture::new().await;
    let bytes = std::fs::read(fixture_plugin(LEGACY_CATALOG)).unwrap();

    let response = upload(&fixture, &bytes).await;
    let status = response.status();
    let view = json(response).await;
    assert_eq!(status, StatusCode::CREATED, "{view}");
    assert_eq!(view["id"], "com.example.legacy-catalog");
    assert_eq!(view["api"], "hearthsound:plugin@0.3.0");
    assert_eq!(view["source"], "sideload");
    assert_eq!(view["state"], "enabled");

    let created = admin(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/providers",
        Some(json!({ "slug": "legacy", "pluginId": "com.example.legacy-catalog" })),
    )
    .await;
    let status = created.status();
    let instance = json(created).await;
    assert_eq!(status, StatusCode::CREATED, "{instance}");

    let assigned = admin(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/admin/layers/catalog",
        Some(json!(["legacy"])),
    )
    .await;
    let status = assigned.status();
    let layers = json(assigned).await;
    assert_eq!(status, StatusCode::OK, "{layers}");
    assert_eq!(served(&fixture, Layer::Catalog), vec!["legacy".to_owned()]);
}

/// The inspection route reports an archive and installs nothing.
///
/// The manifest's `provides` and its requested permissions are read from the
/// file alone, no row or plugin directory appears, and the sideload gate refuses
/// the route when it is switched off.
#[tokio::test]
async fn an_inspection_reports_an_archive_without_installing_it() {
    let fixture = Fixture::new().await;
    let bytes = std::fs::read(fixture_plugin(HTTP_CATALOG)).unwrap();

    let response = inspect(&fixture, &bytes).await;
    let status = response.status();
    let view = json(response).await;
    assert_eq!(status, StatusCode::OK, "{view}");
    assert_eq!(view["id"], "com.example.http-catalog");
    assert_eq!(view["name"], "HTTP catalog");
    assert_eq!(view["version"], VERSION);
    assert_eq!(view["api"], API);
    assert_eq!(view["provides"], json!(["catalog"]));
    let permissions = view["requestedPermissions"].as_array().unwrap();
    assert!(
        permissions
            .iter()
            .any(|permission| permission["scope"] == "http.hosts"),
        "{view}"
    );

    assert_untouched(&fixture, "com.example.http-catalog").await;

    // With sideloading off the route refuses to read the file at all.
    let changed = admin(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(json!({ "settings": { "plugins.allow_sideload": false } })),
    )
    .await;
    assert_eq!(changed.status(), StatusCode::OK);
    let refused = inspect(&fixture, &bytes).await;
    assert_problem(refused, StatusCode::FORBIDDEN, "sideload-disabled").await;
}

/// A file that is not a plugin archive is refused by the inspection route.
#[tokio::test]
async fn an_inspection_refuses_a_file_that_is_not_an_archive() {
    let fixture = Fixture::new().await;

    let refused = inspect(&fixture, b"not a plugin archive").await;
    let problem = assert_problem(refused, StatusCode::UNPROCESSABLE_ENTITY, "invalid-plugin").await;
    assert!(problem["detail"].is_string(), "{problem}");
    assert_untouched(&fixture, "com.example.http-catalog").await;
}
