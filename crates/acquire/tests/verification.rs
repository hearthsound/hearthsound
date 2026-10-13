//! The verification stage: the checks a downloaded candidate passes before it is
//! imported, and what a rejected candidate does to the acquisition.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;

use common::{Env, InstanceLimits, candidate, config, one_candidate};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle};
use hearthsound_core::{AcquisitionState, Availability, EntityId, FailureReason, Priority};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};

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
async fn place(env: &Env, format: AudioFormat, seconds: f64, name: &str) -> PathBuf {
    AudioFixture::new(format)
        .seconds(seconds)
        .file_name(name.to_owned())
        .write_into(&env.downloads)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_file_that_does_not_decode_fails_verification() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    std::fs::write(env.downloads.join("broken.flac"), b"this is not audio").unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::VerificationFailed));
    assert_eq!(row.attempts, 1, "the only candidate was attempted");
    assert!(
        row.error_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("does not decode")),
        "{:?}",
        row.error_detail
    );
    assert_eq!(
        env.track(track).await.availability,
        Availability::Unavailable
    );
    assert_eq!(env.acquisitions_for(track).await, 1);
}

#[tokio::test]
async fn a_file_far_off_the_catalog_duration_fails_verification() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    // The fixture's track lasts 200 ms; a 30 second file cannot be the same
    // recording.
    place(&env, AudioFormat::Flac, 30.0, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    let row = env.wait_for_terminal(id).await;
    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::VerificationFailed));
    assert!(
        row.error_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("from the track's")),
        "{:?}",
        row.error_detail
    );
}

#[tokio::test]
async fn a_lossy_file_behind_a_lossless_claim_is_rejected_and_the_next_candidate_is_tried() {
    // The first candidate claims a lossless FLAC but the backend hands back an
    // MP3; the second candidate claims the lossy file it actually gets, so it
    // passes and the acquisition reaches verification.
    let env = Env::new(
        config(
            serde_json::json!([
                candidate("cand-1", "flac", Some(900)),
                candidate("cand-2", "mp3", Some(320)),
            ]),
            "normal",
            0,
            "downloads",
            ".mp3",
        ),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, AudioFormat::Mp3Cbr, 0.2, "01 - Hello Song.mp3").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Verifying).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Verifying);
    assert_eq!(row.failure_reason, None);
    assert_eq!(
        row.attempts, 2,
        "the rejected candidate was followed by the next"
    );
    assert_eq!(env.downloads_started(common::INSTANCE).await, 2);
    let messages: Vec<String> = env
        .log(id)
        .await
        .into_iter()
        .filter_map(|entry| entry.message)
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("claimed as lossless but decodes as a lossy codec")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("the candidate passed verification")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn a_matching_file_passes_verification_and_leaves_the_track_acquiring() {
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    place(&env, AudioFormat::Flac, 0.2, "01 - Hello Song.flac").await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Verifying).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Verifying);
    assert_eq!(row.failure_reason, None);
    assert_eq!(env.track(track).await.availability, Availability::Acquiring);
}
