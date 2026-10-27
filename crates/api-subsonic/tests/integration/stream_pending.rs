//! Integration tests for the pending path of `stream`: what `/rest/stream`
//! answers for a track the server has not stored yet.
//!
//! The acquisition layer is a scripted [`PendingSource`] behind the stream
//! service's seam, so every test drives an exact outcome - a file after a delay,
//! a failure, a cooldown, a denial - while the bytes, the headers, and the
//! timing are the real ones a client would receive.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderName, Method, Request, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use hearthsound_acquire::AcquireError;
use hearthsound_acquire::orchestrator::{AcquisitionHandle, AcquisitionProgress};
use hearthsound_api_subsonic::{PendingOutcome, PendingSource};
use hearthsound_config::{SettingValue, keys};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, FailureReason, ParkReason, Priority,
};
use hearthsound_db::Database;
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, ScannedMediaFile, TrackInput,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use tokio::sync::watch;
use tower::ServiceExt;
use uuid::Uuid;

use crate::common::Fixture;

/// One scripted acquisition outcome.
#[derive(Clone)]
enum Script {
    /// The track already has a file.
    Available,
    /// An acquisition that completes with this file after the delay.
    Complete {
        /// The file the acquisition reports.
        file: PathBuf,
        /// How long the acquisition takes.
        delay: Duration,
    },
    /// An acquisition that imports a file as the track's media file and then
    /// completes.
    Import {
        /// The library write a real import performs.
        importer: Arc<Importer>,
        /// How long the acquisition takes.
        delay: Duration,
    },
    /// An acquisition that fails after the delay.
    Fail {
        /// How long the acquisition takes before it fails.
        delay: Duration,
    },
    /// An acquisition that never ends within the test.
    Waiting,
    /// An acquisition that runs, then parks for want of a source, and waits.
    Parked {
        /// How long the acquisition runs before it parks.
        delay: Duration,
    },
    /// The track is cooling down after a failed acquisition.
    CoolingDown,
    /// The acquisition layer refuses the request for `reason`.
    Denied(FailureReason),
}

/// The library write a completing acquisition performs.
///
/// A real import records the downloaded file as the track's media file; this
/// does the same for a generated file, so the hold path serves real bytes from
/// the library the way it would after a real import.
struct Importer {
    /// The open database.
    database: Arc<Database>,
    /// The directory holding the file, registered as a library folder.
    directory: PathBuf,
    /// The file's name inside the directory.
    name: String,
    /// The file's size in bytes.
    size: i64,
    /// The file's modification time in unix milliseconds.
    mtime_ms: i64,
    /// The file's duration in milliseconds.
    duration_ms: i64,
    /// The path the acquisition reports.
    path: PathBuf,
}

impl Importer {
    /// Builds an importer over a generated file.
    ///
    /// # Parameters
    ///
    /// - `database`: the fixture's open database.
    /// - `path`: the generated file to import.
    /// - `seconds`: the file's duration.
    fn new(database: Arc<Database>, path: &Path, seconds: f64) -> Arc<Importer> {
        let metadata = std::fs::metadata(path).unwrap();
        Arc::new(Importer {
            database,
            directory: path.parent().unwrap().to_path_buf(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            size: metadata.len() as i64,
            mtime_ms: metadata
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64,
            duration_ms: (seconds * 1000.0) as i64,
            path: path.to_path_buf(),
        })
    }

    /// Records the file as the track's media file.
    async fn import(&self, track: EntityId) {
        let library = LibraryRepository::new(&self.database);
        let folder = library
            .add_library_folder(&self.directory, None)
            .await
            .unwrap();
        library
            .upsert_folder_media_file(
                track,
                &ScannedMediaFile {
                    folder_id: folder.id,
                    rel_path: self.name.clone(),
                    size: self.size,
                    mtime_ms: self.mtime_ms,
                    format: Some("flac".to_owned()),
                    codec: Some("flac".to_owned()),
                    bitrate_kbps: Some(900),
                    sample_rate: Some(44_100),
                    bit_depth: None,
                    channels: Some(2),
                    duration_ms: Some(self.duration_ms),
                    lossless: Some(true),
                    replaygain: None,
                    has_embedded_art: Some(false),
                },
            )
            .await
            .unwrap();
    }
}

/// A scripted acquisition layer for the pending-stream tests.
struct FakeSource {
    /// What a request returns.
    script: Script,
    /// How many requests the stream service made.
    calls: AtomicUsize,
}

impl FakeSource {
    /// Builds a source that answers every request with `script`.
    fn new(script: Script) -> Arc<FakeSource> {
        Arc::new(FakeSource {
            script,
            calls: AtomicUsize::new(0),
        })
    }

    /// Returns how many requests the source was asked for.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PendingSource for FakeSource {
    async fn request(
        &self,
        track: EntityId,
        priority: Priority,
        _requested_by: Option<Uuid>,
        _can_acquire: bool,
    ) -> Result<PendingOutcome, AcquireError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(priority, Priority::Play, "a stream asks at priority play");
        Ok(match &self.script {
            Script::Available => PendingOutcome::Available,
            Script::CoolingDown => PendingOutcome::CoolingDown,
            Script::Denied(reason) => PendingOutcome::Denied { reason: *reason },
            Script::Waiting => {
                let (handle, sender) = channel(track, AcquisitionState::Downloading);
                // A dropped sender ends the channel, which a waiter reads as a
                // lost acquisition; parking it in a task keeps the acquisition
                // alive for the whole test.
                tokio::spawn(async move {
                    let _hold = sender;
                    std::future::pending::<()>().await;
                });
                PendingOutcome::Pending(handle)
            }
            Script::Complete { file, delay } => {
                let (handle, sender) = channel(track, AcquisitionState::Downloading);
                let file = file.clone();
                let delay = *delay;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = sender.send(progress(track, AcquisitionState::Completed, Some(file)));
                });
                PendingOutcome::Pending(handle)
            }
            Script::Import { importer, delay } => {
                let (handle, sender) = channel(track, AcquisitionState::Downloading);
                let importer = Arc::clone(importer);
                let delay = *delay;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    importer.import(track).await;
                    let file = importer.path.clone();
                    let _ = sender.send(progress(track, AcquisitionState::Completed, Some(file)));
                });
                PendingOutcome::Pending(handle)
            }
            Script::Fail { delay } => {
                let (handle, sender) = channel(track, AcquisitionState::Downloading);
                let delay = *delay;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = sender.send(progress(track, AcquisitionState::Failed, None));
                });
                PendingOutcome::Pending(handle)
            }
            Script::Parked { delay } => {
                let (handle, sender) = channel(track, AcquisitionState::Downloading);
                let delay = *delay;
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let mut parked = progress(track, AcquisitionState::Queued, None);
                    parked.awaiting_source = true;
                    parked.park_reason = Some(ParkReason::SourceUnavailable);
                    let _ = sender.send(parked);
                    // The acquisition stays parked: a dropped sender ends the
                    // channel, which a waiter reads as a lost acquisition.
                    std::future::pending::<()>().await;
                });
                PendingOutcome::Pending(handle)
            }
        })
    }
}

/// Builds a progress snapshot for a track.
fn progress(
    track: EntityId,
    state: AcquisitionState,
    file: Option<PathBuf>,
) -> AcquisitionProgress {
    AcquisitionProgress {
        acquisition_id: Uuid::now_v7(),
        target_id: track.to_string(),
        state,
        priority: Priority::Play,
        percent: None,
        eta_secs: None,
        instance: None,
        failure_reason: None,
        awaiting_source: false,
        park_reason: None,
        file,
        requested_by: None,
    }
}

/// Builds a handle and the sender that drives it.
fn channel(
    track: EntityId,
    state: AcquisitionState,
) -> (AcquisitionHandle, watch::Sender<AcquisitionProgress>) {
    let initial = progress(track, state, None);
    let (sender, receiver) = watch::channel(initial.clone());
    (
        AcquisitionHandle {
            id: initial.acquisition_id,
            track_id: Some(track),
            priority: Priority::Play,
            progress: receiver,
        },
        sender,
    )
}

/// Inserts one stored track without a file, which is the pending case.
async fn pending_track(fixture: &Fixture) -> EntityId {
    let library = LibraryRepository::new(&fixture.database);
    let credit = ArtistCredit::new("Echo", "main");
    let album = library
        .upsert_album(&AlbumInput::new("Pending"), std::slice::from_ref(&credit))
        .await
        .unwrap();
    let mut input = TrackInput::new("Not stored yet", Availability::Remote);
    input.album_id = Some(album);
    library
        .insert_track(&input, std::slice::from_ref(&credit), &[])
        .await
        .unwrap()
}

/// Writes a real audio file of the given length into a directory.
async fn fixture_file(dir: &Path, name: &str, seconds: f64) -> PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(seconds)
        .file_name(name.to_owned())
        .write_into(dir)
        .await
        .unwrap()
}

/// Starts a request through the router and returns the streaming response.
async fn start(app: &Router, uri: &str) -> Response {
    let request = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

/// Reads the whole body, failing the test if it does not end in time.
async fn collect(response: Response) -> Vec<u8> {
    let mut stream = Box::pin(response.into_body().into_data_stream());
    let mut collected = Vec::new();
    loop {
        let next = tokio::time::timeout(Duration::from_secs(30), stream.next())
            .await
            .expect("the stream ends before the test's deadline");
        match next {
            Some(Ok(chunk)) => collected.extend_from_slice(&chunk),
            Some(Err(error)) => panic!("the stream failed: {error}"),
            None => return collected,
        }
    }
}

/// Returns one response header as text.
fn header_of(response: &Response, name: HeaderName) -> String {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Sets a pending-stream setting to a short value for a test.
async fn set_mode(fixture: &Fixture, mode: &str) {
    fixture
        .settings
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String(mode.to_owned()),
            None,
        )
        .await
        .unwrap();
}

/// Shortens the wait bound so a stream that never gets a file still ends.
async fn set_max_wait(fixture: &Fixture, max_wait_secs: i64) {
    fixture
        .settings
        .set(
            keys::STREAM_PENDING_MAX_WAIT_SECS.name(),
            SettingValue::Integer(max_wait_secs),
            None,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn hold_serves_the_imported_file_whole_when_the_acquisition_completes() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture_file(dir.path(), "song.flac", 1.0).await;
    let bytes = std::fs::read(&file).unwrap();
    let fixture = Fixture::new().await;
    let source = FakeSource::new(Script::Import {
        importer: Importer::new(Arc::clone(&fixture.database), &file, 1.0),
        delay: Duration::from_millis(300),
    });
    let track = pending_track(&fixture).await;
    let app = fixture.app_with(source);

    // No mode is set: `hold` is what a server that was never configured uses.
    let started = Instant::now();
    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "hold answered before the file was ready"
    );
    // The file is served as a stored file is: with ranges and a length.
    assert_eq!(header_of(&response, header::ACCEPT_RANGES), "bytes");
    assert_eq!(
        header_of(&response, header::CONTENT_LENGTH),
        bytes.len().to_string()
    );
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "audio/flac");
    assert_eq!(collect(response).await, bytes);
}

#[tokio::test]
async fn hold_answers_a_failed_acquisition_with_404() {
    let source = FakeSource::new(Script::Fail {
        delay: Duration::from_millis(200),
    });
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();

    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn hold_answers_a_wait_that_elapsed_with_503() {
    let source = FakeSource::new(Script::Waiting);
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();
    set_max_wait(&fixture, 2).await;

    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_of(&response, header::RETRY_AFTER), "10");
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn fail_answers_503_immediately() {
    let source = FakeSource::new(Script::Waiting);
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();
    set_mode(&fixture, "fail").await;

    let started = Instant::now();
    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_of(&response, header::RETRY_AFTER), "10");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn hold_answers_a_park_that_begins_while_it_holds_with_503() {
    let source = FakeSource::new(Script::Parked {
        delay: Duration::from_millis(300),
    });
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();
    // Long enough that the hold is the only thing that could still be running
    // when the answer arrives: the park, not the bound, ends it.
    set_max_wait(&fixture, 60).await;

    let started = Instant::now();
    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header_of(&response, header::RETRY_AFTER), "10");
    assert!(
        elapsed >= Duration::from_millis(250),
        "the acquisition had not parked when the request was answered: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the hold noticed the park instead of waiting out `stream.pending.max_wait_secs`: {elapsed:?}"
    );
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn a_cooling_down_track_is_404_and_starts_nothing() {
    let source = FakeSource::new(Script::CoolingDown);
    let fixture = Fixture::with_pending(source.clone()).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();
    let uri = format!("/rest/stream?id={track}&apiKey={}", fixture.api_key);

    for _ in 0..2 {
        let response = start(&app, &uri).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(collect(response).await.is_empty());
    }
    assert_eq!(source.calls(), 2, "the service asked the acquisition layer");
    assert_eq!(
        fixture.ffmpeg().in_flight(),
        0,
        "a cooling-down track starts no encoder"
    );
}

#[tokio::test]
async fn a_denied_acquisition_is_403() {
    let source = FakeSource::new(Script::Denied(FailureReason::PermissionDenied));
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();

    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(collect(response).await.is_empty());
}

/// A request over the user's daily quota is refused the same way.
#[tokio::test]
async fn an_over_quota_request_is_403() {
    let source = FakeSource::new(Script::Denied(FailureReason::QuotaExceeded));
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();

    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn a_user_without_the_acquire_permission_is_403() {
    let source = FakeSource::new(Script::Waiting);
    let fixture = Fixture::with_pending(source.clone()).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();
    let key = fixture.account_with(&[]).await;

    let response = start(&app, &format!("/rest/stream?id={track}&apiKey={key}")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(collect(response).await.is_empty());
    assert_eq!(
        source.calls(),
        0,
        "a denied user's request never reaches the acquisition layer"
    );
}

#[tokio::test]
async fn a_raw_request_is_held_until_the_file_is_ready() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture_file(dir.path(), "song.flac", 1.0).await;
    let bytes = std::fs::read(&file).unwrap();
    let fixture = Fixture::new().await;
    let source = FakeSource::new(Script::Import {
        importer: Importer::new(Arc::clone(&fixture.database), &file, 1.0),
        delay: Duration::from_millis(300),
    });
    let track = pending_track(&fixture).await;
    let app = fixture.app_with(source);

    let started = Instant::now();
    let response = start(
        &app,
        &format!(
            "/rest/stream?id={track}&format=raw&apiKey={}",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "a raw request was answered before the file was ready"
    );
    assert_eq!(header_of(&response, header::CONTENT_TYPE), "audio/flac");
    assert_eq!(collect(response).await, bytes);
}

#[tokio::test]
async fn an_available_track_without_a_file_is_404() {
    let source = FakeSource::new(Script::Available);
    let fixture = Fixture::with_pending(source).await;
    // The track is stored without a file, so the request reaches the pending
    // path; the acquisition layer reports it available, which is what a request
    // that raced an import it did not see looks like.
    let track = pending_track(&fixture).await;
    let app = fixture.app();

    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    // Nothing imported a file for this track, so it is still not found.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(collect(response).await.is_empty());
}

#[tokio::test]
async fn hold_answers_a_completed_acquisition_without_a_file_with_404() {
    let dir = tempfile::tempdir().unwrap();
    let file = fixture_file(dir.path(), "song.flac", 1.0).await;
    let source = FakeSource::new(Script::Complete {
        file,
        delay: Duration::from_millis(100),
    });
    let fixture = Fixture::with_pending(source).await;
    let track = pending_track(&fixture).await;
    let app = fixture.app();

    // The acquisition reports a file, but it was never imported into the
    // managed library, so the track still has no preferred file to serve.
    let response = start(
        &app,
        &format!("/rest/stream?id={track}&apiKey={}", fixture.api_key),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(collect(response).await.is_empty());
}
