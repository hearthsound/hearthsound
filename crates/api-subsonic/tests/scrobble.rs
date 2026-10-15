//! Integration tests for `scrobble`: the play a stored track records, the ids
//! that are recorded, resolved, or discarded, and the "no song audio delivered"
//! guard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use hearthsound_activity::AudioDelivered;
use hearthsound_core::{CatalogId, EntityKind, Slug};

use common::seed::{Seeded, library};
use common::{Fixture, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library and the account's id.
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
async fn scrobble_records_a_play_for_a_stored_track() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let probe = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        !probe.body.contains("\"error\""),
        "an empty ok envelope: {}",
        probe.body
    );

    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), track.uuid())
        .await
        .unwrap();
    assert_eq!(plays, 1);
}

#[tokio::test]
async fn scrobble_discards_an_unmaterialized_catalog_id() {
    let (fixture, seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();
    let id = CatalogId::new(
        Slug::new("hello").unwrap(),
        EntityKind::Track,
        "hello-track-1",
    )
    .unwrap();

    let probe = get(
        &app,
        &format!("/rest/scrobble?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&probe);

    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), seeded.alpha_tracks[0].uuid())
        .await
        .unwrap();
    assert_eq!(plays, 0);
}

#[tokio::test]
async fn scrobble_records_a_play_for_a_materialized_catalog_id() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let id = CatalogId::new(
        Slug::new("hello").unwrap(),
        EntityKind::Track,
        "hello-track-1",
    )
    .unwrap();
    let stored = fixture
        .state
        .catalog()
        .materialize_track(&id)
        .await
        .unwrap();
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/scrobble?id={id}&apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&probe);

    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), stored.uuid())
        .await
        .unwrap();
    assert_eq!(
        plays, 1,
        "a catalog id resolves through its provider alias to the stored track"
    );
}

#[tokio::test]
async fn scrobble_honors_the_no_song_audio_guard() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    // The stream the client heard never delivered song audio.
    fixture.state.activity().sessions().register(
        fixture.user_id.uuid(),
        track.uuid(),
        Some("dsub"),
        AudioDelivered::probe(|| false),
    );

    let probe = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);

    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), track.uuid())
        .await
        .unwrap();
    assert_eq!(plays, 0, "a play needs song audio");
}

#[tokio::test]
async fn scrobble_discards_a_now_playing_report() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let probe = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&submission=false&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);

    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), track.uuid())
        .await
        .unwrap();
    assert_eq!(plays, 0);
}

#[tokio::test]
async fn scrobble_requires_an_id() {
    let (fixture, _seeded) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/scrobble?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 10);
}
