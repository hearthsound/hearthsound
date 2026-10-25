//! Admission, the queue, and the worker slots.
//!
//! These tests drive the orchestrator the way the API layer does and read the
//! effects from the places the rest of the server reads: the `acquisitions` and
//! `acquisition_log` rows, the `tracks.availability` column, the staging tree,
//! the event bus and the waiter channels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::{
    Env, INSTANCE, InstanceLimits, album_folder, album_instance_config, config, drain,
    has_acquisition_event, has_availability_event, one_candidate,
};
use hearthsound_acquire::orchestrator::{
    AcquireOutcome, AcquireRequest, AcquisitionProgress, AlbumAcquireRequest,
};
use hearthsound_core::events::ServerEvent;
use hearthsound_core::{AcquisitionState, Availability, FailureReason, ParkReason, Priority};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn admits_one_acquisition_per_track_and_raises_its_priority() {
    let env = Env::normal(&[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let mut events = env.events.subscribe();

    let queue = AcquireRequest::new(track, Priority::Queue, "playlist add");
    let started = env.orchestrator.request(queue).await.unwrap();
    let AcquireOutcome::Started(handle) = &started else {
        panic!("the first request must start an acquisition, got {started:?}");
    };
    let id = handle.id;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Queued);
    assert_eq!(row.priority, Priority::Queue);
    assert_eq!(row.track_id, Some(track));
    assert_eq!(row.reason, "playlist add");
    assert_eq!(row.attempts, 0);
    assert_eq!(env.track(track).await.availability, Availability::Acquiring);
    assert!(
        env.orchestrator.staging_dir(id).is_dir(),
        "admission creates the acquisition's staging directory"
    );
    assert_eq!(env.log(id).await.len(), 1);
    assert_eq!(env.log(id).await[0].state, AcquisitionState::Queued);

    let published = drain(&mut events);
    assert!(has_acquisition_event(
        &published,
        id,
        AcquisitionState::Queued
    ));
    assert!(has_availability_event(
        &published,
        track,
        Availability::Acquiring
    ));

    // The waiter already holds the current snapshot.
    assert_eq!(handle.snapshot().state, AcquisitionState::Queued);
    assert_eq!(handle.priority, Priority::Queue);

    // A second, more urgent request joins the job instead of adding one.
    let play = AcquireRequest::new(track, Priority::Play, "user pressed play");
    let joined = env.orchestrator.request(play).await.unwrap();
    let AcquireOutcome::Joined(joined) = &joined else {
        panic!("the second request must join, got {joined:?}");
    };
    assert_eq!(joined.id, id);
    assert_eq!(joined.priority, Priority::Play);
    assert_eq!(joined.snapshot().priority, Priority::Play);
    assert_eq!(joined.snapshot().state, AcquisitionState::Queued);
    assert_eq!(env.acquisitions_for(track).await, 1);
    assert_eq!(env.acquisition(id).await.priority, Priority::Play);

    let log = env.log(id).await;
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].state, AcquisitionState::Queued);
    assert_eq!(log[1].message.as_deref(), Some("priority raised to play"));

    // A less urgent request does not lower the priority.
    let background = AcquireRequest::new(track, Priority::Background, "scheduled run");
    let joined = env.orchestrator.request(background).await.unwrap();
    assert_eq!(joined.acquisition_id(), Some(id));
    assert_eq!(env.acquisition(id).await.priority, Priority::Play);
    assert_eq!(env.log(id).await.len(), 2);

    let published = drain(&mut events);
    assert!(has_acquisition_event(
        &published,
        id,
        AcquisitionState::Queued
    ));
    let updated = published
        .iter()
        .find_map(|event| match event {
            ServerEvent::AcquisitionUpdated(update) => Some(update),
            _ => None,
        })
        .expect("the raise publishes an acquisition.updated");
    assert_eq!(updated.priority, Priority::Play);
}

#[tokio::test]
async fn refuses_a_track_that_is_cooling_down_until_it_is_forced() {
    let env = Env::normal(&[]).await;
    let until = now_millis() + 3_600_000;
    let track = env
        .add_track(
            "Hello Song",
            "Hello Artist",
            Availability::Unavailable,
            Some(until),
        )
        .await;

    let refused = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    match refused {
        AcquireOutcome::CoolingDown {
            track_id,
            unavailable_until,
        } => {
            assert_eq!(track_id, track);
            assert_eq!(unavailable_until, until);
        }
        other => panic!("a cooling track must be refused, got {other:?}"),
    }
    assert_eq!(env.acquisitions_for(track).await, 0);
    let row = env.track(track).await;
    assert_eq!(row.availability, Availability::Unavailable);
    assert_eq!(row.unavailable_until, Some(until));

    let forced = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "admin retry").forced())
        .await
        .unwrap();
    assert!(
        matches!(forced, AcquireOutcome::Started(_)),
        "a forced request ignores the cooldown, got {forced:?}"
    );
    assert_eq!(env.acquisitions_for(track).await, 1);
    assert_eq!(env.track(track).await.availability, Availability::Acquiring);
    assert_eq!(env.track(track).await.unavailable_until, None);
}

#[tokio::test]
async fn records_nothing_when_the_requester_may_not_acquire() {
    let env = Env::normal(&[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;

    let denied = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request").with_can_acquire(false))
        .await
        .unwrap();
    match denied {
        AcquireOutcome::Denied { track_id, reason } => {
            assert_eq!(track_id, track);
            assert_eq!(reason, FailureReason::PermissionDenied);
        }
        other => panic!("the request must be denied, got {other:?}"),
    }
    assert_eq!(env.acquisitions_for(track).await, 0);
    assert_eq!(env.track(track).await.availability, Availability::Remote);
    assert!(
        env.staging.read_dir().unwrap().next().is_none(),
        "a denied request does not create a staging directory"
    );
}

#[tokio::test]
async fn serves_a_track_that_is_already_on_disk() {
    let env = Env::normal(&[]).await;
    for availability in [Availability::Local, Availability::Cached] {
        let track = env
            .add_track("Hello Song", "Hello Artist", availability, None)
            .await;
        let outcome = env
            .orchestrator
            .request(AcquireRequest::new(track, Priority::Play, "user request"))
            .await
            .unwrap();
        match outcome {
            AcquireOutcome::AlreadyAvailable { track_id } => assert_eq!(track_id, track),
            other => panic!("an available track needs no acquisition, got {other:?}"),
        }
        assert_eq!(env.acquisitions_for(track).await, 0);
    }
}

#[tokio::test]
async fn the_queue_runs_the_most_urgent_acquisition_first() {
    let env = Env::normal(&[]).await;
    let mut ids = Vec::new();
    for (priority, title) in [
        (Priority::Upgrade, "Upgrade Song"),
        (Priority::Queue, "Queue Song"),
        (Priority::Play, "Play Song"),
    ] {
        let track = env.remote_track(title, "Hello Artist").await;
        let outcome = env
            .orchestrator
            .request(AcquireRequest::new(track, priority, "test"))
            .await
            .unwrap();
        ids.push(outcome.acquisition_id().unwrap());
    }

    let repository = env.orchestrator.repository();
    assert_eq!(repository.next_queued().await.unwrap().unwrap().id, ids[2]);
    repository.claim(ids[2]).await.unwrap();
    assert_eq!(repository.next_queued().await.unwrap().unwrap().id, ids[1]);
    repository.claim(ids[1]).await.unwrap();
    assert_eq!(repository.next_queued().await.unwrap().unwrap().id, ids[0]);
    repository.claim(ids[0]).await.unwrap();
    assert!(repository.next_queued().await.unwrap().is_none());
}

#[tokio::test]
async fn a_claimed_acquisition_is_logged_at_every_state() {
    let env = Env::normal(&[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let started = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    let id = started.acquisition_id().unwrap();

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let log = env.log(id).await;
    let states: Vec<AcquisitionState> = log.iter().map(|row| row.state).collect();
    assert_eq!(
        &states[..3],
        [
            AcquisitionState::Queued,
            AcquisitionState::Resolving,
            AcquisitionState::Searching
        ]
    );
    assert!(
        states.contains(&AcquisitionState::Downloading),
        "{states:?}"
    );
    assert!(states.contains(&AcquisitionState::Importing), "{states:?}");
    assert_eq!(states.last(), Some(&AcquisitionState::Completed));
    assert!(
        log.iter()
            .filter_map(|row| row.message.as_deref())
            .any(|message| message.contains("selected")),
        "the search result is logged: {log:?}"
    );
    assert_eq!(env.acquisition(id).await.attempts, 1);
}

#[tokio::test]
async fn fans_an_album_request_out_into_one_acquisition_per_track() {
    let env = Env::normal(&[]).await;
    let first = env.remote_track("First Song", "Hello Artist").await;
    let album = env.track(first).await.album_id.unwrap();
    let mut second = hearthsound_library::TrackInput::new(
        "Second Song",
        hearthsound_library::Origin::Catalog,
        Availability::Remote,
    );
    second.album_id = Some(album);
    second.display_artist = Some("Hello Artist".to_owned());
    second.track_number = Some(2);
    let second = env
        .library
        .insert_track(
            &second,
            &[hearthsound_library::ArtistCredit::new(
                "Hello Artist",
                "main",
            )],
            &[],
        )
        .await
        .unwrap();

    let outcomes = env
        .orchestrator
        .request_album(AlbumAcquireRequest {
            album_id: album,
            priority: Priority::Queue,
            reason: "save album".to_owned(),
            requested_by: None,
            can_acquire: true,
            force: false,
        })
        .await
        .unwrap();

    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].track_id, first);
    assert_eq!(outcomes[1].track_id, second);
    for outcome in &outcomes {
        let id = outcome.outcome.acquisition_id().unwrap();
        let row = env.acquisition(id).await;
        assert_eq!(row.reason, "save album");
        assert_eq!(row.priority, Priority::Queue);
        assert_eq!(row.state, AcquisitionState::Queued);
    }
    assert_eq!(env.acquisitions_for(first).await, 1);
    assert_eq!(env.acquisitions_for(second).await, 1);
}

#[tokio::test]
async fn waiters_observe_the_acquisition_at_every_attach_point() {
    let env = Env::normal(&[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let started = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    let handle = started.handle().unwrap();
    let id = handle.id;
    assert_eq!(handle.snapshot().state, AcquisitionState::Queued);
    assert!(!handle.snapshot().is_terminal());

    // A subscriber attached later receives the current snapshot, and the
    // channel follows the acquisition as the pipeline advances.
    let mut late = env.orchestrator.subscribe(id).unwrap();
    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;
    // The channel carries the same advance the ledger does, but it is written
    // after the row, so the newest value can still be an earlier state when
    // the ledger already reads completed; wait for the channel itself.
    let snapshot: AcquisitionProgress = tokio::time::timeout(
        crate::common::WAIT_TIMEOUT,
        late.wait_for(|progress| progress.state == AcquisitionState::Completed),
    )
    .await
    .expect("the channel carries the pipeline's advance")
    .unwrap()
    .clone();
    assert_eq!(snapshot.state, AcquisitionState::Completed);
    assert_eq!(snapshot.priority, Priority::Play);
    assert_eq!(snapshot.acquisition_id, id);
    assert_eq!(snapshot.target_id, track.to_string());
    assert_eq!(snapshot.failure_reason, None);
    assert!(snapshot.file.is_some());
    assert!(snapshot.succeeded());
    assert!(!snapshot.failed());

    // The ledger is the view a client without a subscription reads.
    let progress = env.orchestrator.progress_of(id).await.unwrap().unwrap();
    assert_eq!(progress.state, AcquisitionState::Completed);
    assert_eq!(progress.priority, Priority::Play);

    // A track the process did not admit has no channel and no row.
    assert!(env.orchestrator.subscribe(uuid::Uuid::now_v7()).is_none());
    assert!(
        env.orchestrator
            .progress_of(uuid::Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_failed_acquisition_cools_the_track_down_for_the_configured_window() {
    let env = Env::new(
        config(json!([]), "no-candidates", 0, "staging", ".flac"),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__UNAVAILABLE_COOLDOWN_HOURS", "1")],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let started = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    let id = started.acquisition_id().unwrap();

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::NoCandidates));

    // The failure marked the track unavailable for the cooldown, and left no
    // staging directory behind. The staging directory is removed after the
    // failed row is written, so it is waited for rather than asserted on sight.
    let stored = env.track(track).await;
    assert_eq!(stored.availability, Availability::Unavailable);
    let until = stored
        .unavailable_until
        .expect("the cooldown end is stored");
    assert!(
        until > now_millis() && until <= now_millis() + 3_600_000,
        "the cooldown end is inside the configured hour: {until}"
    );
    crate::common::wait_for_removal(
        &env.orchestrator.staging_dir(id),
        "the failed acquisition removed its staging directory",
    )
    .await;

    // A replay inside the window starts no new search.
    let refused = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user replay"))
        .await
        .unwrap();
    match refused {
        AcquireOutcome::CoolingDown {
            track_id,
            unavailable_until,
        } => {
            assert_eq!(track_id, track);
            assert_eq!(unavailable_until, until);
        }
        other => panic!("a cooling track must be refused, got {other:?}"),
    }
    assert_eq!(env.searches_started(crate::common::INSTANCE).await, 1);
    assert_eq!(env.acquisitions_for(track).await, 1);

    // A forced retry ignores the cooldown and searches again.
    let forced = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "admin retry").forced())
        .await
        .unwrap();
    assert!(
        matches!(forced, AcquireOutcome::Started(_)),
        "a forced request ignores the cooldown, got {forced:?}"
    );
    env.wait_for_terminal(forced.acquisition_id().unwrap())
        .await;
    assert_eq!(env.searches_started(crate::common::INSTANCE).await, 2);
}

#[tokio::test]
async fn allows_a_new_search_after_the_cooldown_elapsed() {
    let env = Env::normal(&[]).await;
    let track = env
        .add_track(
            "Hello Song",
            "Hello Artist",
            Availability::Unavailable,
            Some(now_millis() - 1),
        )
        .await;

    let outcome = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    assert!(
        matches!(outcome, AcquireOutcome::Started(_)),
        "a cooldown that elapsed no longer stops a search, got {outcome:?}"
    );
    let stored = env.track(track).await;
    assert_eq!(stored.availability, Availability::Acquiring);
    assert_eq!(stored.unavailable_until, None);
}

/// Returns the current time in unix milliseconds.
fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

/// Sets one user's acquisition quota.
///
/// # Parameters
///
/// - `env`: the environment whose database the row is written to.
/// - `user`: the account the quota belongs to.
/// - `quota`: the limits to store.
async fn set_quota(env: &Env, user: uuid::Uuid, quota: hearthsound_auth::UserQuota) {
    hearthsound_auth::Quotas::new(&env.database)
        .set(user, &quota)
        .await
        .unwrap();
}

/// Requests a track on a user's behalf.
///
/// # Parameters
///
/// - `env`: the environment to request in.
/// - `track`: the track to acquire.
/// - `user`: the account the request is made for.
async fn request_for(
    env: &Env,
    track: hearthsound_core::EntityId,
    user: uuid::Uuid,
) -> AcquireOutcome {
    env.orchestrator
        .request(
            AcquireRequest::new(track, Priority::Play, "user request")
                .with_requested_by(Some(user)),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn refuses_a_request_over_the_daily_count_quota() {
    let env = Env::normal(&[]).await;
    let user = env.add_user().await;
    set_quota(
        &env,
        user,
        hearthsound_auth::UserQuota {
            acquisitions_per_day: Some(1),
            acquisition_bytes_per_day: None,
        },
    )
    .await;
    let first = env.remote_track("Hello Song", "Hello Artist").await;
    let second = env.remote_track("Other Song", "Hello Artist").await;

    let outcome = request_for(&env, first, user).await;
    assert!(
        matches!(outcome, AcquireOutcome::Started(_)),
        "the first request is within the quota, got {outcome:?}"
    );

    let staging_before = env.staging.read_dir().unwrap().count();
    let denied = request_for(&env, second, user).await;
    match denied {
        AcquireOutcome::Denied { track_id, reason } => {
            assert_eq!(track_id, second);
            assert_eq!(reason, FailureReason::QuotaExceeded);
        }
        other => panic!("the second request is over the quota, got {other:?}"),
    }

    assert_eq!(env.acquisitions_for(second).await, 0, "nothing was written");
    assert_eq!(env.track(second).await.availability, Availability::Remote);
    assert_eq!(
        env.staging.read_dir().unwrap().count(),
        staging_before,
        "a denied request leaves no staging directory"
    );
    assert_eq!(
        env.searches_started(crate::common::INSTANCE).await,
        0,
        "no search was started for the denied track"
    );
}

#[tokio::test]
async fn joining_an_active_acquisition_is_not_counted() {
    let env = Env::normal(&[]).await;
    let user = env.add_user().await;
    set_quota(
        &env,
        user,
        hearthsound_auth::UserQuota {
            acquisitions_per_day: Some(1),
            acquisition_bytes_per_day: None,
        },
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let other = env.remote_track("Other Song", "Hello Artist").await;

    let started = request_for(&env, track, user).await;
    assert!(
        matches!(started, AcquireOutcome::Started(_)),
        "the first request is admitted, got {started:?}"
    );

    let joined = request_for(&env, track, user).await;
    assert!(
        matches!(joined, AcquireOutcome::Joined(_)),
        "attaching to a running acquisition costs the caller nothing, got {joined:?}"
    );
    assert_eq!(env.acquisitions_for(track).await, 1);

    let denied = request_for(&env, other, user).await;
    assert!(
        matches!(denied, AcquireOutcome::Denied { .. }),
        "the joining request did not spend the quota, got {denied:?}"
    );
}

#[tokio::test]
async fn refuses_a_request_over_the_daily_byte_quota() {
    let env = Env::normal(&[]).await;
    let user = env.add_user().await;
    set_quota(
        &env,
        user,
        hearthsound_auth::UserQuota {
            acquisitions_per_day: None,
            acquisition_bytes_per_day: Some(1_000_000),
        },
    )
    .await;
    let first = env.remote_track("Hello Song", "Hello Artist").await;
    let second = env.remote_track("Other Song", "Hello Artist").await;

    let started = request_for(&env, first, user).await;
    let AcquireOutcome::Started(handle) = started else {
        panic!("an acquisition with no file yet is within the byte quota, got {started:?}");
    };
    assert_eq!(env.acquisitions_for(first).await, 1);

    // The file an acquisition produces is what the byte quota counts, so the
    // row is written the way an import writes it.
    sqlx::query(
        "INSERT INTO media_files (id, track_id, storage, rel_path, size, mtime_ms, added_at,
                                  acquisition_id)
         VALUES (?, ?, 'library', 'Hello Artist/Hello Song.flac', 2000000, 0, 0, ?)",
    )
    .bind(uuid::Uuid::now_v7().as_bytes().as_slice())
    .bind(first.uuid().as_bytes().as_slice())
    .bind(handle.id.as_bytes().as_slice())
    .execute(env.database.writer())
    .await
    .unwrap();

    let denied = request_for(&env, second, user).await;
    match denied {
        AcquireOutcome::Denied { track_id, reason } => {
            assert_eq!(track_id, second);
            assert_eq!(reason, FailureReason::QuotaExceeded);
        }
        other => panic!("the produced file puts the user over the byte quota, got {other:?}"),
    }
    assert_eq!(env.acquisitions_for(second).await, 0);
}

#[tokio::test]
async fn a_user_without_a_quota_is_unlimited_and_the_window_moves() {
    let env = Env::normal(&[]).await;
    let user = env.add_user().await;
    let first = env.remote_track("Hello Song", "Hello Artist").await;
    let second = env.remote_track("Other Song", "Hello Artist").await;
    let third = env.remote_track("Third Song", "Hello Artist").await;

    for track in [first, second] {
        let outcome = request_for(&env, track, user).await;
        assert!(
            matches!(outcome, AcquireOutcome::Started(_)),
            "a user with no quota row is unlimited, got {outcome:?}"
        );
    }

    // One acquisition inside the window uses the single admitted request up,
    // and one started more than a day ago does not count any more.
    set_quota(
        &env,
        user,
        hearthsound_auth::UserQuota {
            acquisitions_per_day: Some(1),
            acquisition_bytes_per_day: None,
        },
    )
    .await;
    let denied = request_for(&env, third, user).await;
    assert!(
        matches!(denied, AcquireOutcome::Denied { .. }),
        "two acquisitions in the window are over a limit of one, got {denied:?}"
    );

    sqlx::query("UPDATE acquisitions SET created_at = ? WHERE requested_by = ?")
        .bind(now_millis() - 25 * 60 * 60 * 1000)
        .bind(user.as_bytes().as_slice())
        .execute(env.database.writer())
        .await
        .unwrap();
    let outcome = request_for(&env, third, user).await;
    assert!(
        matches!(outcome, AcquireOutcome::Started(_)),
        "an acquisition that aged out of the window is not counted, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_request_with_no_source_parked_waits_and_resumes_when_one_appears() {
    let env = Env::normal(&[]).await;
    env.clear_acquisition_layer().await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let started = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Queue, "user like"))
        .await
        .unwrap();
    let AcquireOutcome::Started(handle) = &started else {
        panic!("a request must start an acquisition, got {started:?}");
    };
    let id = handle.id;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Queued);
    assert_eq!(row.park_reason, Some(ParkReason::NoSource));
    assert_eq!(row.attempts, 0);
    assert_eq!(row.retry_at, None, "a missing source needs no timer");
    assert!(handle.snapshot().awaiting_source);
    assert_eq!(handle.snapshot().park_reason, Some(ParkReason::NoSource));
    assert_eq!(env.track(track).await.availability, Availability::Acquiring);
    assert!(
        env.track(track).await.unavailable_until.is_none(),
        "a missing source must not cool the song down"
    );
    assert!(
        env.log(id)
            .await
            .iter()
            .all(|entry| entry.state != AcquisitionState::Failed),
        "no log line may say the acquisition failed"
    );

    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        env.acquisition(id).await.attempts,
        0,
        "nothing may be attempted without a source"
    );

    env.set_acquisition_layer(&[INSTANCE]).await;
    env.orchestrator.providers_changed().await.unwrap();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert!(row.park_reason.is_none());
    assert!(env.track(track).await.unavailable_until.is_none());
}

#[tokio::test]
async fn a_source_outage_parks_instead_of_failing() {
    let fixture = config(one_candidate(), "unavailable", 0, "downloads", ".flac");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;

    let started = env
        .orchestrator
        .request(AcquireRequest::new(
            track,
            Priority::Play,
            "user pressed play",
        ))
        .await
        .unwrap();
    let AcquireOutcome::Started(handle) = &started else {
        panic!("a request must start an acquisition, got {started:?}");
    };
    let id = handle.id;

    // Written before the scheduler exists, so the park is what removes it.
    let marker = env.orchestrator.staging_dir(id).join("leftover.part");
    tokio::fs::write(&marker, b"a candidate's leftovers")
        .await
        .unwrap();
    env.start();

    let row = env.wait_for_park(id).await;
    assert_eq!(row.state, AcquisitionState::Queued);
    assert_eq!(row.park_reason, Some(ParkReason::SourceUnavailable));
    assert_eq!(row.attempts, 0);
    let now = now_millis();
    let retry_at = row.retry_at.expect("a source outage schedules a retry");
    assert!(
        retry_at > now + 50_000 && retry_at <= now + 130_000,
        "the first retry is about a minute away, was {} ms",
        retry_at - now
    );
    assert_eq!(env.track(track).await.availability, Availability::Acquiring);
    assert!(env.track(track).await.unavailable_until.is_none());
    assert!(
        env.log(id)
            .await
            .iter()
            .all(|entry| entry.state != AcquisitionState::Failed)
    );
    assert!(
        !marker.exists(),
        "a park removes what the candidate left in staging"
    );
    assert!(
        env.orchestrator.staging_dir(id).is_dir(),
        "the staging directory itself stays for the next attempt"
    );
    let progress = env.orchestrator.progress_of(id).await.unwrap().unwrap();
    assert!(progress.awaiting_source);
    assert_eq!(progress.park_reason, Some(ParkReason::SourceUnavailable));
}

#[tokio::test]
async fn a_parked_acquisition_recovers_when_the_source_answers() {
    let fixture = config(one_candidate(), "unavailable", 0, "downloads", ".flac");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();
    let id = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Queue, "user like"))
        .await
        .unwrap()
        .acquisition_id()
        .unwrap();
    env.wait_for_park(id).await;

    env.place_download("01 - Hello Song.flac").await;
    let healthy = config(one_candidate(), "normal", 0, "downloads", ".flac");
    env.add_instance("healthy", healthy).await;
    env.orchestrator.providers_changed().await.unwrap();

    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert!(env.track(track).await.unavailable_until.is_none());
}

#[tokio::test]
async fn the_park_retry_timer_alone_wakes_a_parked_row() {
    let fixture = config(one_candidate(), "unavailable", 0, "downloads", ".flac");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();
    let id = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Queue, "user like"))
        .await
        .unwrap()
        .acquisition_id()
        .unwrap();
    let row = env.wait_for_park(id).await;
    assert_eq!(row.park_reason, Some(ParkReason::SourceUnavailable));

    // A source appears without a registry change being announced: only the
    // parked row's own timer brings the acquisition back.
    env.place_download("01 - Hello Song.flac").await;
    let healthy = config(one_candidate(), "normal", 0, "downloads", ".flac");
    env.add_instance("healthy", healthy).await;
    env.make_parked_due(id).await;

    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
}

#[tokio::test]
async fn a_song_specific_failure_still_cools_down() {
    let fixture = config(one_candidate(), "no-candidates", 0, "downloads", ".flac");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();
    let id = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Queue, "user like"))
        .await
        .unwrap()
        .acquisition_id()
        .unwrap();

    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::NoCandidates));
    let until = env
        .track(track)
        .await
        .unavailable_until
        .expect("a song-specific failure cools the track down");
    assert!(until > now_millis() + 5 * 3_600_000);

    let again = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Queue, "user like"))
        .await
        .unwrap();
    assert!(
        matches!(again, AcquireOutcome::CoolingDown { .. }),
        "a cooled-down track must not be searched again, got {again:?}"
    );
}

#[tokio::test]
async fn a_parked_album_job_creates_no_per_track_rows() {
    let folders = json!([album_folder(
        "Album",
        &[("01 - Song One.flac", "flac", None)]
    )]);
    let mut fixture = album_instance_config(folders);
    fixture["scenario"] = json!("unavailable");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let (album, tracks) = env.add_album("Album", "Hello Artist", 2).await;
    env.start();

    let outcomes = env
        .orchestrator
        .request_album(AlbumAcquireRequest {
            album_id: album,
            priority: Priority::Queue,
            reason: "album like".to_owned(),
            requested_by: None,
            can_acquire: true,
            force: false,
        })
        .await
        .unwrap();
    let id = outcomes.first().unwrap().outcome.acquisition_id().unwrap();
    let row = env.wait_for_park(id).await;
    assert_eq!(row.granularity, hearthsound_core::Granularity::Album);
    assert_eq!(row.park_reason, Some(ParkReason::SourceUnavailable));
    for track in &tracks {
        assert_eq!(
            env.acquisitions_for(*track).await,
            0,
            "a parked album job must not hand its tracks to per-track acquisitions"
        );
    }
}
