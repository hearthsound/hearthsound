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

/// Returns the number of albums a JSON answer lists.
///
/// # Parameters
///
/// - `body`: the response body.
fn album_count(body: &str) -> usize {
    body_json(body)["subsonic-response"]["recommendedAlbums"]["album"]
        .as_array()
        .map_or(0, Vec::len)
}

/// A recommended-albums answer lists the distinct albums of the resolved
/// recommendations, in both encodings.
#[tokio::test]
async fn recommended_albums_are_distinct_and_ordered() {
    let fixture = Fixture::new().await;
    fixture
        .install_album_catalog(json!({
            "searchResults": [{ "text": "song 1", "track": 1 }]
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "discovery",
            json!({
                "recommended": [
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] }
                ]
            }),
        )])
        .await;
    fixture.link_discovery("discovery").await;
    let app = fixture.app();
    let key = fixture.api_key.clone();

    let json_answer = get(
        &app,
        &format!("/rest/hsGetRecommendedAlbums?apiKey={key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&json_answer);
    assert_eq!(album_count(&json_answer.body), 1, "{}", json_answer.body);
    let album = &body_json(&json_answer.body)["subsonic-response"]["recommendedAlbums"]["album"][0];
    assert_eq!(album["name"], "Fixture Album");
    assert!(
        album["id"].as_str().unwrap().starts_with("x_album_a_"),
        "{}",
        json_answer.body
    );
    insta::assert_snapshot!("recommended-albums-json", json_answer.body);

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
        xml_answer.body.contains("name=\"Fixture Album\""),
        "{}",
        xml_answer.body
    );
    insta::assert_snapshot!("recommended-albums-xml", xml_answer.body);
}

/// A recommended album whose reference is bare names the artist of the resolved
/// track without a second catalog album lookup.
#[tokio::test]
async fn recommended_albums_name_the_artist_of_a_catalog_album() {
    let fixture = Fixture::new().await;
    fixture
        .install_album_catalog(json!({
            "searchResults": [{ "text": "song 1", "track": 1 }],
            "bareAlbumRefs": true
        }))
        .await;
    fixture
        .install_discovery_layer(&[(
            "discovery",
            json!({
                "recommended": [
                    { "title": "Fixture Song 1", "artists": ["Fixture Artist"] }
                ]
            }),
        )])
        .await;
    fixture.link_discovery("discovery").await;
    let app = fixture.app();

    let answer = get(
        &app,
        &format!(
            "/rest/hsGetRecommendedAlbums?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&answer);
    let response = body_json(&answer.body);
    let album = &response["subsonic-response"]["recommendedAlbums"]["album"][0];
    assert_eq!(album["artist"], "Fixture Artist", "{}", answer.body);
    assert_eq!(album["displayArtist"], "Fixture Artist", "{}", answer.body);
    assert_eq!(
        album["artists"][0]["name"], "Fixture Artist",
        "{}",
        answer.body
    );
    let artist_id = hearthsound_core::CatalogId::new(
        hearthsound_core::Slug::new("album").unwrap(),
        hearthsound_core::EntityKind::Artist,
        "album-catalog-artist-1",
    )
    .unwrap();
    assert_eq!(album["artistId"], artist_id.to_string(), "{}", answer.body);
    // A bare album reference carries no year, and the resolved track already
    // names the artist, so the shelf entry is not completed from a second
    // catalog lookup.
    assert_eq!(album["year"], 0, "{}", answer.body);
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

/// A similar-artist album listing names the seed artist and interleaves one
/// personal-library album per similar artist.
#[tokio::test]
async fn similar_artist_albums_interleave_personal_library_albums() {
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
            Some("test"),
            1_000,
            200_000,
            true,
        )
        .await
        .unwrap();
    fixture
        .seed_track("Personal One", "First", "First Song", 100_000)
        .await;
    fixture
        .seed_track("Personal Two", "Second", "Second Song", 100_000)
        .await;
    fixture
        .install_discovery_layer(&[(
            "discovery",
            json!({
                "similarArtists": [{ "name": "First" }, { "name": "Second" }]
            }),
        )])
        .await;
    fixture.link_discovery("discovery").await;
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
        "a personal album is the stored one: {}",
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

/// A similar artist with no personal-library album falls back to the catalog.
#[tokio::test]
async fn similar_artist_albums_fall_back_to_the_catalog() {
    let fixture = Fixture::new().await;
    fixture.install_album_catalog(json!({})).await;
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
    fixture
        .install_discovery_layer(&[(
            "discovery",
            json!({ "similarArtists": [{ "name": "Fixture Artist" }] }),
        )])
        .await;
    fixture.link_discovery("discovery").await;
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
    let section = &response["subsonic-response"]["similarArtistAlbums"];
    let albums = section["album"].as_array().unwrap();
    assert_eq!(albums.len(), 1, "{}", answer.body);
    assert_eq!(albums[0]["name"], "Fixture Album");
    assert!(
        albums[0]["id"].as_str().unwrap().starts_with("x_album_a_"),
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

/// Answers a catalog search with one track named after the query.
struct SearchResponder;

impl wiremock::Respond for SearchResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let query = request
            .url
            .query_pairs()
            .find(|(name, _)| name == "q")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
        let body = serde_json::json!([{
            "id": "remote-1",
            "title": query,
            "artists": ["Fixture Artist"],
        }]);
        wiremock::ResponseTemplate::new(200).set_body_string(body.to_string())
    }
}

#[tokio::test]
async fn recommended_albums_resolve_only_what_they_show_and_remember_it() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/search"))
        .respond_with(SearchResponder)
        .mount(&server)
        .await;

    let fixture = Fixture::new().await;
    fixture
        .install_catalog_instance(
            "httpcat",
            hearthsound_test_support::HTTP_CATALOG,
            json!({ "url": format!("{}/search", server.uri()) }),
        )
        .await;
    fixture
        .assign_layer(hearthsound_core::Layer::Catalog, &["httpcat"])
        .await;
    let recommended: Vec<serde_json::Value> = (1..=100)
        .map(|number| json!({ "title": format!("song {number}"), "artists": [] }))
        .collect();
    fixture
        .install_discovery_layer(&[("discovery", json!({ "recommended": recommended }))])
        .await;
    fixture.link_discovery("discovery").await;
    let app = fixture.app();
    let uri = format!(
        "/rest/hsGetRecommendedAlbums?apiKey={}&hearthsound=1&f=json&size=1",
        fixture.api_key
    );

    let first = get(&app, &uri).await;
    assert_ok(&first);
    assert_eq!(album_count(&first.body), 1, "{}", first.body);
    let after_first = server.received_requests().await.unwrap().len();
    assert!(
        after_first <= 4,
        "resolved {after_first} suggestions for one album"
    );

    let second = get(&app, &uri).await;
    assert_ok(&second);
    let after_second = server.received_requests().await.unwrap().len();
    assert_eq!(
        after_second, after_first,
        "a repeated call must reuse the resolved suggestions"
    );
}
