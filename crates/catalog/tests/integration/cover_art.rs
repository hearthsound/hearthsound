//! Integration tests for serving catalog cover art through the issuing
//! instance.
//!
//! The tests drive the service over the fixture plugins and a local wiremock
//! server: `hello-catalog` declares `catalog.imageCachePolicy: cache` and
//! describes three cover sizes, and `http-catalog` declares `no-store` and
//! describes one image without dimensions. What the tests assert is where the
//! bytes came from and what was left on disk afterwards.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::{
    Env, HELLO_CATALOG_ID, HTTP_CATALOG_ID, METADATA_ID, catalog_id, count_rows, stored_artwork,
};
use hearthsound_catalog::{CatalogError, CoverArtRequest};
use hearthsound_core::{ClientId, EntityKind, Layer};
use hearthsound_plugin_host::manifest::ImageCachePolicy;
use hearthsound_test_support::{ARTWORK_FIXTURE, HELLO_CATALOG, HTTP_CATALOG, METADATA_FIXTURE};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The bytes every test image consists of.
const IMAGE: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

/// Serves one image from the mock server.
///
/// # Parameters
///
/// - `server`: the server to mount the endpoint on.
/// - `name`: the image's path, for example `/cover-500.png`.
/// - `status`: the HTTP status to answer with.
async fn serve_image(server: &MockServer, name: &str, status: u16) {
    let mut template = ResponseTemplate::new(status);
    if status < 300 {
        template = template
            .insert_header("content-type", "image/png")
            .set_body_bytes(IMAGE);
    }
    Mock::given(method("GET"))
        .and(path(name.to_owned()))
        .respond_with(template)
        .mount(server)
        .await;
}

/// The paths the mock server has been asked for.
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

/// Builds an environment with a caching `hello` instance whose covers the mock
/// server serves.
///
/// # Returns
///
/// The environment and the server, which the caller keeps alive.
async fn caching_env() -> (Env, MockServer) {
    let server = MockServer::start().await;
    for size in [250, 500, 1_000] {
        serve_image(&server, &format!("/cover-{size}.png"), 200).await;
    }
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    (env, server)
}

#[tokio::test]
async fn the_smallest_image_that_covers_the_request_is_served_and_cached() {
    let (env, server) = caching_env().await;
    let service = env.service();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(300),
        })
        .await
        .unwrap();

    assert_eq!(cover.mime_type, "image/png");
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        cover.cache_policy,
        ImageCachePolicy::Cache,
        "a caching instance's image may be stored by whoever serves it"
    );
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-500.png"],
        "the smallest image at least 300 pixels wide is the one to serve"
    );

    assert_eq!(count_rows(&env.database, "artwork_cache").await, 1);
    let files = env.artwork_files();
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::fs::read(env.artwork_dir().join(&files[0])).unwrap(),
        IMAGE,
        "the bytes are stored under <data>/artwork/"
    );
    let stored = stored_artwork(
        &env.database,
        &format!(
            "catalog|hello|track|hello-track-1|500|{}/cover-500.png",
            server.uri()
        ),
    )
    .await
    .expect("the image must be indexed");
    assert_eq!(stored.2, "image/png");
    assert_eq!((stored.3, stored.4), (Some(500), Some(500)));

    let again = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(300),
        })
        .await
        .unwrap();

    assert_eq!(again, cover);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-500.png"],
        "a cached image must not be fetched again"
    );
}

#[tokio::test]
async fn a_request_larger_than_every_image_serves_the_largest() {
    let (env, server) = caching_env().await;

    let cover = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(4_000),
        })
        .await
        .unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(requested_paths(&server).await, vec!["/cover-1000.png"]);
}

#[tokio::test]
async fn a_stored_entity_is_served_through_its_provider_alias() {
    let (env, server) = caching_env().await;
    let service = env.service();
    let track = service
        .materialize_track(&catalog_id("hello", EntityKind::Track, "hello-track-1"))
        .await
        .unwrap();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(track),
            size: None,
        })
        .await
        .unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-1000.png"],
        "a materialized entity reaches its instance through the p:hello alias"
    );
}

#[tokio::test]
async fn a_no_store_instance_returns_the_image_without_caching_it() {
    let server = MockServer::start().await;
    serve_image(&server, "/remote-cover.png", 200).await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"]}]"#,
        ))
        .mount(&server)
        .await;

    let env = Env::new().await;
    env.instance(
        "httpcat",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        json!({
            "url": format!("{}/search", server.uri()),
            "coverUrl": format!("{}/remote-cover.png", server.uri()),
        }),
    )
    .await;
    env.assign(Layer::Catalog, &["httpcat"]).await;
    let service = env.service();

    let request = CoverArtRequest {
        id: ClientId::Catalog(catalog_id("httpcat", EntityKind::Track, "remote-1")),
        size: None,
    };
    let cover = service.cover_art(&request).await.unwrap();

    assert_eq!(cover.mime_type, "image/png");
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        cover.cache_policy,
        ImageCachePolicy::NoStore,
        "the issuing instance's policy reaches the caller that serves the bytes"
    );
    assert_eq!(count_rows(&env.database, "artwork_cache").await, 0);
    assert!(
        env.artwork_files().is_empty(),
        "a no-store instance's images are never written to disk"
    );

    service.cover_art(&request).await.unwrap();
    assert_eq!(
        requested_paths(&server)
            .await
            .iter()
            .filter(|path| *path == "/remote-cover.png")
            .count(),
        2,
        "nothing was cached, so the second request fetches again"
    );
}

#[tokio::test]
async fn an_entity_that_describes_no_image_is_not_found() {
    let env = Env::new().await;
    env.instance("hello", HELLO_CATALOG, HELLO_CATALOG_ID, json!({}))
        .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Artist, "hello-artist-1")),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn a_stored_entity_no_instance_issued_is_not_found() {
    let env = Env::new().await;
    let local = crate::common::insert_local_track(&env.database, "Local Song", None, None).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(local),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
}

#[tokio::test]
async fn an_image_that_is_gone_is_not_found() {
    let server = MockServer::start().await;
    serve_image(&server, "/cover-1000.png", 404).await;
    let env = Env::new().await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;

    let error = env
        .service()
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(error, CatalogError::NotFound), "{error}");
    assert_eq!(count_rows(&env.database, "artwork_cache").await, 0);
}

#[tokio::test]
async fn a_cached_image_that_disappeared_is_fetched_again() {
    let (env, server) = caching_env().await;
    let service = env.service();
    let request = CoverArtRequest {
        id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
        size: None,
    };
    service.cover_art(&request).await.unwrap();

    let files = env.artwork_files();
    assert_eq!(files.len(), 1);
    std::fs::remove_file(env.artwork_dir().join(&files[0])).unwrap();

    let cover = service.cover_art(&request).await.unwrap();

    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        vec!["/cover-1000.png", "/cover-1000.png"],
        "a row whose file is gone is a miss, not a failure"
    );
    assert_eq!(env.artwork_files().len(), 1);
}

/// The plugin id of the artwork fixture.
const ARTWORK_FIXTURE_ID: &str = "com.example.artwork-fixture";

/// Builds an environment with an artwork instance whose images the mock server
/// serves.
///
/// # Parameters
///
/// - `images`: the paths the instance configures, in the order it lists them.
///
/// # Returns
///
/// The environment and the server, which the caller keeps alive.
async fn artwork_env(images: &[&str]) -> (Env, MockServer) {
    let server = MockServer::start().await;
    for name in images.iter().chain(["/portrait.png"].iter()) {
        serve_image(&server, name, 200).await;
    }
    let env = Env::new().await;
    env.instance(
        "artwork",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{
                "title": "Hello Album",
                "mbid": "22222222-2222-4222-8222-222222222222",
                "images": images
                    .iter()
                    .enumerate()
                    .map(|(index, name)| json!({"path": name, "width": 250 * (index as u32 + 1)}))
                    .collect::<Vec<_>>(),
            }],
            "artists": [{
                "title": "Hello Artist",
                "images": [{ "path": "/portrait.png", "width": 500 }],
            }],
        }),
    )
    .await;
    env.assign(Layer::Artwork, &["artwork"]).await;
    (env, server)
}

/// A stored album no catalog instance issued is served by the artwork layer.
#[tokio::test]
async fn the_artwork_layer_serves_a_stored_album() {
    let (env, server) = artwork_env(&["/cover-250.png", "/cover-500.png"]).await;
    let service = env.service();
    let album = env.stored_album("Hello Album", "Hello Artist").await;

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(album),
            size: Some(300),
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(cover.mime_type, "image/png");
    assert_eq!(
        requested_paths(&server).await,
        ["/cover-500.png"],
        "the smallest image that covers the request was fetched"
    );
}

/// A stored artist portrait is served by the artwork layer.
#[tokio::test]
async fn the_artwork_layer_serves_a_stored_artist() {
    let (env, server) = artwork_env(&["/cover-250.png"]).await;
    let service = env.service();
    let artist = env.stored_artist("Hello Artist").await;

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(artist),
            size: None,
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(requested_paths(&server).await, ["/portrait.png"]);
}

/// An instance that answers nothing leaves the catalog's own images to answer.
#[tokio::test]
async fn an_unknown_album_falls_back_to_the_issuing_instance() {
    let server = MockServer::start().await;
    serve_image(&server, "/cover-500.png", 200).await;
    let env = Env::new().await;
    // The artwork instance knows no image of the album the catalog issued.
    env.instance(
        "artwork",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({"imageUrl": server.uri(), "albums": [], "artists": []}),
    )
    .await;
    env.assign(Layer::Artwork, &["artwork"]).await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Track, "hello-track-1")),
            size: Some(500),
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);
    assert!(
        requested_paths(&server)
            .await
            .contains(&"/cover-500.png".to_owned()),
        "the issuing instance's image was fetched"
    );
}

/// An instance whose image URL answers 404 leaves the next instance to answer.
#[tokio::test]
async fn a_not_found_image_falls_through_to_the_next_instance() {
    let server = MockServer::start().await;
    // The first instance describes an image the server does not have.
    serve_image(&server, "/missing.png", 404).await;
    serve_image(&server, "/cover-250.png", 200).await;
    let env = Env::new().await;
    env.instance(
        "first",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{ "title": "Hello Album", "images": [{ "path": "/missing.png" }] }],
            "artists": [],
        }),
    )
    .await;
    env.instance(
        "second",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{ "title": "Hello Album", "images": [{ "path": "/cover-250.png" }] }],
            "artists": [],
        }),
    )
    .await;
    env.assign(Layer::Artwork, &["first", "second"]).await;
    let service = env.service();
    let album = env.stored_album("Hello Album", "Hello Artist").await;

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(album),
            size: None,
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);
    assert_eq!(
        requested_paths(&server).await,
        ["/cover-250.png", "/missing.png"],
        "the second instance answered after the first one had no image"
    );
}

/// An instance whose image URL fails leaves the next instance to answer.
#[tokio::test]
async fn a_failing_image_falls_through_to_the_next_instance() {
    let server = MockServer::start().await;
    // The first instance describes an image its host cannot deliver right now.
    serve_image(&server, "/broken.png", 500).await;
    serve_image(&server, "/cover-250.png", 200).await;
    let env = Env::new().await;
    env.instance(
        "first",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{ "title": "Hello Album", "images": [{ "path": "/broken.png" }] }],
            "artists": [],
        }),
    )
    .await;
    env.instance(
        "second",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{ "title": "Hello Album", "images": [{ "path": "/cover-250.png" }] }],
            "artists": [],
        }),
    )
    .await;
    env.assign(Layer::Artwork, &["first", "second"]).await;
    let service = env.service();
    let album = env.stored_album("Hello Album", "Hello Artist").await;

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(album),
            size: None,
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);
}

/// A failing artwork layer leaves the issuing catalog instance to answer, and
/// with nothing else to answer the failure is reported as one a retry may get
/// past.
#[tokio::test]
async fn a_failing_artwork_layer_falls_back_to_the_issuing_instance() {
    let server = MockServer::start().await;
    serve_image(&server, "/broken.png", 500).await;
    serve_image(&server, "/cover-500.png", 200).await;
    let env = Env::new().await;
    env.instance(
        "artwork",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [
                { "title": "Hello Album", "images": [{ "path": "/broken.png" }] },
                { "title": "Lonely Album", "images": [{ "path": "/broken.png" }] },
            ],
            "artists": [],
        }),
    )
    .await;
    env.assign(Layer::Artwork, &["artwork"]).await;
    env.instance(
        "hello",
        HELLO_CATALOG,
        HELLO_CATALOG_ID,
        json!({"coverUrl": format!("{}/cover-{{size}}.png", server.uri())}),
    )
    .await;
    env.assign(Layer::Catalog, &["hello"]).await;
    let service = env.service();

    let cover = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Catalog(catalog_id("hello", EntityKind::Album, "hello-album-1")),
            size: Some(500),
        })
        .await
        .unwrap();
    assert_eq!(cover.bytes, IMAGE);

    // A stored album no catalog instance issued has nothing to fall back on.
    let lonely = env.stored_album("Lonely Album", "Hello Artist").await;
    let failure = service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(lonely),
            size: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(failure, CatalogError::Unavailable { ref instance } if instance == "artwork"),
        "unexpected failure: {failure:?}"
    );
}

/// The metadata layer answers artist information, and an artist it does not
/// know is reported as unknown.
#[tokio::test]
async fn artist_info_is_answered_by_the_metadata_layer() {
    use hearthsound_core::providers::ArtistInfoRequest;

    let env = Env::new().await;
    env.instance(
        "metadata",
        METADATA_FIXTURE,
        METADATA_ID,
        json!({
            "artists": [{
                "name": "Hello Artist",
                "mbid": "44444444-4444-4444-8444-444444444444",
                "biography": "A greeting in human form.",
                "links": [{ "kind": "lastfm", "url": "https://www.last.fm/music/Hello+Artist" }],
            }],
        }),
    )
    .await;
    env.assign(Layer::Metadata, &["metadata"]).await;
    let service = env.service();

    let info = service
        .artist_info(&ArtistInfoRequest {
            name: "Hello Artist".to_owned(),
            ids: Vec::new(),
        })
        .await
        .unwrap()
        .expect("the configured artist is known");
    assert_eq!(info.biography.as_deref(), Some("A greeting in human form."));
    assert_eq!(info.links.len(), 1);
    assert_eq!(info.links[0].kind, "lastfm");

    // The second call is answered from the cache, and an artist no instance
    // knows is reported as unknown.
    let again = service
        .artist_info(&ArtistInfoRequest {
            name: "Hello Artist".to_owned(),
            ids: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(again, Some(info));

    let unknown = service
        .artist_info(&ArtistInfoRequest {
            name: "Nobody".to_owned(),
            ids: Vec::new(),
        })
        .await
        .unwrap();
    assert!(unknown.is_none(), "an unknown artist is not an error");
}

/// Serves one image after a delay, so a slow instance can be modelled.
///
/// # Parameters
///
/// - `server`: the server to mount the endpoint on.
/// - `name`: the image's path.
/// - `delay`: how long the endpoint waits before answering.
async fn serve_slow_image(server: &MockServer, name: &str, delay: std::time::Duration) {
    Mock::given(method("GET"))
        .and(path(name.to_owned()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(IMAGE)
                .set_delay(delay),
        )
        .mount(server)
        .await;
}

/// Reads an artist's portraits from two instances that both answer.
#[tokio::test]
async fn the_earlier_artwork_instance_wins_a_portrait() {
    let fast = MockServer::start().await;
    serve_image(&fast, "/portrait.png", 200).await;
    let slow = MockServer::start().await;
    serve_slow_image(
        &slow,
        "/portrait.png",
        std::time::Duration::from_millis(500),
    )
    .await;

    let env = Env::new().await;
    for (slug, url) in [("first", fast.uri()), ("second", slow.uri())] {
        env.instance(
            slug,
            ARTWORK_FIXTURE,
            ARTWORK_FIXTURE_ID,
            json!({
                "imageUrl": url,
                "albums": [],
                "artists": [{
                    "title": "Hello Artist",
                    "images": [{ "path": "/portrait.png", "width": 500 }],
                }],
            }),
        )
        .await;
    }
    env.assign(Layer::Artwork, &["first", "second"]).await;
    let artist = env.stored_artist("Hello Artist").await;

    let started = std::time::Instant::now();
    let portraits = env
        .service()
        .artist_portraits(&ClientId::Entity(artist))
        .await
        .unwrap();

    assert_eq!(portraits.len(), 1, "the earlier instance answers");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(400),
        "the answer does not wait for the slower instance: {:?}",
        started.elapsed()
    );
}

/// The cover key names the format version, the subject and the image, URL
/// included, and one URL is served from the cache the second time.
#[tokio::test]
async fn the_cover_key_names_the_image_url() {
    let server = MockServer::start().await;
    serve_image(&server, "/cover-a.png", 200).await;
    serve_image(&server, "/cover-b.png", 200).await;
    let env = Env::new().await;
    env.instance(
        "artwork",
        ARTWORK_FIXTURE,
        ARTWORK_FIXTURE_ID,
        json!({
            "imageUrl": server.uri(),
            "albums": [{
                "title": "Hello Album",
                "images": [
                    { "path": "/cover-a.png", "width": 250 },
                    { "path": "/cover-b.png", "width": 500 },
                ],
            }],
            "artists": [],
        }),
    )
    .await;
    env.assign(Layer::Artwork, &["artwork"]).await;
    let album = env.stored_album("Hello Album", "Hello Artist").await;

    // The two described images are two entries under one subject: the key names
    // the image, URL included, so a provider that changes the picture for a size
    // is not served the one it described before.
    let request = |size: u32| CoverArtRequest {
        id: ClientId::Entity(album),
        size: Some(size),
    };
    env.service().cover_art(&request(250)).await.unwrap();
    env.service().cover_art(&request(500)).await.unwrap();
    let keys = cache_keys(&env).await;
    assert_eq!(keys.len(), 2, "each image keeps its own entry: {keys:?}");
    for key in &keys {
        assert!(
            key.starts_with("artwork|v2|artwork|album|"),
            "the key carries the format version: {key}"
        );
    }
    assert!(
        keys.iter().any(|key| key.ends_with("/cover-a.png")),
        "{keys:?}"
    );
    assert!(
        keys.iter().any(|key| key.ends_with("/cover-b.png")),
        "{keys:?}"
    );

    // A repeated request for one image is a cache hit.
    let before = requested_paths(&server).await.len();
    env.service().cover_art(&request(250)).await.unwrap();
    assert_eq!(requested_paths(&server).await.len(), before);
}

/// Returns the keys of the artwork cache, sorted.
async fn cache_keys(env: &Env) -> Vec<String> {
    let mut keys: Vec<String> = sqlx::query_scalar("SELECT key FROM artwork_cache")
        .fetch_all(env.database.readers())
        .await
        .unwrap();
    keys.sort();
    keys
}

/// A cached image older than the retention is deleted with its file; a fresh one
/// survives.
#[tokio::test]
async fn the_artwork_sweep_deletes_old_rows_and_files() {
    let (env, _server) = artwork_env(&["/cover-250.png"]).await;
    let service = env.service();
    let album = env.stored_album("Hello Album", "Hello Artist").await;
    let fresh = env.stored_album("Hello Album", "Hello Artist").await;
    service
        .cover_art(&CoverArtRequest {
            id: ClientId::Entity(album),
            size: None,
        })
        .await
        .unwrap();
    let _ = fresh;
    assert_eq!(cache_keys(&env).await.len(), 1);
    assert_eq!(env.artwork_files().len(), 1);

    // Age the row past the retention.
    let retention = hearthsound_catalog::artwork::ARTWORK_CACHE_RETENTION;
    let old = crate::common::now_millis() - i64::try_from(retention.as_millis()).unwrap() - 1_000;
    sqlx::query("UPDATE artwork_cache SET fetched_at = ?")
        .bind(old)
        .execute(env.database.writer())
        .await
        .unwrap();

    let deleted = service.sweep_artwork_cache(retention).await.unwrap();
    assert_eq!(deleted, 1);
    assert!(cache_keys(&env).await.is_empty());
    assert!(
        env.artwork_files().is_empty(),
        "the file goes with its row: {:?}",
        env.artwork_files()
    );
}
