//! The identity stage: resolving a wanted track before the search runs, and the
//! guarantee that it never delays a play request or fails an acquisition.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Instant;

use crate::common::{Env, InstanceLimits, config, one_candidate};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle};
use hearthsound_core::{AcquisitionState, EntityId, EntityKind, Priority};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use serde_json::json;

/// The MusicBrainz recording id the metadata fixture answers with.
const MBID: &str = "11111111-1111-1111-1111-111111111111";

/// Admits one acquisition and returns its handle.
async fn request(env: &Env, track: EntityId, priority: Priority) -> AcquisitionHandle {
    env.orchestrator
        .request(AcquireRequest::new(track, priority, "test request"))
        .await
        .unwrap()
        .handle()
        .expect("the request must be admitted")
        .clone()
}

/// Writes a generated audio file into the instance's import root.
async fn place(env: &Env) {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.2)
        .file_name("01 - Hello Song.flac".to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap();
}

/// Waits until a track carries a MusicBrainz recording id.
///
/// # Returns
///
/// The id, or `None` when the track still has none after
/// [`crate::common::WAIT_TIMEOUT`].
async fn wait_for_recording_id(env: &Env, track: EntityId) -> Option<String> {
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(id) = recording_id(env, track).await {
            return Some(id);
        }
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
    }
    recording_id(env, track).await
}

/// Returns the MusicBrainz recording id stored for a track, when it has one.
///
/// # Parameters
///
/// - `env`: the test environment.
/// - `track`: the track to read.
async fn recording_id(env: &Env, track: EntityId) -> Option<String> {
    env.ids(EntityKind::Track, track)
        .await
        .into_iter()
        .find(|id| id.namespace == "mb-recording")
        .map(|id| id.value)
}

#[tokio::test]
async fn a_bulk_acquisition_applies_the_resolved_recording_identity() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    env.add_metadata_instance(json!({
        "recordings": [
            {
                "title": "Hello Song",
                "artists": ["Hello Artist"],
                "isrc": "USHW12600001",
                "mbid": MBID,
                "score": 0.95,
                "releases": [],
            }
        ]
    }))
    .await;
    place(&env).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Background).await.id;

    env.start();
    assert_eq!(
        wait_for_recording_id(&env, track).await.as_deref(),
        Some(MBID)
    );
    // The identity is applied before the search runs, so the acquisition is
    // already past `resolving`.
    let mut state = env.acquisition(id).await.state;
    let deadline = Instant::now() + crate::common::WAIT_TIMEOUT;
    while state == AcquisitionState::Queued || state == AcquisitionState::Resolving {
        assert!(
            Instant::now() < deadline,
            "the acquisition stayed in {state}"
        );
        tokio::time::sleep(crate::common::WAIT_POLL_INTERVAL).await;
        state = env.acquisition(id).await.state;
    }
    assert!(
        state != AcquisitionState::Queued && state != AcquisitionState::Resolving,
        "the acquisition stayed in {state}"
    );
}

#[tokio::test]
async fn an_unmatched_recording_leaves_the_track_and_the_acquisition_alone() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    env.add_metadata_instance(json!({
        "recordings": [
            {
                "title": "Another Song",
                "artists": ["Another Artist"],
                "isrc": "GBAYE0000001",
                "mbid": MBID,
                "score": 0.95,
                "releases": [],
            }
        ]
    }))
    .await;
    place(&env).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;
    assert_eq!(recording_id(&env, track).await, None);
    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert_eq!(row.failure_reason, None);
}
