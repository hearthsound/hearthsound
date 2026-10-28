//! The indexer: a library album is walked into its artist's biography and
//! discography, and every fetched page and image is stored, so it still answers
//! after the provider endpoint is gone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::Viewer;
use hearthsound_core::providers::MB_RELEASE;
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix, Layer, StorageClass};
use hearthsound_jobs::Shutdown;
use hearthsound_library::index_queue::Depth;
use hearthsound_library::{AlbumInput, ArtistCredit, ArtistInput, ManagedMediaFile, TrackInput};
use hearthsound_test_support::audio::{CoverFormat, CoverSpec, write_cover};
use hearthsound_test_support::{
    ALBUM_CATALOG, ARTWORK_FIXTURE, DISCOGRAPHY_CATALOG, HTTP_CATALOG, METADATA_FIXTURE,
    SCROBBLE_FIXTURE,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{
    self, ALBUM_CATALOG_ID, ARTWORK_ID, DISCOGRAPHY_CATALOG_ID, Env, HTTP_CATALOG_ID, METADATA_ID,
    SCROBBLE_ID,
};

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
    assert!(
        page.albums
            .iter()
            .all(|entry| entry.album.title != "Live at the Depot"),
        "a discography holds albums, EPs and singles, not the artist's live releases"
    );

    // A page read never walks: the remote album's page answers what is stored and
    // queues its own walk.
    let remote_page = service
        .views()
        .album(EntityId::from_uuid(IdPrefix::Album, remote), viewer)
        .await
        .unwrap()
        .expect("the remote album row exists");
    assert!(
        !remote_page.complete,
        "the remote album's page is queued, not walked by the read"
    );

    // The queued walk stores the page while the endpoint is still up.
    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };
    env.wait_for_queue(Duration::from_secs(120)).await;
    shutdown.trigger();
    worker.await.unwrap();

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

/// An album's walk answers its tracks, so no track costs its own lookup.
///
/// A catalog album carries its tracklist - titles, numbers, durations,
/// recordings, ISRCs and credits - which is everything a track's own walk would
/// fetch. The album is queued ahead of its tracks for exactly that reason, and
/// the track rows only have to queue the artists they credit. The endpoint here
/// counts every track lookup it serves: a walk that answered the tracks from the
/// album makes none.
#[tokio::test]
async fn an_album_walk_answers_its_tracks_without_a_track_lookup() {
    let env = Env::new().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/lookup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "id": "remote-track-1",
            "title": "Remote Song",
            "artists": ["Remote Artist"],
        }])))
        .mount(&server)
        .await;

    // One instance answers the album, the other would answer each track.
    env.instance(
        "alb",
        ALBUM_CATALOG,
        ALBUM_CATALOG_ID,
        json!({ "canonicalIds": false }),
    )
    .await;
    env.instance(
        "remote",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        common::http_config(&format!("{}/lookup", server.uri())),
    )
    .await;
    env.assign(Layer::Catalog, &["alb", "remote"]).await;

    let library = env.library();
    let album = library
        .upsert_album(
            &AlbumInput::new("Fixture Album"),
            &[ArtistCredit::new("Fixture Artist", "main")],
        )
        .await
        .unwrap();
    env.attach(
        EntityKind::Album,
        album,
        &[common::external_id("p:alb", "album-catalog-album-1")],
    )
    .await;

    let mut tracks = Vec::new();
    for number in 1..=5 {
        let track = library
            .insert_track(
                &TrackInput {
                    album_id: Some(album),
                    duration_ms: Some(180_000),
                    disc_number: Some(1),
                    track_number: Some(number),
                    ..TrackInput::new(format!("Fixture Song {number}"), Availability::Local)
                },
                &[ArtistCredit::new("Fixture Artist", "main")],
                &[],
            )
            .await
            .unwrap();
        env.attach(
            EntityKind::Track,
            track,
            &[common::external_id(
                "p:remote",
                &format!("remote-track-{number}"),
            )],
        )
        .await;
        tracks.push(track);
    }

    // What a scan does: every track of the library queued at library urgency.
    let mut connection = env.database.writer().acquire().await.unwrap();
    for track in &tracks {
        hearthsound_library::index_queue::enqueue_library(
            &mut connection,
            EntityKind::Track,
            track.uuid(),
        )
        .await
        .unwrap();
    }
    drop(connection);

    let service = env.service();
    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };
    env.wait_for_queue(Duration::from_secs(120)).await;
    shutdown.trigger();
    worker.await.unwrap();

    let mut connection = env.database.writer().acquire().await.unwrap();
    let fetched: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT fetched_at FROM tracks WHERE album_id = ? ORDER BY track_number",
    )
    .bind(album.uuid().as_bytes().as_slice())
    .fetch_all(&mut *connection)
    .await
    .unwrap();
    let page: Option<Option<i64>> =
        sqlx::query_scalar("SELECT page_fetched_at FROM albums WHERE id = ?")
            .bind(album.uuid().as_bytes().as_slice())
            .fetch_one(&mut *connection)
            .await
            .unwrap();
    assert!(page.flatten().is_some(), "the album page was walked");
    assert_eq!(
        fetched.len(),
        5,
        "the album's tracklist matched the library's five rows instead of          inserting five of its own, got {fetched:?}"
    );
    assert!(
        fetched.iter().all(Option::is_some),
        "the album's answer set every track's fetched_at, got {fetched:?}"
    );
    drop(connection);
    let lookups: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.url.path() == "/lookup")
        .filter_map(|request| request.url.query())
        .filter(|query| query.contains("remote-track-"))
        .map(str::to_owned)
        .collect();
    assert!(
        lookups.is_empty(),
        "no track was looked up on its own, got {lookups:?}"
    );
}

/// Eight walks run at once instead of one after another.
///
/// Every walk here asks a provider that takes 300 ms to answer; eight of them in
/// sequence is 2.4 s and the worker pool finishes in the time of two rounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn walks_run_at_once() {
    const ALBUMS: i64 = 8;
    const DELAY_MS: i64 = 300;

    let env = Env::new().await;
    let recordings: Vec<serde_json::Value> = (1..=ALBUMS)
        .map(|number| {
            json!({
                "title": format!("Walk Song {number}"),
                "artists": ["Walk Artist"],
                "mbid": format!("walk-recording-{number}"),
                "releases": [{
                    "mbid": format!("walk-release-{number}"),
                    "title": format!("Walk Album {number}"),
                    "date": "2001-02-03",
                    "trackCount": 1,
                }],
            })
        })
        .collect();
    env.instance(
        "meta",
        METADATA_FIXTURE,
        METADATA_ID,
        common::delayed_metadata_config(json!(recordings), json!([]), DELAY_MS),
    )
    .await;
    env.assign(Layer::Metadata, &["meta"]).await;
    env.set(
        "index.workers",
        hearthsound_config::SettingValue::Integer(8),
    )
    .await;

    let library = env.library();
    let mut albums = Vec::new();
    for number in 1..=ALBUMS {
        let mut album = AlbumInput::new(format!("Walk Album {number}"));
        album.year = Some(2001);
        let album = library
            .upsert_album(&album, &[ArtistCredit::new("Walk Artist", "main")])
            .await
            .unwrap();
        env.attach(
            EntityKind::Album,
            album,
            &[common::external_id(
                MB_RELEASE,
                &format!("walk-release-{number}"),
            )],
        )
        .await;
        albums.push(album);
    }

    let mut connection = env.database.writer().acquire().await.unwrap();
    for album in &albums {
        hearthsound_library::index_queue::enqueue(
            &mut connection,
            EntityKind::Album,
            album.uuid(),
            Depth::Page,
            1,
        )
        .await
        .unwrap();
    }
    drop(connection);

    let service = env.service();
    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };
    let started = std::time::Instant::now();
    env.wait_for_queue(Duration::from_secs(120)).await;
    let elapsed = started.elapsed();
    shutdown.trigger();
    worker.await.unwrap();

    let serial = Duration::from_millis(u64::try_from(ALBUMS * DELAY_MS).unwrap());
    assert!(
        elapsed < serial.mul_f32(0.8),
        "eight walks of {DELAY_MS} ms finished in {elapsed:?}, which is not faster than {serial:?}"
    );

    let mut connection = env.database.writer().acquire().await.unwrap();
    for album in &albums {
        let page: Option<Option<i64>> =
            sqlx::query_scalar("SELECT page_fetched_at FROM albums WHERE id = ?")
                .bind(album.uuid().as_bytes().as_slice())
                .fetch_one(&mut *connection)
                .await
                .unwrap();
        assert!(page.flatten().is_some(), "every album's page was walked");
    }
}

/// A discovery instance that refuses the similar-artist question is answered
/// once, so the artist's page stops waiting for it.
///
/// The question used to stay open forever when the instance failed - no answer
/// row was stored - so every read of the page queued another walk at the front of
/// the queue: a page that never completes and a queue that never drains. A
/// refusal is an answer, and it is stored as one.
#[tokio::test]
async fn a_refused_similar_artists_question_is_answered_once() {
    let env = Env::new().await;
    env.instance(
        "broken",
        SCROBBLE_FIXTURE,
        SCROBBLE_ID,
        json!({ "endpoint": "https://listenbrainz.invalid", "failDiscovery": true }),
    )
    .await;
    env.assign(Layer::Discovery, &["broken"]).await;

    let library = env.library();
    let artist = library
        .upsert_artist(&ArtistInput::new("Refused Artist"))
        .await
        .unwrap();
    let user = env.user().await;
    env.save(user, EntityKind::Artist, artist).await;

    let service = env.service();
    let viewer = Viewer {
        user,
        vendor: false,
    };

    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };
    let first = service
        .views()
        .artist(artist, viewer)
        .await
        .unwrap()
        .expect("the artist row exists");
    assert!(!first.complete, "the queued walk has not run yet");
    env.wait_for_queue(Duration::from_secs(120)).await;
    shutdown.trigger();
    worker.await.unwrap();

    let mut connection = env.database.writer().acquire().await.unwrap();
    let answered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM entity_sources WHERE entity_kind = 'artist' AND entity_id = ? \
         AND operation = 'similar-artists' AND status = 'missing'",
    )
    .bind(artist.uuid().as_bytes().as_slice())
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    drop(connection);
    assert_eq!(answered, 1, "the refusal is stored as a missing answer");

    let second = service
        .views()
        .artist(artist, viewer)
        .await
        .unwrap()
        .expect("the artist row exists");
    assert!(second.complete, "the page stops waiting for the answer");
    let mut connection = env.database.writer().acquire().await.unwrap();
    let queued: Option<String> = sqlx::query_scalar(
        "SELECT state FROM index_queue WHERE entity_kind = 'artist' AND entity_id = ?",
    )
    .bind(artist.uuid().as_bytes().as_slice())
    .fetch_optional(&mut *connection)
    .await
    .unwrap();
    drop(connection);
    assert!(
        queued.is_none(),
        "the page does not queue itself again, got {queued:?}"
    );
}
