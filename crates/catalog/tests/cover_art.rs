//! Integration tests for serving catalog cover art through the issuing
//! instance.
//!
//! The tests drive the service over the fixture plugins and a local wiremock
//! server: `hello-catalog` declares `catalog.imageCachePolicy: cache` and
//! describes three cover sizes, and `http-catalog` declares `no-store` and
//! describes one image without dimensions. What the tests assert is where the
//! bytes came from and what was left on disk afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, catalog_id, count_rows, stored_artwork};
use hearthsound_catalog::{CatalogError, CoverArtRequest};
use hearthsound_core::{ClientId, EntityKind, Layer};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The bytes every test image consists of.
const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

/// Serves one image from the mock server.
///
/// # Parameters
///
/// - `server`: the server to mount the endpoint on.
/// - `name`: the image's path, for example `/cover-500.png`.
/// - `status`: the HTTP status to answer with.
async fn serve_image(server: &MockServer, name: &str, status: u16) {
    let mut template = ResponseTemplate::new(status);
    if status < 300 {
        template = template
            .insert_header("content-type", "image/png")
            .set_body_bytes(IMAGE);
    }
    Mock::given(method("GET"))
        .and(path(name.to_owned()))
        .respond_with(template)
        .mount(server)
        .await;
}

/// The paths the mock server has been asked for.
async fn requested_paths(server: &MockServer) -> Vec<String> {
    let mut paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    paths.sort();
    paths
}

/// Builds an environment with a caching `hello` instance whose covers the mock
/// server serves.
///
/// # Returns
///
/// The environment and the server, which the caller keeps alive.
async fn caching_env() -> (Env, MockServer) {
    let server = MockServer::start().await;
    for size in [250, 500, 1_000] {
        serve_image(&server, &format!("/cover-{size}.png"), 200).await;
    }
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    (env, server)
}

#[tokio::test]
async fn the_smallest_image_that_covers_the_request_is_served_and_cached() {
    let (env, server) = caching_env().await;
    let service = env.service();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(300),
        })
        .await
        .unwrap();

    assert_eq!(cover.mime_type, "image/png");
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-500.png"],
        "the smallest image at least 300 pixels wide is the one to serve"
    );

    assert_eq!(count_rows(&env.database, "artwork_cache").await, 1);
    let files = env.artwork_files();
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::fs::read(env.artwork_dir().join(&files[0])).unwrap(),
        IMAGE,
        "the bytes are stored under <data>/artwork/"
    );
    let stored = stored_artwork(
        &env.database,
        &format!(
            "catalog|hello|track|hello-track-1|500|{}/cover-500.png",
            server.uri()
        ),
    )
    .await
    .expect("the image must be indexed");
    assert_eq!(stored.2, "image/png");
    assert_eq!((stored.3, stored.4), (Some(500), Some(500)));

    let again = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(300),
        })
        .await
        .unwrap();

    assert_eq!(again, cover);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-500.png"],
        "a cached image must not be fetched again"
    );
}

#[tokio::test]
async fn a_request_larger_than_every_image_serves_the_largest() {
    let (env, server) = caching_env().await;

    let cover = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(4_000),
        })
        .await
        .unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(requested_paths(&server).await, vec!["/cover-1000.png"]);
}

#[tokio::test]
async fn a_stored_entity_is_served_through_its_provider_alias() {
    let (env, server) = caching_env().await;
    let service = env.service();
    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(track),
            size: None,
        })
        .await
        .unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-1000.png"],
        "a materialized entity reaches its instance through the p:hello alias"
    );
}

#[tokio::test]
async fn a_no_store_instance_returns_the_image_without_caching_it() {
    let server = MockServer::start().await;
    serve_image(&server, "/remote-cover.png", 200).await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"]}]"#,
        ))
        .mount(&server)
        .await;

    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({
            "url": format!("{}/search", server.uri()),
            "coverUrl": format!("{}/remote-cover.png", server.uri()),
        }),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();

    let request = CoverArtRequest {
        id: ClientId::Catalog(catalog_id("httpcat", EntityKind::Track, "remote-1")),
        size: None,
    };
    let cover = service.cover_art(&request).await.unwrap();

    assert_eq!(cover.mime_type, "image/png");
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(count_rows(&env.database, "artwork_cache").await, 0);
    assert!(
        env.artwork_files().is_empty(),
        "a no-store instance's images are never written to disk"
    );

    service.cover_art(&request).await.unwrap();
    assert_eq!(
        requested_paths(&server)
            .await
            .iter()
            .filter(|path| *path == "/remote-cover.png")
            .count(),
        2,
        "nothing was cached, so the second request fetches again"
    );
}

#[tokio::test]
async fn an_entity_that_describes_no_image_is_not_found() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Artist, "hello-artist-1")),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn a_stored_entity_no_instance_issued_is_not_found() {
    let env = Env::new().await;
    let local = common::insert_local_track(&env.database, "Local Song", None, None).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(local),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn an_image_that_is_gone_is_not_found() {
    let server = MockServer::start().await;
    serve_image(&server, "/cover-1000.png", 404).await;
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
    assert_eq!(count_rows(&env.database, "artwork_cache").await, 0);
}

#[tokio::test]
async fn a_cached_image_that_disappeared_is_fetched_again() {
    let (env, server) = caching_env().await;
    let service = env.service();
    let request = CoverArtRequest {
        id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
        size: None,
    };
    service.cover_art(&request).await.unwrap();

    let files = env.artwork_files();
    assert_eq!(files.len(), 1);
    std::fs::remove_file(env.artwork_dir().join(&files[0])).unwrap();

    let cover = service.cover_art(&request).await.unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-1000.png", "/cover-1000.png"],
        "a row whose file is gone is a miss, not a failure"
    );
    assert_eq!(env.artwork_files().len(), 1);
}
