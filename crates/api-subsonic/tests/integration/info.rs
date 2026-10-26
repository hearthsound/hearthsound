//! Integration tests for the information endpoints: the biography and links a
//! metadata instance answers, the identifiers the library holds, and the ids
//! each endpoint refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, PORTRAIT_BASE, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

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

/// `getArtistInfo2` lists similar artists from the discovery layer, describes
/// their portraits, and keeps the metadata layer's order for the biography.
#[tokio::test]
async fn artist_info_lists_similar_artists_and_portraits() {
    let (fixture, seeded) = fixture().await;
    fixture.install_album_catalog(serde_json::json!({})).await;
    fixture
        .install_discovery_layer(&[(
            "scrobble",
            serde_json::json!({
                "similarArtists": [
                    { "name": "The Cardinals" },
                    { "name": "Fixture Artist" },
                ],
            }),
        )])
        .await;
    fixture.install_artist_portraits(&["The Cardinals"]).await;
    fixture
        .install_metadata_layer(&[
            serde_json::json!({
                "artists": [{
                    "name": "The Cardinals",
                    "biography": "First biography",
                }],
            }),
            serde_json::json!({
                "artists": [{
                    "name": "The Cardinals",
                    "biography": "Second biography",
                }],
            }),
        ])
        .await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getArtistInfo2?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("First biography"),
        "the first metadata instance answers: {}",
        listed.body
    );
    assert!(
        !listed.body.contains("Second biography"),
        "the second instance is not asked: {}",
        listed.body
    );
    let portrait = format!("{PORTRAIT_BASE}/portrait.jpg");
    assert_eq!(
        listed.body.matches(&portrait).count(),
        3,
        "one portrait fills the three sizes: {}",
        listed.body
    );
    assert!(
        listed.body.contains(&seeded.cardinals.to_string()),
        "the stored similar artist is listed: {}",
        listed.body
    );
    assert!(
        !listed.body.contains("x_album_r_"),
        "a catalog artist is not listed without includeNotPresent: {}",
        listed.body
    );
    let local = listed.body.find(&seeded.cardinals.to_string()).unwrap();
    assert!(local > listed.body.find("similarArtist").unwrap());

    let with_catalog = get(
        &app,
        &format!(
            "/rest/getArtistInfo2?id={}&apiKey={}&includeNotPresent=true&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&with_catalog);
    assert!(
        with_catalog.body.contains(catalog_artist().as_str()),
        "the catalog artist is listed with includeNotPresent: {}",
        with_catalog.body
    );
    let stored = with_catalog
        .body
        .find(&seeded.cardinals.to_string())
        .unwrap();
    let catalog = with_catalog.body.find(catalog_artist().as_str()).unwrap();
    assert!(
        stored < catalog,
        "the stored artists are listed first: {}",
        with_catalog.body
    );

    let xml = get(
        &app,
        &format!(
            "/rest/getArtistInfo2.view?id={}&apiKey={}&includeNotPresent=true",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains(&portrait),
        "the portrait URL is an element text: {}",
        xml.body
    );
    assert!(
        xml.body.contains("<similarArtist"),
        "the similar artists are child elements: {}",
        xml.body
    );
}

/// Returns the client id of the album catalog fixture's artist.
fn catalog_artist() -> String {
    hearthsound_core::CatalogId::new(
        hearthsound_core::Slug::new("album").expect("the fixture slug is valid"),
        hearthsound_core::EntityKind::Artist,
        "album-catalog-artist-1",
    )
    .expect("the fixture id is valid")
    .to_string()
}

/// A stored artist's biography is served on every first-party read once the
/// enrichment run has fetched it, including after a browser refresh.
///
/// The first-party read answers from the cache alone, so a biography the run
/// stored has to be found by the identifiers the page asks with. It also has to
/// be fetched at all for a library artist no catalog instance knows: the
/// metadata layer answers by name, so the biography does not depend on a catalog
/// pairing.
#[tokio::test]
async fn a_stored_artist_keeps_its_biography_across_refreshes() {
    use hearthsound_core::ClientId;

    let (fixture, seeded) = fixture().await;
    fixture
        .install_metadata_layer(&[serde_json::json!({
            "artists": [{
                "name": "The Cardinals",
                "biography": "A quartet from the valley.",
            }],
        })])
        .await;
    fixture.enrich(&ClientId::Entity(seeded.cardinals)).await;
    let app = fixture.app();

    for attempt in 0..2 {
        let json = get(
            &app,
            &format!(
                "/rest/getArtistInfo2?id={}&hearthsound=1&apiKey={}&f=json",
                seeded.cardinals, fixture.api_key
            ),
        )
        .await;
        assert_ok(&json);
        assert!(
            json.body.contains("A quartet from the valley."),
            "read {attempt} lost the biography: {}",
            json.body
        );
    }
}
