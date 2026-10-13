//! Helpers shared by the full-stack tests.
//!
//! Every test builds a server over a temporary data directory: the data and
//! storage trees are created inside a [`TempDir`], the database is migrated by
//! the composition root, and the HTTP listener binds an ephemeral port on the
//! loopback interface. The account and its API key are created through
//! `hearthsound-auth`, so the tests exercise the same tables and hashing the
//! command line and the API write.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_server::{Server, Services};
use tempfile::TempDir;

/// The login name of the test account.
pub const USERNAME: &str = "echo";

/// The account password of the test account.
pub const ACCOUNT_PASSWORD: &str = "hunter2";

/// A server running over a temporary data directory.
pub struct Fixture {
    /// Keeps the data directory alive for the whole test.
    _dir: TempDir,
    /// The running server.
    pub server: Server,
    /// The services the server runs.
    pub services: Arc<Services>,
    /// The API key of the test account, usable as `/rest`'s `apiKey`.
    pub api_key: String,
    /// The HTTP client every test request goes through.
    pub client: reqwest::Client,
}

impl Fixture {
    /// Builds a server with one account and one API key.
    pub async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = write_config(dir.path(), None);
        let services = Services::build(config, &BTreeMap::new()).await.unwrap();

        let user = Users::new(services.database())
            .create(
                &services.settings().current(),
                &NewUser::new(USERNAME, "Echo", ACCOUNT_PASSWORD, true),
            )
            .await
            .unwrap();
        let api_key = ApiKeys::new(services.database())
            .generate(user.id, "test")
            .await
            .unwrap()
            .plaintext;

        let server = Server::start_on(Arc::clone(&services), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        Fixture {
            _dir: dir,
            server,
            services,
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Returns the base URL of the running server.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.server.local_addr())
    }

    /// Sends a GET request and returns the response.
    ///
    /// # Parameters
    ///
    /// - `path`: the path and query to request, starting with `/`.
    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base_url()))
            .send()
            .await
            .unwrap()
    }

    /// Returns the path of the database file.
    pub fn database_path(&self) -> &Path {
        &self.services.config().database.path
    }
}

/// Writes a config file into `dir` and loads it.
///
/// # Parameters
///
/// - `dir`: the directory holding the data tree and the config file.
/// - `settings`: the `[settings]` table to write, as `(key, TOML value)` pairs,
///   or `None` for no table.
pub fn write_config(dir: &Path, settings: Option<&[(&str, &str)]>) -> BootstrapConfig {
    let data: PathBuf = dir.join("data");
    let mut text = format!("paths.data = \"{}\"\n", data.to_string_lossy());
    if let Some(settings) = settings {
        text.push_str("\n[settings]\n");
        for (key, value) in settings {
            text.push_str(&format!("\"{key}\" = {value}\n"));
        }
    }
    let path = dir.join("hearthsound.toml");
    std::fs::write(&path, text).unwrap();
    BootstrapConfig::load(Some(&path), &BTreeMap::new()).unwrap()
}
