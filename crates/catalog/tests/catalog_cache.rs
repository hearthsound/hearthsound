//! Integration tests for the two-level catalog response cache.
//!
//! The observable effect of a cache is a provider call that does not happen, so
//! every test here watches the wiremock endpoint the `http-catalog` fixture
//! searches: a request count that stays at one across two searches is the cache
//! working, and a row's `expires_at` is the lifetime the service chose.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, cache_row_lifetime, catalog_endpoint, catalog_id,
    count_cache_rows, http_config, now_millis, query, setting_keys, write_cache_row,
};
use hearthsound_catalog::{ENTITY_CACHE_TTL, RequestKey, SEARCH_CACHE_TTL};
use hearthsound_config::SettingValue;
use hearthsound_core::{EntityKind, Layer};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG};
use wiremock::MockServer;

/// One search response the HTTP catalog fixture maps into a track.
const ONE_TRACK: &str = r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"],"duration_ms":1000,"isrc":"USRC17607839"}]"#;

/// The stored form of the same response, as `catalog_cache` holds it.
///
/// A catalog track's artists are credits with names and identifiers, which is
/// why the stored shape spells them as objects where the remote payload spells
/// them as strings.
const STORED_RESULTS: &str = r#"{"tracks":[{"id":"remote-1","title":"Remote Song","artists":[{"name":"Remote Artist","ids":[]}],"album":null,"duration_ms":1000,"disc_number":null,"track_number":null,"isrc":"USRC17607839","explicit":null,"ids":[]}],"albums":[],"artists":[]}"#;

/// Builds the environment one cache test starts from: an `http-catalog` instance
/// pointing at a wiremock endpoint, assigned as the primary catalog instance.
///
/// # Returns
///
/// The environment and the server, which the caller keeps alive.
async fn http_env(body: &str) -> (Env, MockServer) {
    let (server, url) = catalog_endpoint(200, body, None).await;
    let env = Env::new().await;
    env.instance("httpcat", HTTP_CATALOG, HTTP_CATALOG_ID, http_config(&url))
        .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    (env, server)
}

/// The number of requests the endpoint has received.
async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn a_repeated_search_is_answered_from_memory() {
    let (env, server) = http_env(ONE_TRACK).await;
    let service = env.service();

    let first = service.search(&query("world")).await.unwrap();
    let second = service.search(&query("world")).await.unwrap();

    assert_eq!(first, second);
    assert_eq!(
        requests(&server).await,
        1,
        "the second search must be cached"
    );
    assert_eq!(first.tracks[0].track.title, "Remote Song");
}

#[tokio::test]
async fn a_second_service_answers_from_the_table() {
    let (env, server) = http_env(ONE_TRACK).await;

    let first = env.service().search(&query("world")).await.unwrap();
    // A fresh service has an empty in-memory tier but the same database.
    let second = env.service().search(&query("world")).await.unwrap();

    assert_eq!(first, second);
    assert_eq!(
        requests(&server).await,
        1,
        "the persistent tier must answer a fresh service"
    );
}

#[tokio::test]
async fn the_cache_is_keyed_by_instance() {
    let (first_server, first_url) = catalog_endpoint(200, ONE_TRACK, None).await;
    let (second_server, second_url) = catalog_endpoint(200, ONE_TRACK, None).await;
    let env = Env::new().await;
    env.instance(
        "one",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        http_config(&first_url),
    )
    .await;
    env.instance(
        "two",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        http_config(&second_url),
    )
    .await;
    env.assign(Layer::Catalog, &["one", "two"]).await;
    let service = env.service();

    service.search(&query("world")).await.unwrap();
    service.search(&query("world")).await.unwrap();

    assert_eq!(requests(&first_server).await, 1);
    assert_eq!(
        requests(&second_server).await,
        0,
        "the fallback is not asked while the primary answers"
    );
}

#[tokio::test]
async fn a_stale_row_is_ignored() {
    let (env, server) = http_env(ONE_TRACK).await;
    let key = RequestKey::search(&query("world"));
    write_cache_row(
        &env.database,
        env.instance_id("httpcat"),
        key.as_str(),
        r#"{"tracks":[],"albums":[],"artists":[]}"#,
        Some(now_millis() - 1_000),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(requests(&server).await, 1, "a stale row must not be served");
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.title, "Remote Song");
}

#[tokio::test]
async fn a_stored_response_is_served_without_asking_the_provider() {
    let (env, server) = http_env(ONE_TRACK).await;
    let key = RequestKey::search(&query("world"));
    write_cache_row(
        &env.database,
        env.instance_id("httpcat"),
        key.as_str(),
        STORED_RESULTS,
        Some(now_millis() + 60_000),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(requests(&server).await, 0);
    assert_eq!(results.tracks.len(), 1);
    assert_eq!(results.tracks[0].track.title, "Remote Song");
    assert_eq!(
        results.tracks[0].track.isrc.as_deref(),
        Some("USRC17607839")
    );
    assert_eq!(results.tracks[0].id.provider_id(), "remote-1");
    assert_eq!(results.source.unwrap().as_str(), "httpcat");
}

#[tokio::test]
async fn a_stored_response_that_no_longer_decodes_is_a_miss() {
    let (env, server) = http_env(ONE_TRACK).await;
    let key = RequestKey::search(&query("world"));
    write_cache_row(
        &env.database,
        env.instance_id("httpcat"),
        key.as_str(),
        r#"{"tracks":"not a list"}"#,
        Some(now_millis() + 60_000),
    )
    .await;

    let results = env.service().search(&query("world")).await.unwrap();

    assert_eq!(requests(&server).await, 1);
    assert_eq!(results.tracks.len(), 1);
}

#[tokio::test]
async fn a_failed_call_is_not_cached() {
    let (server, url) = catalog_endpoint(500, "", None).await;
    let env = Env::new().await;
    env.instance("httpcat", HTTP_CATALOG, HTTP_CATALOG_ID, http_config(&url))
        .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();

    service.search(&query("world")).await.unwrap_err();
    service.search(&query("world")).await.unwrap_err();

    assert_eq!(requests(&server).await, 2, "a failure must be retried");
    assert_eq!(count_cache_rows(&env.database).await, 0);
}

#[tokio::test]
async fn search_results_are_cached_for_less_time_than_entity_lookups() {
    let (env, _server) = http_env(ONE_TRACK).await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        serde_json::json!({}),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat", "hello"]).await;
    let service = env.service();

    service.search(&query("world")).await.unwrap();
    service
        .track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    let search = cache_row_lifetime(
        &env.database,
        env.instance_id("httpcat"),
        RequestKey::search(&query("world")).as_str(),
    )
    .await
    .unwrap();
    let track = cache_row_lifetime(
        &env.database,
        env.instance_id("hello"),
        RequestKey::track("hello-track-1").as_str(),
    )
    .await
    .unwrap();

    assert_eq!(search, i64::try_from(SEARCH_CACHE_TTL.as_millis()).unwrap());
    assert_eq!(track, i64::try_from(ENTITY_CACHE_TTL.as_millis()).unwrap());
    assert!(
        track > search,
        "an entity lookup is cached for longer than a search: {track} vs {search}"
    );
}

#[tokio::test]
async fn a_changed_setting_does_not_invalidate_a_cached_answer() {
    let (env, server) = http_env(ONE_TRACK).await;
    let service = env.service();
    service.search(&query("world")).await.unwrap();

    env.set(
        setting_keys::SEARCH_CATALOG_TIMEOUT_MS.name(),
        SettingValue::Integer(5_000),
    )
    .await;

    let results = service.search(&query("world")).await.unwrap();

    assert_eq!(results.tracks.len(), 1);
    assert_eq!(requests(&server).await, 1);
}

#[tokio::test]
async fn the_sweep_deletes_expired_rows_and_keeps_fresh_ones() {
    let (env, _server) = http_env(ONE_TRACK).await;
    let instance = env.instance_id("httpcat");
    let now = now_millis();
    write_cache_row(
        &env.database,
        instance,
        "search|stale",
        "{}",
        Some(now - 1_000),
    )
    .await;
    write_cache_row(
        &env.database,
        instance,
        "search|fresh",
        "{}",
        Some(now + 60_000),
    )
    .await;
    write_cache_row(&env.database, instance, "search|forever", "{}", None).await;
    assert_eq!(count_cache_rows(&env.database).await, 3);

    let deleted = env.service().sweep_expired_cache().await.unwrap();

    assert_eq!(deleted, 1);
    assert_eq!(count_cache_rows(&env.database).await, 2);
}
