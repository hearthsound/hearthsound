//! Helpers shared by the full-stack tests.
//!
//! Every test builds a server over a temporary data directory: the data and
//! storage trees are created inside a [`TempDir`], the database is migrated by
//! the composition root, and the HTTP listener binds an ephemeral port on the
//! loopback interface. The account and its API key are created through
//! `hearthsound-auth`, so the tests exercise the same tables and hashing the
//! command line and the API write.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_server::{Server, Services};
use tempfile::TempDir;
use uuid::Uuid;

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
    /// The test account's id, which the ledger records as a requester.
    pub user_id: Uuid,
    /// The HTTP client every test request goes through.
    pub client: reqwest::Client,
}

impl Fixture {
    /// Builds a server with one account and one API key.
    pub async fn new() -> Fixture {
        Fixture::with_config(|_| {}).await
    }

    /// Builds a server with one account and one API key over an adjusted
    /// bootstrap configuration.
    ///
    /// # Parameters
    ///
    /// - `adjust`: changes the configuration the default config file loads to,
    ///   before the services are built from it.
    pub async fn with_config(adjust: impl FnOnce(&mut BootstrapConfig)) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut config = write_config(dir.path());
        adjust(&mut config);
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
            user_id: user.id,
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

    /// Stops the server and hands the data directory back.
    ///
    /// This is what a restart looks like to a test: the data tree, the config
    /// file, the installed plugins and the ledger are the ones the stopped
    /// process left behind, and the caller starts a new process over them with
    /// [`Fixture::reopen`].
    pub async fn stop(self) -> TempDir {
        self.server.shutdown().await;
        self._dir
    }

    /// Starts a process over the data directory a stopped fixture left.
    ///
    /// Nothing is created or installed: the account, its api key, the plugin
    /// instances and every row are the ones the previous process wrote, and the
    /// new services are built over them, which is exactly what restarting the
    /// server does.
    ///
    /// # Parameters
    ///
    /// - `dir`: the directory a [`Fixture::stop`] returned.
    /// - `api_key`: the api key of the account the directory holds, whose
    ///   plaintext is only known when it is created.
    pub async fn reopen(dir: TempDir, api_key: String) -> Fixture {
        let services = Services::build(load_config(dir.path()), &BTreeMap::new())
            .await
            .unwrap();
        let user = Users::new(services.database())
            .find_by_username(USERNAME)
            .await
            .unwrap()
            .expect("the account the previous process created");
        let server = Server::start_on(Arc::clone(&services), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            server,
            services,
            api_key,
            user_id: user.id,
            client: reqwest::Client::new(),
        }
    }
}

/// Loads the config file a previous [`write_config`] wrote.
///
/// # Panics
///
/// Panics when the file is missing, which means `dir` never held a fixture.
pub fn load_config(dir: &Path) -> BootstrapConfig {
    BootstrapConfig::load(Some(&dir.join("hearthsound.toml")), &BTreeMap::new()).unwrap()
}

/// The `[settings]` table every test config pins: the orchestrator polls a
/// backend every 50 ms instead of every second or more, so a simulated
/// acquisition takes milliseconds.
const FAST_POLLING: [(&str, &str); 4] = [
    ("acquisition.poll_interval_ms.interactive", "50"),
    ("acquisition.poll_interval_ms.background", "50"),
    ("acquisition.search_poll_interval_ms", "50"),
    ("acquisition.completion_check_interval_ms", "50"),
];

/// Writes a config file into `dir` and loads it.
///
/// # Parameters
///
/// - `dir`: the directory holding the data tree and the config file.
pub fn write_config(dir: &Path) -> BootstrapConfig {
    let data: PathBuf = dir.join("data");
    let mut text = format!(
        "paths.data = \"{}\"\n\n[settings]\n",
        data.to_string_lossy()
    );
    for (key, value) in FAST_POLLING {
        text.push_str(&format!("\"{key}\" = {value}\n"));
    }
    let path = dir.join("hearthsound.toml");
    std::fs::write(&path, text).unwrap();
    BootstrapConfig::load(Some(&path), &BTreeMap::new()).unwrap()
}
