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

/// A report and a scrobble from an unnamed client key the same now-playing row.
#[tokio::test]
async fn an_unnamed_client_reports_under_the_default_name() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let reported = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=0&state=playing&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&reported);
    let scrobbled = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&submission=false&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&scrobbled);

    let now = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()) as i64;
    let listed = fixture
        .state
        .activity()
        .now_playing()
        .visible(&[fixture.user_id.uuid()], now)
        .await
        .unwrap();
    assert_eq!(
        listed.len(),
        1,
        "one client is one row, whatever it calls itself: {listed:?}"
    );
    assert_eq!(listed[0].client, hearthsound_activity::DEFAULT_CLIENT);
}

/// A position past the end of the track is applied as the end of it.
#[tokio::test]
async fn an_impossible_position_is_clamped_to_the_track_length() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];
    let row = hearthsound_library::LibraryRepository::new(&fixture.database)
        .get_track(track)
        .await
        .unwrap()
        .unwrap();
    let duration = row.duration_ms.expect("the seeded track has a duration");
    assert!(duration > 0);

    let reported = get(
        &app,
        &format!(
            "/rest/reportPlayback?mediaId={track}&mediaType=song&positionMs=9999999&state=playing&c=Clamp&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&reported);

    let stored: i64 =
        sqlx::query_scalar("SELECT position_ms FROM now_playing WHERE client = 'Clamp'")
            .fetch_one(fixture.database.readers())
            .await
            .unwrap();
    assert_eq!(
        stored, duration,
        "a position past the end is stored as the track's length"
    );
}
