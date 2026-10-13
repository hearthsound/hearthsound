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
use hearthsound_api_subsonic::SubsonicState;
use hearthsound_auth::{ApiKeys, AppPasswords, Authenticator, NewUser, Users};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, keys};
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_db::{Database, MIGRATOR};
use tempfile::TempDir;
use tower::ServiceExt;

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
}

impl Fixture {
    /// Builds the fixture.
    pub async fn new() -> Fixture {
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
        let authenticator = Arc::new(Authenticator::new(&database, secret_box, 10));
        let state = SubsonicState::new(Arc::clone(&database), settings.clone(), authenticator);

        Fixture {
            _dir: dir,
            database,
            settings,
            user_id: EntityId::from_uuid(IdPrefix::User, user.id),
            api_key,
            app_password,
            state,
        }
    }

    /// Returns the `/rest` router under test.
    pub fn app(&self) -> Router {
        hearthsound_api_subsonic::router(self.state.clone())
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
    /// The response's body, decoded as UTF-8.
    pub body: String,
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
            body: String::from_utf8(bytes.to_vec()).unwrap(),
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
    let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
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
