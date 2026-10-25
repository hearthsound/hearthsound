//! Integration tests for `getNowPlaying`: the row a scrobble stores, the
//! visibility rule `privacy.now_playing_visible` decides, and the rows that
//! stop being listed once the track has run out.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_ok, get};

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

/// A scrobble that submits no play is listed as now playing.
#[tokio::test]
async fn a_report_is_listed_as_now_playing() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    let empty = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&empty);
    assert!(empty.body.contains("\"entry\":[]"), "{}", empty.body);

    let reported = get(
        &app,
        &format!(
            "/rest/scrobble?id={track}&submission=false&c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&reported);

    let listed = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains(&format!("\"id\":\"{track}\"")),
        "the reported track is listed: {}",
        listed.body
    );
    assert!(
        listed.body.contains("\"username\":\"echo\""),
        "{}",
        listed.body
    );
    assert!(
        listed.body.contains("\"playerName\":\"Rufin\""),
        "{}",
        listed.body
    );
    assert!(listed.body.contains("\"playerId\":0"), "{}", listed.body);
    assert!(listed.body.contains("\"minutesAgo\":0"), "{}", listed.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("now-playing-json", seeded.stabilize(&listed.body));
    });
}

/// An account that turns its visibility off is not listed to another account.
#[tokio::test]
async fn the_visibility_setting_hides_another_accounts_rows() {
    let (fixture, seeded) = fixture().await;
    let (other, other_key) = {
        let user = hearthsound_auth::Users::new(&fixture.database)
            .create(
                &fixture.settings.current(),
                &hearthsound_auth::NewUser::new("other", "Other", "hunter22", false),
            )
            .await
            .unwrap();
        let key = hearthsound_auth::ApiKeys::new(&fixture.database)
            .generate(user.id, "other")
            .await
            .unwrap()
            .plaintext;
        (user.id, key)
    };
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    // The other account reports a play, and the server-wide default is on, so
    // the report is visible.
    let reported = get(
        &app,
        &format!("/rest/scrobble?id={track}&submission=false&c=Feishin&apiKey={other_key}&f=json"),
    )
    .await;
    assert_ok(&reported);
    let seen = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&seen);
    assert!(
        seen.body.contains("\"username\":\"other\""),
        "another account's report is visible by default: {}",
        seen.body
    );

    // The other account hides its activity, and the row is gone for everyone
    // but that account itself.
    fixture
        .state
        .user_settings()
        .set(
            other,
            keys::PRIVACY_NOW_PLAYING_VISIBLE.name(),
            SettingValue::Bool(false),
        )
        .await
        .unwrap();
    let hidden = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&hidden);
    assert!(
        !hidden.body.contains("\"username\":\"other\""),
        "the hidden report is not listed: {}",
        hidden.body
    );
    let own = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={other_key}&f=json"),
    )
    .await;
    assert_ok(&own);
    assert!(
        own.body.contains("\"username\":\"other\""),
        "an account always sees its own rows: {}",
        own.body
    );
}

/// A report whose track has run out is no longer listed.
#[tokio::test]
async fn a_finished_report_is_no_longer_listed() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let track = seeded.alpha_tracks[0];

    // A report from long ago, before the track's duration and its grace period
    // had passed, is what the endpoint reads.
    let activity = fixture.state.activity();
    let long_ago = 1_000_000;
    activity
        .now_playing()
        .upsert(
            fixture.user_id.uuid(),
            "Rufin",
            track,
            long_ago,
            0,
            "playing",
        )
        .await
        .unwrap();

    let listed = get(
        &app,
        &format!("/rest/getNowPlaying?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&listed);
    assert!(
        listed.body.contains("\"entry\":[]"),
        "a report from before the track ended is gone: {}",
        listed.body
    );
}
