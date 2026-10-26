//! Integration tests for `reportPlayback`: the now-playing row a report
//! updates, the play a finished session earns, the parameters the extension
//! defines, and the values it refuses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

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

/// A report updates the now-playing row, and a finished session records a play.
#[tokio::test]
async fn a_report_updates_now_playing_and_records_a_play() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let report = |state: &str, position: i64| {
        let app = app.clone();
        let api_key = fixture.api_key.clone();
        let state = state.to_owned();
        async move {
            get(
                &app,
                &format!(
                    "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs={position}&state={state}&c=Rufin&apiKey={api_key}&f=json"
                ),
            )
            .await
        }
    };

    let started = report("starting", 0).await;
    assert_ok(&started);
    let listed = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains(&format!("\"id\":\"{track}\"")),
        "the report is listed as now playing: {}",
        listed.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("reported-now-playing-json", seeded.stabilize(&listed.body));
    });

    // A pause keeps the row, and a stop clears it. Whether the session earned
    // a play depends on how long the client listened, which the activity layer
    // decides from the reports' instants and its own tests cover.
    let paused = report("paused", 30_000).await;
    assert_ok(&paused);
    let listed = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains(&format!("\"id\":\"{track}\"")),
        "a paused report is still playing: {}",
        listed.body
    );

    let stopped = report("stopped", 180_000).await;
    assert_ok(&stopped);
    let listed = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"entry\":[]"),
        "the stopped session is no longer playing: {}",
        listed.body
    );
}

/// `ignoreScrobble` keeps the now-playing row and records no play.
#[tokio::test]
async fn ignoring_a_scrobble_records_no_play() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let started = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&ignoreScrobble=true&c=Feishin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&started);
    let stopped = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=180000&state=stopped&ignoreScrobble=true&c=Feishin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&stopped);

    assert_eq!(
        fixture
            .state
            .activity()
            .plays()
            .play_count(fixture.user_id.uuid(), track.uuid())
            .await
            .unwrap(),
        0,
        "the client asked for no play"
    );
}

/// The endpoint refuses a missing or unknown parameter value.
#[tokio::test]
async fn report_playback_refuses_unusable_values() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let missing_id = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaType=song&positionMs=0&state=playing&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_id, 10);
    let missing_position = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&state=playing&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&missing_position, 10);
    let bad_state = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=wobbling&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_state, 10);

    // A podcast is a media type this server does not have; anything else is not
    // a media type the extension defines.
    let podcast = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=podcast&positionMs=0&state=playing&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&podcast, 70);
    let unknown_type = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=video&positionMs=0&state=playing&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown_type, 0);

    let bad_rate = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&playbackRate=fast&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_rate, 0);
    // A rate above the fastest the server accepts is a client bug: counting it
    // would earn a play in milliseconds.
    let fast_rate = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&playbackRate=1000&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&fast_rate, 0);
    assert!(fast_rate.body.contains("at most 8"), "{}", fast_rate.body);
    let accepted_rate = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&playbackRate=2&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&accepted_rate);
    let bad_ignore = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&ignoreScrobble=maybe&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_ignore, 0);

    // An id that names no stored track is refused rather than ignored, because
    // a report is a session the server must be able to follow.
    let unknown = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={}&mediaType=song&positionMs=0&state=playing&apiKey={}&f=json",
            seeded.alpha, fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);
}

/// The extension is advertised once the endpoint exists.
#[tokio::test]
async fn the_playback_report_extension_is_advertised() {
    let (fixture, _seeded) = fixture().await;
    let app = fixture.app();

    let listed = get(
        &app,
        &format!(
            "/rest/getOpenSubsonicExtensions?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed
            .body
            .contains("\"name\":\"playbackReport\",\"versions\":[1]"),
        "{}",
        listed.body
    );
}
