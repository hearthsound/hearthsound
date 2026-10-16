//! Helpers shared by the integration tests.
//!
//! Every test builds a temporary database with the server schema, one account
//! with an API key and an app password, and the shared state; the request
//! helpers send requests through the real router.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::response::Response as HttpResponse;
use hearthsound_activity::ActivityService;
use hearthsound_api_subsonic::{PendingSource, SubsonicState};
use hearthsound_auth::{ApiKeys, AppPasswords, Authenticator, NewUser, Users};
use hearthsound_catalog::CatalogService;
use hearthsound_config::secret::SecretBox;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::events::EventBus;
use hearthsound_core::{EntityId, IdPrefix, Layer};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{JobQueue, QueueOptions};
use hearthsound_library::StorageRoots;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{
    InstanceService, InstanceSpec, PluginHost, PluginRepository, PluginSource, ProviderRegistry,
};
use hearthsound_test_support::{ARTWORK_FIXTURE, HELLO_CATALOG, fixture_plugin};
use tempfile::TempDir;
use tower::ServiceExt;

/// The bytes the artwork layer's mock server serves.
pub const ARTWORK_IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nartwork layer image";

/// The login name of the test account.
pub const USERNAME: &str = "echo";

/// The account password of the test account.
pub const ACCOUNT_PASSWORD: &str = "hunter2";

/// The filters that make a seeded response stable across runs.
///
/// A seeded row's `created` timestamp is the moment the test inserted it, so it
/// differs from run to run; the filter replaces it with a fixed marker.
/// Everything else in a snapshot is exactly what the endpoint computed, because
/// the seeded client ids are replaced by name through
/// [`seed::Seeded::stabilize`].
pub const SNAPSHOT_FILTERS: [(&str, &str); 1] = [(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})",
    "TIMESTAMP",
)];

pub mod artwork;
pub mod seed;

/// A migrated temporary database with one account and its credentials.
pub struct Fixture {
    /// Keeps the temporary directory alive.
    _dir: TempDir,
    /// The open database.
    pub database: Arc<Database>,
    /// The settings service, which a test changes to arrange its case.
    pub settings: Settings,
    /// The test account's client id, `us_<...>`.
    pub user_id: EntityId,
    /// An API key of the test account.
    pub api_key: String,
    /// An app password of the test account.
    pub app_password: String,
    /// The shared state the router is built with.
    pub state: SubsonicState,
    /// The managed storage roots the state resolves stored files against.
    pub roots: StorageRoots,
    /// The ffmpeg runner the state transcodes and verifies through.
    ffmpeg: Arc<FfmpegRunner>,
    /// The artwork cache directory catalog cover art is written to.
    artwork: PathBuf,
    /// The plugin host a fixture plugin is installed into.
    host: Arc<PluginHost>,
    /// The plugin tables an instance is configured in.
    repository: PluginRepository,
    /// The secret box instance configurations are sealed with.
    secrets: SecretBox,
    /// The provider registry the catalog layer resolves instances through.
    registry: Arc<ProviderRegistry>,
    /// The library scanner the scan endpoints report.
    pub scanner: Arc<LibraryScanner>,
    /// The job queue a scan is enqueued on.
    pub jobs: Arc<JobQueue>,
}

impl Fixture {
    /// Builds the fixture without an acquisition layer.
    pub async fn new() -> Fixture {
        Fixture::build(None).await
    }

    /// Builds a fixture whose stream service answers pending tracks with the
    /// given acquisition source.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer behind `/rest/stream` for a track the
    ///   server has not stored.
    pub async fn with_pending(pending: Arc<dyn PendingSource>) -> Fixture {
        Fixture::build(Some(pending)).await
    }

    /// Builds the fixture.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer the shared state is built with, or
    ///   `None` for the `404` behavior of a process without one.
    async fn build(pending: Option<Arc<dyn PendingSource>>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let database = Arc::new(
            Database::open(&dir.path().join("hearthsound.db"))
                .await
                .unwrap(),
        );
        MIGRATOR.run(database.writer()).await.unwrap();
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
        )
        .await
        .unwrap();

        let user = Users::new(&database)
            .create(
                &settings.current(),
                &NewUser::new(USERNAME, "Echo", ACCOUNT_PASSWORD, false),
            )
            .await
            .unwrap();
        let secret_box = hearthsound_config::secret::SecretKey::generate().secret_box();
        let api_key = ApiKeys::new(&database)
            .generate(user.id, "test")
            .await
            .unwrap()
            .plaintext;
        let app_password = AppPasswords::new(&database, secret_box.clone())
            .generate(user.id, "test")
            .await
            .unwrap()
            .plaintext;
        let authenticator = Arc::new(Authenticator::new(&database, secret_box.clone(), 10));

        let host = Arc::new(
            PluginHost::new(
                dir.path().join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&host),
            repository.clone(),
            secret_box.clone(),
        ));
        let artwork = dir.path().join("artwork");
        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            artwork.clone(),
        ));
        let activity = Arc::new(ActivityService::new(&database));
        let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));
        std::fs::create_dir_all(roots.library.as_path()).unwrap();
        std::fs::create_dir_all(roots.cache.as_path()).unwrap();
        std::fs::create_dir_all(dir.path().join("artwork")).unwrap();
        let ffmpeg = Arc::new(FfmpegRunner::new(None, 2));
        let state = SubsonicState::new(
            Arc::clone(&database),
            settings.clone(),
            authenticator,
            catalog,
            activity,
            Arc::clone(&ffmpeg),
            roots.clone(),
        );
        let state = match pending {
            Some(pending) => state.with_pending_source(pending),
            None => state,
        };
        // The scanner the scan endpoints report and the queue they enqueue on.
        let scanner = Arc::new(LibraryScanner::new(
            &database,
            Arc::new(EventBus::new()),
            settings.clone(),
        ));
        let jobs = Arc::new(JobQueue::new(&database, QueueOptions::default()));
        let state = state.with_scanner(Arc::clone(&scanner), Arc::clone(&jobs));

        Fixture {
            _dir: dir,
            database,
            settings,
            user_id: EntityId::from_uuid(IdPrefix::User, user.id),
            api_key,
            app_password,
            state,
            roots,
            ffmpeg,
            artwork,
            host,
            repository,
            secrets: secret_box,
            registry,
            scanner,
            jobs,
        }
    }

    /// Installs the `hello-catalog` fixture and assigns it to the catalog layer.
    ///
    /// The instance answers every non-empty search with one greeting track,
    /// album, and artist, so a search response's catalog side is exactly those
    /// three entities under the `hello` slug.
    pub async fn install_hello_catalog(&self) {
        let installed = self
            .host
            .install(&fixture_plugin(HELLO_CATALOG))
            .await
            .unwrap();
        let plugin_id = self
            .repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap()
            .id;
        let instances = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        instances
            .create(&InstanceSpec {
                slug: "hello".to_owned(),
                plugin_id,
                display_name: "Hello catalog".to_owned(),
                server_config: serde_json::json!({ "greeting": "Hello" }),
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        instances
            .set_layer(Layer::Catalog, &["hello".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
    }

    /// Installs the artwork fixture as the artwork layer and serves one image.
    ///
    /// The fixture answers a portrait for any artist whose name is `Hello
    /// Artist` and a cover for `Hello Album`, from the path the caller names,
    /// which a mock server serves.
    ///
    /// # Parameters
    ///
    /// - `path`: the image path the fixture configures.
    /// - `artists`: the artist names the fixture answers a portrait for.
    ///
    /// # Returns
    ///
    /// The bytes the mock server serves as the image.
    pub async fn install_artwork_layer(&self, path: &str, artists: &[&str]) -> Vec<u8> {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(path.to_owned()))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(ARTWORK_IMAGE),
            )
            .mount(&server)
            .await;
        let installed = self
            .host
            .install(&fixture_plugin(ARTWORK_FIXTURE))
            .await
            .unwrap();
        let plugin_id = self
            .repository
            .register_installed(&installed, PluginSource::Sideload)
            .await
            .unwrap()
            .id;
        let instances = InstanceService::new(
            Arc::clone(&self.host),
            self.repository.clone(),
            self.secrets.clone(),
        );
        instances
            .create(&InstanceSpec {
                slug: "artwork".to_owned(),
                plugin_id,
                display_name: "Artwork".to_owned(),
                server_config: serde_json::json!({
                    "imageUrl": server.uri(),
                    "albums": [{
                        "title": "Hello Album",
                        "images": [{ "path": path, "width": 500 }],
                    }],
                    "artists": artists
                        .iter()
                        .map(|name| serde_json::json!({
                            "title": name,
                            "images": [{ "path": path, "width": 500 }],
                        }))
                        .collect::<Vec<_>>(),
                }),
                host_config: serde_json::Value::Null,
            })
            .await
            .unwrap();
        instances
            .set_layer(Layer::Artwork, &["artwork".to_owned()])
            .await
            .unwrap();
        self.registry.rebuild().await.unwrap();
        // The server lives as long as the fixture keeps its own runtime alive;
        // leaking it here is what a test's mock server does in this harness.
        std::mem::forget(server);
        ARTWORK_IMAGE.to_vec()
    }

    /// Returns the managed storage roots the state resolves files against.
    pub fn storage_roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Returns the ffmpeg runner the state transcodes and verifies through.
    ///
    /// A test reads its in-flight count to assert that a request started (or
    /// did not start) an ffmpeg process.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.ffmpeg
    }

    /// Returns the managed library root a `library` file resolves against.
    ///
    /// The directory exists from [`Fixture::new`], so a test can write a file
    /// into it and insert the matching row.
    pub fn library_root(&self) -> PathBuf {
        self.roots.library.clone()
    }

    /// Returns the artwork cache directory, which exists from
    /// [`Fixture::new`].
    pub fn artwork_dir(&self) -> PathBuf {
        self.artwork.clone()
    }

    /// Returns the `/rest` router under test.
    pub fn app(&self) -> Router {
        hearthsound_api_subsonic::router(self.state.clone())
    }

    /// Returns the `/rest` router with a different acquisition layer.
    ///
    /// A test that needs the acquisition layer to write to the fixture's own
    /// database builds the source after the fixture and installs it here.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer behind `/rest/stream`.
    pub fn app_with(&self, pending: Arc<dyn PendingSource>) -> Router {
        hearthsound_api_subsonic::router(self.state.clone().with_pending_source(pending))
    }

    /// Creates a directory inside the fixture's temporary directory.
    ///
    /// A test that seeds library folders needs a path that exists and is a
    /// directory, because adding a folder canonicalizes it.
    ///
    /// # Parameters
    ///
    /// - `name`: the directory's name.
    ///
    /// # Returns
    ///
    /// The absolute path of the created directory.
    pub fn subdir(&self, name: &str) -> PathBuf {
        let path = self._dir.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Creates a second account with the given permissions and returns an API
    /// key for it.
    ///
    /// The permissions are written to `users.default_permissions` first, which
    /// is where a new account's permission flags come from.
    pub async fn account_with(&self, permissions: &[&str]) -> String {
        self.settings
            .set(
                keys::USERS_DEFAULT_PERMISSIONS.name(),
                SettingValue::StringList(permissions.iter().map(|name| name.to_string()).collect()),
                None,
            )
            .await
            .unwrap();
        let user = Users::new(&self.database)
            .create(
                &self.settings.current(),
                &NewUser::new("limited", "Limited", ACCOUNT_PASSWORD, false),
            )
            .await
            .unwrap();
        ApiKeys::new(&self.database)
            .generate(user.id, "limited")
            .await
            .unwrap()
            .plaintext
    }
}

/// One response, read into memory.
pub struct Probe {
    /// The HTTP status.
    pub status: StatusCode,
    /// The response's head.
    pub headers: HeaderMap,
    /// The response's body, decoded as UTF-8 with invalid sequences replaced.
    ///
    /// An envelope is always UTF-8; a binary body is read through
    /// [`Probe::bytes`] instead, because decoding it as text would lose bytes.
    pub body: String,
    /// The response's body, exactly as it was written.
    pub bytes: Vec<u8>,
}

impl Probe {
    /// Reads a response into a probe.
    async fn read(response: HttpResponse) -> Probe {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Probe {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes: bytes.to_vec(),
        }
    }

    /// Returns the response's content type.
    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
    }
}

/// Sends a GET request through a router.
pub async fn get(app: &Router, uri: &str) -> Probe {
    request(app, Method::GET, uri, &[]).await
}

/// Sends a request with explicit method and headers through a router.
///
/// # Parameters
///
/// - `app`: the router under test.
/// - `method`: the request method, `GET` or `HEAD`.
/// - `uri`: the request target, including its query string.
/// - `headers`: the request headers, for example `Range` or `If-None-Match`.
pub async fn request(app: &Router, method: Method, uri: &str, headers: &[(&str, &str)]) -> Probe {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).unwrap();
    Probe::read(app.clone().oneshot(request).await.unwrap()).await
}

/// Sends a POST request with an `application/x-www-form-urlencoded` body.
pub async fn post_form(app: &Router, uri: &str, form: &str) -> Probe {
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form.to_owned()))
        .unwrap();
    Probe::read(app.clone().oneshot(request).await.unwrap()).await
}

/// Asserts that a response is a failed envelope carrying an error code.
///
/// The code is looked for in the JSON member and in the XML attribute, so a
/// test can assert either encoding.
pub fn assert_error(probe: &Probe, code: i32) {
    assert_eq!(probe.status, StatusCode::OK, "body: {}", probe.body);
    assert!(
        probe.body.contains(&format!("\"code\":{code}"))
            || probe.body.contains(&format!("code=\"{code}\"")),
        "expected error {code} in: {}",
        probe.body
    );
}

/// Asserts that a response is a successful envelope.
pub fn assert_ok(probe: &Probe) {
    assert_eq!(probe.status, StatusCode::OK, "body: {}", probe.body);
    assert!(
        probe.body.contains("\"status\":\"ok\"") || probe.body.contains("status=\"ok\""),
        "expected a successful envelope in: {}",
        probe.body
    );
}
