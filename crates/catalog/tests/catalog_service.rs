//! Integration tests for the catalog service over the fixture plugins.
//!
//! Every test builds a real plugin host, registry, instance service and
//! database in a temporary directory through [`common::Env`], installs fixture
//! plugins, and drives the service the way the API layer will. Nothing here
//! reaches the network: the `hello-catalog` fixture answers in process, and
//! `http-catalog` talks to a local wiremock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{
    Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, SPIN_CATALOG_ID, catalog_endpoint, catalog_id, query,
    setting_keys as keys,
};
use hearthsound_catalog::CatalogError;
use hearthsound_config::SettingValue;
use hearthsound_core::{EntityKind, Layer, Slug};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG, SPIN_CATALOG};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn search_returns_the_primary_instances_results_with_their_ids() {
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    let track = &results.tracks[0];
    assert_eq!(track.id.slug().as_str(), "hello");
    assert_eq!(track.id.kind(), EntityKind::Track);
    assert_eq!(track.id.provider_id(), "hello-track-1");
    assert_eq!(track.track.title, "Hi, world");
    assert_eq!(track.track.duration_ms, Some(180_000));
    assert_eq!(results.albums.len(), 1);
    assert_eq!(results.albums[0].id.provider_id(), "hello-album-1");
    assert_eq!(results.albums[0].album.title, "Hello Album");
    assert_eq!(results.artists.len(), 1);
    assert_eq!(results.artists[0].id.provider_id(), "hello-artist-1");
    assert_eq!(results.artists[0].artist.name, "Hello Artist");
}

#[tokio::test]
async fn search_is_empty_when_catalog_search_is_disabled() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    env.set(
        keys::SEARCH_CATALOG_ENABLED.name(),
        SettingValue::Bool(false),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert_eq!(results.source, None);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a disabled catalog search must not contact an instance"
    );
}

#[tokio::test]
async fn search_asks_nothing_when_the_result_cap_is_zero() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    env.set(
        keys::SEARCH_CATALOG_MAX_RESULTS.name(),
        SettingValue::Integer(0),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a zero result cap must not contact an instance"
    );
}

#[tokio::test]
async fn search_without_a_catalog_instance_is_an_empty_search() {
    let env = Env::new().await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert!(results.is_empty());
    assert_eq!(results.source, None);
}

#[tokio::test]
async fn lookups_route_by_the_slug_the_id_carries() {
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let track = service
        .track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();
    assert_eq!(track.track.title, "Hi, world");
    assert_eq!(track.id.provider_id(), "hello-track-1");

    let album = service
        .album(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap();
    assert_eq!(album.album.title, "Hello Album");
    assert_eq!(album.album.tracks.len(), 1);
    assert_eq!(album.album.tracks[0].id, "hello-track-1");

    let artist = service
        .artist(&catalog_id("hello", EntityKind::Artist, "hello-artist-1"))
        .await
        .unwrap();
    assert_eq!(artist.artist.name, "Hello Artist");

    let albums = service.artist_albums(&artist.id, 10, 0).await.unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].id.provider_id(), "hello-album-1");

    let top = service.artist_top_tracks(&artist.id, 5).await.unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].id.provider_id(), "hello-track-1");

    assert!(
        service
            .artist_albums(&artist.id, 10, 1)
            .await
            .unwrap()
            .is_empty(),
        "the offset must reach the provider"
    );
}

#[tokio::test]
async fn an_unknown_slug_or_a_wrong_kind_is_not_found() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let unknown = service
        .track(&catalog_id("nowhere", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap_err();
    assert!(matches!(unknown, CatalogError::NotFound), "{unknown}");

    let mixed = service
        .track(&catalog_id("hello", EntityKind::Album, "hello-album-1"))
        .await
        .unwrap_err();
    assert!(matches!(mixed, CatalogError::NotFound), "{mixed}");

    let missing = service
        .track(&catalog_id("hello", EntityKind::Track, "nope"))
        .await
        .unwrap_err();
    assert!(matches!(missing, CatalogError::NotFound), "{missing}");
}

#[tokio::test]
async fn an_instance_that_is_not_assigned_serves_no_lookup() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    let service = env.service();

    let error = service
        .track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap_err();
    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn search_falls_back_when_the_primary_errors() {
    let (server, url) = catalog_endpoint(500, "", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat", "hello"]).await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.title, "Hi, world");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn search_falls_back_when_the_primary_exceeds_its_deadline() {
    let (server, url) = catalog_endpoint(200, "[]", Some(Duration::from_secs(2))).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"greeting": "Hi"}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat", "hello"]).await;
    env.set(
        keys::SEARCH_CATALOG_TIMEOUT_MS.name(),
        SettingValue::Integer(100),
    )
    .await;

    let started = Instant::now();
    let results = env.service().search(&query("world")).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    assert!(
        elapsed < Duration::from_secs(1),
        "the fallback must not wait for the slow primary: {elapsed:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_rate_limited_instance_is_reported_with_its_retry_delay() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "2"))
        .mount(&server)
        .await;
    let url = format!("{}/search", server.uri());

    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;

    let error = env.service().search(&query("world")).await.unwrap_err();

    assert_eq!(error.retry_after_ms(), Some(2_000), "{error}");
    assert_eq!(error.instance(), Some("httpcat"));
    assert!(error.is_transient());
}

#[tokio::test]
async fn a_failing_primary_is_unavailable_when_no_fallback_answers() {
    let (_server, url) = catalog_endpoint(503, "", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;

    let error = env.service().search(&query("world")).await.unwrap_err();

    assert!(
        matches!(&error, CatalogError::Unavailable { instance } if instance == "httpcat"),
        "{error}"
    );
}

#[tokio::test]
async fn lookup_isrc_uses_the_primary_and_never_merges() {
    let (server, url) = catalog_endpoint(200, "[]", None).await;
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello", "httpcat"]).await;

    let tracks = env.service().lookup_isrc("USRC17607839").await.unwrap();

    assert!(tracks.is_empty());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "a later instance is consulted only when the primary does not answer"
    );
}

#[tokio::test]
async fn lookup_isrc_reports_the_failure_of_the_last_instance() {
    let env = Env::new().await;
    env.instance("spin", SPIN_CATALOG, SPIN_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["spin"]).await;

    let error = env.service().lookup_isrc("USRC17607839").await.unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}
