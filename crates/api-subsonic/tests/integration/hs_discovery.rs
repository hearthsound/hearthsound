//! Integration tests for the discovery vendor endpoints.
//!
//! `hsGetRecommendedAlbums` answers with the distinct albums of the listener's
//! recommended tracks, and `hsGetSimilarArtistAlbums` seeds itself with the
//! artist of the listener's most recent play and lists the albums of the
//! similar artists, personal library first. Both are vendor endpoints, so both
//! answer Subsonic error 70 without `hearthsound=1`.
//!
//! Every test installs the catalog fixture and the discovery fixture through
//! the real plugin host, links the test account to the discovery instance, and
//! sends the request through the `/rest` router.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use hearthsound_core::EntityId;

use crate::common::seed::library;
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Reads the JSON answer of one call.
///
/// # Parameters
///
/// - `body`: the response body.
///
/// # Returns
///
/// The parsed value.
fn body_json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap()
}

/// The filters that make a seeded answer stable across runs.
///
/// A personal album's client id and creation instant come from the row the
/// test inserted, so the filters replace them with fixed markers; the titles
/// and the order stay exactly what the endpoint computed.
fn stable_filters() -> Vec<(&'static str, &'static str)> {
    let mut filters: Vec<(&'static str, &'static str)> = SNAPSHOT_FILTERS.to_vec();
    filters.push((r"al_[0-9a-z]{20,}", "al_ID"));
    filters.push((r"ar_[0-9a-z]{20,}", "ar_ID"));
    filters
}

/// A recommended-albums answer lists the albums the discovery layer resolved,
/// in both encodings.
///
/// The layer's own resolution is exercised by the catalog crate's tests, so this
/// case stores the answer the layer keeps (an id list) and asserts what the
/// endpoint makes of it.
#[tokio::test]
async fn recommended_albums_are_projected_in_order() {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    fixture
        .install_discovery_layer(&[(
            "discovery",
            json!({ "recommended": [
                { "title": "Fixture Song 1", "artists": ["Fixture Artist"] }
            ] }),
        )])
        .await;
    fixture.link_discovery("discovery").await;
    let instance = fixture.instance_id("discovery");
    let answer = serde_json::to_string(&vec![seeded.bright.uuid(), seeded.alpha.uuid()]).unwrap();
    let mut conn = fixture.database.writer().acquire().await.unwrap();
    sqlx::query(
        "INSERT INTO answer_cache (instance_id, request_key, entity_refs, fetched_at, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(instance.as_bytes().as_slice())
    .bind(format!(
        "recommended-albums|discovery|{}|20",
        fixture.user_id.uuid()
    ))
    .bind(answer)
    .bind(0_i64)
    .bind(i64::MAX)
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let app = fixture.app();
    let key = fixture.api_key.clone();

    let json_answer = get(
        &app,
        &format!("/rest/hsGetRecommendedAlbums?apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&json_answer);
    let albums =
        body_json(&json_answer.body)["subsonic-response"]["recommendedAlbums"]["album"].clone();
    let albums = albums.as_array().expect("the shelf is a list");
    assert_eq!(albums.len(), 2, "{}", json_answer.body);
    assert_eq!(albums[0]["name"], "Bright", "{}", json_answer.body);
    assert_eq!(albums[1]["name"], "Alpha", "{}", json_answer.body);
    for album in albums {
        assert!(
            album["id"].as_str().unwrap().starts_with("al_"),
            "a shelf entry is a row: {}",
            json_answer.body
        );
    }
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("recommended-albums-json", seeded.stabilize(&json_answer.body));
    });

    let xml_answer = get(
        &app,
        &format!("/rest/hsGetRecommendedAlbums?apiKey={key}&hearthsound=1&f=xml"),
    )
    .await;
    assert_ok(&xml_answer);
    assert!(
        xml_answer.body.contains("<recommendedAlbums>"),
        "{}",
        xml_answer.body
    );
    assert!(
        xml_answer.body.contains("name=\"Bright\""),
        "{}",
        xml_answer.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("recommended-albums-xml", seeded.stabilize(&xml_answer.body));
    });
}

/// The recommended-albums endpoint answers error 70 without the flag.
#[tokio::test]
async fn recommended_albums_need_the_vendor_flag() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let refused = get(
        &app,
        &format!(
            "/rest/hsGetRecommendedAlbums?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;

    assert_error(&refused, 70);
}

/// Stores one similar-artist relationship.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database is written.
/// - `artist`: the artist the shelf belongs to.
/// - `position`: its place in the shelf.
/// - `similar`: the artist it names.
async fn store_similar(fixture: &Fixture, artist: EntityId, position: i64, similar: EntityId) {
    let mut conn = fixture.database.writer().acquire().await.unwrap();
    sqlx::query("INSERT INTO artist_similar (artist_id, position, similar_id) VALUES (?, ?, ?)")
        .bind(artist.uuid().as_bytes().as_slice())
        .bind(position)
        .bind(similar.uuid().as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
}

/// A similar-artist album listing names the seed artist and lists the albums of
/// the similar artists the indexer stored, newest first.
#[tokio::test]
async fn similar_artist_albums_list_the_stored_similar_artists() {
    let fixture = Fixture::new().await;
    let (seed_track, _seed_album, seed_artist) = fixture
        .seed_track("Seed Album", "Seed Artist", "Seed Song", 200_000)
        .await;
    fixture
        .state
        .activity()
        .plays()
        .record_play(
            fixture.user_id.uuid(),
            seed_track.uuid(),
            Some("test"),
            1_000,
            200_000,
            true,
        )
        .await
        .unwrap();
    let (_first_track, _first_album, first) = fixture
        .seed_track("Personal One", "First", "First Song", 100_000)
        .await;
    let (_second_track, _second_album, second) = fixture
        .seed_track("Personal Two", "Second", "Second Song", 100_000)
        .await;
    store_similar(&fixture, seed_artist, 0, first).await;
    store_similar(&fixture, seed_artist, 1, second).await;
    let app = fixture.app();
    let key = fixture.api_key.clone();

    let answer = get(
        &app,
        &format!("/rest/hsGetSimilarArtistAlbums?apiKey={key}&hearthsound=1&f=json&size=10"),
    )
    .await;
    assert_ok(&answer);
    let response = body_json(&answer.body);
    let section = &response["subsonic-response"]["similarArtistAlbums"];
    assert_eq!(section["seedArtist"]["name"], "Seed Artist");
    let albums = section["album"].as_array().unwrap();
    assert_eq!(albums.len(), 2, "{}", answer.body);
    assert_eq!(albums[0]["name"], "Personal One");
    assert_eq!(albums[1]["name"], "Personal Two");
    assert!(
        albums[0]["id"].as_str().unwrap().starts_with("al_"),
        "the album is the stored row: {}",
        answer.body
    );
    insta::with_settings!({filters => stable_filters()}, {
        insta::assert_snapshot!("similar-artist-albums-personal-json", answer.body);
    });

    let xml_answer = get(
        &app,
        &format!("/rest/hsGetSimilarArtistAlbums?apiKey={key}&hearthsound=1&size=10"),
    )
    .await;
    assert_ok(&xml_answer);
    assert!(
        xml_answer.body.contains("<seedArtist") && xml_answer.body.contains("<album"),
        "{}",
        xml_answer.body
    );
    insta::with_settings!({filters => stable_filters()}, {
        insta::assert_snapshot!("similar-artist-albums-personal-xml", xml_answer.body);
    });

    // The size caps the listing.
    let capped = get(
        &app,
        &format!("/rest/hsGetSimilarArtistAlbums?apiKey={key}&hearthsound=1&f=json&size=1"),
    )
    .await;
    assert_ok(&capped);
    let response = body_json(&capped.body);
    assert_eq!(
        response["subsonic-response"]["similarArtistAlbums"]["album"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{}",
        capped.body
    );
}

/// A shelf with no stored similar artists lists no albums.
#[tokio::test]
async fn similar_artist_albums_are_empty_without_stored_similar_artists() {
    let fixture = Fixture::new().await;
    let (seed_track, _seed_album, _seed_artist) = fixture
        .seed_track("Seed Album", "Seed Artist", "Seed Song", 200_000)
        .await;
    fixture
        .state
        .activity()
        .plays()
        .record_play(
            fixture.user_id.uuid(),
            seed_track.uuid(),
            None,
            1_000,
            200_000,
            true,
        )
        .await
        .unwrap();
    let app = fixture.app();

    let answer = get(
        &app,
        &format!(
            "/rest/hsGetSimilarArtistAlbums?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&answer);
    let response = body_json(&answer.body);
    assert!(
        response["subsonic-response"]["similarArtistAlbums"]["album"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{}",
        answer.body
    );
}

/// The similar-artist album endpoint answers error 70 without the flag.
#[tokio::test]
async fn similar_artist_albums_need_the_vendor_flag() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let refused = get(
        &app,
        &format!(
            "/rest/hsGetSimilarArtistAlbums?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;

    assert_error(&refused, 70);
}
