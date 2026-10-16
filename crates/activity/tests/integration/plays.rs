//! Integration tests for play recording and the audio-delivered guard.
//!
//! Every test runs against a temporary migrated database, so the statements
//! under test are the ones the server will run, and the rows a play references
//! (a user and a track) are arranged by hand because the crates that own
//! `users` and `tracks` are not dependencies of this one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hearthsound_activity::{
    ActivityService, AudioDelivered, Plays, Scrobble, ScrobbleOutcome, SessionOptions,
    StreamSessions,
};
use hearthsound_db::Database;
use uuid::Uuid;

/// Opens a fresh temporary database with the server schema applied.
///
/// # Parameters
///
/// - `dir`: the temporary directory the database file lives in.
///
/// # Returns
///
/// The open database.
async fn open_database(dir: &Path) -> Database {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    database
        .migrate(&hearthsound_db::MIGRATOR, &dir.join("backups"))
        .await
        .unwrap();
    database
}

/// Returns the current time as unix milliseconds.
fn now_millis() -> i64 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()) as i64
}

/// Inserts a user, so a play can reference one.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `id`: the id to give the user.
async fn insert_user(database: &Database, id: Uuid) {
    let now = now_millis();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .bind("Test User")
    .bind("not-a-hash")
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Inserts a catalog track, so a play can reference one.
///
/// The values are irrelevant to the tests; only the row's existence matters.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `id`: the id to give the track.
async fn insert_track(database: &Database, id: Uuid) {
    let now = now_millis();
    sqlx::query(
        "INSERT INTO tracks (
             id, title, sort_title, origin, availability, created_at, updated_at
         ) VALUES (?, ?, ?, 'catalog', 'remote', ?, ?)",
    )
    .bind(id.as_bytes().as_slice())
    .bind("Test Track")
    .bind("test track")
    .bind(now)
    .bind(now)
    .execute(database.writer())
    .await
    .unwrap();
}

/// Builds a scrobble that submits a play.
///
/// # Parameters
///
/// - `user_id`: the account reporting the play.
/// - `track_id`: the track that was played.
fn submission(user_id: Uuid, track_id: Uuid) -> Scrobble<'static> {
    Scrobble {
        user_id,
        track_id,
        client: Some("dsub"),
        submission: true,
        started_at: 1_700_000_000_000,
        listened_ms: 180_000,
    }
}

#[tokio::test]
async fn a_recorded_play_is_stored_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let plays = Plays::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    assert_eq!(plays.play_count(user, track).await.unwrap(), 0);

    let id = plays
        .record_play(user, track, Some("dsub"), 1_700_000_000_000, 180_000, true)
        .await
        .unwrap();

    let row = sqlx::query!(
        "SELECT user_id, track_id, client, started_at, listened_ms, audio_delivered, scrobbled
         FROM plays WHERE id = ?",
        id.as_bytes().as_slice(),
    )
    .fetch_one(database.readers())
    .await
    .unwrap();
    assert_eq!(row.user_id, user.as_bytes().to_vec());
    assert_eq!(row.track_id, track.as_bytes().to_vec());
    assert_eq!(row.client.as_deref(), Some("dsub"));
    assert_eq!(row.started_at, 1_700_000_000_000);
    assert_eq!(row.listened_ms, 180_000);
    assert_eq!(row.audio_delivered, 1);
    assert_eq!(
        row.scrobbled, 0,
        "a new play has not been scrobbled out yet"
    );

    assert_eq!(plays.play_count(user, track).await.unwrap(), 1);
}

#[tokio::test]
async fn a_play_insert_joins_the_callers_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let plays = Plays::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    let mut rolled_back = database.writer().begin().await.unwrap();
    Plays::record_play_in(&mut rolled_back, user, track, None, 0, 1000, true)
        .await
        .unwrap();
    rolled_back.rollback().await.unwrap();
    assert_eq!(
        plays.play_count(user, track).await.unwrap(),
        0,
        "a rolled back insert leaves no play"
    );

    let mut committed = database.writer().begin().await.unwrap();
    Plays::record_play_in(&mut committed, user, track, None, 0, 1000, true)
        .await
        .unwrap();
    committed.commit().await.unwrap();
    assert_eq!(plays.play_count(user, track).await.unwrap(), 1);
}

#[tokio::test]
async fn play_counts_lists_only_tracks_the_user_played() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let plays = Plays::new(&database);
    let user = Uuid::now_v7();
    let other_user = Uuid::now_v7();
    let (played_twice, played_once, never_played) =
        (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_user(&database, other_user).await;
    for track in [played_twice, played_once, never_played] {
        insert_track(&database, track).await;
    }

    plays
        .record_play(user, played_twice, None, 0, 1000, true)
        .await
        .unwrap();
    plays
        .record_play(user, played_twice, None, 0, 1000, true)
        .await
        .unwrap();
    plays
        .record_play(user, played_once, None, 0, 1000, true)
        .await
        .unwrap();
    plays
        .record_play(other_user, never_played, None, 0, 1000, true)
        .await
        .unwrap();

    let counts = plays
        .play_counts(user, &[played_twice, played_once, never_played])
        .await
        .unwrap();
    assert_eq!(counts.get(&played_twice), Some(&2));
    assert_eq!(counts.get(&played_once), Some(&1));
    assert_eq!(
        counts.get(&never_played),
        None,
        "another user's play is not this user's count, and the track has no entry"
    );
}

#[tokio::test]
async fn a_now_playing_report_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    let mut report = submission(user, track);
    report.submission = false;
    assert_eq!(
        activity.scrobble(&report).await.unwrap(),
        ScrobbleOutcome::Discarded
    );
    assert_eq!(activity.plays().play_count(user, track).await.unwrap(), 0);
}

#[tokio::test]
async fn a_submission_without_a_session_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    let outcome = activity.scrobble(&submission(user, track)).await.unwrap();
    let ScrobbleOutcome::Recorded(first) = outcome else {
        panic!("a submission with no known session is recorded, got {outcome:?}");
    };
    assert_eq!(activity.plays().play_count(user, track).await.unwrap(), 1);

    let outcome = activity.scrobble(&submission(user, track)).await.unwrap();
    let ScrobbleOutcome::Recorded(second) = outcome else {
        panic!("every submission is recorded, got {outcome:?}");
    };
    assert_ne!(first, second, "each play is its own row");
    assert_eq!(activity.plays().play_count(user, track).await.unwrap(), 2);
}

#[tokio::test]
async fn a_session_that_delivered_no_audio_blocks_the_play() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    // A stream that delivered no song audio reports false.
    activity
        .sessions()
        .register(user, track, Some("dsub"), AudioDelivered::probe(|| false));

    assert_eq!(
        activity.scrobble(&submission(user, track)).await.unwrap(),
        ScrobbleOutcome::NoSongAudio
    );
    assert_eq!(
        activity.plays().play_count(user, track).await.unwrap(),
        0,
        "a song the user never heard is not a play"
    );
}

#[tokio::test]
async fn a_delivered_session_records_the_play() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    // A raw file stream that wrote bytes to the client sets its flag.
    let (delivered, flag) = AudioDelivered::flag();
    activity
        .sessions()
        .register(user, track, Some("dsub"), delivered);
    assert_eq!(
        activity.scrobble(&submission(user, track)).await.unwrap(),
        ScrobbleOutcome::NoSongAudio,
        "before any bytes were written the session delivered no song audio"
    );

    flag.store(true, Ordering::SeqCst);
    assert!(matches!(
        activity.scrobble(&submission(user, track)).await.unwrap(),
        ScrobbleOutcome::Recorded(_)
    ));
    assert_eq!(activity.plays().play_count(user, track).await.unwrap(), 1);
}

#[tokio::test]
async fn the_latest_session_decides() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (user, first_track, second_track) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, first_track).await;
    insert_track(&database, second_track).await;

    // An earlier attempt that delivered nothing followed by a stream that
    // delivered: the restarted stream owns the answer.
    activity
        .sessions()
        .register(user, first_track, None, AudioDelivered::probe(|| false));
    activity
        .sessions()
        .register(user, first_track, None, AudioDelivered::probe(|| true));
    assert!(matches!(
        activity
            .scrobble(&submission(user, first_track))
            .await
            .unwrap(),
        ScrobbleOutcome::Recorded(_)
    ));

    // The other way round: the newest attempt delivered nothing, so the
    // scrobble is not a play even though an earlier attempt did deliver.
    activity
        .sessions()
        .register(user, second_track, None, AudioDelivered::probe(|| true));
    activity
        .sessions()
        .register(user, second_track, None, AudioDelivered::probe(|| false));
    assert_eq!(
        activity
            .scrobble(&submission(user, second_track))
            .await
            .unwrap(),
        ScrobbleOutcome::NoSongAudio
    );
    assert_eq!(
        activity
            .plays()
            .play_count(user, second_track)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn one_users_session_does_not_guard_another_users_play() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let activity = ActivityService::new(&database);
    let (quiet_user, listener) = (Uuid::now_v7(), Uuid::now_v7());
    let track = Uuid::now_v7();
    insert_user(&database, quiet_user).await;
    insert_user(&database, listener).await;
    insert_track(&database, track).await;

    activity
        .sessions()
        .register(quiet_user, track, None, AudioDelivered::probe(|| false));

    assert_eq!(
        activity
            .scrobble(&submission(quiet_user, track))
            .await
            .unwrap(),
        ScrobbleOutcome::NoSongAudio
    );
    assert!(
        matches!(
            activity
                .scrobble(&submission(listener, track))
                .await
                .unwrap(),
            ScrobbleOutcome::Recorded(_)
        ),
        "the listener's own history is not affected by another user's session"
    );
    assert_eq!(
        activity.plays().play_count(listener, track).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn an_expired_session_is_not_consulted() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let sessions = StreamSessions::with_options(SessionOptions {
        ttl: Duration::ZERO,
        ..SessionOptions::default()
    });
    let activity = ActivityService::with_sessions(&database, sessions);
    let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    insert_user(&database, user).await;
    insert_track(&database, track).await;

    activity
        .sessions()
        .register(user, track, None, AudioDelivered::probe(|| false));
    assert!(
        matches!(
            activity.scrobble(&submission(user, track)).await.unwrap(),
            ScrobbleOutcome::Recorded(_)
        ),
        "a session past its lifetime is no evidence against the play"
    );
}
