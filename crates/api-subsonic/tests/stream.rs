//! Integration tests for `stream`: the whole file, the ranges and validators a
//! seeking client uses, and the status codes a failure is answered with.
//!
//! Every test goes through the real router, so the credential check, the
//! endpoint, and the stream service are all exercised, and every body is
//! compared with the bytes of a real audio file generated at test time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use axum::http::{HeaderName, Method, StatusCode, header};
use hearthsound_core::{Availability, CatalogId, EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, Origin, ScannedMediaFile, TrackInput,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};
use uuid::Uuid;

use common::{Fixture, request};

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

/// Builds a fixture with one track and one real mp3 file behind it.
async fn streamed() -> Streamed {
    let fixture = Fixture::new().await;
    let owned = fixture.subdir("owned");
    let path = AudioFixture::new(AudioFormat::Mp3Cbr)
        .seconds(1.0)
        .bitrate_kbps(Some(128))
        .file_name("streamed.mp3")
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
        .upsert_album(
            &AlbumInput::new("Streaming", Origin::Local),
            std::slice::from_ref(&credit),
        )
        .await
        .unwrap();
    let mut input = TrackInput::new("Streamed", Origin::Local, Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some("Echo".to_owned());
    input.track_number = Some(1);
    input.disc_number = Some(1);
    input.duration_ms = Some(1_000);
    let track = library
        .insert_track(&input, std::slice::from_ref(&credit), &[])
        .await
        .unwrap();
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: "streamed.mp3".to_owned(),
                size: metadata.len() as i64,
                mtime_ms: metadata
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64,
                format: Some("mpeg".to_owned()),
                codec: Some("mp3".to_owned()),
                bitrate_kbps: Some(128),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some(1_000),
                lossless: Some(false),
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

/// Returns a header of a response as text.
fn header_of(probe: &common::Probe, name: HeaderName) -> String {
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
async fn stream_answers_a_catalog_id_with_404() {
    let streamed = streamed().await;
    let app = streamed.fixture.app();
    let id = CatalogId::new(Slug::new("hello").unwrap(), EntityKind::Track, "hello-1").unwrap();

    let probe = request(
        &app,
        Method::GET,
        &format!("/rest/stream?id={id}&apiKey={}", streamed.fixture.api_key),
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
        .upsert_album(
            &AlbumInput::new("Empty", Origin::Local),
            std::slice::from_ref(&credit),
        )
        .await
        .unwrap();
    let mut input = TrackInput::new("Pending", Origin::Local, Availability::Remote);
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
