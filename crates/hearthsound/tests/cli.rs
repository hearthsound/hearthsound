//! Integration tests for the command line.
//!
//! Every test runs the built binary (`CARGO_BIN_EXE_hearthsound`) against a
//! temporary data directory, so the real argument parsing, the real
//! configuration loading, and the real database writes are exercised. The
//! child's environment is cleared except for `HOME`, so a `HEARTHSOUND__*`
//! variable in the developer's shell cannot reach a test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// A temporary data directory with a configuration file.
pub struct Sandbox {
    /// Keeps the directory alive.
    _dir: TempDir,
    /// The configuration file every command is pointed at.
    config: PathBuf,
}

impl Sandbox {
    /// Creates a sandbox whose config file holds the given `[settings]` table.
    ///
    /// # Parameters
    ///
    /// - `settings`: additional TOML lines written below a `[settings]` header,
    ///   or an empty string for no table at all.
    pub fn new(settings: &str) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let config = dir.path().join("hearthsound.toml");
        let mut text = format!("paths.data = \"{}\"\n", data.display());
        if !settings.trim().is_empty() {
            text.push_str("\n[settings]\n");
            text.push_str(settings);
            text.push('\n');
        }
        std::fs::write(&config, text).unwrap();
        Sandbox { _dir: dir, config }
    }

    /// Returns the configuration file's path.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// Returns the data directory the config file names.
    pub fn data(&self) -> PathBuf {
        self.config.parent().unwrap().join("data")
    }

    /// Appends text to the config file.
    ///
    /// # Parameters
    ///
    /// - `text`: the TOML to append, for example a `[server]` table.
    pub fn append_config(&self, text: &str) {
        let mut config = std::fs::read_to_string(&self.config).unwrap();
        config.push_str(text);
        std::fs::write(&self.config, config).unwrap();
    }

    /// Runs a command with an empty standard input and returns its output.
    ///
    /// # Parameters
    ///
    /// - `args`: the arguments after the global `--config`.
    pub fn run(&self, args: &[&str]) -> Output {
        self.run_with_stdin(args, "")
    }

    /// Runs a command with `input` on its standard input.
    ///
    /// # Parameters
    ///
    /// - `args`: the arguments after the global `--config`.
    /// - `input`: what the command reads from standard input.
    pub fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self.spawn(args, Stdio::piped(), Stdio::piped(), Stdio::piped());
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Starts a command with the given standard streams.
    ///
    /// # Parameters
    ///
    /// - `args`: the arguments after the global `--config`.
    /// - `stdin`, `stdout`, `stderr`: the streams to give the child.
    pub fn spawn(&self, args: &[&str], stdin: Stdio, stdout: Stdio, stderr: Stdio) -> Child {
        Command::new(env!("CARGO_BIN_EXE_hearthsound"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .env_clear()
            .env("HOME", self.config.parent().unwrap())
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap()
    }
}

/// Returns the text a command wrote to stdout.
pub fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Returns the text a command wrote to stderr.
pub fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts that a command succeeded and returns its stdout.
pub fn succeeded(output: &Output) -> String {
    assert!(
        output.status.success(),
        "the command failed with {:?}\nstderr: {}",
        output.status.code(),
        stderr(output)
    );
    stdout(output)
}

/// `--version` works without a configuration file or a data directory.
#[test]
fn version_is_printed() {
    let output = Command::new(env!("CARGO_BIN_EXE_hearthsound"))
        .arg("--version")
        .env_clear()
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        stdout(&output).trim(),
        format!("hearthsound {}", env!("CARGO_PKG_VERSION"))
    );
}

/// `migrate` applies the pending migrations and reports the versions.
#[test]
fn migrate_reports_the_versions_it_moved_between() {
    let sandbox = Sandbox::new("");

    let first = succeeded(&sandbox.run(&["migrate"]));
    assert!(
        first.contains("migrated the database from version 0 to 1"),
        "{first}"
    );
    assert!(first.contains("no backup was needed"), "{first}");

    let second = succeeded(&sandbox.run(&["migrate"]));
    assert!(second.contains("already at version 1"), "{second}");
    assert!(sandbox.data().join("hearthsound.db").is_file());
}

/// `scan` reports that there is nothing to scan on a fresh installation.
#[test]
fn scan_reports_when_no_folders_are_configured() {
    let sandbox = Sandbox::new("");

    let output = succeeded(&sandbox.run(&["scan"]));

    assert!(
        output.contains("no library folders are configured"),
        "{output}"
    );
}

/// `scan --folder` refuses a path that is not a library folder.
#[test]
fn scan_refuses_a_folder_that_was_not_added() {
    let sandbox = Sandbox::new("");
    let music = sandbox.data().join("music");
    std::fs::create_dir_all(&music).unwrap();

    let output = sandbox.run(&["scan", "--folder", music.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is not a library folder"),
        "{}",
        stderr(&output)
    );
}

/// `serve` answers the health probes and stops on `SIGTERM`.
#[test]
fn serve_serves_and_stops_on_a_signal() {
    let sandbox = Sandbox::new("");
    let port = free_port();
    sandbox.append_config(&format!(
        "\n[server]\nbind = \"127.0.0.1\"\nport = {port}\n"
    ));

    let child = sandbox.spawn(&["serve"], Stdio::null(), Stdio::piped(), Stdio::piped());
    let pid = child.id();

    let started = Instant::now();
    let mut health = None;
    while started.elapsed() < Duration::from_secs(30) {
        if let Some(response) = http_get(port, "/healthz") {
            health = Some(response);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let health = health.expect("the server never answered /healthz");
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "the server exited with {:?}\nstderr: {}",
        output.status.code(),
        stderr(&output)
    );
    assert!(stdout(&output).contains("stopped"), "{}", stdout(&output));
}

/// Sends one HTTP/1.1 GET request and returns the response text.
///
/// # Parameters
///
/// - `port`: the loopback port to connect to.
/// - `path`: the request path.
pub fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

/// Returns a loopback port nobody is listening on.
///
/// The listener is dropped before the port is returned, so the port is only
/// reserved for the moment the operating system holds it; a server that binds it
/// immediately afterwards wins the race in practice.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The account and key a first-run installation creates make a server serve.
#[tokio::test(flavor = "multi_thread")]
async fn a_key_created_by_the_cli_authenticates_against_a_server() {
    let sandbox = Sandbox::new("");
    succeeded(&sandbox.run_with_stdin(
        &["admin", "create-user", "--admin", "--password-stdin"],
        "hunter2\n",
    ));
    let created = succeeded(&sandbox.run(&[
        "admin",
        "create-api-key",
        "--user",
        "admin",
        "--name",
        "test",
    ]));
    let key = created
        .lines()
        .find(|line| line.starts_with("hs_"))
        .unwrap_or_else(|| panic!("no API key in: {created}"));
    assert!(
        created.contains("cannot be shown again"),
        "the key warning is missing: {created}"
    );

    let (server, port) = serve_in_process(&sandbox).await;
    let response = http_get(port, &format!("/rest/ping?apiKey={key}&f=json")).unwrap();

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"status\":\"ok\""), "{response}");
    server.shutdown().await;
}

/// An app password created by the CLI authenticates the legacy `u`/`p` call.
///
/// The account password cannot be used on `/rest` by default
/// (`subsonic.allow_account_password` is off), so a successful `u`/`p` call
/// proves the app password was sealed, stored, and decrypted again.
#[tokio::test(flavor = "multi_thread")]
async fn an_app_password_created_by_the_cli_authenticates_a_rest_call() {
    let sandbox = Sandbox::new("");
    succeeded(&sandbox.run_with_stdin(
        &["admin", "create-user", "--admin", "--password-stdin"],
        "hunter2\n",
    ));
    let created = succeeded(&sandbox.run(&[
        "admin",
        "create-app-password",
        "--user",
        "admin",
        "--name",
        "Feishin",
    ]));
    let secret = created
        .lines()
        .find(|line| line.len() == 20 && line.chars().all(|ch| ch.is_ascii_alphanumeric()))
        .unwrap_or_else(|| panic!("no app password in: {created}"));
    assert!(
        created.contains("cannot be shown again"),
        "the secret warning is missing: {created}"
    );

    let (server, port) = serve_in_process(&sandbox).await;
    let response = http_get(port, &format!("/rest/ping?u=admin&p={secret}&f=json")).unwrap();

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"status\":\"ok\""), "{response}");
    server.shutdown().await;
}

/// `add-folder` requires exactly one of `--owner` and `--generic`.
#[test]
fn add_folder_requires_exactly_one_of_owner_and_generic() {
    let sandbox = Sandbox::new("");
    let music = sandbox.data().join("music");
    std::fs::create_dir_all(&music).unwrap();
    let path = music.to_str().unwrap();

    let missing = sandbox.run(&["admin", "add-folder", "--path", path]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("owner"), "{}", stderr(&missing));

    let both = sandbox.run(&[
        "admin",
        "add-folder",
        "--path",
        path,
        "--generic",
        "--owner",
        "admin",
    ]);
    assert_eq!(both.status.code(), Some(2));
    assert!(
        stderr(&both).contains("cannot be used with"),
        "{}",
        stderr(&both)
    );
}

/// `add-folder --owner` associates the folder with the account.
#[tokio::test(flavor = "multi_thread")]
async fn add_folder_associates_the_folder_with_its_owner() {
    let sandbox = Sandbox::new("");
    let created = succeeded(&sandbox.run_with_stdin(
        &["admin", "create-user", "--admin", "--password-stdin"],
        "hunter2\n",
    ));
    let music = sandbox.data().join("music");
    std::fs::create_dir_all(&music).unwrap();

    let added = succeeded(&sandbox.run(&[
        "admin",
        "add-folder",
        "--path",
        music.to_str().unwrap(),
        "--owner",
        "admin",
        "--name",
        "Music",
    ]));
    assert!(added.contains("owned by `admin`"), "{added}");

    let config = bootstrap_config(&sandbox);
    let database = hearthsound_db::Database::open(&config.database.path)
        .await
        .unwrap();
    let folders = hearthsound_library::LibraryRepository::new(&database)
        .list_library_folders()
        .await
        .unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name, "Music");
    assert_eq!(folders[0].path, music.canonicalize().unwrap());
    let owner = hearthsound_auth::Users::new(&database)
        .find_by_username("admin")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(folders[0].owner_user_id, Some(owner.id));
    assert!(
        created.contains("created administrator account `admin`"),
        "{created}"
    );
}

/// `add-folder --generic` adds a folder nobody owns, and `scan` finds it.
#[test]
fn add_folder_generic_is_scanable() {
    let sandbox = Sandbox::new("");
    let music = sandbox.data().join("music");
    std::fs::create_dir_all(&music).unwrap();

    let added = succeeded(&sandbox.run(&[
        "admin",
        "add-folder",
        "--path",
        music.to_str().unwrap(),
        "--generic",
    ]));
    assert!(added.contains("as a generic folder"), "{added}");

    let scanned = succeeded(&sandbox.run(&["scan", "--folder", music.to_str().unwrap()]));

    assert!(scanned.contains("music ("), "{scanned}");
    assert!(scanned.contains("0 seen"), "{scanned}");
}

/// Runs a server over the sandbox's data directory.
///
/// # Parameters
///
/// - `sandbox`: the sandbox whose data directory the server serves.
///
/// # Returns
///
/// The running server and the port it listens on. The caller shuts it down.
async fn serve_in_process(sandbox: &Sandbox) -> (hearthsound_server::Server, u16) {
    let config = bootstrap_config(sandbox);
    let services = hearthsound_server::Services::build(config, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    let server = hearthsound_server::Server::start_on(services, "127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let port = server.local_addr().port();
    (server, port)
}

/// Loads the sandbox's configuration file.
///
/// # Parameters
///
/// - `sandbox`: the sandbox to read.
fn bootstrap_config(sandbox: &Sandbox) -> hearthsound_config::bootstrap::BootstrapConfig {
    hearthsound_config::bootstrap::BootstrapConfig::load(
        Some(sandbox.config()),
        &std::collections::BTreeMap::new(),
    )
    .unwrap()
}

/// The plugin, provider, and layer commands configure a catalog provider.
#[test]
fn plugins_providers_and_layers_configure_the_hello_fixture() {
    let sandbox = Sandbox::new("");
    let archive = hearthsound_test_support::fixture_plugin(hearthsound_test_support::HELLO_CATALOG);
    let downloads = sandbox.data().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();

    let installed = succeeded(&sandbox.run(&["plugins", "install", archive.to_str().unwrap()]));
    assert!(
        installed.contains("installed com.example.hello-catalog 0.1.0 (sideload)"),
        "{installed}"
    );
    assert!(installed.contains("provides: catalog"), "{installed}");

    let listed = succeeded(&sandbox.run(&["plugins", "list"]));
    assert!(
        listed.contains("com.example.hello-catalog 0.1.0"),
        "{listed}"
    );

    let added = succeeded(&sandbox.run(&[
        "providers",
        "add",
        "--plugin",
        "com.example.hello-catalog",
        "--slug",
        "hello",
        "--plugin-config",
        "{}",
        "--display-name",
        "Hello",
        "--import-root",
        &format!("downloads={}:move", downloads.display()),
    ]));
    assert!(
        added
            .contains("configured provider instance `hello` from plugin com.example.hello-catalog"),
        "{added}"
    );
    assert!(added.contains("import root: downloads"), "{added}");

    let providers = succeeded(&sandbox.run(&["providers", "list"]));
    assert!(
        providers.contains("hello from com.example.hello-catalog"),
        "{providers}"
    );
    assert!(providers.contains("(move)"), "{providers}");

    let before = succeeded(&sandbox.run(&["layers", "list"]));
    assert!(before.contains("catalog: (none)"), "{before}");

    let assigned = succeeded(&sandbox.run(&["layers", "set", "catalog", "hello"]));
    assert!(
        assigned.contains("assigned hello to the catalog layer"),
        "{assigned}"
    );

    let after = succeeded(&sandbox.run(&["layers", "list"]));
    assert!(after.contains("catalog: hello"), "{after}");
}

/// `plugins install --bundled` records the plugin as shipped with the server.
#[test]
fn plugins_install_can_record_a_bundled_plugin() {
    let sandbox = Sandbox::new("");
    let archive =
        hearthsound_test_support::fixture_plugin(hearthsound_test_support::ACQUISITION_FIXTURE);

    let installed =
        succeeded(&sandbox.run(&["plugins", "install", archive.to_str().unwrap(), "--bundled"]));
    assert!(installed.contains("(bundled)"), "{installed}");

    let listed = succeeded(&sandbox.run(&["plugins", "list"]));
    assert!(listed.contains("(bundled)"), "{listed}");
}

/// `layers set` refuses a layer name that does not exist.
#[test]
fn layers_set_refuses_an_unknown_layer() {
    let sandbox = Sandbox::new("");

    let output = sandbox.run(&["layers", "set", "bogus", "hello"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is not a layer name"),
        "{}",
        stderr(&output)
    );
}

/// `providers add` refuses an import root that is not `<id>=<path>:<mode>`.
#[test]
fn providers_add_refuses_a_malformed_import_root() {
    let sandbox = Sandbox::new("");
    let archive = hearthsound_test_support::fixture_plugin(hearthsound_test_support::HELLO_CATALOG);
    succeeded(&sandbox.run(&["plugins", "install", archive.to_str().unwrap()]));

    let output = sandbox.run(&[
        "providers",
        "add",
        "--plugin",
        "com.example.hello-catalog",
        "--slug",
        "hello",
        "--plugin-config",
        "{}",
        "--import-root",
        "/downloads:move",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("is not `<id>=<path>:<mode>`"),
        "{}",
        stderr(&output)
    );
}

/// `settings set`/`get`/`unset` round-trip and a pinned key is refused.
#[test]
fn settings_round_trip_and_refuse_a_pinned_key() {
    let sandbox = Sandbox::new("\"stream.pending.mode\" = \"hold\"\n");

    let listed = succeeded(&sandbox.run(&["settings", "list"]));
    assert!(
        listed.contains("stream.pending.mode = hold [pinned]"),
        "{listed}"
    );
    assert!(
        listed.contains("stream.max_concurrent_transcodes = 8"),
        "{listed}"
    );

    let pinned = sandbox.run(&["settings", "set", "stream.pending.mode", "fail"]);
    assert_eq!(pinned.status.code(), Some(1));
    assert!(
        stderr(&pinned).contains("pinned by the configuration file"),
        "{}",
        stderr(&pinned)
    );

    let malformed = sandbox.run(&[
        "settings",
        "set",
        "stream.max_concurrent_transcodes",
        "many",
    ]);
    assert_eq!(malformed.status.code(), Some(1));
    assert!(
        stderr(&malformed).contains("is not an integer"),
        "{}",
        stderr(&malformed)
    );

    let stored =
        succeeded(&sandbox.run(&["settings", "set", "stream.max_concurrent_transcodes", "3"]));
    assert!(
        stored.contains("set stream.max_concurrent_transcodes = 3"),
        "{stored}"
    );

    let read = succeeded(&sandbox.run(&["settings", "get", "stream.max_concurrent_transcodes"]));
    assert!(read.contains("= 3"), "{read}");
    assert!(read.contains("from the database"), "{read}");

    let unknown = sandbox.run(&["settings", "get", "not.a.setting"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("is not registered"),
        "{}",
        stderr(&unknown)
    );

    let unset = succeeded(&sandbox.run(&["settings", "unset", "stream.max_concurrent_transcodes"]));
    assert!(unset.contains("from the built-in default"), "{unset}");

    let read = succeeded(&sandbox.run(&["settings", "get", "stream.max_concurrent_transcodes"]));
    assert!(read.contains("= 8"), "{read}");
    assert!(read.contains("from the built-in default"), "{read}");
}

/// A setting changed by the CLI is seen by a server started afterwards.
#[test]
fn settings_set_is_visible_to_a_later_server_start() {
    let sandbox = Sandbox::new("");
    succeeded(&sandbox.run(&["settings", "set", "stream.pending.mode", "fail"]));

    let config = bootstrap_config(&sandbox);
    assert_eq!(
        config
            .settings
            .get("stream.pending.mode")
            .and_then(|value| value.as_str()),
        None
    );

    let listed = succeeded(&sandbox.run(&["settings", "get", "stream.pending.mode"]));
    assert!(listed.contains("= fail"), "{listed}");
    assert!(listed.contains("from the database"), "{listed}");
}

/// `acquisitions list` and `acquisitions cancel` read and write the ledger.
///
/// The cancellation only touches the acquisition row: the running server's
/// `reconcile` is what stops the worker and puts the track back, so the track's
/// availability is deliberately left `acquiring` here.
#[tokio::test(flavor = "multi_thread")]
async fn acquisitions_lists_and_cancels_an_acquisition() {
    let sandbox = Sandbox::new("");
    succeeded(&sandbox.run(&["migrate"]));
    let data = sandbox.data();

    let empty = succeeded(&sandbox.run(&["acquisitions", "list"]));
    assert_eq!(empty.trim(), "no acquisitions");

    // The track and the acquisition the server would have written: the track is
    // `acquiring` and the job is queued at `play`.
    let database = hearthsound_db::Database::open(&data.join("hearthsound.db"))
        .await
        .unwrap();
    let library = hearthsound_library::LibraryRepository::new(&database);
    let mut album_input = hearthsound_library::AlbumInput::new(
        "Hello Song album",
        hearthsound_library::Origin::Catalog,
    );
    album_input.year = Some(1993);
    album_input.display_artist = Some("Hello Artist".to_owned());
    let album = library
        .upsert_album(
            &album_input,
            &[hearthsound_library::ArtistCredit::new(
                "Hello Artist",
                "main",
            )],
        )
        .await
        .unwrap();
    let mut input = hearthsound_library::TrackInput::new(
        "Hello Song",
        hearthsound_library::Origin::Catalog,
        hearthsound_core::Availability::Remote,
    );
    input.album_id = Some(album);
    input.display_artist = Some("Hello Artist".to_owned());
    input.track_number = Some(1);
    input.duration_ms = Some(200);
    let track = library
        .insert_track(
            &input,
            &[hearthsound_library::ArtistCredit::new(
                "Hello Artist",
                "main",
            )],
            &[],
        )
        .await
        .unwrap();

    let id = uuid::Uuid::now_v7();
    let mut tx = database.writer().begin().await.unwrap();
    hearthsound_acquire::repo::insert(
        &mut tx,
        &hearthsound_acquire::repo::NewAcquisition {
            id,
            track_id: Some(track),
            album_id: None,
            granularity: hearthsound_core::Granularity::Track,
            priority: hearthsound_core::Priority::Play,
            reason: "user pressed play".to_owned(),
            requested_by: None,
        },
    )
    .await
    .unwrap();
    hearthsound_library::tracks::set_track_availability(
        &mut tx,
        track,
        hearthsound_core::Availability::Acquiring,
        None,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let client_id =
        hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Acquisition, id)
            .to_string();
    assert!(client_id.starts_with("ac_"));

    // The listing shows the job with its track, and only while it holds work.
    let listed = succeeded(&sandbox.run(&["acquisitions", "list"]));
    assert!(listed.contains(&client_id), "{listed}");
    assert!(listed.contains("queued"), "{listed}");
    assert!(listed.contains("play"), "{listed}");
    assert!(listed.contains("Hello Artist - Hello Song"), "{listed}");
    assert!(
        listed.trim_end().ends_with('Z'),
        "created is stamped: {listed}"
    );

    let queued = succeeded(&sandbox.run(&["acquisitions", "list", "--state", "queued"]));
    assert!(queued.contains(&client_id), "{queued}");
    let failed = succeeded(&sandbox.run(&["acquisitions", "list", "--state", "failed"]));
    assert_eq!(failed.trim(), "no acquisitions");
    let unknown = sandbox.run(&["acquisitions", "list", "--state", "nonsense"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("is not an acquisition state"),
        "{}",
        stderr(&unknown)
    );

    // The cancel writes the terminal row and its log entry.
    let cancelled = succeeded(&sandbox.run(&["acquisitions", "cancel", &client_id]));
    assert!(
        cancelled.contains(&format!("cancelled {client_id}")),
        "{cancelled}"
    );

    let repository = hearthsound_acquire::repo::AcquisitionRepository::new(&database);
    let row = repository.get(id).await.unwrap().unwrap();
    assert_eq!(row.state, hearthsound_core::AcquisitionState::Cancelled);
    assert_eq!(
        row.failure_reason,
        Some(hearthsound_core::FailureReason::Cancelled)
    );
    assert_eq!(
        row.error_detail.as_deref(),
        Some("cancelled from the command line")
    );
    assert_eq!(
        repository.log(id).await.unwrap().last().unwrap().state,
        hearthsound_core::AcquisitionState::Cancelled
    );
    assert_eq!(
        library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        hearthsound_core::Availability::Acquiring,
        "the running server restores the track, not the command"
    );

    // The row is still listed under `--all`, and the active listing hides it.
    let all = succeeded(&sandbox.run(&["acquisitions", "list", "--all"]));
    assert!(all.contains(&client_id), "{all}");
    assert!(all.contains("cancelled"), "{all}");
    // A state filter selects that state even though the acquisition has ended.
    let cancelled_only = succeeded(&sandbox.run(&["acquisitions", "list", "--state", "cancelled"]));
    assert!(cancelled_only.contains(&client_id), "{cancelled_only}");
    assert!(cancelled_only.contains("cancelled"), "{cancelled_only}");
    let active = succeeded(&sandbox.run(&["acquisitions", "list"]));
    assert_eq!(active.trim(), "no acquisitions");

    // Cancelling an ended acquisition is not an error, and a client id of
    // another kind is refused.
    let again = succeeded(&sandbox.run(&["acquisitions", "cancel", &client_id]));
    assert!(again.contains("had already ended"), "{again}");
    let track_id =
        hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Track, track.uuid())
            .to_string();
    let wrong_kind = sandbox.run(&["acquisitions", "cancel", &track_id]);
    assert_eq!(wrong_kind.status.code(), Some(1));
    assert!(
        stderr(&wrong_kind).contains("is not an acquisition id"),
        "{}",
        stderr(&wrong_kind)
    );
}
