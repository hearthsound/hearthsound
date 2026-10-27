//! Integration tests for the information endpoints: the biography and links the
//! artist row stores, the identifiers the library holds, the stored similar
//! artists, and the ids each endpoint refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::EntityId;

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

/// Gives one artist the profile the indexer stores: a biography, an external
/// link and a MusicBrainz identifier.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database is written.
/// - `artist`: the artist row.
async fn store_profile(fixture: &Fixture, artist: EntityId) {
    let mut conn = fixture.database.writer().acquire().await.unwrap();
    // The page was fetched, so the view serves the row as it is stored instead
    // of walking it again.
    sqlx::query(
        "UPDATE artists SET biography = ?, links = ?, fetched_at = 1, page_fetched_at = 1 \
         WHERE id = ?",
    )
    .bind("A quartet from the valley.")
    .bind(r#"[{"kind":"lastfm","url":"https://www.last.fm/music/The+Cardinals"}]"#)
    .bind(artist.uuid().as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO external_ids (entity_kind, entity_id, namespace, value) \
         VALUES ('artist', ?, 'mb-artist', 'artist-1')",
    )
    .bind(artist.uuid().as_bytes().as_slice())
    .execute(&mut *conn)
    .await
    .unwrap();
}

/// Stores one similar-artist relationship.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database is written.
/// - `artist`: the artist the shelf belongs to.
/// - `similar`: the artist it names.
async fn store_similar(fixture: &Fixture, artist: EntityId, similar: EntityId) {
    let mut conn = fixture.database.writer().acquire().await.unwrap();
    sqlx::query("INSERT INTO artist_similar (artist_id, position, similar_id) VALUES (?, 0, ?)")
        .bind(artist.uuid().as_bytes().as_slice())
        .bind(similar.uuid().as_bytes().as_slice())
        .execute(&mut *conn)
        .await
        .unwrap();
}

/// `getArtistInfo2` answers the profile stored on the row and lists the similar
/// artists the indexer stored, filtered to the caller's scopes for a third-party
/// client.
#[tokio::test]
async fn artist_info_lists_the_stored_profile_and_similar_artists() {
    let (fixture, seeded) = fixture().await;
    store_profile(&fixture, seeded.cardinals).await;
    // `2 Mello` sits in a generic folder, so the account's personal scope does
    // not hold it: only a caller that asked for absent entities sees it.
    store_similar(&fixture, seeded.cardinals, seeded.mello).await;
    let app = fixture.app();

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
        "the row's biography is served: {}",
        json.body
    );
    assert!(
        json.body
            .contains("https://www.last.fm/music/The+Cardinals"),
        "a last.fm link fills lastFmUrl: {}",
        json.body
    );
    assert!(
        json.body.contains("\"musicBrainzId\":\"artist-1\""),
        "the stored identifier is reported: {}",
        json.body
    );
    assert!(
        json.body.contains(&seeded.mello.to_string()),
        "the stored similar artist is listed: {}",
        json.body
    );
    assert!(
        json.body.contains("\"smallImageUrl\":\"\""),
        "no unauthenticated image URL is emitted: {}",
        json.body
    );
    let local = json.body.find(&seeded.mello.to_string()).unwrap();
    assert!(local > json.body.find("similarArtist").unwrap());

    let third_party = get(
        &app,
        &format!(
            "/rest/getArtistInfo2?id={}&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&third_party);
    assert!(
        !third_party.body.contains(&seeded.mello.to_string()),
        "a third-party client sees only in-scope artists: {}",
        third_party.body
    );

    let wide = get(
        &app,
        &format!(
            "/rest/getArtistInfo2?id={}&includeNotPresent=true&apiKey={}&f=json",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&wide);
    assert!(
        wide.body.contains(&seeded.mello.to_string()),
        "includeNotPresent lists the artists outside the scope: {}",
        wide.body
    );

    let xml = get(
        &app,
        &format!(
            "/rest/getArtistInfo2.view?id={}&includeNotPresent=true&apiKey={}",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(
        xml.body.contains("<similarArtist"),
        "the similar artists are child elements: {}",
        xml.body
    );
    assert!(
        !xml.body.contains(&format!("{PORTRAIT_BASE}/portrait.jpg")),
        "no portrait URL is emitted: {}",
        xml.body
    );
}

/// A stored artist's biography is served on every read, including a repeat.
#[tokio::test]
async fn a_stored_artist_keeps_its_biography_across_refreshes() {
    let (fixture, seeded) = fixture().await;
    store_profile(&fixture, seeded.cardinals).await;
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
