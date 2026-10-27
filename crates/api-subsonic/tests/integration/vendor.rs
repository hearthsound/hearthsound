//! Integration tests for the `hearthsound` member: what the vendor flag adds to
//! track, album and artist objects in both encodings, how an album's availability
//! aggregates over its tracks, and that a request without the flag carries no
//! member.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_ok, get};

/// Builds a fixture with the seeded library and the account's id.
///
/// # Returns
///
/// The fixture and the ids of the seeded entities.
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
async fn get_song_carries_the_hearthsound_member_in_both_encodings() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getSong?id={}&apiKey={}&f=json&hearthsound=1",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"hearthsound\":{"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-song-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getSong.view?id={}&apiKey={}&hearthsound=1",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("<hearthsound"), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-song-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_album_carries_the_hearthsound_member_in_both_encodings() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&apiKey={}&f=json&hearthsound=1",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"hearthsound\":{"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-album-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getAlbum.view?id={}&apiKey={}&hearthsound=1",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("<hearthsound"), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-album-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn search3_carries_the_hearthsound_member_in_both_encodings() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"hearthsound\":{"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-search3-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/search3.view?query=cardinals&apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("<hearthsound"), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-search3-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn get_artist_carries_the_hearthsound_member_in_both_encodings() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getArtist?id={}&apiKey={}&f=json&hearthsound=1",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains("\"hearthsound\":{"), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-artist-json", seeded.stabilize(&json.body));
    });

    let xml = get(
        &app,
        &format!(
            "/rest/getArtist.view?id={}&apiKey={}&hearthsound=1",
            seeded.cardinals, fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("<hearthsound"), "{}", xml.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-artist-xml", seeded.stabilize(&xml.body));
    });
}

#[tokio::test]
async fn the_member_is_absent_without_the_vendor_flag() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    assert!(!json.body.contains("\"hearthsound\":{"), "{}", json.body);
}

#[tokio::test]
async fn an_album_with_only_cached_files_aggregates_to_cached() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/getAlbum?id={}&musicFolderId=2&apiKey={}&f=json&hearthsound=1",
            seeded.cache_album, fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("vendor-cache-album-json", seeded.stabilize(&json.body));
    });
}
