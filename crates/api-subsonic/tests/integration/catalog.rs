//! Integration tests for browsing catalog entities by their `x_` id: what
//! `getSong`, `getAlbum`, and `getArtist` answer for an unmaterialized entity,
//! and the errors an unknown or mismatched id is answered with.
//!
//! The catalog side is the `hello-catalog` fixture, so the ids and entities the
//! snapshots show are exactly what a real provider would issue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{CatalogId, EntityKind, Slug};

use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the hello catalog installed.
async fn fixture() -> Fixture {
    let fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    fixture
}

/// Builds the `x_` id of one hello-catalog entity.
fn catalog_id(kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new("hello").unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn get_song_answers_a_catalog_id() {
    let fixture = fixture().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Track, "hello-track-1");

    let json = get(
        &app,
        &format!("/rest/getSong?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert!(json.body.contains(&id), "{}", json.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("catalog-song-json", &json.body);
    });

    let xml = get(
        &app,
        &format!("/rest/getSong.view?id={id}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("catalog-song-xml", &xml.body);
    });
}

#[tokio::test]
async fn get_album_answers_a_catalog_id() {
    let fixture = fixture().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Album, "hello-album-1");

    let json = get(
        &app,
        &format!("/rest/getAlbum?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body
            .contains(&catalog_id(EntityKind::Track, "hello-track-1")),
        "{}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("catalog-album-json", &json.body);
    });

    let xml = get(
        &app,
        &format!("/rest/getAlbum.view?id={id}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("catalog-album-xml", &xml.body);
    });
}

#[tokio::test]
async fn get_artist_answers_a_catalog_id() {
    let fixture = fixture().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Artist, "hello-artist-1");

    let json = get(
        &app,
        &format!("/rest/getArtist?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body
            .contains(&catalog_id(EntityKind::Album, "hello-album-1")),
        "{}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("catalog-artist-json", &json.body);
    });
}

#[tokio::test]
async fn a_catalog_id_of_another_kind_is_not_found() {
    let fixture = fixture().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Album, "hello-album-1");

    let probe = get(
        &app,
        &format!("/rest/getSong?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn an_unknown_catalog_id_is_not_found() {
    let fixture = fixture().await;
    let app = fixture.app();
    let id = catalog_id(EntityKind::Track, "hello-track-404");

    let probe = get(
        &app,
        &format!("/rest/getSong?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 70);
}
