//! Integration tests for the unified artist and album pages.
//!
//! A page is the same listing whichever id form opened it: the catalog
//! discography with the stored albums in the places they belong, and the catalog
//! tracklist with the stored tracks in theirs. The catalog side is the
//! `discography-catalog` fixture, whose artist released three albums, one of
//! them a remaster; the stored side is a library that holds two of that artist's
//! songs and an album the catalog does not list.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, Slug};

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

/// Primes the cache for one entity the way a background enrichment run does.
///
/// A vendor page read answers from the library and the cached catalog data, so
/// the merged page is asserted after priming. The cold read is a test of its own.
///
/// # Parameters
///
/// - `fixture`: the fixture whose cache is filled.
/// - `id`: the stored or catalog id to enrich.
async fn prime(fixture: &Fixture, id: ClientId) {
    fixture.enrich(&id).await;
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
    let cold = artist_page(&seed.fixture, &seed.artist.to_string(), true).await;
    assert_eq!(
        album_ids(&cold),
        vec![seed.basement.to_string(), seed.second_wind.to_string()],
        "a cold page answers from the library alone and never waits for the catalog"
    );
    prime(&seed.fixture, ClientId::Entity(seed.artist)).await;
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
    let artist = CatalogId::new(Slug::new(SLUG).unwrap(), EntityKind::Artist, ARTIST).unwrap();
    prime(&seed.fixture, ClientId::Catalog(artist)).await;
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
    prime(&seed.fixture, ClientId::Entity(seed.second_wind)).await;
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
    let album = CatalogId::new(Slug::new(SLUG).unwrap(), EntityKind::Album, SECOND_WIND).unwrap();
    prime(&seed.fixture, ClientId::Catalog(album)).await;
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
    prime(&seed.fixture, ClientId::Entity(seed.artist)).await;
    prime(&seed.fixture, ClientId::Entity(seed.second_wind)).await;
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
    let artist = CatalogId::new(Slug::new(SLUG).unwrap(), EntityKind::Artist, ARTIST).unwrap();
    prime(&seed.fixture, ClientId::Catalog(artist)).await;

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

/// Calls `hsResolve` and returns the `resolved` object of a successful answer.
///
/// # Parameters
///
/// - `fixture`: the fixture whose router answers.
/// - `query`: the parameters, without the leading `?`.
async fn resolve_path(fixture: &Fixture, query: &str) -> serde_json::Value {
    let response = get(
        &fixture.app(),
        &format!(
            "/rest/hsResolve?{query}&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&response);
    parsed(&response.body)["subsonic-response"]["resolved"].clone()
}

/// Calls `hsResolve` and asserts the failure code it answers with.
///
/// # Parameters
///
/// - `fixture`: the fixture whose router answers.
/// - `query`: the parameters, without the leading `?`.
/// - `code`: the Subsonic error code the request must answer with.
async fn resolve_fails(fixture: &Fixture, query: &str, code: i32) {
    let response = get(
        &fixture.app(),
        &format!(
            "/rest/hsResolve?{query}&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    crate::common::assert_error(&response, code);
}

#[tokio::test]
async fn an_artist_path_names_the_library_artist() {
    let seed = seeded().await;
    let resolved = resolve_path(&seed.fixture, "artist=discography-artist").await;
    assert_eq!(resolved["type"].as_str(), Some("artist"));
    assert_eq!(
        resolved["id"].as_str(),
        Some(seed.artist.to_string().as_str())
    );
    assert_eq!(resolved["path"].as_str(), Some("discography-artist"));

    let spelled = resolve_path(&seed.fixture, "artist=Discography%20Artist").await;
    assert_eq!(
        spelled["id"].as_str(),
        Some(seed.artist.to_string().as_str()),
        "a name resolves the way its segment does"
    );
    assert_eq!(spelled["path"].as_str(), Some("discography-artist"));
}

#[tokio::test]
async fn an_album_path_names_the_album_the_artist_page_lists() {
    let seed = seeded().await;
    prime(&seed.fixture, ClientId::Entity(seed.artist)).await;
    let stored = resolve_path(&seed.fixture, "artist=discography-artist&album=second-wind").await;
    assert_eq!(stored["type"].as_str(), Some("album"));
    assert_eq!(
        stored["id"].as_str(),
        Some(seed.second_wind.to_string().as_str())
    );
    assert_eq!(
        stored["path"].as_str(),
        Some("discography-artist/second-wind")
    );

    let catalog = resolve_path(&seed.fixture, "artist=discography-artist&album=first-light").await;
    let first_light = catalog_id(EntityKind::Album, "disco-album-1");
    assert_eq!(catalog["id"].as_str(), Some(first_light.as_str()));
    assert_eq!(
        catalog["path"].as_str(),
        Some("discography-artist/first-light")
    );

    let basement = resolve_path(
        &seed.fixture,
        "artist=discography-artist&album=basement-tapes",
    )
    .await;
    assert_eq!(
        basement["id"].as_str(),
        Some(seed.basement.to_string().as_str()),
        "an album the catalog does not list is still found"
    );
}

#[tokio::test]
async fn an_id_resolves_to_its_canonical_path() {
    let seed = seeded().await;
    let artist = CatalogId::new(Slug::new(SLUG).unwrap(), EntityKind::Artist, ARTIST).unwrap();
    prime(&seed.fixture, ClientId::Catalog(artist)).await;
    prime(&seed.fixture, ClientId::Entity(seed.artist)).await;
    let artist_id = catalog_id(EntityKind::Artist, ARTIST);
    let artist = resolve_path(&seed.fixture, &format!("id={artist_id}")).await;
    assert_eq!(
        artist["id"].as_str(),
        Some(seed.artist.to_string().as_str()),
        "a catalog artist resolves to the stored artist it pairs with"
    );
    assert_eq!(artist["path"].as_str(), Some("discography-artist"));

    let album_id = catalog_id(EntityKind::Album, THIRD_SINGLE);
    let third = CatalogId::new(Slug::new(SLUG).unwrap(), EntityKind::Album, THIRD_SINGLE).unwrap();
    prime(&seed.fixture, ClientId::Catalog(third)).await;
    let album = resolve_path(&seed.fixture, &format!("id={album_id}")).await;
    assert_eq!(album["id"].as_str(), Some(album_id.as_str()));
    assert_eq!(album["type"].as_str(), Some("album"));
    assert_eq!(
        album["path"].as_str(),
        Some("discography-artist/third-single")
    );
}

#[tokio::test]
async fn a_path_nothing_answers_to_is_not_found() {
    let seed = seeded().await;
    resolve_fails(&seed.fixture, "artist=discography-artist--2", 70).await;
    resolve_fails(&seed.fixture, "artist=nobody-at-all", 70).await;
    resolve_fails(
        &seed.fixture,
        "artist=discography-artist&album=nobody-at-all",
        70,
    )
    .await;
}

#[tokio::test]
async fn hs_resolve_needs_the_vendor_flag() {
    let seed = seeded().await;
    let response = get(
        &seed.fixture.app(),
        &format!(
            "/rest/hsResolve?artist=discography-artist&apiKey={}&f=json",
            seed.fixture.api_key
        ),
    )
    .await;
    crate::common::assert_error(&response, 70);
}

#[tokio::test]
async fn a_stored_artist_is_listed_once_in_search() {
    let seed = seeded().await;
    let response = get(
        &seed.fixture.app(),
        &format!(
            "/rest/search3?query=Discography%20Artist&apiKey={}&f=json",
            seed.fixture.api_key
        ),
    )
    .await;
    assert_ok(&response);
    let body = parsed(&response.body);
    let artists = body["subsonic-response"]["searchResult3"]["artist"]
        .as_array()
        .unwrap();
    assert_eq!(
        artists.len(),
        1,
        "the stored artist and its catalog twin are one entry: {}",
        response.body
    );
    assert_eq!(
        artists[0]["id"].as_str(),
        Some(seed.artist.to_string().as_str())
    );
}

#[tokio::test]
async fn a_stored_album_is_listed_once_in_search() {
    let seed = seeded().await;
    let response = get(
        &seed.fixture.app(),
        &format!(
            "/rest/search3?query=Second%20Wind&apiKey={}&f=json",
            seed.fixture.api_key
        ),
    )
    .await;
    assert_ok(&response);
    let body = parsed(&response.body);
    let albums: Vec<String> = body["subsonic-response"]["searchResult3"]["album"]
        .as_array()
        .unwrap()
        .iter()
        .map(|album| album["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        albums.contains(&seed.second_wind.to_string()),
        "the stored album is listed: {}",
        response.body
    );
    assert!(
        !albums.contains(&catalog_id(EntityKind::Album, SECOND_WIND)),
        "its catalog twin is not: {}",
        response.body
    );
}

#[tokio::test]
async fn a_member_carries_the_readable_path() {
    let seed = seeded().await;
    prime(&seed.fixture, ClientId::Entity(seed.artist)).await;
    prime(&seed.fixture, ClientId::Entity(seed.second_wind)).await;
    let body = artist_page(&seed.fixture, &seed.artist.to_string(), true).await;
    let artist = &body["subsonic-response"]["artist"];
    assert_eq!(
        artist["hearthsound"]["path"].as_str(),
        Some("discography-artist")
    );
    let first_light = &artist["album"][0];
    assert_eq!(
        first_light["hearthsound"]["path"].as_str(),
        Some("discography-artist/first-light")
    );
    let second_wind = &artist["album"][1];
    assert_eq!(
        second_wind["hearthsound"]["path"].as_str(),
        Some("discography-artist/second-wind"),
        "a title the listing disambiguates keeps both segments"
    );

    let body = album_page(&seed.fixture, &seed.second_wind.to_string()).await;
    let gust = &body["subsonic-response"]["album"]["song"][0];
    assert_eq!(gust["id"].as_str(), Some(seed.gust.to_string().as_str()));
    assert_eq!(
        gust["hearthsound"]["artistPath"].as_str(),
        Some("discography-artist")
    );
    assert_eq!(
        gust["hearthsound"]["albumPath"].as_str(),
        Some("discography-artist/second-wind"),
        "a stored song's album path is built from the album's own title"
    );
}

/// Counts the queued `catalog.enrich` jobs.
async fn enrich_jobs(fixture: &Fixture) -> i64 {
    let queued = fixture
        .state
        .jobs()
        .expect("the fixture queues jobs")
        .list_by_state(hearthsound_jobs::JobState::Ready, 100)
        .await
        .unwrap();
    queued
        .iter()
        .filter(|job| job.kind.as_str() == "catalog.enrich")
        .count()
        .try_into()
        .unwrap()
}

/// A cold first-party page answers from the library, and its burst of reads asks
/// for one enrichment while a second read asks for none.
#[tokio::test]
async fn a_cold_artist_page_answers_at_once_and_asks_for_one_enrichment() {
    let seed = seeded().await;
    let artist = seed.artist.to_string();
    let key = seed.fixture.api_key.clone();
    let app = seed.fixture.app();

    let page = artist_page(&seed.fixture, &artist, true).await;
    assert_eq!(
        album_ids(&page),
        vec![seed.basement.to_string(), seed.second_wind.to_string()],
        "a cold page is the library's, without waiting for the catalog"
    );

    let info = get(
        &app,
        &format!("/rest/getArtistInfo2?id={artist}&apiKey={key}&f=json&hearthsound=1"),
    )
    .await;
    assert_ok(&info);
    let songs = get(
        &app,
        &format!("/rest/getTopSongs?artist=Discography%20Artist&apiKey={key}&f=json&hearthsound=1"),
    )
    .await;
    assert_ok(&songs);
    assert_eq!(
        enrich_jobs(&seed.fixture).await,
        1,
        "the burst of reads asks for one enrichment"
    );

    let _ = artist_page(&seed.fixture, &artist, true).await;
    assert_eq!(
        enrich_jobs(&seed.fixture).await,
        1,
        "a second read adds no job"
    );
}
