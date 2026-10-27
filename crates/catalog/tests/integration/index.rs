//! The indexer: a library album is walked into its artist's biography and
//! discography, and every fetched page and image is stored, so it still answers
//! after the provider endpoint is gone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::Viewer;
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix, Layer, StorageClass};
use hearthsound_jobs::Shutdown;
use hearthsound_library::index_queue::Depth;
use hearthsound_library::{AlbumInput, ArtistCredit, ArtistInput, ManagedMediaFile, TrackInput};
use hearthsound_test_support::audio::{CoverFormat, CoverSpec, write_cover};
use hearthsound_test_support::{ARTWORK_FIXTURE, DISCOGRAPHY_CATALOG, METADATA_FIXTURE};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, ARTWORK_ID, DISCOGRAPHY_CATALOG_ID, Env, METADATA_ID};

/// The name the discography fixture gives its artist.
const ARTIST_NAME: &str = "Discography Artist";

/// The MusicBrainz id the fixture reports for the artist.
const ARTIST_MBID: &str = "disco-mb-artist-1";

/// The provider id of the library album in the fixture's discography.
const LIBRARY_ALBUM: &str = "disco-album-1";

/// The title of the non-owned album whose cover the artwork fixture serves.
const REMOTE_ALBUM: &str = "Second Wind (Remastered 2009)";

#[tokio::test]
async fn a_library_album_is_deep_indexed_and_answers_after_the_mock_stops() {
    let env = Env::new().await;

    // The artwork endpoint serves one cover; every other call is a missing
    // image, so the test counts what the endpoint saw.
    let cover_path = env.artwork_dir().parent().unwrap().join("cover.png");
    write_cover(
        &cover_path,
        CoverSpec {
            format: CoverFormat::Png,
            size: 300,
            rgb: 0x0033_6699,
        },
    )
    .await
    .unwrap();
    let cover = std::fs::read(&cover_path).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/cover.png"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/png")
                .set_body_bytes(cover),
        )
        .mount(&server)
        .await;

    // The fixture slugs matter: the discography step resolves its references
    // under the `catalog` slug.
    env.instance(
        "catalog",
        DISCOGRAPHY_CATALOG,
        DISCOGRAPHY_CATALOG_ID,
        json!({}),
    )
    .await;
    env.instance(
        "meta",
        METADATA_FIXTURE,
        METADATA_ID,
        common::metadata_config(json!([{
            "name": ARTIST_NAME,
            "mbid": ARTIST_MBID,
            "biography": "A fixture biography.",
            "links": [{ "kind": "wikipedia", "url": "https://en.wikipedia.org/wiki/Fixture" }],
        }])),
    )
    .await;
    env.instance(
        "art",
        ARTWORK_FIXTURE,
        ARTWORK_ID,
        common::artwork_config(
            &server.uri(),
            json!([{ "title": REMOTE_ALBUM, "images": [{ "path": "/cover.png", "width": 600 }] }]),
            json!([]),
        ),
    )
    .await;
    env.assign(Layer::Catalog, &["catalog"]).await;
    env.assign(Layer::Metadata, &["meta"]).await;
    env.assign(Layer::Artwork, &["art"]).await;

    // A library album: one owned album with one managed track, identified by
    // the ids the providers will name.
    let library = env.library();
    let artist = library
        .upsert_artist(&ArtistInput::new(ARTIST_NAME))
        .await
        .unwrap();
    env.attach(
        EntityKind::Artist,
        artist,
        &[
            common::external_id("p:catalog", "disco-artist-1"),
            common::external_id("mb-artist", ARTIST_MBID),
        ],
    )
    .await;

    let mut album = AlbumInput::new("First Light");
    album.year = Some(2001);
    let album = library
        .upsert_album(&album, &[ArtistCredit::new(ARTIST_NAME, "main")])
        .await
        .unwrap();
    env.attach(
        EntityKind::Album,
        album,
        &[common::external_id("p:catalog", LIBRARY_ALBUM)],
    )
    .await;

    let track = library
        .insert_track(
            &TrackInput {
                album_id: Some(album),
                duration_ms: Some(180_000),
                ..TrackInput::new("Dawn", Availability::Local)
            },
            &[ArtistCredit::new(ARTIST_NAME, "main")],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: "discography/dawn.flac".to_owned(),
            size: 1_024,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(180_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();

    let service = env.service();
    env.enqueue(EntityKind::Artist, artist, Depth::Deep, 0)
        .await;
    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };

    // The whole walk has to finish: the artist's page queues its discography,
    // and each queued album page fetches its own tracklist and cover.
    env.wait_for_queue(Duration::from_secs(120)).await;
    shutdown.trigger();
    worker.await.unwrap();

    let remote = remote_album(&env, REMOTE_ALBUM)
        .await
        .expect("the discography stored the non-owned album");
    assert_eq!(
        biography(&env, artist).await.as_deref(),
        Some("A fixture biography."),
        "the metadata provider's biography is stored"
    );
    assert_eq!(
        album_tracks(&env, remote).await,
        3,
        "the non-owned album's tracklist is stored as rows"
    );
    assert!(
        env.count_rows("entity_images").await > 0,
        "the artwork provider's cover is stored as an image row"
    );

    // The provider named a cover URL; serving it stores the bytes once.
    let served = service
        .images()
        .serve(EntityKind::Album, remote, None)
        .await
        .unwrap()
        .expect("the artwork endpoint answered");
    assert_eq!(served.content_type, "image/png");
    assert!(!served.bytes.is_empty());
    let files = env.artwork_files();
    assert!(
        files.iter().any(|file| file.starts_with("blobs/")),
        "the fetched cover is a stored blob, got {files:?}"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "one cover fetch"
    );

    let user = env.user().await;
    let viewer = Viewer {
        user,
        vendor: false,
    };
    let page = service
        .views()
        .artist(artist, viewer)
        .await
        .unwrap()
        .expect("the artist page is stored");
    assert_eq!(
        page.artist.biography.as_deref(),
        Some("A fixture biography.")
    );
    assert!(
        page.links.iter().any(|link| link.kind == "wikipedia"),
        "the metadata provider's links are stored"
    );
    assert!(
        page.albums
            .iter()
            .any(|entry| entry.album.id.uuid() == remote),
        "the discography holds the non-owned album"
    );
    assert!(
        page.albums.iter().any(|entry| entry.album.id == album),
        "and the owned album"
    );

    // With the endpoint gone, everything answers from the database and store.
    drop(server);
    let album_page = service
        .views()
        .album(EntityId::from_uuid(IdPrefix::Album, remote), viewer)
        .await
        .unwrap()
        .expect("the remote album page is stored");
    assert!(album_page.complete);
    assert_eq!(album_page.tracks.len(), 3);
    assert!(album_page.has_image);
    let scaled = service
        .images()
        .serve(EntityKind::Album, remote, Some(96))
        .await
        .unwrap()
        .expect("the stored cover serves a thumbnail");
    assert!(!scaled.bytes.is_empty());
    assert_eq!(scaled.content_type, "image/jpeg");
    let artist_page = service
        .views()
        .artist(artist, viewer)
        .await
        .unwrap()
        .expect("the artist page still answers");
    assert!(artist_page.complete);
}

/// Reads an artist's stored biography.
async fn biography(env: &Env, artist: EntityId) -> Option<String> {
    sqlx::query_scalar("SELECT biography FROM artists WHERE id = ?")
        .bind(artist.uuid().as_bytes().as_slice())
        .fetch_one(env.database.writer())
        .await
        .unwrap()
}

/// Returns the id of the album with `title`, when it is stored.
async fn remote_album(env: &Env, title: &str) -> Option<uuid::Uuid> {
    let bytes: Option<Vec<u8>> = sqlx::query_scalar("SELECT id FROM albums WHERE title = ?")
        .bind(title)
        .fetch_optional(env.database.writer())
        .await
        .unwrap();
    bytes.map(|bytes| uuid::Uuid::from_slice(&bytes).unwrap())
}

/// Counts an album's stored tracks.
async fn album_tracks(env: &Env, album: uuid::Uuid) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM tracks WHERE album_id = ?")
        .bind(album.as_bytes().as_slice())
        .fetch_one(env.database.writer())
        .await
        .unwrap()
}
