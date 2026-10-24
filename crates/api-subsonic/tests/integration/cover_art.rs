//! Integration tests for `getCoverArt` through the router.
//!
//! Every case drives the real router over a fixture whose library holds real
//! generated audio with artwork, and over fixture catalog plugins pointed at a
//! local mock server. What the tests assert is the bytes a client receives, the
//! headers that tell it whether to keep them, and the HTTP status a refusal
//! carries, because this endpoint must never answer an error as an envelope.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, RETRY_AFTER};
use axum::http::{HeaderMap, HeaderName, Request, StatusCode};
use hearthsound_core::{CatalogId, EntityId, EntityKind, IdPrefix, Layer, Slug};
use hearthsound_test_support::{HELLO_CATALOG, HTTP_CATALOG};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::artwork::{Art, files_in, seed_album};
use crate::common::{Fixture, artwork_server};

/// The bytes the mock server serves as an image.
const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

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

/// Builds a catalog id.
///
/// # Parameters
///
/// - `slug`: the issuing instance.
/// - `kind`: the entity kind.
/// - `provider_id`: the provider's own id.
///
/// # Returns
///
/// The `x_` id as a client sends it.
fn catalog_id(slug: &str, kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new(slug).unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

/// Builds an environment whose `hello` instance serves images the mock server
/// has, one per size the provider describes.
async fn caching_env(sizes: &[u32]) -> (Fixture, MockServer) {
    let server = MockServer::start().await;
    for size in sizes {
        Mock::given(method("GET"))
            .and(path(format!("/cover-{size}.png")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_bytes(IMAGE),
            )
            .mount(&server)
            .await;
    }
    let fixture = Fixture::new().await;
    fixture
        .install_catalog_instance(
            "hello",
            HELLO_CATALOG,
            json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
        )
        .await;
    fixture.assign_layer(Layer::Catalog, &["hello"]).await;
    (fixture, server)
}

#[tokio::test]
async fn embedded_art_of_a_stored_track_is_served() {
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
        probe.bytes,
        seeded.embedded.clone().unwrap(),
        "the embedded picture is served unchanged"
    );
}

#[tokio::test]
async fn a_sidecar_image_beside_the_file_is_served() {
    let bytes = b"\xff\xd8\xff\xe0a jpeg beside the track".to_vec();
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Beta", &[Art::Sidecar(bytes.clone())]).await;

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

/// A local cover is scaled to the requested size and cached on disk.
#[tokio::test]
async fn a_local_cover_is_scaled_to_the_requested_size() {
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
    let original = seeded.embedded.clone().unwrap();
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
        full.bytes, original,
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

#[tokio::test]
async fn an_album_and_an_artist_are_served_the_first_track_that_has_art() {
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
    let app = fixture.app();
    let picture = seeded.embedded.clone().unwrap();

    let album = request(&app, &url(&fixture, &seeded.album.to_string()), &[]).await;
    assert_eq!(album.status, StatusCode::OK);
    assert_eq!(
        album.bytes, picture,
        "the album is served the first of its tracks that has an image"
    );

    // An artist has no image of its own, so the local fallback walks its albums
    // and serves the first image it finds. No artwork instance is configured
    // here, which is the case this covers; the artwork layer is asked first
    // when one is.
    let artist = request(&app, &url(&fixture, &seeded.artist.to_string()), &[]).await;
    assert_eq!(artist.status, StatusCode::OK);
    assert_eq!(
        artist.bytes, picture,
        "an artist has no image of its own, so its album's is served"
    );
}

/// An artwork instance's portrait is served for a stored artist that has local
/// art of its own, because the layer is asked first for artists.
#[tokio::test]
async fn an_artwork_instance_is_asked_before_a_stored_artists_own_art() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Gamma",
        &[Art::Embedded {
            size: 32,
            rgb: 0x00ff_0000,
        }],
    )
    .await;
    let portrait = fixture
        .install_artwork_layer("/portrait.png", &["Gamma Artist"])
        .await;
    let app = fixture.app();

    let artist = request(&app, &url(&fixture, &seeded.artist.to_string()), &[]).await;
    assert_eq!(artist.status, StatusCode::OK);
    assert_eq!(
        artist.bytes, portrait,
        "the artwork instance's portrait is what a stored artist is served"
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

/// A refusal the client can stop repeating for an hour.
#[tokio::test]
async fn an_entity_without_art_is_answered_with_a_cacheable_not_found() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(&fixture, "Delta", &[Art::None]).await;
    let app = fixture.app();

    for id in [seeded.first_track(), seeded.album, seeded.artist] {
        let probe = request(&app, &url(&fixture, &id.to_string()), &[]).await;
        assert_eq!(probe.status, StatusCode::NOT_FOUND, "{id}");
        assert_eq!(
            probe.header(CACHE_CONTROL),
            Some("private, max-age=3600"),
            "a grid of cards asks for the same missing image only once an hour"
        );
    }
}

/// An artist's local cover stands in for a portrait the image host refused.
#[tokio::test]
async fn an_artist_whose_portrait_host_refuses_is_served_its_local_art() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Gamma",
        &[Art::Embedded {
            size: 32,
            rgb: 0x00ff_0000,
        }],
    )
    .await;
    let picture = seeded.embedded.clone().unwrap();
    let server = artwork_server("/portrait.png", 403).await;
    fixture
        .install_artwork_layer_at(&server, "/portrait.png", &["Gamma Artist"])
        .await;
    let app = fixture.app();

    let artist = request(&app, &url(&fixture, &seeded.artist.to_string()), &[]).await;
    assert_eq!(
        artist.status,
        StatusCode::OK,
        "a refused portrait falls back to local art instead of a refusal"
    );
    assert_eq!(artist.bytes, picture);
    assert_eq!(
        artist.header(CACHE_CONTROL),
        Some("public, max-age=86400"),
        "a picture the host says it does not have is not retried soon"
    );
}

/// A URL the host refused is not asked again within the refusal window.
#[tokio::test]
async fn a_refused_image_url_is_not_asked_again_at_once() {
    let fixture = Fixture::new().await;
    let seeded = seed_album(
        &fixture,
        "Gamma",
        &[Art::Embedded {
            size: 32,
            rgb: 0x00ff_0000,
        }],
    )
    .await;
    let picture = seeded.embedded.clone().unwrap();
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/portrait.png"))
        .respond_with(wiremock::ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;
    fixture
        .install_artwork_layer_at(&server, "/portrait.png", &["Gamma Artist"])
        .await;
    let app = fixture.app();
    let uri = url(&fixture, &seeded.artist.to_string());

    let first = request(&app, &uri, &[]).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.bytes, picture);

    let second = request(&app, &uri, &[]).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(
        second.bytes, picture,
        "the second request is answered without asking the refusing host again"
    );
    drop(server);
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

#[tokio::test]
async fn a_catalog_id_is_proxied_through_its_issuing_instance() {
    let (fixture, server) = caching_env(&[250, 500, 1_000]).await;
    let app = fixture.app();
    let id = catalog_id("hello", EntityKind::Track, "hello-track-1");

    let probe = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={id}&size=300&apiKey={}",
            fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.header(CONTENT_TYPE), Some("image/png"));
    assert_eq!(probe.header(CACHE_CONTROL), Some("public, max-age=86400"));
    assert_eq!(probe.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-500.png"],
        "the size is handed to the catalog image choice"
    );
}

#[tokio::test]
async fn a_no_store_instance_is_streamed_through_and_never_stored() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"]}]"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/remote-cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(IMAGE),
        )
        .mount(&server)
        .await;

    let fixture = Fixture::new().await;
    fixture
        .install_catalog_instance(
            "httpcat",
            HTTP_CATALOG,
            json!({
                "url": format!("{}/search", server.uri()),
                "coverUrl": format!("{}/remote-cover.png", server.uri()),
            }),
        )
        .await;
    fixture.assign_layer(Layer::Catalog, &["httpcat"]).await;
    let app = fixture.app();
    let uri = format!(
        "/rest/getCoverArt?id={}&apiKey={}",
        catalog_id("httpcat", EntityKind::Track, "remote-1"),
        fixture.api_key
    );

    let probe = request(&app, &uri, &[]).await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.bytes, IMAGE);
    assert_eq!(probe.header(CONTENT_TYPE), Some("image/png"));
    assert_eq!(
        probe.header(CACHE_CONTROL),
        Some("no-store"),
        "an instance that forbids caching tells the client not to store the image either"
    );
    assert!(
        files_in(&fixture.artwork_dir()).is_empty(),
        "a no-store image is never written to the artwork directory"
    );

    let again = request(&app, &uri, &[]).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(
        requested_paths(&server)
            .await
            .iter()
            .filter(|route| *route == "/remote-cover.png")
            .count(),
        2,
        "nothing was stored, so the second request asks the instance again"
    );
}

#[tokio::test]
async fn a_stored_entity_without_local_art_falls_back_to_its_instance() {
    let (fixture, _server) = caching_env(&[250, 500, 1_000]).await;
    let catalog = fixture.state.catalog();
    let stored = catalog
        .materialize_track(
            &CatalogId::new(
                Slug::new("hello").unwrap(),
                EntityKind::Track,
                "hello-track-1",
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stored.kind(), IdPrefix::Track);

    let probe = request(&fixture.app(), &url(&fixture, &stored.to_string()), &[]).await;

    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(
        probe.bytes, IMAGE,
        "a stored entity is served its instance's image through its p:hello alias"
    );
    assert_eq!(probe.header(CACHE_CONTROL), Some("public, max-age=86400"));
}

#[tokio::test]
async fn a_catalog_id_no_instance_serves_is_not_found() {
    let fixture = Fixture::new().await;

    let probe = request(
        &fixture.app(),
        &format!(
            "/rest/getCoverArt?id={}&apiKey={}",
            catalog_id("deezer", EntityKind::Track, "123"),
            fixture.api_key
        ),
        &[],
    )
    .await;

    assert_eq!(probe.status, StatusCode::NOT_FOUND);
    assert!(probe.bytes.is_empty());
}

#[tokio::test]
async fn an_image_the_instance_lacks_is_404_and_a_broken_one_is_503() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cover-250.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cover-500.png"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/cover-1000.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let fixture = Fixture::new().await;
    fixture
        .install_catalog_instance(
            "hello",
            HELLO_CATALOG,
            json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
        )
        .await;
    fixture.assign_layer(Layer::Catalog, &["hello"]).await;
    let app = fixture.app();
    let id = catalog_id("hello", EntityKind::Track, "hello-track-1");

    let gone = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={id}&size=250&apiKey={}",
            fixture.api_key
        ),
        &[],
    )
    .await;
    assert_eq!(gone.status, StatusCode::NOT_FOUND);
    assert!(gone.bytes.is_empty());

    let broken = request(
        &app,
        &format!(
            "/rest/getCoverArt?id={id}&size=500&apiKey={}",
            fixture.api_key
        ),
        &[],
    )
    .await;
    assert_eq!(broken.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(broken.header(RETRY_AFTER), Some("5"));
    assert!(broken.bytes.is_empty());
}

/// The paths the mock server has been asked for, sorted.
async fn requested_paths(server: &MockServer) -> Vec<String> {
    let mut paths: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect();
    paths.sort();
    paths
}
