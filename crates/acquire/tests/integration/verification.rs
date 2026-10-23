//! The verification stage: the checks a downloaded candidate passes before it is
//! imported, and what a rejected candidate does to the acquisition.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use crate::common::{
    Env, INSTANCE, InstanceLimits, album_folder, album_instance_config, candidate, config,
    one_candidate,
};
use hearthsound_acquire::orchestrator::{AcquireRequest, AcquisitionHandle, AlbumAcquireRequest};
use hearthsound_core::events::ServerEvent;
use hearthsound_core::{AcquisitionState, Availability, EntityId, FailureReason, Priority};
use hearthsound_media::{FfmpegRunner, fingerprint};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, Signal};
use serde_json::{Value, json};

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
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert_eq!(row.failure_reason, None);
    assert_eq!(
        row.attempts, 2,
        "the rejected candidate was followed by the next"
    );
    assert_eq!(env.downloads_started(crate::common::INSTANCE).await, 2);
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
async fn a_matching_file_passes_verification_and_is_imported() {
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
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Completed);
    assert_eq!(row.failure_reason, None);
    assert_eq!(env.track(track).await.availability, Availability::Cached);
}

// ---------------------------------------------------------------------------
// Fingerprint verification and quarantine.
// ---------------------------------------------------------------------------

/// Generates a three second tone and returns its directory, path and print.
///
/// The directory keeps the file alive for the caller, which copies it where the
/// simulated backend expects its download.
async fn tone(hz: f32) -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(3.0)
        .signal(Signal::Sine { hz })
        .file_name("tone.flac")
        .write_into(dir.path())
        .await
        .unwrap();
    let print = fingerprint(&path, &FfmpegRunner::new(None, 2))
        .await
        .unwrap();
    (dir, path, print.value)
}

/// A candidate that offers one file of the given name and reports a speed.
fn candidate_with(token: &str, file: &str, format: &str, speed_bps: u64, size: u64) -> Value {
    json!({
        "token": token,
        "title": "Hello Song",
        "path": "Hello Artist/Hello Song album/01 - Hello Song.flac",
        "files": [{
            "name": file,
            "size": size,
            "format": format,
            "bitrateKbps": 900,
            "sampleRate": 44100,
            "bitDepth": 16,
            "durationMs": 3000,
            "trackIndex": 0,
        }],
        "source": {
            "key": "peer-1",
            "speedBps": speed_bps,
            "queueLength": 0,
            "freeSlot": true,
        },
    })
}

#[tokio::test]
async fn a_wrong_fingerprint_quarantines_the_source() {
    let (_tones, tone_path, print) = tone(500.0).await;
    let env = Env::new(
        config(
            json!([candidate_with(
                "cand-1",
                "wrong.wav",
                "wav",
                10_000_000,
                4000
            )]),
            "normal",
            0,
            "downloads",
            ".wav",
        ),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [print] }))
        .await;
    tokio::fs::copy(&tone_path, env.downloads.join("wrong.wav"))
        .await
        .unwrap();

    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Background).await.id;
    env.start();
    let row = env.wait_for_terminal(id).await;

    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::VerificationFailed));
    assert!(
        row.error_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("fingerprint mismatch")),
        "{:?}",
        row.error_detail
    );
    let quarantined = env.quarantined().await;
    assert_eq!(quarantined.len(), 1, "{quarantined:?}");
    assert_eq!(quarantined[0].0, "peer-1");
    assert_eq!(quarantined[0].1, "wrong.wav");
    assert!(
        quarantined[0].2.starts_with("fingerprint mismatch:"),
        "{:?}",
        quarantined[0].2
    );
    let messages: Vec<String> = env
        .log(id)
        .await
        .into_iter()
        .filter_map(|entry| entry.message)
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("quarantined")),
        "{messages:?}"
    );
}

#[tokio::test]
async fn a_wrong_staged_file_is_removed_and_one_fresh_search_follows() {
    let (_tones, tone_path, print) = tone(500.0).await;
    let env = Env::new(
        config(
            json!([candidate_with(
                "cand-1",
                "01 - Hello Song.flac",
                "flac",
                10_000_000,
                4000
            )]),
            "normal",
            0,
            "staging",
            ".flac",
        ),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [print] }))
        .await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Background).await.id;
    let staged_dir = env.staging.join(id.to_string());
    tokio::fs::create_dir_all(&staged_dir).await.unwrap();
    let staged = staged_dir.join("01 - Hello Song.flac");
    tokio::fs::copy(&tone_path, &staged).await.unwrap();
    env.start();
    let row = env.wait_for_terminal(id).await;

    assert_eq!(row.state, AcquisitionState::Failed);
    assert_eq!(row.failure_reason, Some(FailureReason::VerificationFailed));
    assert_eq!(row.attempts, 1, "the rejected candidate was not retried");
    assert!(
        !staged.exists(),
        "the staged copy of a quarantined file must be removed"
    );
    assert_eq!(
        env.searches_started(INSTANCE).await,
        2,
        "one fresh search followed the rejection"
    );
    assert_eq!(env.quarantined().await.len(), 1);
}

#[tokio::test]
async fn the_right_candidate_is_imported_after_a_rejected_one() {
    let (_tones, wrong_path, print) = tone(500.0).await;
    let env = Env::new(
        config(
            json!([candidate_with(
                "cand-1",
                "wrong.wav",
                "wav",
                10_000_000,
                4000
            )]),
            "normal",
            0,
            "downloads",
            ".wav",
        ),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
    )
    .await;
    let (_good, good_path, _) = tone(620.0).await;
    env.add_instance(
        "second",
        config(
            json!([candidate_with(
                "cand-2",
                "01 - Hello Song.flac",
                "flac",
                1_200_000,
                40_000_000
            )]),
            "normal",
            0,
            "downloads",
            ".flac",
        ),
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [print] }))
        .await;
    tokio::fs::copy(&wrong_path, env.downloads.join("wrong.wav"))
        .await
        .unwrap();
    tokio::fs::copy(&good_path, env.downloads.join("01 - Hello Song.flac"))
        .await
        .unwrap();

    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Background).await.id;
    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let row = env.acquisition(id).await;
    assert_eq!(row.attempts, 2, "the rejected candidate was followed");
    assert_eq!(
        env.track(track).await.availability,
        Availability::Cached,
        "the matching candidate was imported"
    );
    assert_eq!(env.media_files_for(track).await, 1);
    assert_eq!(env.quarantined().await.len(), 1);
}

#[tokio::test]
async fn a_play_import_that_mismatches_is_removed_and_acquired_again() {
    let (_tones, tone_path, print) = tone(500.0).await;
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [print] }))
        .await;
    tokio::fs::copy(&tone_path, env.downloads.join("01 - Hello Song.flac"))
        .await
        .unwrap();
    let mut events = env.events.subscribe();

    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;
    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut saw_remote = false;
    loop {
        let computed =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM acquisitions WHERE track_id = ?")
                .bind(track.uuid().as_bytes().to_vec())
                .fetch_one(env.database.readers())
                .await
                .unwrap();
        while let Ok(envelope) = events.try_recv() {
            if let ServerEvent::TrackAvailability(change) = envelope.event
                && change.track_id == track.to_string()
                && change.availability == Availability::Remote
            {
                saw_remote = true;
            }
        }
        if computed >= 2 && saw_remote {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the mismatch was not handled: {computed} acquisitions, remote={saw_remote}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    assert_eq!(env.media_files_for(track).await, 0);
    assert_eq!(env.quarantined().await.len(), 1);
    assert_ne!(
        env.track(track).await.availability,
        Availability::Cached,
        "the mismatching file is gone"
    );
}

#[tokio::test]
async fn verification_off_imports_a_mismatching_file() {
    let (_tones, tone_path, print) = tone(500.0).await;
    let env = Env::new(
        config(one_candidate(), "normal", 0, "downloads", ".flac"),
        InstanceLimits::default(),
        &[],
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [print] }))
        .await;
    tokio::fs::copy(&tone_path, env.downloads.join("01 - Hello Song.flac"))
        .await
        .unwrap();
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let id = request(&env, track, Priority::Play).await.id;

    env.start();
    env.wait_for_state(id, AcquisitionState::Completed).await;

    assert_eq!(env.track(track).await.availability, Availability::Cached);
    assert!(env.quarantined().await.is_empty());
}

#[tokio::test]
async fn an_unknown_verdict_and_a_provider_error_let_the_file_pass() {
    for index in 0..2 {
        let (_tones, tone_path, print) = tone(500.0).await;
        let verification_config = if index == 0 {
            json!({ "unknownFingerprints": [print] })
        } else {
            json!({ "fail": true })
        };
        let env = Env::new(
            config(one_candidate(), "normal", 0, "downloads", ".flac"),
            InstanceLimits::default(),
            &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
        )
        .await;
        env.add_verification_instance(verification_config).await;
        tokio::fs::copy(&tone_path, env.downloads.join("01 - Hello Song.flac"))
            .await
            .unwrap();
        let track = env.remote_track("Hello Song", "Hello Artist").await;
        let id = request(&env, track, Priority::Background).await.id;

        env.start();
        env.wait_for_state(id, AcquisitionState::Completed).await;
        assert_eq!(
            env.track(track).await.availability,
            Availability::Cached,
            "case {index}"
        );
        assert!(env.quarantined().await.is_empty(), "case {index}");
    }
}

#[tokio::test]
async fn an_album_file_that_mismatches_is_quarantined_while_its_siblings_import() {
    let (_bad_dir, bad_path, bad_print) = tone(500.0).await;
    let (_good_dir, good_path, _) = tone(620.0).await;
    let folders = json!([album_folder(
        "Hello Album",
        &[
            ("01 - Hello Album 1.flac", "flac", None),
            ("02 - Hello Album 2.flac", "flac", None),
        ],
    )]);
    let env = Env::new(
        album_instance_config(folders),
        InstanceLimits::default(),
        &[("HEARTHSOUND__ACQUISITION__VERIFY__FINGERPRINT", "true")],
    )
    .await;
    env.add_verification_instance(json!({ "rejectFingerprints": [bad_print] }))
        .await;
    let (album, tracks) = env.add_album("Hello Album", "Hello Artist", 2).await;
    let folder = env.downloads.join("Hello Album");
    tokio::fs::create_dir_all(&folder).await.unwrap();
    tokio::fs::copy(&good_path, folder.join("01 - Hello Album 1.flac"))
        .await
        .unwrap();
    tokio::fs::copy(&bad_path, folder.join("02 - Hello Album 2.flac"))
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
    let id = outcomes[0].outcome.acquisition_id().unwrap();
    env.start();
    let row = env.wait_for_terminal(id).await;

    assert_eq!(row.state, AcquisitionState::Completed);
    assert_eq!(
        env.track(tracks[0]).await.availability,
        Availability::Cached
    );
    assert_eq!(env.media_files_for(tracks[0]).await, 1);
    assert_eq!(
        env.media_files_for(tracks[1]).await,
        0,
        "the mismatching file was not imported"
    );
    let quarantined = env.quarantined().await;
    let messages: Vec<String> = env
        .log(id)
        .await
        .into_iter()
        .filter_map(|entry| entry.message)
        .collect();
    assert_eq!(quarantined.len(), 1, "{quarantined:?} log={messages:?}");
    assert_eq!(quarantined[0].1, "02 - Hello Album 2.flac");
}
