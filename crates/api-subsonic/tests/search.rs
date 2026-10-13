//! Integration tests for `search3`: the local-first ordering, the catalog
//! results a fixture provider adds, the scope rule, and the empty-query
//! listing.
//!
//! The catalog side is the `hello-catalog` fixture installed through the
//! provider registry, so the catalog results in a snapshot are exactly what a
//! real provider would contribute.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use hearthsound_config::{SettingValue, keys};
use hearthsound_core::Availability;
use hearthsound_library::{ArtistCredit, LibraryRepository, Origin, TrackInput};

use common::seed::{Seeded, library};
use common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library and the account's id.
async fn fixture() -> (Fixture, Seeded) {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    (fixture, seeded)
}

#[tokio::test]
async fn search3_answers_local_results_then_catalog_results() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    // The catalog track is named `x_hello_t_...` and appended after the local
    // songs.
    assert!(json.body.contains("x_hello_t_"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("local-and-catalog-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/search3.view?query=cardinals&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("local-and-catalog-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn search3_with_an_empty_query_lists_the_local_library() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/search3?query=&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    // The empty query lists everything local and never reaches the catalog.
    assert!(
        json.body.contains(&seeded.alpha.to_string()),
        "{}",
        json.body
    );
    assert!(
        !json.body.contains("x_hello_"),
        "an empty query returns no catalog results: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("empty-query-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!("/rest/search3.view?query=&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("empty-query-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn search3_answers_local_results_when_the_catalog_is_disabled() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    fixture
        .settings
        .set(
            keys::SEARCH_CATALOG_ENABLED.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&seeded.alpha.to_string()),
        "{}",
        probe.body
    );
    assert!(
        !probe.body.contains("x_hello_"),
        "a disabled catalog contributes nothing: {}",
        probe.body
    );
}

#[tokio::test]
async fn search3_drops_a_catalog_track_the_server_already_stores() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;

    // A local track the hello catalog also returns: the same title, artist, and
    // duration, so deduplication drops the catalog copy instead of listing it
    // twice.
    let library = LibraryRepository::new(&fixture.database);
    let mut input = TrackInput::new("Hello, cardinals", Origin::Local, Availability::Local);
    input.display_artist = Some("Hello Artist".to_owned());
    input.duration_ms = Some(180_000);
    let local = library
        .insert_track(&input, &[ArtistCredit::new("Hello Artist", "main")], &[])
        .await
        .unwrap();
    let app = fixture.app();

    // The server scope sees the stored track, which no folder owns.
    let probe = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&musicFolderId=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(probe.body.contains(&local.to_string()), "{}", probe.body);
    assert_eq!(
        probe.body.matches("Hello, cardinals").count(),
        1,
        "the stored recording is listed once: {}",
        probe.body
    );
    assert!(
        !probe.body.contains("x_hello_t_"),
        "the catalog track is already stored: {}",
        probe.body
    );
    assert!(
        probe.body.contains("x_hello_a_"),
        "the catalog album is not stored and is appended: {}",
        probe.body
    );
}

#[tokio::test]
async fn search3_requires_a_query() {
    let (fixture, _seeded) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/search3?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 10);
}
