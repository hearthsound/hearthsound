//! Integration tests for `stream`: the whole file, the ranges and validators a
//! seeking client uses, and the status codes a failure is answered with.
//!
//! Every test goes through the real router, so the credential check, the
//! endpoint, and the stream service are all exercised, and every body is
//! compared with the bytes of a real audio file generated at test time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use axum::http::{HeaderName, Method, StatusCode, header};
use hearthsound_core::{Availability, EntityId, IdPrefix};
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, ScannedMediaFile, TrackInput,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use uuid::Uuid;

use crate::common::{Fixture, request};

/// A fixture with one stored track whose file exists on disk.
struct Streamed {
    /// The fixture, which keeps its temporary directory alive.
    fixture: Fixture,
    /// The id of the track.
    track: EntityId,
    /// The path of the file the track's row points at.
    path: PathBuf,
    /// The bytes of that file.
    bytes: Vec<u8>,
}

impl Streamed {
    /// Returns the request target that streams the track.
    fn uri(&self) -> String {
        format!(
            "/rest/stream?id={}&apiKey={}",
            self.track, self.fixture.api_key
        )
    }
}

/// Builds a fixture with one track and one real file behind it.
///
/// # Parameters
///
/// - `format`: the audio format to generate.
/// - `seconds`: the file's duration.
/// - `container`: the container literal a probe would record for the file.
/// - `codec`: the codec literal a probe would record for the file.
/// - `bitrate_kbps`: the bitrate a probe would record for the file.
async fn streamed_as(
    format: AudioFormat,
    seconds: f64,
    container: &str,
    codec: &str,
    bitrate_kbps: u32,
) -> Streamed {
    let fixture = Fixture::new().await;
    let owned = fixture.subdir("owned");
    let name = format!("streamed.{}", format.extension());
    let path = AudioFixture::new(format)
        .seconds(seconds)
        .bitrate_kbps(Some(bitrate_kbps))
        .file_name(name.clone())
        .write_into(&owned)
        .await
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let metadata = std::fs::metadata(&path).unwrap();

    let library = LibraryRepository::new(&fixture.database);
    let folder = library
        .add_library_folder(&owned, Some(fixture.user_id.uuid()))
        .await
        .unwrap();
    let credit = ArtistCredit::new("Echo", "main");
    let album = library
        .upsert_album(&AlbumInput::new("Streaming"), std::slice::from_ref(&credit))
        .await
        .unwrap();
    let mut input = TrackInput::new("Streamed", Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some("Echo".to_owned());
    input.track_number = Some(1);
    input.disc_number = Some(1);
    input.duration_ms = Some((seconds * 1000.0) as i64);
    let track = library
        .insert_track(&input, std::slice::from_ref(&credit), &[])
        .await
        .unwrap();
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: name,
                size: metadata.len() as i64,
                mtime_ms: metadata
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
                format: Some(container.to_owned()),
                codec: Some(codec.to_owned()),
                bitrate_kbps: Some(i64::from(bitrate_kbps)),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some((seconds * 1000.0) as i64),
                lossless: Some(false),
                replaygain: None,
                has_embedded_art: Some(false),
            },
        )
        .await
        .unwrap();

    Streamed {
        fixture,
        track,
        path,
        bytes,
    }
}

/// Builds a fixture with one track whose file is a one second 128 kbit/s mp3.
async fn streamed() -> Streamed {
    streamed_as(AudioFormat::Mp3Cbr, 1.0, "mpeg", "mp3", 128).await
}

/// Reads one ffprobe field of a container or its streams.
///
/// # Parameters
///
/// - `path`: the file to inspect.
/// - `section`: `format` or `stream`.
/// - `field`: the field name, for example `duration`.
async fn ffprobe(path: &std::path::Path, section: &str, field: &str) -> Option<String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            &format!("{section}={field}"),
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}=")))
        .map(str::to_owned)
}

/// Writes one response body into a temporary file and returns the path.
///
/// The directory is returned so the caller keeps it alive.
fn write_body(bytes: &[u8], name: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    (dir, path)
}

/// Returns a header of a response as text.
fn header_of(probe: &crate::common::Probe, name: HeaderName) -> String {
    probe
        .headers
        .get(&name)
        .unwrap_or_else(|| panic!("the response has no {name} header"))
        .to_str()
        .unwrap()
        .to_owned()
}

/// Waits for the detached last-access record to land.
///
/// The stream service records the access on its own task, so a test polls the
/// row instead of asserting immediately.
async fn wait_for_access(fixture: &Fixture, track: EntityId) -> Option<i64> {
    let library = LibraryRepository::new(&fixture.database);
    for _ in 0..500 {
        let files = library.list_media_files_for_track(track).await.unwrap();
        if let Some(at) = files.first().and_then(|file| file.last_accessed_at) {
            return Some(at);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    None
}

#[tokio::test]
async fn stream_serves_the_whole_file() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(&app, Method::GET, &streamed.uri(), &[]).await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.bytes, streamed.bytes);
    assert_eq!(header_of(&probe, header::CONTENT_TYPE), "audio/mpeg");
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "bytes");
    assert_eq!(
        header_of(&probe, header::CONTENT_LENGTH),
        streamed.bytes.len().to_string()
    );
    assert!(probe.headers.contains_key(header::ETAG));
    assert!(probe.headers.contains_key(header::LAST_MODIFIED));
}

#[tokio::test]
async fn stream_answers_a_range_with_206() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let size = streamed.bytes.len();

    let probe = request(
        &app,
        Method::GET,
        &streamed.uri(),
        &[("Range", "bytes=4-9")],
    )
    .await;

    assert_eq!(probe.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(probe.bytes, streamed.bytes[4..10]);
    assert_eq!(
        header_of(&probe, header::CONTENT_RANGE),
        format!("bytes 4-9/{size}")
    );
    assert_eq!(header_of(&probe, header::CONTENT_LENGTH), "6");
}

#[tokio::test]
async fn stream_answers_a_suffix_range() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let size = streamed.bytes.len();

    let probe = request(&app, Method::GET, &streamed.uri(), &[("Range", "bytes=-5")]).await;

    assert_eq!(probe.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(probe.bytes, streamed.bytes[size - 5..]);
    assert_eq!(
        header_of(&probe, header::CONTENT_RANGE),
        format!("bytes {}-{}/{size}", size - 5, size - 1)
    );
}

#[tokio::test]
async fn stream_refuses_a_range_past_the_end() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let size = streamed.bytes.len();

    let range = format!("bytes={size}-");
    let probe = request(&app, Method::GET, &streamed.uri(), &[("Range", &range)]).await;

    assert_eq!(probe.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        header_of(&probe, header::CONTENT_RANGE),
        format!("bytes */{size}")
    );
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_a_replayed_etag_with_304() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let first = request(&app, Method::GET, &streamed.uri(), &[]).await;
    let etag = header_of(&first, header::ETAG);

    let probe = request(
        &app,
        Method::GET,
        &streamed.uri(),
        &[("If-None-Match", &etag)],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_MODIFIED);
    assert!(probe.bytes.is_empty());
    assert_eq!(header_of(&probe, header::ETAG), etag);
}

#[tokio::test]
async fn stream_answers_a_replayed_last_modified_with_304() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let first = request(&app, Method::GET, &streamed.uri(), &[]).await;
    let modified = header_of(&first, header::LAST_MODIFIED);

    let probe = request(
        &app,
        Method::GET,
        &streamed.uri(),
        &[("If-Modified-Since", &modified)],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_MODIFIED);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_serves_a_head_without_a_body() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(&app, Method::HEAD, &streamed.uri(), &[]).await;

    assert_eq!(probe.status, StatusCode::OK);
    assert!(probe.bytes.is_empty());
    assert_eq!(
        header_of(&probe, header::CONTENT_LENGTH),
        streamed.bytes.len().to_string()
    );
    assert_eq!(header_of(&probe, header::CONTENT_TYPE), "audio/mpeg");
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "bytes");
}

#[tokio::test]
async fn stream_serves_a_range_only_when_if_range_matches() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let first = request(&app, Method::GET, &streamed.uri(), &[]).await;
    let etag = header_of(&first, header::ETAG);

    let stale = request(
        &app,
        Method::GET,
        &streamed.uri(),
        &[("Range", "bytes=4-9"), ("If-Range", "\"stale\"")],
    )
    .await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.bytes, streamed.bytes);

    let fresh = request(
        &app,
        Method::GET,
        &streamed.uri(),
        &[("Range", "bytes=4-9"), ("If-Range", &etag)],
    )
    .await;
    assert_eq!(fresh.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(fresh.bytes, streamed.bytes[4..10]);
}

#[tokio::test]
async fn stream_registers_a_session_and_records_the_access() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(&app, Method::GET, &streamed.uri(), &[]).await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.bytes, streamed.bytes);

    let delivered = streamed
        .fixture
        .state
        .activity()
        .sessions()
        .audio_delivered(streamed.fixture.user_id.uuid(), streamed.track.uuid());
    assert_eq!(delivered, Some(true));

    assert!(
        wait_for_access(&streamed.fixture, streamed.track)
            .await
            .is_some(),
        "the served file's last access was not recorded"
    );
    assert!(streamed.path.exists());
}

#[tokio::test]
async fn stream_does_not_register_a_session_for_a_head() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(&app, Method::HEAD, &streamed.uri(), &[]).await;
    assert_eq!(probe.status, StatusCode::OK);

    let delivered = streamed
        .fixture
        .state
        .activity()
        .sessions()
        .audio_delivered(streamed.fixture.user_id.uuid(), streamed.track.uuid());
    assert_eq!(delivered, None);
}

#[tokio::test]
async fn stream_requires_an_id() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("/rest/stream?apiKey={}", streamed.fixture.api_key),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_requires_credentials() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("/rest/stream?id={}", streamed.track),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::UNAUTHORIZED);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_a_malformed_id_with_404() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!(
            "/rest/stream?id=not-an-id&apiKey={}",
            streamed.fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_FOUND);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_an_unknown_track_with_404() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let unknown = EntityId::from_uuid(IdPrefix::Track, Uuid::now_v7());

    let probe = request(
        &app,
        Method::GET,
        &format!(
            "/rest/stream?id={unknown}&apiKey={}",
            streamed.fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_FOUND);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_a_provider_id_with_404() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    // A provider id is not an entity id: every entity the server knows has a
    // row, so a client that kept one answers 404 rather than a stream.
    let probe = request(
        &app,
        Method::GET,
        &format!(
            "/rest/stream?id=x_hello_t_hello-1&apiKey={}",
            streamed.fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_FOUND);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_a_track_without_a_file_with_404() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let library = LibraryRepository::new(&streamed.fixture.database);
    let credit = ArtistCredit::new("Echo", "main");
    let album = library
        .upsert_album(&AlbumInput::new("Empty"), std::slice::from_ref(&credit))
        .await
        .unwrap();
    let mut input = TrackInput::new("Pending", Availability::Remote);
    input.album_id = Some(album);
    let pending = library
        .insert_track(&input, std::slice::from_ref(&credit), &[])
        .await
        .unwrap();

    let probe = request(
        &app,
        Method::GET,
        &format!(
            "/rest/stream?id={pending}&apiKey={}",
            streamed.fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_FOUND);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_transcodes_to_the_requested_format() {
    let streamed = streamed_as(AudioFormat::Flac, 2.0, "flac", "flac", 1012).await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&format=mp3", streamed.uri()),
        // A range is not honored for a transcode: the whole stream is sent.
        &[("Range", "bytes=0-99")],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(header_of(&probe, header::CONTENT_TYPE), "audio/mpeg");
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "none");
    assert!(!probe.headers.contains_key(header::CONTENT_LENGTH));

    let (_dir, path) = write_body(&probe.bytes, "transcoded.mp3");
    assert_eq!(
        ffprobe(&path, "stream", "codec_name").await.as_deref(),
        Some("mp3")
    );
    let duration: f64 = ffprobe(&path, "format", "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    assert!(
        (1.7..=2.3).contains(&duration),
        "the transcode produced {duration}s from a two second file"
    );
}

#[tokio::test]
async fn stream_transcodes_a_bitrate_cap_without_a_format() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&maxBitRate=64", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(header_of(&probe, header::CONTENT_TYPE), "audio/mpeg");
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "none");

    let (_dir, path) = write_body(&probe.bytes, "capped.mp3");
    let bitrate: f64 = ffprobe(&path, "format", "bit_rate")
        .await
        .expect("ffprobe reports a bitrate")
        .parse()
        .unwrap();
    assert!(
        bitrate < 100_000.0,
        "the cap of 64 kbit/s produced {bitrate} bit/s from a 128 kbit/s file"
    );
}

#[tokio::test]
async fn stream_transcodes_a_time_offset() {
    let streamed = streamed_as(AudioFormat::Flac, 3.0, "flac", "flac", 1012).await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&format=mp3&timeOffset=2", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    let (_dir, path) = write_body(&probe.bytes, "offset.mp3");
    let duration: f64 = ffprobe(&path, "format", "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    assert!(
        (0.6..=1.4).contains(&duration),
        "an offset of two seconds left {duration}s of a three second file"
    );
}

/// An encoder that cannot decode the file is answered 500, not an empty 200.
#[tokio::test]
async fn a_failing_encoder_is_answered_internal_error() {
    let streamed = streamed_as(AudioFormat::Flac, 2.0, "flac", "flac", 1012).await;
    // The file the row points at is real audio; bytes no decoder reads make
    // ffmpeg fail before it produces a single frame.
    std::fs::write(&streamed.path, b"not audio at all").unwrap();
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&format=mp3", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(
        probe.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a failed encode is a failed request: {}",
        String::from_utf8_lossy(&probe.bytes)
    );
}

#[tokio::test]
async fn a_transcode_never_declares_a_length() {
    let streamed = streamed_as(AudioFormat::Flac, 2.0, "flac", "flac", 1012).await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!(
            "{}&format=mp3&maxBitRate=96&estimateContentLength=true",
            streamed.uri()
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert!(
        !probe.headers.contains_key(header::CONTENT_LENGTH),
        "an encoded body is chunked, because its size is only known when the encoder exits"
    );
    assert!(
        !probe.bytes.is_empty(),
        "the answer carries the encoded audio"
    );

    // The head of a transcode starts no encoder, so it declares nothing about
    // the encoded stream; the `0` hyper writes describes the empty HEAD body.
    let head = request(
        &app,
        Method::HEAD,
        &format!("{}&format=mp3&estimateContentLength=true", streamed.uri()),
        &[],
    )
    .await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(
        header_of(&head, header::CONTENT_LENGTH),
        "0",
        "the head of a transcode declares no size for the encoded stream"
    );
    assert!(head.bytes.is_empty(), "a head carries no body");
}

#[tokio::test]
async fn stream_serves_the_file_while_the_cap_is_not_below_the_bitrate() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&maxBitRate=192", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.bytes, streamed.bytes);
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "bytes");
}

#[tokio::test]
async fn stream_serves_the_file_for_raw() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&format=raw&maxBitRate=64", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.bytes, streamed.bytes);
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "bytes");
}

#[tokio::test]
async fn stream_requires_a_format_the_server_can_encode() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&format=flac", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_requires_a_numeric_bitrate_cap() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&maxBitRate=lots", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_rejects_an_unusable_time_offset() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::GET,
        &format!("{}&timeOffset=-3", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn stream_answers_a_transcode_head_without_starting_ffmpeg() {
    let streamed = streamed_as(AudioFormat::Flac, 2.0, "flac", "flac", 1012).await;
    let app = streamed.fixture.app();

    let probe = request(
        &app,
        Method::HEAD,
        &format!("{}&format=opus", streamed.uri()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(header_of(&probe, header::CONTENT_TYPE), "audio/ogg");
    assert_eq!(header_of(&probe, header::ACCEPT_RANGES), "none");
    assert!(probe.bytes.is_empty());
}
