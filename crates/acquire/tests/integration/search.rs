//! The search stage: instance order, budgets, scoring, and the failure path.
//!
//! The fixture's searches and downloads are counted in the host's key-value
//! store, so a test can assert exactly how many searches an instance was asked
//! for, which is what makes the slot and rate budgets observable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use crate::common::{
    Env, INSTANCE, InstanceLimits, candidate, config, drain, has_acquisition_event,
    has_availability_event, one_candidate,
};
use hearthsound_acquire::orchestrator::{AcquireOutcome, AcquireRequest};
use hearthsound_core::{AcquisitionState, Availability, EntityId, FailureReason, Priority};
use serde_json::json;
use uuid::Uuid;

/// Admits one acquisition and returns its id.
async fn request(env: &Env, track: EntityId, priority: Priority) -> Uuid {
    let outcome = env
        .orchestrator
        .request(AcquireRequest::new(track, priority, "test request"))
        .await
        .unwrap();
    outcome
        .acquisition_id()
        .expect("the request must be admitted")
}

/// Waits until the search stage has logged its result for an acquisition.
///
/// # Panics
///
/// Panics when the log entry does not appear within [`crate::common::WAIT_TIMEOUT`].
async fn wait_for_search_result(env: &Env, id: Uuid) -> String {
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    loop {
        let message = env
            .log(id)
            .await
            .into_iter()
            .filter_map(|entry| entry.message)
            .find(|message| message.contains("selected"));
        if let Some(message) = message {
            return message;
        }
        assert!(
            Instant::now() < deadline,
            "acquisition {id} never logged a search result"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
}

/// Returns the current time in unix milliseconds.
fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}

#[tokio::test]
async fn a_search_that_finds_nothing_fails_the_acquisition_into_cooldown() {
    let env = Env::new(
        config(json!([]), "no-candidates", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let mut events = env.events.subscribe();
    let started = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "user request"))
        .await
        .unwrap();
    let handle = started.handle().unwrap().clone();
    let id = handle.id;
    let before = now_millis();

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::NoCandidates));
    assert_eq!(env.searches_started(INSTANCE).await, 1);

    let log = env.log(id).await;
    let states: Vec<AcquisitionState> = log.iter().map(|entry| entry.state).collect();
    assert_eq!(
        states,
        [
            AcquisitionState::Queued,
            AcquisitionState::Resolving,
            AcquisitionState::Searching,
            AcquisitionState::Failed
        ]
    );

    let track_row = env.track(track).await;
    assert_eq!(track_row.availability, Availability::Unavailable);
    let until = track_row.unavailable_until.unwrap();
    let cooldown_ms = 6 * 3_600_000;
    assert!(
        until >= before + cooldown_ms && until <= now_millis() + cooldown_ms,
        "the cooldown end {until} is not six hours after the failure"
    );

    let published = drain(&mut events);
    assert!(has_acquisition_event(
        &published,
        id,
        AcquisitionState::Failed
    ));
    assert!(has_availability_event(
        &published,
        track,
        Availability::Unavailable
    ));

    let terminal = handle.wait().await.unwrap();
    assert_eq!(terminal.state, AcquisitionState::Failed);
    assert_eq!(terminal.failure_reason, Some(FailureReason::NoCandidates));
    assert!(terminal.failed());
    assert!(!terminal.succeeded());

    // The cooldown is what keeps a track nobody can find from hitting the
    // backends on every client retry.
    let refused = env
        .orchestrator
        .request(AcquireRequest::new(track, Priority::Play, "retry"))
        .await
        .unwrap();
    assert!(
        matches!(refused, AcquireOutcome::CoolingDown { .. }),
        "a failed track must cool down, got {refused:?}"
    );
    assert_eq!(env.searches_started(INSTANCE).await, 1);
}

#[tokio::test]
async fn candidates_below_the_profile_fail_as_no_acceptable_quality() {
    let env = Env::new(
        config(
            json!([candidate("cand-1", "mp3", Some(64))]),
            "normal",
            0,
            "downloads",
            ".flac",
        ),
        InstanceLimits::default(),
        &[],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await;

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::NoAcceptableQuality));
    assert_eq!(
        env.track(track).await.availability,
        Availability::Unavailable
    );
}

#[tokio::test]
async fn an_interactive_search_stops_at_the_first_instance_with_a_candidate() {
    let env = Env::normal(&[]).await;
    env.add_instance(
        "slow",
        config(one_candidate(), "normal", 1, "downloads", ".flac"),
    )
    .await;

    // A user is waiting: the first instance's candidate is enough.
    let play_track = env.remote_track("Hello Song", "Hello Artist").await;
    let play_id = request(&env, play_track, Priority::Play).await;
    env.start();
    let message = wait_for_search_result(&env, play_id).await;
    assert!(message.contains("selected 1 candidate"), "{message}");
    assert_eq!(
        env.searches_started("slow").await,
        0,
        "an interactive search must not walk the rest of the layer"
    );

    // Bulk work lets every instance finish, to maximize the choice of quality.
    let queue_track = env.remote_track("Other Song", "Hello Artist").await;
    let queue_id = request(&env, queue_track, Priority::Queue).await;
    wait_for_search_result(&env, queue_id).await;
    assert_eq!(env.searches_started("slow").await, 1);
    assert_eq!(env.searches_started(INSTANCE).await, 2);
}

#[tokio::test]
async fn a_search_slot_bounds_the_concurrent_searches_of_an_instance() {
    // Twenty polls 50 ms apart: each search runs for about a second, longer than
    // the window the test watches for a second search.
    let env = Env::new(
        config(json!([]), "no-candidates", 20, "downloads", ".flac"),
        InstanceLimits {
            max_concurrent_searches: 1,
            ..InstanceLimits::default()
        },
        &[],
    )
    .await;
    let first = env.remote_track("First Song", "Hello Artist").await;
    let second = env.remote_track("Second Song", "Hello Artist").await;
    let first_id = request(&env, first, Priority::Play).await;
    let second_id = request(&env, second, Priority::Play).await;

    env.start();
    // Both acquisitions are being worked on, but the instance runs one search.
    env.wait_for_state(first_id, AcquisitionState::Searching)
        .await;
    env.wait_for_state(second_id, AcquisitionState::Searching)
        .await;
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.searches_started(INSTANCE).await < 1 {
        assert!(
            Instant::now() < deadline,
            "the instance never started a search"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    assert_eq!(env.searches_started(INSTANCE).await, 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        env.searches_started(INSTANCE).await,
        1,
        "the instance must run one search at a time"
    );

    assert_eq!(
        env.wait_for_terminal(first_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    assert_eq!(
        env.wait_for_terminal(second_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    assert_eq!(
        env.searches_started(INSTANCE).await,
        2,
        "the waiting search runs once the slot is free"
    );
}

#[tokio::test]
async fn the_hourly_budget_throttles_bulk_searches_but_not_interactive_ones() {
    let env = Env::new(
        config(json!([]), "no-candidates", 0, "downloads", ".flac"),
        InstanceLimits {
            searches_per_hour: 1,
            ..InstanceLimits::default()
        },
        &[],
    )
    .await;

    // The first bulk search spends the instance's only token for the hour.
    let first = env.remote_track("First Song", "Hello Artist").await;
    let first_id = request(&env, first, Priority::Queue).await;
    env.start();
    assert_eq!(
        env.wait_for_terminal(first_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    assert_eq!(env.searches_started(INSTANCE).await, 1);

    // The second one waits for a token that is an hour away.
    let second = env.remote_track("Second Song", "Hello Artist").await;
    let second_id = request(&env, second, Priority::Queue).await;
    env.wait_for_state(second_id, AcquisitionState::Searching)
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        env.acquisition(second_id).await.state,
        AcquisitionState::Searching
    );
    assert_eq!(
        env.searches_started(INSTANCE).await,
        1,
        "a throttled bulk search must not reach the backend"
    );

    // An interactive search is limited by the search slot alone.
    let third = env.remote_track("Third Song", "Hello Artist").await;
    let third_id = request(&env, third, Priority::Play).await;
    assert_eq!(
        env.wait_for_terminal(third_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    assert_eq!(env.searches_started(INSTANCE).await, 2);
}

#[tokio::test]
async fn the_global_concurrency_bound_serves_the_most_urgent_acquisition_first() {
    // Twenty polls 50 ms apart keep the play request's search running for
    // about a second, longer than the window the test watches the queue.
    let env = Env::new(
        config(json!([]), "no-candidates", 20, "downloads", ".flac"),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__MAX_CONCURRENT", "1")],
    )
    .await;
    let background = env.remote_track("Background Song", "Hello Artist").await;
    let play = env.remote_track("Play Song", "Hello Artist").await;
    let background_id = request(&env, background, Priority::Background).await;
    let play_id = request(&env, play, Priority::Play).await;

    env.start();
    env.wait_for_state(play_id, AcquisitionState::Searching)
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        env.acquisition(play_id).await.state,
        AcquisitionState::Searching,
        "the play request takes the only slot"
    );
    assert_eq!(
        env.acquisition(background_id).await.state,
        AcquisitionState::Queued,
        "a background run waits behind the play request"
    );
    assert_eq!(env.orchestrator.running(), 1);

    assert_eq!(
        env.wait_for_terminal(play_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    // The freed slot goes to the acquisition that was waiting.
    env.wait_for_state(background_id, AcquisitionState::Searching)
        .await;
    assert_eq!(
        env.wait_for_terminal(background_id).await.failure_reason,
        Some(FailureReason::NoCandidates)
    );
    assert_eq!(env.searches_started(INSTANCE).await, 2);
}
