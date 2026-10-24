//! Integration tests for the unified artist and album pages.
//!
//! A page is the same listing whichever id form opened it: the catalog
//! discography with the stored albums in the places they belong, and the catalog
//! tracklist with the stored tracks in theirs. The catalog side is the
//! `discography-catalog` fixture, whose artist released three albums, one of
//! them a remaster; the stored side is a library that holds two of that artist's
//! songs and an album the catalog does not list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{CatalogId, EntityId, EntityKind, Slug};

use crate::common::{Fixture, assert_ok, get};

/// The instance slug the discography fixture is installed under.
const SLUG: &str = "disco";

/// The provider id of the fixture's artist.
const ARTIST: &str = "disco-artist-1";

/// The provider id of the fixture's remastered album.
const SECOND_WIND: &str = "disco-album-2";

/// The provider id of the fixture's third album.
const THIRD_SINGLE: &str = "disco-album-3";

/// The provider id of the fixture's unlisted third track.
const GALE: &str = "disco-track-2-3";

/// Builds a `disco` catalog id.
///
/// # Parameters
///
/// - `kind`: the entity kind.
/// - `provider_id`: the provider's own id.
fn catalog_id(kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new(SLUG).unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

/// A fixture with the discography catalog installed and a library holding two
/// of that artist's songs.
struct Seeded {
    /// The fixture.
    fixture: Fixture,
    /// The artist row the songs were filed under.
    artist: EntityId,
    /// The stored album the catalog calls "Second Wind (Remastered 2009)".
    second_wind: EntityId,
    /// The stored "Gust" track.
    gust: EntityId,
    /// The stored "Breeze" track.
    breeze: EntityId,
    /// The stored album the catalog does not list.
    basement: EntityId,
}

/// Seeds the fixture.
async fn seeded() -> Seeded {
    let fixture = Fixture::new().await;
    fixture.install_discography_catalog().await;
    let (gust, second_wind, artist) = fixture
        .seed_track("Second Wind", "Discography Artist", "Gust", 150_500)
        .await;
    let (breeze, _, _) = fixture
        .seed_track("Second Wind", "Discography Artist", "Breeze", 160_000)
        .await;
    let (_, basement, _) = fixture
        .seed_track("Basement Tapes", "Discography Artist", "Hum", 90_000)
        .await;
    Seeded {
        fixture,
        artist,
        second_wind,
        gust,
        breeze,
        basement,
    }
}

/// Parses a successful response body.
///
/// # Parameters
///
/// - `body`: the JSON body the endpoint wrote.
fn parsed(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap()
}

/// Returns the album ids of a `getArtist` response, in order.
///
/// # Parameters
///
/// - `body`: the parsed response.
fn album_ids(body: &serde_json::Value) -> Vec<String> {
    body["subsonic-response"]["artist"]["album"]
        .as_array()
        .unwrap()
        .iter()
        .map(|album| album["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Returns the song ids of a `getAlbum` response, in order.
///
/// # Parameters
///
/// - `body`: the parsed response.
fn song_ids(body: &serde_json::Value) -> Vec<String> {
    body["subsonic-response"]["album"]["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| song["id"].as_str().unwrap().to_owned())
        .collect()
}

/// Reads `getArtist` for one id.
///
/// # Parameters
///
/// - `fixture`: the fixture whose router answers.
/// - `id`: the artist id.
/// - `vendor`: whether the request opts into the vendor extension.
async fn artist_page(fixture: &Fixture, id: &str, vendor: bool) -> serde_json::Value {
    let vendor = if vendor { "&hearthsound=1" } else { "" };
    let response = get(
        &fixture.app(),
        &format!(
            "/rest/getArtist?id={id}&apiKey={}&f=json{vendor}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&response);
    parsed(&response.body)
}

/// Reads `getAlbum` for one id with the vendor extension.
///
/// # Parameters
///
/// - `fixture`: the fixture whose router answers.
/// - `id`: the album id.
async fn album_page(fixture: &Fixture, id: &str) -> serde_json::Value {
    let response = get(
        &fixture.app(),
        &format!(
            "/rest/getAlbum?id={id}&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&response);
    parsed(&response.body)
}

#[tokio::test]
async fn a_library_artist_lists_the_whole_discography() {
    let seed = seeded().await;
    let body = artist_page(&seed.fixture, &seed.artist.to_string(), true).await;

    assert_eq!(
        album_ids(&body),
        vec![
            catalog_id(EntityKind::Album, "disco-album-1"),
            seed.second_wind.to_string(),
            catalog_id(EntityKind::Album, THIRD_SINGLE),
            seed.basement.to_string(),
        ],
        "the stored albums sit in their catalog positions, and the rest follow"
    );
    let artist = &body["subsonic-response"]["artist"];
    assert_eq!(artist["albumCount"].as_i64(), Some(4));
    assert_eq!(
        artist["id"].as_str(),
        Some(seed.artist.to_string().as_str())
    );

    let albums = artist["album"].as_array().unwrap();
    assert_eq!(
        albums[1]["hearthsound"]["inLibrary"].as_bool(),
        Some(true),
        "the album the library holds is marked"
    );
    assert_eq!(
        albums[0]["hearthsound"]["inLibrary"].as_bool(),
        Some(false),
        "an album the library does not hold is not marked"
    );
}

#[tokio::test]
async fn a_catalog_artist_opens_as_the_stored_artist() {
    let seed = seeded().await;
    let body = artist_page(&seed.fixture, &catalog_id(EntityKind::Artist, ARTIST), true).await;

    let artist = &body["subsonic-response"]["artist"];
    assert_eq!(
        artist["id"].as_str(),
        Some(seed.artist.to_string().as_str())
    );
    assert_eq!(
        album_ids(&body),
        vec![
            catalog_id(EntityKind::Album, "disco-album-1"),
            seed.second_wind.to_string(),
            catalog_id(EntityKind::Album, THIRD_SINGLE),
            seed.basement.to_string(),
        ],
    );
}

#[tokio::test]
async fn a_library_album_lists_the_whole_tracklist() {
    let seed = seeded().await;
    let body = album_page(&seed.fixture, &seed.second_wind.to_string()).await;

    assert_eq!(
        song_ids(&body),
        vec![
            seed.gust.to_string(),
            seed.breeze.to_string(),
            catalog_id(EntityKind::Track, GALE),
        ],
        "the stored tracks keep their positions in the provider's tracklist"
    );
    let album = &body["subsonic-response"]["album"];
    assert_eq!(
        album["id"].as_str(),
        Some(seed.second_wind.to_string().as_str())
    );
    assert_eq!(album["songCount"].as_i64(), Some(3));
}

#[tokio::test]
async fn a_catalog_album_opens_as_the_stored_album() {
    let seed = seeded().await;
    let body = album_page(&seed.fixture, &catalog_id(EntityKind::Album, SECOND_WIND)).await;

    let album = &body["subsonic-response"]["album"];
    assert_eq!(
        album["id"].as_str(),
        Some(seed.second_wind.to_string().as_str())
    );
    assert_eq!(
        song_ids(&body),
        vec![
            seed.gust.to_string(),
            seed.breeze.to_string(),
            catalog_id(EntityKind::Track, GALE),
        ],
    );
}

#[tokio::test]
async fn pairing_records_the_provider_alias() {
    let seed = seeded().await;
    artist_page(&seed.fixture, &seed.artist.to_string(), true).await;
    album_page(&seed.fixture, &seed.second_wind.to_string()).await;

    assert_eq!(
        seed.fixture
            .catalog
            .entity_by_alias("p:disco", EntityKind::Artist, ARTIST)
            .await
            .unwrap(),
        Some(seed.artist)
    );
    assert_eq!(
        seed.fixture
            .catalog
            .entity_by_alias("p:disco", EntityKind::Album, SECOND_WIND)
            .await
            .unwrap(),
        Some(seed.second_wind)
    );
}

#[tokio::test]
async fn without_the_vendor_flag_a_library_artist_stays_library_only() {
    let seed = seeded().await;
    let body = artist_page(&seed.fixture, &seed.artist.to_string(), false).await;

    assert_eq!(
        album_ids(&body),
        vec![seed.basement.to_string(), seed.second_wind.to_string()],
        "a third-party client keeps the library-only answer"
    );
    assert_eq!(
        seed.fixture
            .catalog
            .entity_by_alias("p:disco", EntityKind::Artist, ARTIST)
            .await
            .unwrap(),
        None,
        "a library-only page records no pairing"
    );
}

#[tokio::test]
async fn without_a_catalog_layer_the_pages_fall_back_to_the_library() {
    let fixture = Fixture::new().await;
    let (_, second_wind, artist) = fixture
        .seed_track("Second Wind", "Discography Artist", "Gust", 150_500)
        .await;
    fixture
        .seed_track("Second Wind", "Discography Artist", "Breeze", 160_000)
        .await;
    let (_, basement, _) = fixture
        .seed_track("Basement Tapes", "Discography Artist", "Hum", 90_000)
        .await;

    let body = artist_page(&fixture, &artist.to_string(), true).await;
    assert_eq!(
        album_ids(&body),
        vec![basement.to_string(), second_wind.to_string()],
        "with no catalog the page is the library's"
    );

    let body = album_page(&fixture, &second_wind.to_string()).await;
    assert_eq!(
        body["subsonic-response"]["album"]["song"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn a_non_administrator_sees_stored_albums_outside_their_library_by_catalog_id() {
    let seed = seeded().await;
    let (_, key) = seed.fixture.account(&[]).await;

    let response = get(
        &seed.fixture.app(),
        &format!(
            "/rest/getArtist?id={}&apiKey={key}&f=json&hearthsound=1",
            catalog_id(EntityKind::Artist, ARTIST)
        ),
    )
    .await;
    assert_ok(&response);
    let body = parsed(&response.body);

    assert_eq!(
        album_ids(&body),
        vec![
            catalog_id(EntityKind::Album, "disco-album-1"),
            catalog_id(EntityKind::Album, SECOND_WIND),
            catalog_id(EntityKind::Album, THIRD_SINGLE),
        ],
        "another account's stored albums stand in as their catalog entities"
    );
    assert_eq!(
        body["subsonic-response"]["artist"]["albumCount"].as_i64(),
        Some(3)
    );
}
