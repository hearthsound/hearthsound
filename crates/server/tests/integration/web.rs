//! Full-stack tests of serving the web client's files.
//!
//! Each test points `web.root` at a directory holding the two files a built
//! client has - the entry document and one hashed asset - and speaks HTTP to
//! the running server, so the root dispatch, the fallback, the path
//! normalization and the response headers are exercised together.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use hearthsound_server::{ServerError, Services};
use reqwest::StatusCode;
use tempfile::TempDir;

use crate::common::{Fixture, write_config};

/// The entry document the client directories hold.
const INDEX: &str = "<!doctype html><title>Hearthsound</title>";

/// The hashed asset the client directories hold.
const APP_JS: &str = "console.log('hearthsound');";

/// Writes the files a built client has: the entry document and one hashed
/// asset below `_app/immutable/`.
fn client_directory() -> TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("index.html"), INDEX).unwrap();
    let immutable = directory.path().join("_app").join("immutable");
    std::fs::create_dir_all(&immutable).unwrap();
    std::fs::write(immutable.join("app.js"), APP_JS).unwrap();
    directory
}

/// Builds a running server over a client directory.
///
/// # Parameters
///
/// - `root`: the directory the server serves as the web client.
async fn fixture_with_client(root: &Path) -> Fixture {
    let root = root.to_path_buf();
    Fixture::with_config(move |config| {
        config.web.root = Some(root);
    })
    .await
}

/// Reads one response header as text.
///
/// # Parameters
///
/// - `response`: the response to read.
/// - `name`: the header name.
fn header(response: &reqwest::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The server root serves the client's entry document to a browser.
#[tokio::test]
async fn the_root_serves_the_client() {
    let directory = client_directory();
    let fixture = fixture_with_client(directory.path()).await;

    let response = fixture
        .client
        .get(format!("{}/", fixture.base_url()))
        .header(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        header(&response, "content-type").as_deref(),
        Some("text/html")
    );
    assert_eq!(
        header(&response, "cache-control").as_deref(),
        Some("no-cache"),
        "the entry document is revalidated so a new build is picked up"
    );
    assert_eq!(response.text().await.unwrap(), INDEX);
}

/// A path the client's router owns serves the entry document.
#[tokio::test]
async fn an_unknown_route_serves_the_client() {
    let directory = client_directory();
    let fixture = fixture_with_client(directory.path()).await;

    let response = fixture
        .client
        .get(format!("{}/library/albums", fixture.base_url()))
        .header("accept", "text/html")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), INDEX);
}

/// A hashed asset is served with its content type and a long cache life.
#[tokio::test]
async fn a_hashed_asset_is_served_immutably() {
    let directory = client_directory();
    let fixture = fixture_with_client(directory.path()).await;

    let response = fixture.get("/_app/immutable/app.js").await;

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = header(&response, "content-type").unwrap_or_default();
    assert!(
        content_type == "application/javascript" || content_type == "text/javascript",
        "unexpected content type: {content_type}"
    );
    assert_eq!(
        header(&response, "cache-control").as_deref(),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(response.text().await.unwrap(), APP_JS);
}

/// A missing asset is an empty 404 for a request that does not ask for HTML.
#[tokio::test]
async fn a_missing_asset_is_not_found() {
    let directory = client_directory();
    let fixture = fixture_with_client(directory.path()).await;

    let response = fixture.get("/missing.js").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.text().await.unwrap().is_empty());
}

/// A path that tries to leave the web root names no asset at all.
#[tokio::test]
async fn a_traversal_attempt_is_not_found() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "not for the client").unwrap();
    let root = outside.path().join("client");
    std::fs::create_dir_all(root.join("_app").join("immutable")).unwrap();
    std::fs::write(root.join("index.html"), INDEX).unwrap();
    let fixture = fixture_with_client(&root).await;

    let response = fixture.get("/..%2fsecret").await;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.text().await.unwrap().is_empty());
}

/// The API surfaces answer their own paths, client or no client.
#[tokio::test]
async fn the_api_surfaces_are_not_the_client() {
    let directory = client_directory();
    let fixture = fixture_with_client(directory.path()).await;

    let response = fixture
        .get(&format!("/rest/ping?apiKey={}&f=json", fixture.api_key))
        .await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");

    let response = fixture.get("/api/v1/nope").await;
    let status = response.status();
    let content_type = header(&response, "content-type");
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(content_type.as_deref(), Some("application/problem+json"));
    assert!(body.contains("not-found"), "body: {body}");
}

/// A `web.root` that is not a directory stops the server from starting.
#[tokio::test]
async fn a_web_root_that_is_not_a_directory_fails_startup() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("client");
    std::fs::write(&file, "not a client").unwrap();
    let mut config = write_config(directory.path());
    config.web.root = Some(file.clone());

    let error = match Services::build(config, &BTreeMap::new()).await {
        Ok(_) => panic!("a web root that is not a directory must not build"),
        Err(error) => error,
    };

    match error {
        ServerError::WebRoot { path, .. } => assert_eq!(path, file),
        other => panic!("expected a web root error, got {other}"),
    }
}
