//! Integration tests for the information endpoints: the biography and links a
//! metadata instance answers, the identifiers the library holds, and the ids
//! each endpoint refuses.

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

/// `getArtistInfo` answers a stored artist's identifier, and an artist no
/// instance knows answers without a biography.
#[tokio::test]
async fn artist_info_answers_the_stored_identifier() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getArtistInfo?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"artistInfo\""), "{}", json.body);
    assert!(
        json.body.contains("\"biography\":\"\""),
        "an artist no instance knows has no biography: {}",
        json.body
    );
    assert!(json.body.contains("\"lastFmUrl\":\"\""), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("artist-info-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getArtistInfo2.view?id={}&apiKey={}",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains("<artistInfo2>"),
        "the answer carries the ID3 member: {}",
        xml.body
    );
    assert!(
        xml.body.contains("<biography>") || xml.body.contains("<biography/>"),
        "the elements are children, not attributes: {}",
        xml.body
    );
}

/// `getAlbumInfo` answers a stored album's identifier, and a track answers for
/// its album.
#[tokio::test]
async fn album_info_answers_the_stored_identifier() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let album = get(
        &app,
        &format!(
            "/rest/getAlbumInfo?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&album);
    assert!(album.body.contains("\"albumInfo\""), "{}", album.body);
    assert!(album.body.contains("\"notes\":\"\""), "{}", album.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("album-info-json", seeded.stabilize(&album.body));
    });

    let track = get(
        &app,
        &format!(
            "/rest/getAlbumInfo2?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&track);
    assert!(
        track.body.contains("\"albumInfo2\""),
        "a track answers for its album: {}",
        track.body
    );
}

/// The information endpoints refuse what they cannot describe.
#[tokio::test]
async fn information_requests_are_refused_without_usable_ids() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let missing = get(
        &app,
        &format!("/rest/getArtistInfo?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);
    let missing = get(
        &app,
        &format!("/rest/getAlbumInfo?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);

    // An album id is not an artist id, and a track id is not an album id.
    let other_kind = get(
        &app,
        &format!(
            "/rest/getArtistInfo?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&other_kind, 70);
    let other_kind = get(
        &app,
        &format!(
            "/rest/getAlbumInfo?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_error(&other_kind, 70);

    // A catalog id names an entity the server has not stored; it is answered
    // from the catalog, and one no instance knows is not found.
    let unknown = get(
        &app,
        &format!(
            "/rest/getArtistInfo?id=x_nowhere_r_abc&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
    let unknown = get(
        &app,
        &format!(
            "/rest/getAlbumInfo?id=x_nowhere_a_abc&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
}
