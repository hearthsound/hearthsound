//! Integration tests for the bookmark endpoints: the bookmark a user saves and
//! reads back, the note and position it carries, and the ids the endpoints
//! refuse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library.
///
/// # Returns
///
/// The fixture and the ids of the seeded entities; the fixture keeps its
/// temporary directory alive.
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

/// A saved bookmark is listed with its position, note, and entry.
#[tokio::test]
async fn a_bookmark_round_trip() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // Nothing is bookmarked yet.
    let empty = get(
        &app,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&empty);
    assert!(empty.body.contains("\"bookmark\":[]"), "{}", empty.body);

    let created = get(
        &app,
        &format!(
            "/rest/createBookmark?id={}&position=45000&comment=halfway&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);

    let listed = get(
        &app,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"position\":45000"),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"comment\":\"halfway\""),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"username\":\"echo\""),
        "{}",
        listed.body
    );
    assert!(
        listed
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the bookmark carries the entry: {}",
        listed.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("bookmarks-json", seeded.stabilize(&listed.body));
    });

    // Saving the same track again moves the position instead of adding one.
    let moved = get(
        &app,
        &format!(
            "/rest/createBookmark?id={}&position=90000&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&moved);
    let listed = get(
        &app,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert_eq!(
        listed.body.matches("\"position\":90000").count(),
        1,
        "one bookmark per track: {}",
        listed.body
    );
    assert!(
        listed.body.contains("\"comment\":\"\""),
        "the cleared note is reported empty: {}",
        listed.body
    );

    let deleted = get(
        &app,
        &format!(
            "/rest/deleteBookmark?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&deleted);
    let listed = get(
        &app,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(listed.body.contains("\"bookmark\":[]"), "{}", listed.body);
}

/// A catalog track is materialized before it is bookmarked.
#[tokio::test]
async fn a_bookmark_materializes_a_catalog_track() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let found = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&found);
    let start = found
        .body
        .find("x_hello_t_")
        .expect("the search lists a track");
    let rest = &found.body[start..];
    let catalog_song = &rest[..rest.find('"').unwrap()];

    let created = get(
        &app,
        &format!(
            "/rest/createBookmark?id={catalog_song}&position=1000&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&created);

    let listed = get(
        &app,
        &format!("/rest/getBookmarks?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"entry\":{\"id\":\"tr_"),
        "the bookmark carries the stored row: {}",
        listed.body
    );
}

/// The bookmark endpoints refuse a missing parameter and an id that is not a
/// track id.
#[tokio::test]
async fn bookmark_requests_are_refused_without_usable_values() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let missing_id = get(
        &app,
        &format!(
            "/rest/createBookmark?position=1000&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_id, 10);
    let missing_position = get(
        &app,
        &format!(
            "/rest/createBookmark?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_position, 10);
    let missing_id = get(
        &app,
        &format!("/rest/deleteBookmark?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing_id, 10);

    let bad_position = get(
        &app,
        &format!(
            "/rest/createBookmark?id={}&position=later&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_position, 0);

    // An album id names no track, and a track id that has no row is unknown.
    let other_kind = get(
        &app,
        &format!(
            "/rest/createBookmark?id={}&position=1000&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&other_kind, 70);
    let unknown = get(
        &app,
        &format!(
            "/rest/deleteBookmark?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
}
