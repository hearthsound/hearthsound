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

use hearthsound_auth::{ApiKeys, AppPasswords, NewUser, Users};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::secret::SecretKey;
use hearthsound_core::{Availability, EntityId, Layer};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_server::{Server, Services};
use hearthsound_test_support::{ACQUISITION_FIXTURE, fixture_plugin};
use tempfile::TempDir;
use uuid::Uuid;

/// The login name of the test account.
pub const USERNAME: &str = "echo";

/// The account password of the test account.
pub const ACCOUNT_PASSWORD: &str = "hunter22";

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
    /// An app password of the test account, which is what a client that uses
    /// `u` + `t` + `s` signs in with.
    pub app_password: String,
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
        Fixture::build(adjust, true).await
    }

    /// Builds a server whose users table is empty.
    ///
    /// This is the state a fresh deployment starts in: the first-run setup link
    /// is the only way in, and the fixture has no API key to fall back on.
    ///
    /// # Returns
    ///
    /// The fixture, with an empty API key, an empty app password and a nil
    /// account id.
    pub async fn without_account() -> Fixture {
        Fixture::build(|_| {}, false).await
    }

    /// Builds a server whose users table is empty over an adjusted bootstrap
    /// configuration.
    ///
    /// This is what a test that drives the setup link with a non-default
    /// setting uses.
    ///
    /// # Parameters
    ///
    /// - `adjust`: changes the configuration the default config file loads to.
    pub async fn without_account_with_config(adjust: impl FnOnce(&mut BootstrapConfig)) -> Fixture {
        Fixture::build(adjust, false).await
    }

    /// Builds a server, creating the test account unless `account` is false.
    async fn build(adjust: impl FnOnce(&mut BootstrapConfig), account: bool) -> Fixture {
        install_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let mut config = write_config(dir.path());
        adjust(&mut config);
        let secret_key = SecretKey::load(&config.secrets.key_file, &BTreeMap::new()).unwrap();
        let services = Services::build(config, &BTreeMap::new()).await.unwrap();
        let (user_id, api_key, app_password) = if account {
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
            // The secret box is the one the composition root loaded, so the app
            // password this writes is one the running server can decrypt.
            let app_password = AppPasswords::new(services.database(), secret_key.secret_box())
                .generate(user.id, "test")
                .await
                .unwrap()
                .plaintext;
            (user.id, api_key, app_password)
        } else {
            (Uuid::nil(), String::new(), String::new())
        };

        let server = Server::start_on(Arc::clone(&services), "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        Fixture {
            _dir: dir,
            server,
            services,
            api_key,
            app_password,
            user_id,
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

    /// Returns a directory beside the data tree for the files a library folder
    /// holds.
    ///
    /// A library folder may not be, contain, or sit inside the server's own data
    /// directory, so the folders a test adds are siblings of it.
    ///
    /// # Parameters
    ///
    /// - `name`: the directory's name, unique within the test.
    ///
    /// # Returns
    ///
    /// The path; the caller creates it.
    pub fn library_dir(&self, name: &str) -> PathBuf {
        self.services
            .config()
            .paths
            .data
            .parent()
            .expect("the data directory has a parent")
            .join(name)
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
    ///
    /// # Returns
    ///
    /// The fixture. Its app password is empty: an app password's plaintext is
    /// only known when it is created, and a test that reopens a directory has
    /// the API key it created it with instead.
    pub async fn reopen(dir: TempDir, api_key: String) -> Fixture {
        install_crypto_provider();
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
            app_password: String::new(),
            user_id: user.id,
            client: reqwest::Client::new(),
        }
    }
}

/// Installs ring as the process's TLS provider, which reqwest needs before it
/// builds the test client because the workspace builds it without one. The
/// plugin host installs the same provider; whichever runs first wins.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// The acquisition fixture's manifest id.
pub const ACQ_ID: &str = "com.example.acquisition-fixture";

/// How long the acquisition fixture answers `running` before a search completes.
///
/// The acquisition has to be alive while the test cancels it, so the search is
/// kept running far longer than the test.
pub const SEARCH_POLLS: u32 = 60;

/// Installs the acquisition fixture and assigns it to the acquisition layer.
///
/// The instance simulates a backend that finds one lossless candidate and keeps
/// searching, so an acquisition stays alive for the whole test.
///
/// # Parameters
///
/// - `services`: the running server's services.
/// - `downloads`: the directory granted as the instance's import root.
pub async fn install_acquisition(services: &Arc<Services>, downloads: &std::path::Path) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(ACQUISITION_FIXTURE))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, ACQ_ID);
    services
        .instances()
        .create(&InstanceSpec {
            slug: "acq".to_owned(),
            plugin_id: ACQ_ID.to_owned(),
            display_name: "Acquisition fixture".to_owned(),
            server_config: serde_json::json!({
                "candidates": [{
                    "token": "cand-1",
                    "title": "Reload Song",
                    "path": "Reload Artist/Reload Album/01 - Reload Song.flac",
                    "files": [{
                        "name": "01 - Reload Song.flac",
                        "size": 4000,
                        "format": "flac",
                        "bitrateKbps": 900,
                        "sampleRate": 44100,
                        "bitDepth": 16,
                        "durationMs": 200,
                        "trackIndex": 0,
                    }],
                    "source": { "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true },
                }],
                "scenario": "normal",
                "searchPolls": SEARCH_POLLS,
                "downloadQueuedPolls": 0,
                "downloadRunningPolls": 1,
                "downloadBytesPerPoll": 4000,
                "outputRoot": "downloads",
                "matchSuffix": ".flac",
            }),
            host_config: serde_json::json!({
                "importRoots": [{
                    "id": "downloads",
                    "path": downloads.to_string_lossy(),
                    "mode": "move",
                    "readOnly": false,
                }],
                "maxConcurrent": 2,
                "maxConcurrentSearches": 2,
                "searchesPerHour": 120,
            }),
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Acquisition, &["acq".to_owned()])
        .await
        .unwrap();
}

/// Inserts one catalog-only track and returns its id.
///
/// # Parameters
///
/// - `library`: the library tables of the running server's database.
pub async fn insert_remote_track(library: &LibraryRepository) -> EntityId {
    let mut album_input = AlbumInput::new("Reload Album", Origin::Catalog);
    album_input.year = Some(1993);
    album_input.display_artist = Some("Reload Artist".to_owned());
    let album = library
        .upsert_album(&album_input, &[ArtistCredit::new("Reload Artist", "main")])
        .await
        .unwrap();
    let mut input = TrackInput::new("Reload Song", Origin::Catalog, Availability::Remote);
    input.album_id = Some(album);
    input.display_artist = Some("Reload Artist".to_owned());
    input.track_number = Some(1);
    input.duration_ms = Some(200);
    library
        .insert_track(&input, &[ArtistCredit::new("Reload Artist", "main")], &[])
        .await
        .unwrap()
}
