//! Integration tests for `getCoverArt` through the router.
//!
//! Every case drives the real router over a fixture whose library holds real
//! generated audio and a stored image, which is what the image service serves
//! from. What the tests assert is the bytes a client receives, the headers that
//! tell it whether to keep them, and the HTTP status a refusal carries, because
//! this endpoint must never answer an error as an envelope.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use tower::ServiceExt;

use crate::common::Fixture;
use crate::common::artwork::{Art, files_in, seed_album};

/// One response whose body is kept as bytes.
struct RawProbe {
    /// The HTTP status.
    status: StatusCode,
    /// The response's head.
    headers: HeaderMap,
    /// The response's body.
    bytes: Vec<u8>,
}

impl RawProbe {
    /// Returns a response header as text.
    fn header(&self, name: HeaderName) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Returns the declared content length.
    fn declared_length(&self) -> usize {
        self.header(CONTENT_LENGTH).unwrap().parse().unwrap()
    }
}

/// Sends a GET request through the router with the given request headers.
///
/// # Parameters
///
/// - `app`: the router under test.
/// - `uri`: the request target.
/// - `headers`: the request headers, which is how a client revalidates an image.
///
/// # Returns
///
/// The response, with its body read into memory as bytes.
async fn request(app: &Router, uri: &str, headers: &[(&str, &str)]) -> RawProbe {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let head = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    RawProbe {
        status,
        headers: head,
        bytes: bytes.to_vec(),
    }
}

/// Builds the `getCoverArt` URL of an id for the fixture's account.
///
/// # Parameters
///
/// - `fixture`: the fixture whose API key authenticates the request.
/// - `id`: the client id to ask cover art of.
///
/// # Returns
///
/// The request target.
fn url(fixture: &Fixture, id: &str) -> String {
    format!("/rest/getCoverArt?id={id}&apiKey={}", fixture.api_key)
}

/// Stores an image on an album and makes it the album's cover.
///
/// A scan writes the folder's cover into the image store and attaches it to the
/// album; this is the same write, so the endpoint is exercised against the rows
/// a real library has.
///
/// # Parameters
///
/// - `fixture`: the fixture whose artwork directory and database are written.
/// - `album`: the album the cover belongs to.
/// - `bytes`: the encoded image.
/// - `content_type`: the image's media type.
async fn store_cover(fixture: &Fixture, album: EntityId, bytes: &[u8], content_type: &str) {
    store_image(
        fixture,
        EntityKind::Album,
        album,
        bytes,
        "cover",
        content_type,
    )
    .await;
}

/// Stores an image on one entity in one role.
///
/// # Parameters
///
/// - `fixture`: the fixture whose artwork directory and database are written.
/// - `kind`: the entity kind.
/// - `entity`: the row.
/// - `bytes`: the encoded image.
/// - `role`: `cover` or `portrait`.
/// - `content_type`: the image's media type.
async fn store_image(
    fixture: &Fixture,
    kind: EntityKind,
    entity: EntityId,
    bytes: &[u8],
    role: &str,
    content_type: &str,
) {
    let store = hearthsound_library::images::ImageStore::new(fixture.artwork_dir());
    let mut conn = fixture.database.writer().acquire().await.unwrap();
    let hash = store.store(&mut conn, bytes, content_type).await.unwrap();
    hearthsound_library::images::set_file_image(&mut conn, kind, entity.uuid(), role, &hash)
        .await
        .unwrap();
}

/// A track is served the cover its album stores.
#[tokio::test]
async fn a_stored_cover_is_served_for_its_track() {
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
    let picture = seeded.embedded.clone().unwrap();
    store_cover(&fixture, seeded.album, &picture, "image/png").await;
    let app = fixture.app();

    let probe = request(&app, &url(&fixture, &seeded.first_track().to_string()), &[]).await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.header(CONTENT_TYPE), Some("image/png"));
    assert_eq!(probe.header(CACHE_CONTROL), Some("public, max-age=86400"));
    assert_eq!(probe.declared_length(), probe.bytes.len());
    let etag = probe.header(ETAG).unwrap();
    assert!(
        etag.starts_with('"') && etag.ends_with('"') && !etag.starts_with("W/"),
        "the tag is strong and quoted: {etag}"
    );
    assert_eq!(
        probe.bytes, picture,
        "the stored picture is served unchanged"
    );
}

/// A stored cover keeps the type it was stored with.
#[tokio::test]
async fn a_stored_jpeg_cover_keeps_its_type() {
    let bytes = b"\xff\xd8\xff\xe0a jpeg cover".to_vec();
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Beta", &[Art::Sidecar(bytes.clone())]).await;
    store_cover(&fixture, seeded.album, &bytes, "image/jpeg").await;

    let probe = request(
        &fixture.app(),
        &url(&fixture, &seeded.first_track().to_string()),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.header(CONTENT_TYPE), Some("image/jpeg"));
    assert_eq!(probe.header(CACHE_CONTROL), Some("public, max-age=86400"));
    assert_eq!(probe.bytes, bytes);
}

/// A stored cover is scaled to the requested size and kept in the thumbnail
/// cache.
#[tokio::test]
async fn a_stored_cover_is_scaled_to_the_requested_size() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Epsilon",
        &[Art::Embedded {
            size: 1000,
            rgb: 0x00_ff00,
        }],
    )
    .await;
    let picture = seeded.embedded.clone().unwrap();
    store_cover(&fixture, seeded.album, &picture, "image/png").await;
    let app = fixture.app();
    let id = seeded.first_track().to_string();

    let scaled = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={id}&size=96&apiKey={}",
            fixture.api_key
        ),
        &[],
    )
    .await;
    assert_eq!(scaled.status, StatusCode::OK);
    assert_eq!(scaled.header(CONTENT_TYPE), Some("image/jpeg"));
    let (width, height) = jpeg_dimensions(&scaled.bytes);
    assert!(
        width <= 96 && height <= 96,
        "a 96-pixel request is served at most 96 pixels, got {width}x{height}"
    );

    let cached = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={id}&size=96&apiKey={}",
            fixture.api_key
        ),
        &[],
    )
    .await;
    assert_eq!(
        cached.bytes, scaled.bytes,
        "a second request is served from the cache"
    );
    let files = files_in(&fixture.thumbs_dir());
    assert_eq!(
        files.len(),
        1,
        "the cache holds one scaled image and no scratch copy, got {files:?}"
    );
    assert!(
        files[0].ends_with("-96.jpg"),
        "the cached file is named for the size it holds, got {files:?}"
    );

    let full = request(&app, &url(&fixture, &id), &[]).await;
    assert_eq!(
        full.bytes, picture,
        "a request without a size is served the bytes on disk"
    );
}

/// Returns the dimensions of a JPEG's frame header.
///
/// # Parameters
///
/// - `bytes`: the encoded JPEG.
///
/// # Returns
///
/// The width and height the first start-of-frame segment declares.
///
/// # Panics
///
/// Panics when the bytes carry no start-of-frame segment, which is a test's own
/// arrangement mistake.
fn jpeg_dimensions(bytes: &[u8]) -> (u32, u32) {
    let mut index = 2;
    while index + 9 < bytes.len() {
        assert_eq!(bytes[index], 0xff, "a marker starts with 0xff");
        let marker = bytes[index + 1];
        let length = usize::from(u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]));
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let height = u32::from(u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]));
            let width = u32::from(u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]));
            return (width, height);
        }
        index += 2 + length;
    }
    panic!("the image carries no start-of-frame segment");
}

#[tokio::test]
async fn a_request_that_already_has_the_image_is_answered_with_304() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Alpha",
        &[Art::Embedded {
            size: 32,
            rgb: 0x00_ff00,
        }],
    )
    .await;
    let picture = seeded.embedded.clone().unwrap();
    store_cover(&fixture, seeded.album, &picture, "image/png").await;
    let app = fixture.app();
    let uri = url(&fixture, &seeded.first_track().to_string());

    let first = request(&app, &uri, &[]).await;
    let etag = first.header(ETAG).unwrap().to_owned();

    let fresh = request(&app, &uri, &[("if-none-match", &etag)]).await;
    assert_eq!(fresh.status, StatusCode::NOT_MODIFIED);
    assert!(fresh.bytes.is_empty());
    assert_eq!(fresh.header(ETAG), Some(etag.as_str()));
    assert_eq!(fresh.header(CACHE_CONTROL), Some("public, max-age=86400"));

    let weak = format!("W/{etag}");
    let weak_match = request(&app, &uri, &[("if-none-match", &weak)]).await;
    assert_eq!(
        weak_match.status,
        StatusCode::NOT_MODIFIED,
        "a weak copy of the tag is fresh for a GET"
    );

    let other = request(&app, &uri, &[("if-none-match", "\"other\"")]).await;
    assert_eq!(other.status, StatusCode::OK);
    assert_eq!(other.bytes, first.bytes);
}

/// An album is served its cover and an artist its portrait; an artist without
/// one has no image.
#[tokio::test]
async fn an_album_and_an_artist_are_served_their_stored_image() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Gamma",
        &[
            Art::None,
            Art::Embedded {
                size: 32,
                rgb: 0x00ff_0000,
            },
        ],
    )
    .await;
    let picture = seeded.embedded.clone().unwrap();
    store_cover(&fixture, seeded.album, &picture, "image/png").await;
    let app = fixture.app();

    let album = request(&app, &url(&fixture, &seeded.album.to_string()), &[]).await;
    assert_eq!(album.status, StatusCode::OK);
    assert_eq!(
        album.bytes, picture,
        "the album is served the cover it stores"
    );

    // An artist has no portrait of its own here, so the request is a 404 rather
    // than another entity's cover.
    let artist = request(&app, &url(&fixture, &seeded.artist.to_string()), &[]).await;
    assert_eq!(artist.status, StatusCode::NOT_FOUND);

    store_image(
        &fixture,
        EntityKind::Artist,
        seeded.artist,
        &picture,
        "portrait",
        "image/png",
    )
    .await;
    let portrait = request(&app, &url(&fixture, &seeded.artist.to_string()), &[]).await;
    assert_eq!(portrait.status, StatusCode::OK);
    assert_eq!(
        portrait.bytes, picture,
        "an artist is served the portrait it stores"
    );
}

#[tokio::test]
async fn an_entity_without_art_is_not_found() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Delta", &[Art::None]).await;
    let app = fixture.app();

    for id in [seeded.first_track(), seeded.album, seeded.artist] {
        let probe = request(&app, &url(&fixture, &id.to_string()), &[]).await;
        assert_eq!(probe.status, StatusCode::NOT_FOUND, "{id}");
        assert!(probe.bytes.is_empty());
    }
}

/// A refusal the client revalidates: the entity may gain an image later.
#[tokio::test]
async fn an_entity_without_art_is_answered_with_a_revalidating_not_found() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Delta", &[Art::None]).await;
    let app = fixture.app();

    for id in [seeded.first_track(), seeded.album, seeded.artist] {
        let probe = request(&app, &url(&fixture, &id.to_string()), &[]).await;
        assert_eq!(probe.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(
            probe.header(CACHE_CONTROL),
            Some("no-cache"),
            "the client revalidates rather than caching the refusal"
        );
    }
}

#[tokio::test]
async fn a_missing_id_is_400_and_an_unknown_id_is_404() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let missing = request(
        &app,
        &format!("/rest/getCoverArt?apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert!(missing.bytes.is_empty());

    let empty = request(
        &app,
        &format!("/rest/getCoverArt?id=&apiKey={}", fixture.api_key),
        &[],
    )
    .await;
    assert_eq!(empty.status, StatusCode::BAD_REQUEST);

    for id in [
        "nope".to_owned(),
        "tr_0000".to_owned(),
        EntityId::new(IdPrefix::Track).to_string(),
        EntityId::new(IdPrefix::Playlist).to_string(),
    ] {
        let probe = request(&app, &url(&fixture, &id), &[]).await;
        assert_eq!(probe.status, StatusCode::NOT_FOUND, "{id}");
        assert!(probe.bytes.is_empty());
    }
}

#[tokio::test]
async fn an_unusable_size_is_400() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Alpha",
        &[Art::Embedded {
            size: 32,
            rgb: 0x00_00ff,
        }],
    )
    .await;

    let probe = request(
        &fixture.app(),
        &format!(
            "/rest/getCoverArt?id={}&size=wide&apiKey={}",
            seeded.first_track(),
            fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn a_request_without_credentials_is_401() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Alpha", &[Art::None]).await;
    let app = fixture.app();

    let anonymous = request(
        &app,
        &format!("/rest/getCoverArt?id={}", seeded.first_track()),
        &[],
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert!(anonymous.bytes.is_empty());

    let wrong = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={}&apiKey=not-a-key",
            seeded.first_track()
        ),
        &[],
    )
    .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert!(wrong.bytes.is_empty());
}

/// A cover the encoder cannot read is refused, and no scratch file is left.
///
/// The stored bytes are what a client asked for, so a broken image is answered
/// `503` with the delay the client waits before asking again; the failed encode
/// leaves nothing behind in the thumbnail cache.
#[tokio::test]
async fn a_broken_cover_is_refused_without_leaving_a_scratch_file() {
    let fixture = Fixture::new().await;
    let bytes = b"this is not an image".to_vec();
    let seeded = seed_album(&fixture, "Broken", &[Art::Sidecar(bytes.clone())]).await;
    store_cover(&fixture, seeded.album, &bytes, "image/png").await;

    let probe = request(
        &fixture.app(),
        &format!(
            "/rest/getCoverArt?id={}&size=64&apiKey={}",
            seeded.first_track(),
            fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(probe.header(RETRY_AFTER), Some("5"));
    assert!(probe.bytes.is_empty());
    let leftovers: Vec<String> = std::fs::read_dir(fixture.thumbs_dir())
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp") || name.ends_with(".src"))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "the failed encode left {leftovers:?}");
}
