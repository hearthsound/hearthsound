//! Integration tests for the deduplication of catalog results.
//!
//! Every test searches the `hello-catalog` or `http-catalog` fixture, builds the
//! local matches a search would have returned, and asserts which catalog
//! entities survive, which are reported as already materialized, and which
//! stored row they resolve to. The rule under test is "stored and shown": a
//! materialized row the caller's results do not show never hides the catalog
//! entity it came from.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::{
    Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, catalog_endpoint, catalog_id, http_config,
    insert_local_album, insert_local_track, query, write_alias,
};
use hearthsound_catalog::{DURATION_TOLERANCE_MS, LocalMatches, LocalTrackHit};
use hearthsound_core::{EntityKind, Layer};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG};

/// Builds the environment one dedupe test starts from: the hello catalog
/// fixture, assigned as the primary catalog instance.
async fn hello_env() -> Env {
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        serde_json::json!({}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    env
}

/// Builds a local hit for a title, artist and duration.
fn hit(
    track_id: hearthsound_core::EntityId,
    title: &str,
    artist: &str,
    duration_ms: Option<i64>,
) -> LocalTrackHit {
    LocalTrackHit {
        track_id,
        isrc: None,
        title: title.to_owned(),
        artist: Some(artist.to_owned()),
        duration_ms,
    }
}

/// Wraps local hits into the bundle deduplication compares against.
fn matches(hits: Vec<LocalTrackHit>) -> LocalMatches {
    LocalMatches {
        tracks: hits,
        ..LocalMatches::default()
    }
}

#[tokio::test]
async fn a_track_matching_a_local_hit_is_dropped_and_reported() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();
    assert_eq!(search.tracks.len(), 1);

    let stored = insert_local_track(&env.database, "Hello, world", None, Some(179_000)).await;
    let local = matches(vec![hit(
        stored,
        "  HELLO, WORLD ",
        "Hello  Artist",
        Some(180_500),
    )]);

    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert!(deduped.tracks.is_empty(), "the catalog track is stored");
    assert_eq!(deduped.materialized.len(), 1);
    assert_eq!(deduped.materialized[0].entity_id, stored);
    assert_eq!(
        deduped.stored_entity(&search.tracks[0].id),
        Some(stored),
        "a caller can answer with the stored track"
    );
    assert_eq!(deduped.albums, search.albums);
    assert_eq!(deduped.artists, search.artists);
}

#[tokio::test]
async fn a_duration_outside_the_tolerance_keeps_the_track() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(&env.database, "Hello, world", None, Some(180_000)).await;
    let far = 180_000 - DURATION_TOLERANCE_MS - 1;
    let local = matches(vec![hit(stored, "Hello, world", "Hello Artist", Some(far))]);

    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert_eq!(deduped.tracks.len(), 1);
    assert!(deduped.materialized.is_empty());
}

#[tokio::test]
async fn a_different_artist_or_title_keeps_the_track() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(&env.database, "Hello, world", None, Some(180_000)).await;
    let other_artist = matches(vec![hit(
        stored,
        "Hello, world",
        "Another Artist",
        Some(180_000),
    )]);
    let other_title = matches(vec![hit(
        stored,
        "Goodbye, world",
        "Hello Artist",
        Some(180_000),
    )]);

    assert_eq!(
        service
            .deduplicate(&other_artist, &search)
            .await
            .unwrap()
            .tracks
            .len(),
        1
    );
    assert_eq!(
        service
            .deduplicate(&other_title, &search)
            .await
            .unwrap()
            .tracks
            .len(),
        1
    );
}

#[tokio::test]
async fn an_unknown_duration_still_matches() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(&env.database, "Hello, world", None, None).await;
    let local = matches(vec![hit(stored, "Hello, world", "Hello Artist", None)]);

    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert!(deduped.tracks.is_empty());
    assert_eq!(deduped.materialized[0].entity_id, stored);
}

#[tokio::test]
async fn a_track_matching_by_isrc_is_dropped_even_with_another_title() {
    let (server, url) = catalog_endpoint(
        200,
        r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"],"duration_ms":1000,"isrc":"usrc17607839"}]"#,
        None,
    )
    .await;
    let _ = server;
    let env = Env::new().await;
    env.instance("httpcat", HTTP_CATALOG, HTTP_CATALOG_ID, http_config(&url))
        .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(
        &env.database,
        "Totally Different",
        Some("USRC17607839"),
        Some(1_000),
    )
    .await;
    let local = matches(vec![LocalTrackHit {
        track_id: stored,
        isrc: Some(" USRC17607839 ".to_owned()),
        title: "Totally Different".to_owned(),
        artist: None,
        duration_ms: Some(1_000),
    }]);

    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert!(deduped.tracks.is_empty());
    assert_eq!(deduped.materialized[0].entity_id, stored);
}

#[tokio::test]
async fn a_stored_isrc_alias_reports_the_track_it_names_and_keeps_it_shown() {
    let (server, url) = catalog_endpoint(
        200,
        r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"],"duration_ms":1000,"isrc":"USRC17607839"}]"#,
        None,
    )
    .await;
    let _ = server;
    let env = Env::new().await;
    env.instance("httpcat", HTTP_CATALOG, HTTP_CATALOG_ID, http_config(&url))
        .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    // The recording is stored but the search did not return it locally, which is
    // what a scope-restricted search looks like: the catalog hit must stay, or
    // the listener loses the only row that names the song.
    let stored = insert_local_track(
        &env.database,
        "Remote Song",
        Some("USRC17607839"),
        Some(1_000),
    )
    .await;
    write_alias(
        &env.database,
        "isrc",
        EntityKind::Track,
        "USRC17607839",
        stored,
    )
    .await;

    let deduped = service
        .deduplicate(&LocalMatches::default(), &search)
        .await
        .unwrap();

    assert_eq!(deduped.tracks.len(), 1, "an invisible row hides nothing");
    assert_eq!(deduped.materialized[0].entity_id, stored);
}

#[tokio::test]
async fn a_materialized_track_absent_from_the_local_results_stays() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(&env.database, "Whatever It Is Called", None, Some(1)).await;
    write_alias(
        &env.database,
        "p:hello",
        EntityKind::Track,
        "hello-track-1",
        stored,
    )
    .await;

    let deduped = service
        .deduplicate(&LocalMatches::default(), &search)
        .await
        .unwrap();

    assert_eq!(
        deduped.tracks.len(),
        1,
        "a played-but-unsaved song keeps its catalog row"
    );
    assert_eq!(deduped.materialized[0].entity_id, stored);
    assert_eq!(
        deduped.stored_entity(&search.tracks[0].id),
        Some(stored),
        "the caller still keeps the id mapping"
    );
}

#[tokio::test]
async fn a_materialized_track_the_local_results_show_is_dropped() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(&env.database, "Whatever It Is Called", None, Some(1)).await;
    write_alias(
        &env.database,
        "p:hello",
        EntityKind::Track,
        "hello-track-1",
        stored,
    )
    .await;

    let local = matches(vec![hit(stored, "Whatever It Is Called", "", Some(1))]);
    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert!(deduped.tracks.is_empty());
    assert_eq!(deduped.materialized[0].entity_id, stored);
}

#[tokio::test]
async fn a_materialized_album_the_local_results_do_not_show_stays() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_album(&env.database, "Hello Album").await;
    write_alias(
        &env.database,
        "p:hello",
        EntityKind::Album,
        "hello-album-1",
        stored,
    )
    .await;

    let deduped = service
        .deduplicate(&LocalMatches::default(), &search)
        .await
        .unwrap();

    assert_eq!(
        deduped.albums.len(),
        1,
        "a materialized album nobody saved stays a catalog hit"
    );
    assert_eq!(
        deduped.stored_entity(&catalog_id("hello", EntityKind::Album, "hello-album-1")),
        Some(stored)
    );
}

#[tokio::test]
async fn a_materialized_album_the_local_results_show_is_dropped() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_album(&env.database, "Hello Album").await;
    write_alias(
        &env.database,
        "p:hello",
        EntityKind::Album,
        "hello-album-1",
        stored,
    )
    .await;

    let local = LocalMatches {
        albums: vec![stored],
        ..LocalMatches::default()
    };
    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert!(deduped.albums.is_empty(), "the album is already listed");
    assert_eq!(
        deduped.stored_entity(&catalog_id("hello", EntityKind::Album, "hello-album-1")),
        Some(stored)
    );
}

#[tokio::test]
async fn a_materialized_artist_follows_the_same_rule() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = env.stored_artist("Hello Artist").await;
    write_alias(
        &env.database,
        "p:hello",
        EntityKind::Artist,
        "hello-artist-1",
        stored,
    )
    .await;

    let unseen = service
        .deduplicate(&LocalMatches::default(), &search)
        .await
        .unwrap();
    assert_eq!(unseen.artists.len(), 1);
    assert_eq!(unseen.materialized[0].entity_id, stored);

    let shown = LocalMatches {
        artists: vec![stored],
        ..LocalMatches::default()
    };
    let deduped = service.deduplicate(&shown, &search).await.unwrap();
    assert!(deduped.artists.is_empty());
    assert_eq!(
        deduped.stored_entity(&catalog_id("hello", EntityKind::Artist, "hello-artist-1")),
        Some(stored)
    );
}

#[tokio::test]
async fn an_unrelated_local_hit_keeps_every_catalog_result() {
    let env = hello_env().await;
    let service = env.service();
    let search = service.search(&query("world")).await.unwrap();

    let stored = insert_local_track(
        &env.database,
        "Something Else",
        Some("GBAYE0601498"),
        Some(200_000),
    )
    .await;
    let local = matches(vec![hit(
        stored,
        "Something Else",
        "Someone Else",
        Some(200_000),
    )]);

    let deduped = service.deduplicate(&local, &search).await.unwrap();

    assert_eq!(deduped.tracks.len(), 1);
    assert_eq!(deduped.albums.len(), 1);
    assert_eq!(deduped.artists.len(), 1);
    assert!(deduped.materialized.is_empty());
    assert!(!deduped.is_empty());
}
