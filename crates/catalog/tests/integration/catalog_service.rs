//! Integration tests for the catalog service over the fixture plugins.
//!
//! Every test builds a real plugin host, registry, instance service and
//! database in a temporary directory through [`crate::common::Env`], installs fixture
//! plugins, and drives the service the way the API layer will. Nothing here
//! reaches the network: the `hello-catalog` fixture answers in process, and
//! `http-catalog` talks to a local wiremock server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use crate::common::{
    Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, SPIN_CATALOG_ID, catalog_endpoint, catalog_id, query,
};
use hearthsound_catalog::CatalogError;
use hearthsound_config::SettingValue;
use hearthsound_config::keys;
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
    assert_eq!(album.album.tracks.len(), 2);
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
async fn a_slow_primary_spends_the_one_fan_out_deadline() {
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
    let error = env.service().search(&query("world")).await.unwrap_err();
    let elapsed = started.elapsed();

    assert!(
        matches!(&error, CatalogError::Unavailable { instance } if instance == "httpcat"),
        "{error}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the fan-out must cost one deadline, not one per instance: {elapsed:?}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_fast_failure_leaves_the_deadline_to_the_fallback() {
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
    env.set(
        keys::SEARCH_CATALOG_TIMEOUT_MS.name(),
        SettingValue::Integer(2_000),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(results.source, Some(Slug::new("hello").unwrap()));
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_artist_name_beyond_the_first_results_is_still_resolved() {
    // The provider ranks three other artists named after the query above the
    // exact match, which is what a search that only read its first page missed.
    let body = json!({
        "artists": [
            { "id": "ar-1", "name": "Night Watch" },
            { "id": "ar-2", "name": "Night Watch Tribute" },
            { "id": "ar-3", "name": "The Night Watch Band" },
            { "id": "ar-4", "name": "Nightwatch" },
        ]
    })
    .to_string();
    let (server, url) = catalog_endpoint(200, &body, None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;

    let found = env
        .service()
        .artist_by_name("Nightwatch")
        .await
        .unwrap()
        .expect("the exact match must be resolved");

    assert_eq!(found.id.provider_id(), "ar-4");
    assert_eq!(found.artist.name, "Nightwatch");
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failing_name_search_is_an_error_the_page_degrades() {
    let (_server, url) = catalog_endpoint(500, "", None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let (_, _, artist) = env
        .stored_album_with_track("Library Album", "Nobody In Particular", 2020)
        .await;

    let error = env
        .service()
        .artist_by_name("Nobody In Particular")
        .await
        .unwrap_err();
    assert!(
        matches!(error, CatalogError::Unavailable { .. }),
        "an outage must not look like a missing artist: {error}"
    );

    let page = env
        .service()
        .artist_page(
            &hearthsound_core::ClientId::Entity(artist),
            &[hearthsound_library::query::Scope::server_for_principal(
                true,
            )],
            hearthsound_catalog::PageDepth::Full,
        )
        .await
        .unwrap()
        .expect("the page the library holds must still answer");
    assert!(matches!(
        page.head,
        hearthsound_catalog::ArtistHead::Stored(id) if id == artist
    ));
    assert_eq!(
        page.albums.len(),
        1,
        "the page answers the album the library holds and no catalog one"
    );
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

#[tokio::test]
async fn album_by_title_costs_one_search_and_is_remembered() {
    let body = json!({
        "albums": [
            { "id": "al-1", "title": "Second Wind (Remastered 2009)", "artists": ["Discography Artist"], "year": 2009 },
            { "id": "al-2", "title": "Second Wind", "artists": ["Someone Else"], "year": 2009 },
        ]
    })
    .to_string();
    let (server, url) = catalog_endpoint(200, &body, None).await;
    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({"url": url}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();

    let found = service
        .album_by_title("Discography Artist", "Second Wind", Some(2008))
        .await
        .unwrap()
        .expect("the provider's album is the same release");
    assert_eq!(found.id.provider_id(), "al-1");
    assert_eq!(requests(&server).await, 1, "one search pairs the album");

    let again = service
        .album_by_title("Discography Artist", "Second Wind", Some(2008))
        .await
        .unwrap()
        .expect("the remembered answer answers");
    assert_eq!(again.id.provider_id(), "al-1");
    assert_eq!(
        requests(&server).await,
        1,
        "the remembered answer costs no provider call"
    );

    // A year the provider contradicts is not the same release.
    assert!(
        service
            .album_by_title("Discography Artist", "Second Wind", Some(1990))
            .await
            .unwrap()
            .is_none(),
        "two years apart is a different release"
    );
}

/// The number of requests the endpoint has received.
///
/// # Parameters
///
/// - `server`: the wiremock endpoint.
async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}
