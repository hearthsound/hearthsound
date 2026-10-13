//! Cancelling an acquisition, and keeping the running process in step with a
//! ledger another process wrote.
//!
//! These tests drive the orchestrator the way the API layer and the CLI do and
//! read the effects from the places the rest of the server reads: the
//! `acquisitions` rows, the `tracks.availability` column, the staging tree, the
//! managed storage roots, the fixture backend's key-value store and the waiter
//! channels.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{
    Env, INSTANCE, InstanceLimits, config, drain, has_acquisition_event, has_availability_event,
    one_candidate,
};
use hearthsound_acquire::orchestrator::AcquisitionHandle;
use hearthsound_acquire::repo::{self, ListFilter};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, FailureReason, IdPrefix, Priority,
};
use hearthsound_db::Database;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use serde_json::json;
use uuid::Uuid;

/// Admits one acquisition and returns its handle.
async fn request(env: &Env, track: EntityId) -> AcquisitionHandle {
    let outcome = env
        .orchestrator
        .request(hearthsound_acquire::orchestrator::AcquireRequest::new(
            track,
            Priority::Play,
            "test request",
        ))
        .await
        .unwrap();
    outcome
        .handle()
        .expect("the request must be admitted")
        .clone()
}

/// A fixture configuration whose download runs long enough to be interrupted.
///
/// The download reports thirty running polls two seconds apart, so a test has
/// about a minute to cancel it, and it lists the files it finds in the
/// acquisition's own staging directory, which is where a partial file would be.
fn long_download() -> serde_json::Value {
    let mut server_config = config(one_candidate(), "normal", 0, "staging", ".flac");
    server_config["downloadRunningPolls"] = json!(30);
    server_config
}

/// Waits until the acquisition has a download token in the ledger.
///
/// The token is what a cancellation cancels on the backend, so a test that wants
/// to cancel a download waits for it instead of for the state, which is written
/// before the download is started.
///
/// # Panics
///
/// Panics when no download starts within twenty seconds.
async fn wait_for_download(env: &Env, id: Uuid) {
    for _ in 0..1000 {
        if env.acquisition(id).await.plugin_token.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("acquisition {id} never started a download");
}

/// Counts the downloads the fixture instance still holds.
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

/// Collects every file under a directory.
fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(files_under(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Writes the file the simulated backend would hand to the import stage.
///
/// The file is written into the acquisition's staging directory, which is the
/// root the fixture reports its output from here, so a download that ran to
/// completion would have imported it.
async fn place_staged_file(env: &Env, id: Uuid) {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.2)
        .file_name("01 - Hello Song.flac".to_owned())
        .write_into(&env.orchestrator.staging_dir(id))
        .await
        .unwrap();
}

#[tokio::test]
async fn cancels_a_download_without_leaving_a_partial_file() {
    let env = Env::new(long_download(), InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let handle = request(&env, track).await;
    let id = handle.id;
    let mut events = env.events.subscribe();

    env.start();
    wait_for_download(&env, id).await;
    assert_eq!(
        env.acquisition(id).await.state,
        AcquisitionState::Downloading
    );
    place_staged_file(&env, id).await;

    env.orchestrator.cancel(id).await.unwrap();

    // The ledger ends in the cancelled state, with the failure reason the design
    // gives a cancellation.
    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Cancelled);
    assert_eq!(row.failure_reason, Some(FailureReason::Cancelled));
    assert_eq!(row.error_detail.as_deref(), Some("cancelled"));
    assert_eq!(
        env.log(id).await.last().unwrap().state,
        AcquisitionState::Cancelled
    );

    // The track goes back to catalog-only, without a cooldown: a cancellation is
    // not a failure the retry policy should remember.
    let stored = env.track(track).await;
    assert_eq!(stored.availability, Availability::Remote);
    assert_eq!(stored.unavailable_until, None);

    // No partial file survives: the staging directory is gone, and nothing
    // reached managed storage.
    assert!(
        !env.orchestrator.staging_dir(id).exists(),
        "the staging directory is removed"
    );
    assert!(files_under(&env.cache_root).is_empty(), "no cache file");
    assert!(files_under(&env.library_root).is_empty(), "no library file");

    // The backend was told to stop, and the waiter sees the end.
    assert_eq!(pending_downloads(&env).await, 0);
    let progress = handle.wait().await.unwrap();
    assert_eq!(progress.state, AcquisitionState::Cancelled);
    assert_eq!(progress.failure_reason, Some(FailureReason::Cancelled));
    assert!(progress.failed());
    assert!(!progress.succeeded());

    let published = drain(&mut events);
    assert!(has_acquisition_event(
        &published,
        id,
        AcquisitionState::Cancelled
    ));
    assert!(has_availability_event(
        &published,
        track,
        Availability::Remote
    ));

    // The acquisition may be requested again: the track is no longer acquiring.
    let again = env
        .orchestrator
        .request(hearthsound_acquire::orchestrator::AcquireRequest::new(
            track,
            Priority::Play,
            "second request",
        ))
        .await
        .unwrap();
    assert!(
        again.acquisition_id().is_some(),
        "a cancelled track can be acquired again, got {again:?}"
    );
}

#[tokio::test]
async fn an_external_cancel_is_picked_up_by_reconcile() {
    let env = Env::new(long_download(), InstanceLimits::default(), &[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let handle = request(&env, track).await;
    let id = handle.id;
    let mut events = env.events.subscribe();

    env.start();
    wait_for_download(&env, id).await;
    place_staged_file(&env, id).await;

    // The CLI writes the cancellation through a connection of its own; the
    // orchestrator has no idea until it is told to look.
    let other = Database::open(&env.directory().join("hearthsound.db"))
        .await
        .unwrap();
    let mut tx = other.writer().begin().await.unwrap();
    assert!(
        repo::cancel(&mut tx, id, Some("cancelled from the CLI"))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    drop(other);
    assert_eq!(
        env.acquisition(id).await.state,
        AcquisitionState::Cancelled,
        "the external write is visible in the ledger"
    );
    assert!(
        env.orchestrator.staging_dir(id).exists(),
        "the running process has not cleaned up yet"
    );

    env.orchestrator.reconcile().await.unwrap();

    let row = env.acquisition(id).await;
    assert_eq!(row.state, AcquisitionState::Cancelled);
    assert_eq!(row.error_detail.as_deref(), Some("cancelled from the CLI"));
    assert_eq!(env.track(track).await.availability, Availability::Remote);
    assert!(
        !env.orchestrator.staging_dir(id).exists(),
        "reconcile removes the staging directory"
    );
    assert!(files_under(&env.cache_root).is_empty(), "no cache file");
    assert!(files_under(&env.library_root).is_empty(), "no library file");
    assert_eq!(pending_downloads(&env).await, 0);

    let progress = handle.wait().await.unwrap();
    assert_eq!(progress.state, AcquisitionState::Cancelled);

    let published = drain(&mut events);
    assert!(has_acquisition_event(
        &published,
        id,
        AcquisitionState::Cancelled
    ));
    assert!(has_availability_event(
        &published,
        track,
        Availability::Remote
    ));
}

#[tokio::test]
async fn lists_acquisitions_with_their_track_and_instance() {
    let env = Env::normal(&[]).await;
    let track = env.remote_track("Hello Song", "Hello Artist").await;
    let handle = request(&env, track).await;

    let rows = env.orchestrator.list(&ListFilter::default()).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        row.client_id,
        EntityId::from_uuid(IdPrefix::Acquisition, handle.id).to_string()
    );
    assert!(row.client_id.starts_with("ac_"));
    assert_eq!(row.id, handle.id);
    assert_eq!(row.state, AcquisitionState::Queued);
    assert_eq!(row.priority, Priority::Play);
    assert_eq!(row.track_id, Some(track));
    assert_eq!(row.track_title.as_deref(), Some("Hello Song"));
    assert_eq!(row.track_artist.as_deref(), Some("Hello Artist"));
    assert_eq!(row.instance, None, "no download has named an instance yet");
    assert_eq!(row.percent, None);
    assert_eq!(row.failure_reason, None);

    let active = env
        .orchestrator
        .list(&ListFilter {
            active: true,
            ..ListFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(active.len(), 1);

    let failed = env
        .orchestrator
        .list(&ListFilter {
            state: Some(AcquisitionState::Failed),
            ..ListFilter::default()
        })
        .await
        .unwrap();
    assert!(failed.is_empty(), "a state filter excludes the row");

    let none = env
        .orchestrator
        .list(&ListFilter {
            limit: Some(0),
            ..ListFilter::default()
        })
        .await
        .unwrap();
    assert!(none.is_empty(), "the limit is honored");
}

#[test]
fn resumes_verification_and_import_after_a_restart() {
    let first = runtime();
    let (dir, verifying_id, verifying_track, importing_id, importing_track) =
        first.block_on(async {
            let mut env = Env::with_import_mode(
                config(one_candidate(), "normal", 0, "downloads", ".flac"),
                InstanceLimits::default(),
                &[],
                // The source stays in the import root, so both stopped acquisitions
                // can import it.
                "copy",
                false,
            )
            .await;
            env.place_download("01 - Hello Song.flac").await;

            // One acquisition runs to completion, which is what leaves a candidate
            // with the files it resolved in the ledger.
            let done_track = env.remote_track("Hello Song", "Hello Artist").await;
            let done = request(&env, done_track).await;
            env.start();
            env.wait_for_state(done.id, AcquisitionState::Completed)
                .await;
            let candidate = env
                .acquisition(done.id)
                .await
                .candidate
                .expect("a completed download records its candidate");

            // Two acquisitions that stopped after their download: one before
            // verification ran, one before its import committed.
            let verifying_track = env.remote_track("Hello Song", "Hello Artist").await;
            let verifying_id = env
                .stopped_after_download(verifying_track, AcquisitionState::Verifying, &candidate)
                .await;
            let importing_track = env.remote_track("Hello Song", "Hello Artist").await;
            let importing_id = env
                .stopped_after_download(importing_track, AcquisitionState::Importing, &candidate)
                .await;

            let dir = env.dir.take().unwrap();
            (
                dir,
                verifying_id,
                verifying_track,
                importing_id,
                importing_track,
            )
        });
    drop(first);

    let second = runtime();
    second.block_on(async {
        let env = Env::reopen(dir.path(), &[]).await;
        env.orchestrator.start();

        // Both acquisitions finish without a download or a search: verification
        // and the import run again from the files the ledger holds.
        for (id, track) in [
            (verifying_id, verifying_track),
            (importing_id, importing_track),
        ] {
            env.wait_for_state(id, AcquisitionState::Completed).await;
            let row = env.acquisition(id).await;
            assert!(row.media_file_id.is_some(), "acquisition {id} imported");
            assert_eq!(row.failure_reason, None);
            assert_eq!(env.track(track).await.availability, Availability::Cached);
            assert!(!env.orchestrator.staging_dir(id).exists());
            let states: Vec<AcquisitionState> = env
                .log(id)
                .await
                .into_iter()
                .map(|entry| entry.state)
                .collect();
            assert_eq!(states.last(), Some(&AcquisitionState::Completed));
        }
        assert_eq!(
            env.downloads_started(INSTANCE).await,
            1,
            "no acquisition downloaded again"
        );
        assert_eq!(
            env.searches_started(INSTANCE).await,
            1,
            "no acquisition searched again"
        );
    });
}

/// Builds a multi-threaded runtime for one simulated process.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Runs the first of two processes: it admits one acquisition and starts its
/// download, then stops with the pipeline task still running.
///
/// The runtime is dropped, which is what a server stopping looks like to the
/// ledger: the download is left in flight, with the token the backend issued in
/// the row and the backend's own state in `plugin_kv`.
///
/// # Parameters
///
/// - `server_config`: the fixture configuration of the simulated backend.
///
/// # Returns
///
/// The directory the process built - the caller keeps it alive, because the
/// reopened environment does not own it - plus the acquisition and its track.
fn first_process_mid_download(
    server_config: serde_json::Value,
) -> (tempfile::TempDir, Uuid, EntityId) {
    let first = runtime();
    let outcome = first.block_on(async {
        let mut env = Env::new(server_config, InstanceLimits::default(), &[]).await;
        env.place_download("01 - Hello Song.flac").await;
        let track = env.remote_track("Hello Song", "Hello Artist").await;
        let handle = request(&env, track).await;
        env.start();
        wait_for_download(&env, handle.id).await;
        assert_eq!(
            env.acquisition(handle.id).await.state,
            AcquisitionState::Downloading
        );
        assert_eq!(env.downloads_started(INSTANCE).await, 1);
        assert!(env.orchestrator.staging_dir(handle.id).is_dir());
        // The directory outlives the process that created it.
        let dir = env.dir.take().unwrap();
        (dir, handle.id, track)
    });
    drop(first);
    outcome
}

#[test]
fn resumes_a_download_after_a_restart() {
    let mut server_config = config(one_candidate(), "normal", 0, "downloads", ".flac");
    // Three running polls two seconds apart: long enough for the first process
    // to be stopped mid-download, short enough for the second to finish it.
    server_config["downloadRunningPolls"] = json!(3);
    let (dir, id, track) = first_process_mid_download(server_config);

    // The second process finds the same database, the same storage and the same
    // fixture backend, and resumes what the first one left in flight.
    let second = runtime();
    second.block_on(async {
        let env = Env::reopen(dir.path(), &[]).await;
        let row = env.acquisition(id).await;
        assert_eq!(row.state, AcquisitionState::Downloading);
        assert!(row.plugin_token.is_some(), "the download token survived");
        assert_eq!(env.downloads_started(INSTANCE).await, 1);
        assert_eq!(env.searches_started(INSTANCE).await, 1);

        env.orchestrator.start();
        env.wait_for_state(id, AcquisitionState::Completed).await;

        // The resumed download imported its file without searching again, and
        // the source left the import root.
        let row = env.acquisition(id).await;
        assert!(row.media_file_id.is_some(), "the file was imported");
        assert_eq!(row.failure_reason, None);
        assert_eq!(
            env.searches_started(INSTANCE).await,
            1,
            "the resumed download did not search again"
        );
        let destination = env
            .cache_root
            .join("Hello Artist/Hello Song album (1993)/01 - Hello Song.flac");
        assert!(destination.is_file(), "{}", destination.display());
        assert!(!env.downloads.join("01 - Hello Song.flac").exists());
        assert_eq!(env.track(track).await.availability, Availability::Cached);
        assert!(!env.orchestrator.staging_dir(id).exists());

        let states: Vec<AcquisitionState> = env
            .log(id)
            .await
            .into_iter()
            .map(|entry| entry.state)
            .collect();
        assert_eq!(states.last(), Some(&AcquisitionState::Completed));
    });
}

#[test]
fn a_download_the_backend_forgot_restarts_the_search_after_a_restart() {
    let mut server_config = config(one_candidate(), "normal", 0, "downloads", ".flac");
    // Three running polls, so the search the resume restarts downloads again and
    // finishes quickly.
    server_config["downloadRunningPolls"] = json!(3);
    let (dir, id, track) = first_process_mid_download(server_config);

    let second = runtime();
    second.block_on(async {
        let env = Env::reopen(dir.path(), &[]).await;
        assert_eq!(
            env.acquisition(id).await.state,
            AcquisitionState::Downloading
        );

        // The backend lost the download while the server was down, as it does
        // when it restarts or evicts a job.
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

        env.orchestrator.start();
        env.wait_for_state(id, AcquisitionState::Completed).await;

        assert!(
            env.searches_started(INSTANCE).await >= 2,
            "the lost download restarted the search"
        );
        assert!(env.acquisition(id).await.media_file_id.is_some());
        assert_eq!(env.track(track).await.availability, Availability::Cached);
        assert!(
            env.log(id)
                .await
                .into_iter()
                .filter_map(|entry| entry.message)
                .any(|message| message.contains("lost the download token"))
        );
    });
}
