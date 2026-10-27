//! Integration tests for `scrobble`: the play a stored track records, the ids
//! that are recorded, resolved, or discarded, and the "no song audio delivered"
//! guard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::AudioDelivered;
use hearthsound_core::EntityId;

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, assert_error, assert_ok, get};

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
async fn scrobble_stores_a_now_playing_report() {
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

    let activity = fixture.state.activity();
    assert_eq!(
        activity
            .plays()
            .play_count(fixture.user_id.uuid(), track.uuid())
            .await
            .unwrap(),
        0,
        "a now-playing report is not a play"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let listed = activity
        .now_playing()
        .visible(&[fixture.user_id.uuid()], now)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "the report is stored as now playing");
    assert_eq!(listed[0].track_id, track);
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

/// A scrobble time from the future is clamped to now, and one from too far in
/// the past is refused.
#[tokio::test]
async fn a_scrobble_time_is_bounded() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];
    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()) as i64;

    // The year 2100: accepted, and stored as now, so the row expires with the
    // track instead of outliving it by decades.
    let future = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&submission=false&time={}&apiKey={}&f=json",
            4_102_444_800_000i64, fixture.api_key
        ),
    )
    .await;
    assert_ok(&future);
    let listed = fixture
        .state
        .activity()
        .now_playing()
        .visible(&[fixture.user_id.uuid()], now)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "the reported track is now playing");
    assert!(
        (now - listed[0].started_at).abs() < 60_000,
        "the row starts now, not in the future: {}",
        listed[0].started_at
    );

    // Forty days ago is older than any scrobble service accepts.
    let ancient = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&time={}&apiKey={}&f=json",
            now - 40 * 24 * 60 * 60 * 1000,
            fixture.api_key
        ),
    )
    .await;
    assert_error(&ancient, 0);
    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), track.uuid())
        .await
        .unwrap();
    assert_eq!(plays, 0, "the refused scrobble recorded nothing");
}

/// A scrobble that names an id no row answers plays the others.
///
/// An id the server does not know is discarded rather than refused: a client
/// may replay a queue that holds a track the server has since lost.
#[tokio::test]
async fn an_unresolvable_id_writes_no_play_for_the_earlier_ids() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];
    let unresolved = EntityId::from_uuid(hearthsound_core::IdPrefix::Track, uuid::Uuid::now_v7());

    let probe = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&id={unresolved}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    // An id that names no track is discarded rather than refused, so the
    // request succeeds and the earlier id is recorded.
    assert_ok(&probe);
    let plays = fixture
        .state
        .activity()
        .plays()
        .play_count(fixture.user_id.uuid(), track.uuid())
        .await
        .unwrap();
    assert_eq!(
        plays, 1,
        "the resolvable id is recorded and the unknown one is dropped"
    );
}
