//! Integration tests for `download`: the stored file a permitted account
//! receives, the headers that tell a client how to save it, and the refusals.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE, ETAG, RANGE};
use axum::http::{HeaderMap, Request, StatusCode};
use tower::ServiceExt;

use hearthsound_auth::{ApiKeys, NewUser, Users};
use hearthsound_config::{SettingValue, keys};

use crate::common::artwork::{Art, seed_album};
use crate::common::{Fixture, get};

/// Creates an account with the given permissions and returns its API key.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database the account is written to.
/// - `username`: the login name.
/// - `permissions`: the permission names the account is created with.
///
/// # Returns
///
/// The account's API key.
async fn account(fixture: &Fixture, username: &str, permissions: &[&str]) -> String {
    fixture
        .settings
        .set(
            keys::USERS_DEFAULT_PERMISSIONS.name(),
            SettingValue::StringList(permissions.iter().map(|name| (*name).to_owned()).collect()),
            None,
        )
        .await
        .unwrap();
    let user = Users::new(&fixture.database)
        .create(
            &fixture.settings.current(),
            &NewUser::new(username, username, "hunter2", false),
        )
        .await
        .unwrap();
    ApiKeys::new(&fixture.database)
        .generate(user.id, username)
        .await
        .unwrap()
        .plaintext
}

/// One response whose body is kept as bytes.
struct RawProbe {
    /// The HTTP status.
    status: StatusCode,
    /// The response's head.
    headers: HeaderMap,
    /// The response's body.
    bytes: Vec<u8>,
}

/// Sends a GET request through the router with the given request headers.
///
/// # Parameters
///
/// - `app`: the router under test.
/// - `uri`: the request target.
/// - `headers`: the request headers.
///
/// # Returns
///
/// The response, with its body read into memory.
async fn request(app: &Router, uri: &str, headers: &[(&str, &str)]) -> RawProbe {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    RawProbe {
        status,
        headers,
        bytes: bytes.to_vec(),
    }
}

/// The endpoint serves the stored file as an attachment.
#[tokio::test]
async fn the_stored_file_is_served_as_an_attachment() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Alpha",
        &[Art::Embedded {
            size: 64,
            rgb: 0x0033_66ff,
        }],
    )
    .await;
    let app = fixture.app();
    let track = seeded.first_track();

    let probe = request(
        &app,
        &format!("/rest/download?id={track}&apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(
        probe.headers.get(CONTENT_TYPE).unwrap(),
        "audio/mpeg",
        "the stored file's type"
    );
    let disposition = probe
        .headers
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        disposition.starts_with("attachment; filename=\"tr_"),
        "the file is offered as an attachment: {disposition}"
    );
    assert!(
        disposition.contains("filename*=UTF-8''tr_"),
        "the encoded name is offered too: {disposition}"
    );
    assert!(probe.headers.contains_key(ETAG), "the file carries a tag");
    assert!(!probe.bytes.is_empty(), "the file's bytes are served");

    // A range is answered with the part a resuming client asked for.
    let part = request(
        &app,
        &format!("/rest/download?id={track}&apiKey={}", fixture.api_key),
        &[(RANGE.as_str(), "bytes=0-9")],
    )
    .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.bytes.len(), 10);
    assert_eq!(
        part.bytes,
        probe.bytes[..10].to_vec(),
        "the part is the file's first ten bytes"
    );
}

/// A download is not playback, so no stream session is registered.
#[tokio::test]
async fn a_download_registers_no_stream_session() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Alpha", &[Art::None]).await;
    let app = fixture.app();
    let track = seeded.first_track();

    let probe = request(
        &app,
        &format!("/rest/download?id={track}&apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(
        fixture
            .state
            .activity()
            .sessions()
            .audio_delivered(fixture.user_id.uuid(), track.uuid()),
        None,
        "a download is not a stream session"
    );
}

/// A catalog id, an album id, and an unknown id are all refused.
#[tokio::test]
async fn only_a_stored_track_is_downloaded() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Alpha", &[Art::None]).await;
    let app = fixture.app();

    let album = request(
        &app,
        &format!(
            "/rest/download?id={}&apiKey={}",
            seeded.album, fixture.api_key
        ),
        &[],
    )
    .await;
    assert_eq!(album.status, StatusCode::NOT_FOUND);
    let catalog = request(
        &app,
        &format!("/rest/download?id=x_hello_t_abc&apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(
        catalog.status,
        StatusCode::NOT_FOUND,
        "an unmaterialized catalog track has no file to download"
    );
    let malformed = request(
        &app,
        &format!("/rest/download?id=nonsense&apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(malformed.status, StatusCode::NOT_FOUND);
    let missing = request(
        &app,
        &format!("/rest/download?apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
}

/// An account without the download permission is refused.
#[tokio::test]
async fn the_download_permission_gates_the_endpoint() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Alpha", &[Art::None]).await;
    let limited = account(&fixture, "limited", &[]).await;
    let app = fixture.app();
    let track = seeded.first_track();

    let refused = request(
        &app,
        &format!("/rest/download?id={track}&apiKey={limited}"),
        &[],
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // The same account with the permission is served.
    let permitted = account(&fixture, "permitted", &["download"]).await;
    let served = request(
        &app,
        &format!("/rest/download?id={track}&apiKey={permitted}"),
        &[],
    )
    .await;
    assert_eq!(served.status, StatusCode::OK);

    // The envelope endpoint answers an envelope whatever the account may
    // download, which is the difference between it and this one.
    let envelope = get(
        &app,
        &format!("/rest/getSong?id={track}&apiKey={limited}&f=json"),
    )
    .await;
    assert!(
        envelope.body.contains("subsonic-response"),
        "{}",
        envelope.body
    );
}
