//! The download stage: polling, stalls, queue timeouts, lost tokens, and the
//! handoff of stable files to verification.
//!
//! The fixture's downloads are counted in the host's key-value store and its
//! cancelled downloads are deleted from it, so a test can assert which
//! downloads ran and which were abandoned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::common::{Env, INSTANCE, InstanceLimits, candidate, config, one_candidate};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle};
use hearthsound_core::{AcquisitionState, Availability, EntityId, FailureReason, Priority};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use serde_json::json;
use uuid::Uuid;

/// Admits one acquisition and returns its handle.
async fn request(env: &Env, track: EntityId, priority: Priority) -> AcquisitionHandle {
    let outcome = env
        .orchestrator
        .request(AcquireRequest::new(track, priority, "test request"))
        .await
        .unwrap();
    outcome
        .handle()
        .expect("the request must be admitted")
        .clone()
}

/// Writes a generated audio file into the instance's import root.
///
/// The fixture reports every file with the configured suffix it finds there when
/// a download completes, which is how a simulated backend produces its output.
///
/// # Parameters
///
/// - `env`: the environment whose import root receives the file.
/// - `name`: the file name.
async fn place_download(env: &Env, name: &str) -> PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.2)
        .file_name(name.to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap()
}

/// Forgets every download the instance has in flight.
///
/// This is what a backend restart looks like from the plugin's side: the token
/// it issued is gone, so polling it reports `not-found`.
async fn forget_downloads(env: &Env) {
    let instance_id = env
        .registry
        .instance(INSTANCE)
        .unwrap()
        .id()
        .as_bytes()
        .to_vec();
    sqlx::query!(
        "DELETE FROM plugin_kv WHERE instance_id = ? AND key LIKE 'download:%'",
        instance_id,
    )
    .execute(env.database.writer())
    .await
    .unwrap();
}

/// Counts the downloads the instance still holds.
async fn pending_downloads(env: &Env) -> i64 {
    let instance_id = env
        .registry
        .instance(INSTANCE)
        .unwrap()
        .id()
        .as_bytes()
        .to_vec();
    sqlx::query_scalar!(
        "SELECT COUNT(*) FROM plugin_kv WHERE instance_id = ? AND key LIKE 'download:%'",
        instance_id,
    )
    .fetch_one(env.database.readers())
    .await
    .unwrap()
}

/// Returns whether an acquisition's log mentions `needle`.
async fn logged(env: &Env, id: Uuid, needle: &str) -> bool {
    env.log(id)
        .await
        .into_iter()
        .filter_map(|entry| entry.message)
        .any(|message| message.contains(needle))
}

#[tokio::test]
async fn a_completed_download_is_verified_and_imported() {
    let mut server_config = config(one_candidate(), "normal", 0, "downloads", ".flac");
    // Two running polls of half the file each, so the percentage the events
    // carry has to advance.
    server_config["downloadRunningPolls"] = json!(2);
    server_config["downloadBytesPerPoll"] = json!(2000);
    let env = Env::new(server_config, InstanceLimits::default(), &[]).await;
    let file = place_download(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let mut events = env.events.subscribe();
    let handle = request(&env, track, Priority::Play).await;
    let id = handle.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert_eq!(
        row.instance_id,
        Some(env.registry.instance(INSTANCE).unwrap().id())
    );
    assert_eq!(row.plugin_token.as_deref(), Some("download-1"));
    assert_eq!(row.attempts, 1);
    assert_eq!(row.failure_reason, None);
    assert!(row.media_file_id.is_some());
    let candidate: serde_json::Value =
        serde_json::from_str(row.candidate.as_deref().unwrap()).unwrap();
    assert_eq!(candidate["token"], json!("cand-1"));
    assert_eq!(
        candidate["path"],
        json!("Hello Artist/Hello Song album/01 - Hello Song.flac")
    );
    let progress: serde_json::Value =
        serde_json::from_str(row.progress.as_deref().unwrap()).unwrap();
    assert_eq!(progress["bytes"], json!(4000));
    assert_eq!(progress["totalBytes"], json!(4000));

    // The live channel carries the final managed path a client waiting for
    // audio reads, and the source file left the import root. Both follow the
    // completed row, so they are waited for rather than read on sight.
    let destination = env
        .cache_root
        .join("Hello Artist/Hello Song album (1993)/01 - Hello Song.flac");
    crate::common::wait_until("the live channel carries the completed acquisition", || {
        handle.snapshot().state == AcquisitionState::Completed
    })
    .await;
    let live = handle.snapshot();
    assert_eq!(live.state, AcquisitionState::Completed);
    assert_eq!(live.file.as_deref(), Some(destination.as_path()));
    assert!(destination.is_file());
    crate::common::wait_for_removal(&file, "a move import root drops the source file").await;

    // The ledger carries the state, the instance and the last percentage; the
    // resolved path is not stored.
    let snapshot = env.orchestrator.progress_of(id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, AcquisitionState::Completed);
    assert_eq!(snapshot.instance.as_deref(), Some(INSTANCE));
    assert_eq!(snapshot.percent, Some(100));
    assert_eq!(snapshot.file, None);

    let log = env.log(id).await;
    let states: Vec<AcquisitionState> = log.iter().map(|entry| entry.state).collect();
    assert!(
        states.contains(&AcquisitionState::Downloading),
        "{states:?}"
    );
    assert!(states.contains(&AcquisitionState::Verifying), "{states:?}");
    assert!(states.contains(&AcquisitionState::Importing), "{states:?}");
    assert_eq!(states.last(), Some(&AcquisitionState::Completed));
    assert!(
        logged(
            &env,
            id,
            "downloading `Hello Artist/Hello Song album/01 - Hello Song.flac`"
        )
        .await
    );
    assert!(logged(&env, id, "the candidate passed verification").await);

    // The percentage advances through the download. Percentage-only changes
    // inside one announce window are memoized, so the 100 percent reading of
    // the download may not reach the bus as its own frame; the terminal
    // transition always does.
    let published = crate::common::drain(&mut events);
    let percents: Vec<u8> = published
        .iter()
        .filter_map(|event| match event {
            hearthsound_core::events::ServerEvent::AcquisitionUpdated(update)
                if update.state == AcquisitionState::Downloading =>
            {
                update.percent
            }
            _ => None,
        })
        .collect();
    assert_eq!(percents.first(), Some(&50));
    assert!(percents.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(published.iter().any(|event| matches!(
        event,
        hearthsound_core::events::ServerEvent::AcquisitionUpdated(update)
            if update.state == AcquisitionState::Completed
    )));
}

#[tokio::test]
async fn a_stalled_candidate_is_abandoned_and_the_next_one_is_tried() {
    let env = Env::new(
        config(
            json!([
                candidate("cand-1", "flac", Some(900)),
                candidate("cand-2", "flac", Some(900)),
            ]),
            "stalled",
            0,
            "downloads",
            ".flac",
        ),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__STALL_TIMEOUT_SECS", "1")],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::Timeout));
    assert_eq!(row.attempts, 2, "both candidates were attempted");
    assert_eq!(env.downloads_started(INSTANCE).await, 2);
    assert!(logged(&env, id, "the transfer stalled").await);
    assert_eq!(
        env.track(track).await.availability,
        Availability::Unavailable
    );
    assert_eq!(
        pending_downloads(&env).await,
        0,
        "a stalled download is cancelled at the backend"
    );
}

#[tokio::test]
async fn a_download_that_sits_queued_is_abandoned() {
    let mut server_config = config(one_candidate(), "normal", 0, "downloads", ".flac");
    // A hundred queued polls 50 ms apart outlast the one second queue timeout.
    server_config["downloadQueuedPolls"] = json!(100);
    let env = Env::new(
        server_config,
        InstanceLimits::default(),
        &[(
            "HEARTHSOUND__ACQUISITION__QUEUE_TIMEOUT_SECS__INTERACTIVE",
            "1",
        )],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::Timeout));
    assert_eq!(row.attempts, 1);
    assert!(logged(&env, id, "the transfer stayed queued").await);
    assert_eq!(pending_downloads(&env).await, 0);
}

#[tokio::test]
async fn a_lost_download_token_restarts_the_search() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    // Wait until the download has started, then make the backend forget it, as
    // a backend restart would.
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.downloads_started(INSTANCE).await < 1 {
        assert!(
            Instant::now() < deadline,
            "the instance never started a download"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
    forget_downloads(&env).await;

    env.wait_for_state(id, AcquisitionState::Completed).await;
    assert_eq!(
        env.searches_started(INSTANCE).await,
        2,
        "the search ran again"
    );
    assert_eq!(env.downloads_started(INSTANCE).await, 2);
    let states: Vec<AcquisitionState> = env
        .log(id)
        .await
        .into_iter()
        .map(|entry| entry.state)
        .collect();
    assert!(
        states
            .windows(2)
            .any(|pair| pair == [AcquisitionState::Downloading, AcquisitionState::Searching]),
        "the retry edge is logged: {states:?}"
    );
    assert!(logged(&env, id, "the backend lost the download token").await);
    assert_eq!(
        env.acquisition(id).await.attempts,
        2,
        "the restarted search started a second download"
    );
}

#[tokio::test]
async fn an_interactive_request_preempts_a_bulk_download() {
    let env = Env::with_import_mode(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits {
            max_concurrent: 1,
            ..InstanceLimits::default()
        },
        &[],
        // Both acquisitions import the same simulated download, so the root
        // keeps it instead of the first import moving it away.
        "copy",
        false,
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;

    // A background run takes the instance's only download slot and sits in a
    // long poll.
    let bulk_track = env.remote_track("Hello Song", "Hello Artist").await;
    let bulk_id = request(&env, bulk_track, Priority::Background).await.id;
    env.start();
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.downloads_started(INSTANCE).await < 1 {
        assert!(
            Instant::now() < deadline,
            "the instance never started a download"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
    assert_eq!(
        env.acquisition(bulk_id).await.state,
        AcquisitionState::Downloading
    );

    // A play request needs the slot the background run holds.
    let play_track = env.remote_track("Hello Song", "Hello Artist").await;
    let play_id = request(&env, play_track, Priority::Play).await.id;
    env.wait_for_state(play_id, AcquisitionState::Completed)
        .await;
    // The preempted run is queued again and restarted by the scheduler, which
    // is a separate step from the interactive download finishing.
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.downloads_started(INSTANCE).await < 3 {
        assert!(
            Instant::now() < deadline,
            "the preempted download was not started again"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    assert_eq!(
        env.downloads_started(INSTANCE).await,
        3,
        "the preempted download was started again"
    );

    // The preempted acquisition is not lost: it downloads again and completes
    // too.
    env.wait_for_state(bulk_id, AcquisitionState::Completed)
        .await;
    assert!(logged(&env, bulk_id, "preempted by an interactive request").await);
    let bulk = env.acquisition(bulk_id).await;
    assert_eq!(bulk.state, AcquisitionState::Completed);
    assert_eq!(bulk.failure_reason, None);
}

#[tokio::test]
async fn a_trickling_transfer_is_abandoned_at_its_deadline() {
    let trickle = config(one_candidate(), "trickle", 0, "downloads", ".flac");
    let env = Env::new(
        trickle,
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__ATTEMPT_TIMEOUT_SECS", "1")],
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;
    let healthy = config(one_candidate(), "normal", 0, "downloads", ".flac");
    env.add_instance("healthy", healthy).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    // Bulk work searches every instance, so the plan holds the trickling
    // candidate first and the healthy one behind it.
    let handle = request(&env, track, Priority::Background).await;
    let row = env.wait_for_terminal(handle.id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert!(
        env.cancels_recorded(INSTANCE).await >= 1,
        "the trickling candidate's transfer must be cancelled"
    );
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
}

#[tokio::test]
async fn the_attempt_deadline_cools_down_when_every_candidate_trickles() {
    let trickle = config(one_candidate(), "trickle", 0, "downloads", ".flac");
    let env = Env::new(
        trickle,
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__ATTEMPT_TIMEOUT_SECS", "1")],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    let row = env.wait_for_terminal(handle.id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::Timeout));
    assert!(
        row.error_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("ran past its deadline")),
        "the detail names the deadline, was {:?}",
        row.error_detail
    );
    assert!(
        env.track(track).await.unavailable_until.is_some(),
        "a deadline that spent every candidate is a song-specific failure"
    );
}

#[tokio::test]
async fn zero_disables_the_attempt_deadline() {
    let trickle = config(one_candidate(), "trickle", 0, "downloads", ".flac");
    let env = Env::new(
        trickle,
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__ATTEMPT_TIMEOUT_SECS", "0")],
    )
    .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    let row = env.acquisition(handle.id).await;
    assert_eq!(
        row.state,
        AcquisitionState::Downloading,
        "with no limit the transfer keeps running"
    );
    assert!(row.plugin_token.is_some());
    assert_eq!(env.cancels_recorded(INSTANCE).await, 0);
}

#[tokio::test]
async fn a_transient_poll_error_is_retried() {
    let mut fixture = config(one_candidate(), "normal", 0, "downloads", ".flac");
    fixture["pollFailures"] = json!(2);
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    place_download(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    let row = env.wait_for_terminal(handle.id).await;
    assert_eq!(
        row.state,
        AcquisitionState::Completed,
        "two failed polls must not throw the transfer away"
    );
}

#[tokio::test]
async fn every_poll_failing_abandons_the_candidate_and_parks() {
    let mut fixture = config(one_candidate(), "normal", 0, "downloads", ".flac");
    fixture["pollFailures"] = json!(10);
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    let row = env.wait_for_park(handle.id).await;
    assert_eq!(row.state, AcquisitionState::Queued);
    assert_eq!(
        row.park_reason,
        Some(hearthsound_core::ParkReason::SourceUnavailable)
    );
    assert!(
        env.cancels_recorded(INSTANCE).await >= 1,
        "the abandoned candidate's transfer must be cancelled"
    );
    assert!(
        env.track(track).await.unavailable_until.is_none(),
        "a backend that stops answering does not cool the song down"
    );
}

#[tokio::test]
async fn a_failed_download_poll_cancels_the_candidate_transfers() {
    let fixture = config(one_candidate(), "download-failure", 0, "downloads", ".flac");
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    let row = env.wait_for_terminal(handle.id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert!(
        env.cancels_recorded(INSTANCE).await >= 1,
        "a backend that reported a failure is told to drop the transfer"
    );
}

#[tokio::test]
async fn a_lost_token_cancels_the_candidate_before_searching_again() {
    let mut fixture = config(one_candidate(), "normal", 0, "downloads", ".flac");
    fixture["lostToken"] = json!(true);
    let env = Env::new(fixture, InstanceLimits::default(), &[]).await;
    place_download(&env, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    env.start();

    let handle = request(&env, track, Priority::Play).await;
    let row = env.wait_for_terminal(handle.id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert!(
        env.cancels_recorded(INSTANCE).await >= 1,
        "a lost token's transfer is cancelled before the search restarts"
    );
    assert!(env.downloads_started(INSTANCE).await >= 2);
}

/// Changes an instance's `maxConcurrent` and rebuilds the registry, which is what
/// an administrator's edit does.
///
/// # Parameters
///
/// - `env`: the environment whose instance changes.
/// - `max_concurrent`: the new download limit.
async fn set_max_concurrent(env: &Env, max_concurrent: u32) {
    use hearthsound_plugin_host::instances::{InstanceService, InstanceSpec};

    let service = InstanceService::new(
        std::sync::Arc::clone(&env.host),
        env.repository.clone(),
        env.secrets.clone(),
    );
    let view = service.get(INSTANCE).await.unwrap();
    service
        .update(
            INSTANCE,
            &InstanceSpec {
                slug: INSTANCE.to_owned(),
                plugin_id: view.plugin_id.clone(),
                display_name: view.display_name.clone(),
                server_config: view.server_config.clone(),
                host_config: crate::common::host_config(
                    &env.downloads,
                    InstanceLimits {
                        max_concurrent,
                        ..InstanceLimits::default()
                    },
                ),
            },
        )
        .await
        .unwrap();
    env.registry.rebuild().await.unwrap();
}

/// Waits until an acquisition holds a started download attempt.
///
/// The ledger's `downloading` state says the pipeline chose a candidate, not that
/// the attempt holds one of the instance's slots: a row waits for a slot while it
/// is already in that stage. The recorded token is what proves the attempt
/// started.
///
/// # Parameters
///
/// - `env`: the environment whose ledger is read.
/// - `id`: the acquisition to wait for.
async fn wait_for_attempt(env: &Env, id: Uuid) {
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.acquisition(id).await.plugin_token.is_none() {
        assert!(
            Instant::now() < deadline,
            "acquisition {id} never started an attempt"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
}

/// Returns whether an acquisition holds a started download attempt.
///
/// # Parameters
///
/// - `env`: the environment whose ledger is read.
/// - `id`: the acquisition to check.
async fn has_attempt(env: &Env, id: Uuid) -> bool {
    env.acquisition(id).await.plugin_token.is_some()
}

/// Waits until one of two acquisitions holds a started download attempt.
///
/// # Parameters
///
/// - `env`: the environment whose ledger is read.
/// - `ids`: the acquisitions to wait for.
///
/// # Returns
///
/// The acquisition that started.
async fn wait_for_either_attempt(env: &Env, ids: [Uuid; 2]) -> Uuid {
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    loop {
        for id in ids {
            if has_attempt(env, id).await {
                return id;
            }
        }
        assert!(
            Instant::now() < deadline,
            "neither acquisition of {ids:?} started an attempt"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
}

/// Two play requests issued together preempt two *different* bulk downloads.
///
/// A preempted attempt keeps its registration until it notices, so without a
/// per-attempt mark the second interactive request would pick the same victim
/// again and the other bulk download would keep its slot for ever.
#[tokio::test]
async fn two_play_requests_preempt_two_different_bulk_downloads() {
    let mut server = config(one_candidate(), "normal", 0, "downloads", ".flac");
    server["downloadRunningPolls"] = json!(40);
    let env = Env::with_import_mode(
        server,
        InstanceLimits {
            max_concurrent: 2,
            ..InstanceLimits::default()
        },
        &crate::common::FAST_POLLING,
        // Every acquisition reads the same simulated download.
        "copy",
        false,
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;

    // Two bulk runs hold both slots.
    let first_bulk = env.remote_track("Hello Song", "Hello Artist").await;
    let second_bulk = env.remote_track("Hello Song", "Hello Artist").await;
    let first_id = request(&env, first_bulk, Priority::Background).await.id;
    let second_id = request(&env, second_bulk, Priority::Background).await.id;
    env.start();
    wait_for_attempt(&env, first_id).await;
    wait_for_attempt(&env, second_id).await;

    // Two users press play together, and both need a slot.
    let first_play = env.remote_track("Hello Song", "Hello Artist").await;
    let second_play = env.remote_track("Hello Song", "Hello Artist").await;
    let first_play_id = request(&env, first_play, Priority::Play).await.id;
    let second_play_id = request(&env, second_play, Priority::Play).await.id;

    // Both bulk attempts are cancelled, each one costing one interruption: the
    // mark is what stops the second request from picking the first one again.
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while env.cancels_recorded(INSTANCE).await < 2 {
        assert!(
            Instant::now() < deadline,
            "only {} attempt(s) were cancelled for two preemptions",
            env.cancels_recorded(INSTANCE).await
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    assert!(
        logged(&env, first_id, "preempted by an interactive request").await,
        "the first bulk download was preempted"
    );
    assert!(
        logged(&env, second_id, "preempted by an interactive request").await,
        "the second bulk download was preempted too, instead of the first one again"
    );

    // Both plays complete, so the preemption handed each of them a slot.
    env.wait_for_state(first_play_id, AcquisitionState::Completed)
        .await;
    env.wait_for_state(second_play_id, AcquisitionState::Completed)
        .await;
}

/// A raised download limit takes effect at once; a lowered one never lets the
/// running attempts exceed it.
#[tokio::test]
async fn changing_the_slot_limit_mid_flight_takes_effect_at_once() {
    let mut server = config(one_candidate(), "normal", 0, "downloads", ".flac");
    server["downloadRunningPolls"] = json!(100);
    let env = Env::with_import_mode(
        server,
        InstanceLimits {
            max_concurrent: 1,
            ..InstanceLimits::default()
        },
        &crate::common::FAST_POLLING,
        "copy",
        false,
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;

    // One download holds the only slot; two more wait for it.
    let running = env.remote_track("Hello Song", "Hello Artist").await;
    let running_id = request(&env, running, Priority::Queue).await.id;
    env.start();
    wait_for_attempt(&env, running_id).await;
    let waiting_one = env.remote_track("Hello Song", "Hello Artist").await;
    let waiting_two = env.remote_track("Hello Song", "Hello Artist").await;
    let waiting_one_id = request(&env, waiting_one, Priority::Queue).await.id;
    let waiting_two_id = request(&env, waiting_two, Priority::Queue).await.id;
    env.wait_for_state(waiting_two_id, AcquisitionState::Downloading)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !has_attempt(&env, waiting_one_id).await && !has_attempt(&env, waiting_two_id).await,
        "a single slot serves one download"
    );

    // Raising the limit wakes a waiter immediately, while the first attempt is
    // still running, and the new limit still bounds the concurrency.
    set_max_concurrent(&env, 2).await;
    let started = wait_for_either_attempt(&env, [waiting_one_id, waiting_two_id]).await;
    let other = if started == waiting_one_id {
        waiting_two_id
    } else {
        waiting_one_id
    };
    assert!(
        has_attempt(&env, running_id).await,
        "the first attempt was never cancelled"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !has_attempt(&env, other).await,
        "the raised limit still bounds how many run at once"
    );

    // Lowering it stops new permits even though two attempts are running.
    set_max_concurrent(&env, 1).await;
    let third = env.remote_track("Hello Song", "Hello Artist").await;
    let third_id = request(&env, third, Priority::Queue).await.id;
    env.wait_for_state(third_id, AcquisitionState::Downloading)
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !has_attempt(&env, third_id).await,
        "the lowered limit refuses a third download while two run"
    );

    // Both running attempts have to end before the lowered limit admits one.
    env.orchestrator.cancel(running_id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !has_attempt(&env, third_id).await,
        "one running attempt is still one more than the lowered limit allows"
    );
    env.orchestrator.cancel(started).await.unwrap();
    wait_for_attempt(&env, third_id).await;
}
