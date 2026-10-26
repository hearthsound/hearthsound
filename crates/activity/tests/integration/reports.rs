//! Tests for playback reports and the plays they earn.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{
    ActivityService, PlaybackOutcome, PlaybackReport, PlaybackState, Scrobble, ScrobbleOutcome,
};
use hearthsound_core::EntityId;
use hearthsound_library::LibraryRepository;

use crate::common::{insert_album, insert_track, insert_user, open_database, track_id};

/// The instant the tests report at.
const START: i64 = 1_700_000_000_000;

/// Returns an instant that is recent enough for a play recorded at it to
/// suppress the scrobble that follows, because the scrobble compares against the
/// current time.
fn recent(millis_ago: i64) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    now - millis_ago
}

/// A report for one state, with the values most tests want.
fn report<'a>(
    user: uuid::Uuid,
    client: &'a str,
    track: EntityId,
    duration_ms: i64,
    state: PlaybackState,
    now: i64,
) -> PlaybackReport<'a> {
    PlaybackReport {
        user_id: user,
        client,
        track_id: track,
        duration_ms,
        state,
        position_ms: 0,
        rate: 1.0,
        ignore_scrobble: false,
        now,
    }
}

/// A track of `duration_ms`, played from the start to the end, earns one play.
#[tokio::test]
async fn a_played_track_is_recorded_when_it_stops() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let (one, two) = (track_id(tracks[0]), track_id(tracks[1]));

    // The instants are close to the present, because the scrobble below is
    // compared against the current time.
    let start = recent(200_000);
    assert_eq!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                200_000,
                PlaybackState::Starting,
                start
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
    assert_eq!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                200_000,
                PlaybackState::Playing,
                start + 100_000
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
    let outcome = reports
        .report(&report(
            user,
            "dsub",
            one,
            200_000,
            PlaybackState::Stopped,
            start + 200_000,
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, PlaybackOutcome::Recorded(_)),
        "{outcome:?}"
    );

    let plays = activity.plays();
    assert_eq!(plays.play_count(user, one.uuid()).await.unwrap(), 1);
    let stored =
        sqlx::query!("SELECT started_at, listened_ms, audio_delivered, client FROM plays",)
            .fetch_one(database.readers())
            .await
            .unwrap();
    assert_eq!(stored.started_at, start);
    assert_eq!(stored.listened_ms, 200_000);
    assert_eq!(stored.audio_delivered, 1);
    assert_eq!(stored.client.as_deref(), Some("dsub"));

    // The now-playing row is gone once the client stopped.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM now_playing")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // A play recorded from a report suppresses the scrobble that follows.
    let scrobble = Scrobble {
        user_id: user,
        track_id: one.uuid(),
        client: Some("dsub"),
        submission: true,
        started_at: start,
        listened_ms: 200_000,
    };
    assert_eq!(
        activity.scrobble(&scrobble).await.unwrap(),
        ScrobbleOutcome::AlreadyRecorded
    );
    assert_eq!(plays.play_count(user, one.uuid()).await.unwrap(), 1);

    // A different track is not suppressed.
    let scrobble = Scrobble {
        track_id: two.uuid(),
        ..scrobble
    };
    assert!(matches!(
        activity.scrobble(&scrobble).await.unwrap(),
        ScrobbleOutcome::Recorded(_)
    ));
}

/// A report that keeps playing keeps the now-playing row current.
#[tokio::test]
async fn a_playing_report_keeps_now_playing_current() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    reports
        .report(&report(
            user,
            "dsub",
            one,
            200_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    reports
        .report(&report(
            user,
            "dsub",
            one,
            200_000,
            PlaybackState::Paused,
            START + 10_000,
        ))
        .await
        .unwrap();
    let stored = sqlx::query!("SELECT track_id, started_at, position_ms, state FROM now_playing",)
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(stored.track_id, one.uuid().as_bytes().to_vec());
    assert_eq!(stored.started_at, START);
    assert_eq!(stored.state, "paused");

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM now_playing")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        0
    );
}

/// A client that listened to too little, or a track that is too short, earns
/// nothing.
#[tokio::test]
async fn too_little_listening_earns_no_play() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let long = track_id(tracks[0]);
    let short = track_id(insert_track(&library, album, "Interlude", 3).await);

    // Stopped after ten seconds of a three-minute track.
    reports
        .report(&report(
            user,
            "dsub",
            long,
            180_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    assert_eq!(
        reports
            .report(&report(
                user,
                "dsub",
                long,
                180_000,
                PlaybackState::Stopped,
                START + 10_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
    assert_eq!(
        activity
            .plays()
            .play_count(user, long.uuid())
            .await
            .unwrap(),
        0
    );

    // A twenty-second track played in full is too short to count.
    reports
        .report(&report(
            user,
            "dsub",
            short,
            20_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    assert_eq!(
        reports
            .report(&report(
                user,
                "dsub",
                short,
                20_000,
                PlaybackState::Stopped,
                START + 20_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
    assert_eq!(
        activity
            .plays()
            .play_count(user, short.uuid())
            .await
            .unwrap(),
        0
    );
}

/// Half of a long track is enough, and four minutes of a longer one is too.
#[tokio::test]
async fn half_or_four_minutes_is_enough() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let half = track_id(tracks[0]);
    let long = track_id(insert_track(&library, album, "Long", 3).await);

    // Half of a ten-minute track.
    reports
        .report(&report(
            user,
            "dsub",
            half,
            600_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                half,
                600_000,
                PlaybackState::Stopped,
                START + 300_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));

    // Four minutes of an hour-long track.
    reports
        .report(&report(
            user,
            "dsub",
            long,
            3_600_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                long,
                3_600_000,
                PlaybackState::Stopped,
                START + 240_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));

    // Three minutes of an hour-long track is not.
    let another = track_id(insert_track(&library, album, "Longer", 4).await);
    reports
        .report(&report(
            user,
            "other",
            another,
            3_600_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    assert_eq!(
        reports
            .report(&report(
                user,
                "other",
                another,
                3_600_000,
                PlaybackState::Stopped,
                START + 180_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
}

/// A client that asks to be ignored is not recorded, and a paused stretch does
/// not count as listening.
#[tokio::test]
async fn paused_time_and_ignored_reports_earn_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    // Fifty seconds playing, then a ten-minute pause, then a stop: only the
    // fifty seconds count.
    reports
        .report(&report(
            user,
            "dsub",
            one,
            100_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    reports
        .report(&report(
            user,
            "dsub",
            one,
            100_000,
            PlaybackState::Paused,
            START + 50_000,
        ))
        .await
        .unwrap();
    let mut stopped = report(
        user,
        "dsub",
        one,
        100_000,
        PlaybackState::Stopped,
        START + 650_000,
    );
    stopped.ignore_scrobble = true;
    assert_eq!(
        reports.report(&stopped).await.unwrap(),
        PlaybackOutcome::Updated
    );
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        0
    );

    // The same pause without the ignore flag earns nothing either, because the
    // listened time is forty seconds of a hundred-second track even though ten
    // minutes passed.
    reports
        .report(&report(
            user,
            "other",
            one,
            100_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    reports
        .report(&report(
            user,
            "other",
            one,
            100_000,
            PlaybackState::Paused,
            START + 40_000,
        ))
        .await
        .unwrap();
    assert_eq!(
        reports
            .report(&report(
                user,
                "other",
                one,
                100_000,
                PlaybackState::Stopped,
                START + 650_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated
    );
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        0
    );
}

/// A playback rate of two counts twice the wall time.
#[tokio::test]
async fn a_double_rate_counts_double() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    let mut starting = report(user, "dsub", one, 200_000, PlaybackState::Starting, START);
    starting.rate = 2.0;
    reports.report(&starting).await.unwrap();
    let mut playing = report(
        user,
        "dsub",
        one,
        200_000,
        PlaybackState::Playing,
        START + 50_000,
    );
    playing.rate = 2.0;
    reports.report(&playing).await.unwrap();
    let mut stopped = report(
        user,
        "dsub",
        one,
        200_000,
        PlaybackState::Stopped,
        START + 100_000,
    );
    // A client playing at double rate reports that rate on every report, so the
    // interval that has passed since the last one is counted at double.
    stopped.rate = 2.0;
    let outcome = reports.report(&stopped).await.unwrap();
    assert!(
        matches!(outcome, PlaybackOutcome::Recorded(_)),
        "{outcome:?}"
    );
    let listened: i64 = sqlx::query_scalar("SELECT listened_ms FROM plays")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(listened, 200_000, "a hundred seconds at double rate");
}

/// Starting another track closes the first session and records the play it
/// earned.
#[tokio::test]
async fn starting_another_track_closes_the_first_session() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let (one, two) = (track_id(tracks[0]), track_id(tracks[1]));

    reports
        .report(&report(
            user,
            "dsub",
            one,
            100_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    let outcome = reports
        .report(&report(
            user,
            "dsub",
            two,
            100_000,
            PlaybackState::Starting,
            START + 60_000,
        ))
        .await
        .unwrap();
    assert!(
        matches!(outcome, PlaybackOutcome::Recorded(_)),
        "{outcome:?}"
    );
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        1
    );

    let stored: Vec<u8> = sqlx::query_scalar("SELECT track_id FROM now_playing")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(stored, two.uuid().as_bytes().to_vec());
}

/// A session that goes quiet is dropped rather than recorded.
#[tokio::test]
async fn a_quiet_session_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let (one, two) = (track_id(tracks[0]), track_id(tracks[1]));

    reports
        .report(&report(
            user,
            "dsub",
            one,
            100_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    // Two hours later, a report about another track arrives: the first session
    // is older than its duration plus the grace period, so its play is lost
    // rather than recorded from a client that never said it stopped.
    reports
        .report(&report(
            user,
            "dsub",
            two,
            100_000,
            PlaybackState::Stopped,
            START + 7_200_000,
        ))
        .await
        .unwrap();
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        0
    );
}

/// The visibility windows are the ones the endpoints promise.
#[tokio::test]
async fn stale_now_playing_rows_are_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let playing = activity.now_playing();
    let user = insert_user(&database).await;
    let (album, tracks) = insert_album(&library).await;
    let known = track_id(tracks[0]);
    let unknown = track_id(insert_track(&library, album, "No duration", 3).await);

    // Give one track a known duration and leave the other without one.
    sqlx::query("UPDATE tracks SET duration_ms = ? WHERE id = ?")
        .bind(200_000)
        .bind(known.uuid().as_bytes().as_slice())
        .execute(database.writer())
        .await
        .unwrap();
    playing
        .upsert(user, "known", known, START, 0, "playing")
        .await
        .unwrap();
    playing
        .upsert(user, "unknown", unknown, START, 0, "playing")
        .await
        .unwrap();

    let listed = playing
        .visible(&[user], START + 200_000 + 600_000 - 1)
        .await
        .unwrap();
    assert_eq!(
        listed.len(),
        2,
        "both rows are fresh just before their windows end"
    );
    let listed = playing
        .visible(&[user], START + 200_000 + 600_000)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "the known track's window has closed");
    assert_eq!(listed[0].client, "unknown");

    let players = playing.players(START + 200_000 + 600_000).await.unwrap();
    assert_eq!(players, vec![user]);
    let listed = playing.visible(&[], START).await.unwrap();
    assert!(
        listed.is_empty(),
        "an account that may not be listed is left out"
    );
}

/// A report suppresses exactly one scrobble of the play it recorded; a second
/// submission of the same track is a play of its own.
#[tokio::test]
async fn a_reported_play_suppresses_one_scrobble_only() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    let start = recent(300_000);
    reports
        .report(&report(
            user,
            "dsub",
            one,
            200_000,
            PlaybackState::Starting,
            start,
        ))
        .await
        .unwrap();
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                200_000,
                PlaybackState::Stopped,
                start + 200_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));

    let scrobble = Scrobble {
        user_id: user,
        track_id: one.uuid(),
        client: Some("dsub"),
        submission: true,
        started_at: start,
        listened_ms: 200_000,
    };
    assert_eq!(
        activity.scrobble(&scrobble).await.unwrap(),
        ScrobbleOutcome::AlreadyRecorded,
        "the scrobble of the reported play is dropped"
    );
    assert!(
        matches!(
            activity.scrobble(&scrobble).await.unwrap(),
            ScrobbleOutcome::Recorded(_)
        ),
        "a second submission is a play of its own"
    );
    assert_eq!(activity.plays().play_count(user, one.uuid()).await.unwrap(), 2);
}

/// Repeat-one: each listen of a track that already counted is a play of its own.
#[tokio::test]
async fn repeat_one_records_each_full_listen() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Playing,
            START + 130_000,
        ))
        .await
        .unwrap();
    // The track starts again after the first listen already counted.
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                240_000,
                PlaybackState::Starting,
                START + 140_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));
    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Playing,
            START + 260_000,
        ))
        .await
        .unwrap();
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                240_000,
                PlaybackState::Stopped,
                START + 270_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));

    let plays = activity.plays().play_count(user, one.uuid()).await.unwrap();
    assert_eq!(plays, 2, "each listen counted");
    let rows = sqlx::query!("SELECT started_at, listened_ms FROM plays ORDER BY started_at")
        .fetch_all(database.readers())
        .await
        .unwrap();
    assert_eq!(rows[0].started_at, START);
    assert_eq!(
        rows[0].listened_ms, 140_000,
        "the first play counts up to the moment the repeat started"
    );
    assert_eq!(rows[1].started_at, START + 140_000);
    assert_eq!(
        rows[1].listened_ms, 130_000,
        "the second play counts only the second listen"
    );
}

/// A start before the threshold continues the same session, which is what a
/// restart mid-track looks like.
#[tokio::test]
async fn a_restart_before_the_threshold_continues_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Playing,
            START + 30_000,
        ))
        .await
        .unwrap();
    assert_eq!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                240_000,
                PlaybackState::Starting,
                START + 31_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Updated,
        "a start before the threshold records nothing"
    );
    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Playing,
            START + 130_000,
        ))
        .await
        .unwrap();
    assert!(matches!(
        reports
            .report(&report(
                user,
                "dsub",
                one,
                240_000,
                PlaybackState::Stopped,
                START + 140_000,
            ))
            .await
            .unwrap(),
        PlaybackOutcome::Recorded(_)
    ));

    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        1
    );
    let stored = sqlx::query!("SELECT started_at, listened_ms FROM plays")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(stored.started_at, START, "the session kept its start");
    assert_eq!(stored.listened_ms, 140_000);
}

/// A track too short to count records nothing however often it repeats.
#[tokio::test]
async fn a_repeat_of_a_track_too_short_to_count_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (album, _) = insert_album(&library).await;
    let short = track_id(insert_track(&library, album, "Interlude", 3).await);

    let mut at = START;
    for listen in 0..3 {
        reports
            .report(&report(
                user,
                "dsub",
                short,
                20_000,
                PlaybackState::Starting,
                at,
            ))
            .await
            .unwrap();
        at += 20_000;
        assert_eq!(
            reports
                .report(&report(
                    user,
                    "dsub",
                    short,
                    20_000,
                    PlaybackState::Stopped,
                    at,
                ))
                .await
                .unwrap(),
            PlaybackOutcome::Updated,
            "listen {} of a twenty-second track", listen + 1
        );
    }
    assert_eq!(
        activity
            .plays()
            .play_count(user, short.uuid())
            .await
            .unwrap(),
        0
    );
}

/// After the threshold every start is a new play, including a seek back: the
/// rule cannot tell a seek from a repeat, and the threshold is the definition
/// of one listen.
#[tokio::test]
async fn a_seek_back_after_the_threshold_starts_a_new_play() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let library = LibraryRepository::new(&database);
    let activity = ActivityService::new(&database);
    let reports = activity.reports();
    let user = insert_user(&database).await;
    let (_, tracks) = insert_album(&library).await;
    let one = track_id(tracks[0]);

    reports
        .report(&report(
            user,
            "dsub",
            one,
            240_000,
            PlaybackState::Starting,
            START,
        ))
        .await
        .unwrap();
    let mut seek = report(
        user,
        "dsub",
        one,
        240_000,
        PlaybackState::Starting,
        START + 130_000,
    );
    seek.position_ms = 0;
    assert!(matches!(
        reports.report(&seek).await.unwrap(),
        PlaybackOutcome::Recorded(_)
    ));
    assert_eq!(
        activity.plays().play_count(user, one.uuid()).await.unwrap(),
        1
    );
}
