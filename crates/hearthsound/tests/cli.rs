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
