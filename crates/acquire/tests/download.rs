//! The download stage: polling, stalls, queue timeouts, lost tokens, and the
//! handoff of stable files to verification.
//!
//! The fixture's downloads are counted in the host's key-value store and its
//! cancelled downloads are deleted from it, so a test can assert which
//! downloads ran and which were abandoned.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Env, INSTANCE, InstanceLimits, candidate, config, one_candidate};
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
async fn a_completed_download_hands_stable_files_to_verification() {
    let mut server_config = config(one_candidate(), "normal", 0, "downloads", ".flac");
    // Two running polls of half the file each, so the percentage the events
    // carry has to advance.
    server_config["downloadRunningPolls"] = json!(2);
    server_config["downloadBytesPerPoll"] = json!(2000);
    let env = Env::new(server_config, InstanceLimits::default(), &[]).await;
    let file = place_download(&env, "01 - Hello Song.flac").await;
    let size = std::fs::metadata(&file).unwrap().len();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let mut events = env.events.subscribe();
    let handle = request(&env, track, Priority::Play).await;
    let id = handle.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Verifying).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Verifying);
    assert_eq!(
        row.instance_id,
        Some(env.registry.instance(INSTANCE).unwrap().id())
    );
    assert_eq!(row.plugin_token.as_deref(), Some("download-1"));
    assert_eq!(row.attempts, 1);
    assert_eq!(row.failure_reason, None);
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

    // The files are resolved and stable, and the live channel carries the path a
    // client waiting for audio reads.
    let live = handle.snapshot();
    assert_eq!(live.state, AcquisitionState::Verifying);
    assert_eq!(live.file.as_deref(), Some(file.as_path()));
    assert_eq!(std::fs::metadata(&file).unwrap().len(), size);

    // The ledger carries the state, the instance and the last percentage; the
    // resolved path is not stored.
    let snapshot = env.orchestrator.progress_of(id).await.unwrap().unwrap();
    assert_eq!(snapshot.state, AcquisitionState::Verifying);
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
    assert!(
        logged(
            &env,
            id,
            "downloading `Hello Artist/Hello Song album/01 - Hello Song.flac`"
        )
        .await
    );
    assert!(logged(&env, id, "the download completed with stable files").await);

    // The percentage and the ETA advance through the download.
    let published = common::drain(&mut events);
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
    assert_eq!(percents, [50, 100]);
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
    server_config["downloadQueuedPolls"] = json!(5);
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
    for _ in 0..500 {
        if env.downloads_started(INSTANCE).await >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
    forget_downloads(&env).await;

    env.wait_for_state(id, AcquisitionState::Verifying).await;
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
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits {
            max_concurrent: 1,
            ..InstanceLimits::default()
        },
        &[],
    )
    .await;
    place_download(&env, "01 - Hello Song.flac").await;

    // A background run takes the instance's only download slot and sits in a
    // long poll.
    let bulk_track = env.remote_track("Hello Song", "Hello Artist").await;
    let bulk_id = request(&env, bulk_track, Priority::Background).await.id;
    env.start();
    for _ in 0..500 {
        if env.downloads_started(INSTANCE).await >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(env.downloads_started(INSTANCE).await, 1);
    assert_eq!(
        env.acquisition(bulk_id).await.state,
        AcquisitionState::Downloading
    );

    // A play request needs the slot the background run holds.
    let play_track = env.remote_track("Hello Song", "Hello Artist").await;
    let play_id = request(&env, play_track, Priority::Play).await.id;
    env.wait_for_state(play_id, AcquisitionState::Verifying)
        .await;
    assert_eq!(
        env.downloads_started(INSTANCE).await,
        3,
        "the preempted download was started again"
    );

    // The preempted acquisition is not lost: it downloads again and reaches
    // verification too.
    env.wait_for_state(bulk_id, AcquisitionState::Verifying)
        .await;
    assert!(logged(&env, bulk_id, "preempted by an interactive request").await);
    let bulk = env.acquisition(bulk_id).await;
    assert_eq!(bulk.state, AcquisitionState::Verifying);
    assert_eq!(bulk.failure_reason, None);
}
