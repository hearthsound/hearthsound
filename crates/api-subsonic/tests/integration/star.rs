//! Integration tests for the star endpoints: the like a `star` records and an
//! `unstar` removes, the two starred listings, and the ids each endpoint
//! refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{EntityId, IdPrefix};

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

/// `star` records a like that `getStarred2` lists and `unstar` removes.
#[tokio::test]
async fn star_and_unstar_change_what_the_listings_show() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // Nothing is liked yet, so both listings are empty.
    let empty = get(
        &app,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&empty);
    assert!(
        empty
            .body
            .contains("\"starred2\":{\"artist\":[],\"album\":[],\"song\":[]}"),
        "{}",
        empty.body
    );

    let starred = get(
        &app,
        &format!(
            "/rest/star?id={}&albumId={}&artistId={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], seeded.alpha, seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&starred);
    assert!(
        starred.body.contains("\"status\":\"ok\""),
        "{}",
        starred.body
    );

    let json = get(
        &app,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body
            .contains(&format!("\"id\":\"{}\"", seeded.cardinals)),
        "the liked artist is listed: {}",
        json.body
    );
    assert!(
        json.body.contains(&format!("\"id\":\"{}\"", seeded.alpha)),
        "the liked album is listed: {}",
        json.body
    );
    assert!(
        json.body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the liked song is listed: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("starred2-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!("/rest/getStarred.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("isDir=\"true\""), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("starred-xml", seeded.stabilize(&xml.body));
    });

    let unstarred = get(
        &app,
        &format!(
            "/rest/unstar?id={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&unstarred);
    let after = get(
        &app,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&after);
    assert!(
        !after
            .body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "the unstarred song is gone: {}",
        after.body
    );
    assert!(
        after.body.contains(&format!("\"id\":\"{}\"", seeded.alpha)),
        "the album is still liked: {}",
        after.body
    );
}

/// A search hit is a row, so it can be starred like any stored track.
#[tokio::test]
async fn star_accepts_a_search_hit() {
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
    let value: serde_json::Value = serde_json::from_str(&found.body).unwrap();
    let track = value["subsonic-response"]["searchResult3"]["song"][0]["id"]
        .as_str()
        .expect("the search lists a track")
        .to_owned();

    let starred = get(
        &app,
        &format!("/rest/star?id={track}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&starred);

    let listed = get(
        &app,
        &format!("/rest/getStarred2?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed
            .body
            .contains(&format!("\"song\":[{{\"id\":\"{track}\"")),
        "the starred song is the row the search answered with: {}",
        listed.body
    );
}

/// `star` refuses a request that names no entity and an id it cannot resolve.
#[tokio::test]
async fn star_refuses_unusable_ids() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let missing = get(
        &app,
        &format!("/rest/star?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing, 10);
    let empty = get(
        &app,
        &format!("/rest/star?id=&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&empty, 10);

    let unknown = get(
        &app,
        &format!("/rest/star?id=nonsense&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&unknown, 70);

    // A valid id of another kind, and a valid id of the right kind that names
    // no stored entity, are both refused.
    let other_kind = get(
        &app,
        &format!(
            "/rest/star?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&other_kind, 70);
    let absent = EntityId::new(IdPrefix::Track);
    let no_such = get(
        &app,
        &format!("/rest/star?id={absent}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&no_such, 70);

    // A catalog id no instance resolves is unknown, not a server failure.
    let unserved = get(
        &app,
        &format!(
            "/rest/star?id=x_nowhere_t_abc&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unserved, 70);
}

/// `setRating` records a rating the listings report, and `0` clears it.
#[tokio::test]
async fn set_rating_records_and_clears_a_rating() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let rated = get(
        &app,
        &format!(
            "/rest/setRating?id={}&rating=4&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&rated);

    let listed = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"userRating\":4"),
        "the album reports the rating: {}",
        listed.body
    );

    // A rating of another entity is the user's own, and zero clears one.
    let cleared = get(
        &app,
        &format!(
            "/rest/setRating?id={}&rating=0&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&cleared);
    let after = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&after);
    assert!(
        !after.body.contains("\"userRating\""),
        "a cleared rating is not reported: {}",
        after.body
    );
}

/// `setRating` refuses a missing parameter, a rating outside 0 to 5, and an id
/// it cannot resolve.
#[tokio::test]
async fn set_rating_refuses_unusable_values() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let missing_id = get(
        &app,
        &format!("/rest/setRating?rating=3&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&missing_id, 10);
    let missing_rating = get(
        &app,
        &format!(
            "/rest/setRating?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_rating, 10);

    for rating in ["6", "-1", "high"] {
        let refused = get(
            &app,
            &format!(
                "/rest/setRating?id={}&rating={rating}&apiKey={}&f=json",
                seeded.alpha, fixture.api_key
            ),
        )
        .await;
        assert_error(&refused, 0);
    }

    let unknown = get(
        &app,
        &format!(
            "/rest/setRating?id=nonsense&rating=3&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);

    // A track id is a kind a rating applies to, but this one names no row.
    let absent = EntityId::new(IdPrefix::Track);
    let no_such = get(
        &app,
        &format!(
            "/rest/setRating?id={absent}&rating=3&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&no_such, 70);
}
