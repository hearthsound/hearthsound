//! Full-stack tests of the mounted surfaces.
//!
//! The tests start the real server on an ephemeral port over a temporary data
//! directory and speak HTTP to it, so the routes, the layers, and the services
//! behind them are exercised together.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_server::Services;

/// The health probe answers once the process is serving.
#[tokio::test]
async fn healthz_reports_the_process_is_up() {
    let fixture = crate::common::Fixture::new().await;

    let response = fixture.get("/healthz").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
}

/// The readiness probe answers positively for a migrated database.
#[tokio::test]
async fn readyz_reports_ready_for_a_migrated_database() {
    let fixture = crate::common::Fixture::new().await;

    let response = fixture.get("/readyz").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ready");
}

/// The readiness probe refuses a database that is not at the bundled schema.
///
/// The migration bookkeeping table is dropped, which is what a database
/// migrated by another binary looks like from this process's point of view.
#[tokio::test]
async fn readyz_refuses_a_database_from_another_schema_version() {
    let fixture = crate::common::Fixture::new().await;
    sqlx::query("DROP TABLE _sqlx_migrations")
        .execute(fixture.services.database().writer())
        .await
        .unwrap();

    let response = fixture.get("/readyz").await;

    assert_eq!(response.status(), 503);
    assert_eq!(response.text().await.unwrap(), "not ready");
}

/// The server root answers the JSON status object.
#[tokio::test]
async fn root_answers_a_json_status() {
    let fixture = crate::common::Fixture::new().await;

    let response = fixture.get("/").await;
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response.text().await.unwrap();

    assert_eq!(content_type.as_deref(), Some("application/json"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"status": "ok"})
    );
}

/// A `/rest` call with an API key created through `hearthsound-auth` succeeds.
#[tokio::test]
async fn rest_accepts_an_api_key_created_through_auth() {
    let fixture = crate::common::Fixture::new().await;

    let response = fixture
        .get(&format!("/rest/ping?apiKey={}&f=json", fixture.api_key))
        .await;
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");

    let response = fixture
        .get(&format!("/rest/ping.view?apiKey={}", fixture.api_key))
        .await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("status=\"ok\""), "body: {body}");
}

/// A second API key for a second account authenticates against the same server.
///
/// The key is created after the server started, which is the path the CLI takes
/// when it writes the database while a server runs.
#[tokio::test]
async fn rest_accepts_a_key_created_after_startup() {
    let fixture = crate::common::Fixture::new().await;
    let user = Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &NewUser::new("later", "Later", "hunter2", false),
        )
        .await
        .unwrap();
    let key = ApiKeys::new(fixture.services.database())
        .generate(user.id, "later")
        .await
        .unwrap()
        .plaintext;

    let response = fixture
        .get(&format!("/rest/ping?apiKey={key}&f=json"))
        .await;
    let status = response.status();
    let body = response.text().await.unwrap();

    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");
}

/// The composition root creates the whole on-disk layout.
#[tokio::test]
async fn build_creates_the_data_and_storage_trees() {
    let fixture = crate::common::Fixture::new().await;
    let paths = &fixture.services.config().paths;

    for path in [
        &paths.data,
        &paths.storage,
        &paths.plugins,
        &paths.artwork,
        &paths.backups,
        &paths.library,
        &paths.cache,
        &paths.staging,
        &paths.uploads_staging,
    ] {
        assert!(path.is_dir(), "{} is missing", path.display());
    }
    assert!(paths.data.join("secret.key").is_file());
    assert!(fixture.database_path().is_file());
}

/// A second build over the same directory keeps the secret key.
#[tokio::test]
async fn a_restart_keeps_the_secret_key() {
    let fixture = crate::common::Fixture::new().await;
    let key_file = fixture.services.config().secrets.key_file.clone();
    let first = std::fs::read(&key_file).unwrap();

    let services = Services::build(
        fixture.services.config().clone(),
        &std::collections::BTreeMap::new(),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&key_file).unwrap(), first);
    assert!(services.is_ready().await);
}
