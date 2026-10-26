//! Full-stack tests of the mounted surfaces.
//!
//! The tests start the real server on an ephemeral port over a temporary data
//! directory and speak HTTP to it, so the routes, the layers, and the services
//! behind them are exercised together.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_server::{Services, WebAssets};

/// The health probe answers once the process is serving.
#[tokio::test]
async fn healthz_reports_the_process_is_up() {
    let fixture = crate::common::Fixture::new().await;

    let response = fixture.get("/healthz").await;

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "ok");
}

/// A host name that resolves to more than one address is served on every one of
/// them, so a client that reaches it over either protocol is answered.
#[tokio::test]
async fn a_host_name_that_resolves_twice_is_served_on_both() {
    use std::net::ToSocketAddrs;

    // One free port, so every resolved address can be bound to the same one.
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let fixture = crate::common::Fixture::on_configured_address(|config| {
        config.server.bind = "localhost".to_owned();
        config.server.port = port;
    })
    .await;

    let mut resolved: Vec<std::net::SocketAddr> =
        ("localhost", port).to_socket_addrs().unwrap().collect();
    resolved.sort();
    resolved.dedup();
    let mut bound = fixture.server.local_addrs().to_vec();
    bound.sort();
    assert_eq!(
        bound, resolved,
        "every address the name resolves to is served"
    );
    assert!(
        resolved.len() > 1,
        "the test needs a name with more than one address"
    );

    for address in fixture.server.local_addrs() {
        let response = fixture
            .client
            .get(format!("http://{address}/healthz"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{address} did not answer");
        assert_eq!(response.text().await.unwrap(), "ok");
    }

    fixture.stop().await;
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
            &NewUser::new("later", "Later", "hunter22", false),
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

    // A restart is the previous process gone: the serving process holds the data
    // directory's lock, so the second build waits until the first stopped.
    let config = fixture.services.config().clone();
    let dir = fixture.stop().await;
    let services = Services::build(config, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    assert_eq!(std::fs::read(&key_file).unwrap(), first);
    assert!(services.is_ready().await);
    drop(services);
    drop(dir);
}

/// The server root answers the JSON body a client probes it with.
///
/// The root serves the web client's entry document only when a client is
/// available and the request asks for HTML, so a client that probes the root
/// to learn whether it is talking to a Hearthsound server - with no `Accept`
/// header or with `*/*` - gets the status object. A build without an embedded
/// client and without `web.root` has nothing to serve, so there even a request
/// asking for HTML gets it.
#[tokio::test]
async fn the_server_root_answers_json() {
    let fixture = crate::common::Fixture::new().await;
    let mut accepts = vec![None, Some("*/*")];
    if *fixture.services.web() == WebAssets::Absent {
        accepts.push(Some("text/html,application/xhtml+xml"));
    }

    for accept in accepts {
        let mut request = fixture.client.get(format!("{}/", fixture.base_url()));
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        let response = request.send().await.unwrap();

        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
            "the root is JSON with accept {accept:?}"
        );
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"status":"ok"}"#,
            "the root body with accept {accept:?}"
        );
    }
}
