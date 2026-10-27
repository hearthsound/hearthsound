//! Full-stack tests of `GET /api/v1/library/queue`: one answer per collection
//! source, the `from` start index, the problem documents, the personal scope,
//! and a six-thousand-track library.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use hearthsound_core::StorageClass;
use hearthsound_core::{Availability, EntityId, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, ScannedMediaFile, TrackInput,
};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode};
use serde_json::Value;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use crate::common::Fixture;
use crate::native::{bearer, json as read_json};

/// How many tracks the generated library holds.
const GENERATED: usize = 6_000;

/// The most a queue of the generated library may take over HTTP.
const GENERATED_BUDGET: Duration = Duration::from_secs(10);

/// Inserts an account directly, for the ownership rows a second account needs.
async fn insert_user(db: &Database, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("{name}-{id}"))
    .bind(name)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts an album by `artist` and returns both ids.
async fn seed_album(
    library: &LibraryRepository,
    artist: &str,
    title: &str,
    year: i64,
) -> (EntityId, EntityId) {
    let mut album = AlbumInput::new(title);
    album.year = Some(year);
    let credits = [ArtistCredit::new(artist, "main")];
    let album_id = library.upsert_album(&album, &credits).await.unwrap();
    let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
    (album_id, artist_id)
}

/// Inserts a track and returns its id.
async fn seed_track(
    library: &LibraryRepository,
    album: EntityId,
    artist: &str,
    title: &str,
    track_number: i64,
) -> EntityId {
    let mut input = TrackInput::new(title, Availability::Local);
    input.album_id = Some(album);
    input.display_artist = Some(artist.to_owned());
    input.track_number = Some(track_number);
    input.duration_ms = Some(180_000);
    library
        .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
        .await
        .unwrap()
}

/// Records an explicit save.
async fn save(db: &Database, user: Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a like.
async fn like(db: &Database, user: Uuid, track: EntityId, at: i64) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, 'track', ?, 'like', ?)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a rating.
async fn rate(db: &Database, user: Uuid, track: EntityId, rating: i64) {
    sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, 'track', ?, ?, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(rating)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a play.
async fn play(db: &Database, user: Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
         VALUES (?, ?, ?, 0, 180000, 1)",
    )
    .bind(Uuid::now_v7().as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Creates a playlist and returns its id.
async fn playlist(db: &Database, owner: Uuid, public: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_public, is_temporary, created_at, updated_at) \
         VALUES (?, ?, 'mix', ?, 0, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(public)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Adds a track to a playlist at `position`.
async fn add_to_playlist(db: &Database, playlist: Uuid, track: EntityId, position: i64) {
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) VALUES (?, ?, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(position)
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Puts a track's file in a folder owned by `owner`.
async fn own_in_folder(
    library: &LibraryRepository,
    dir: &Path,
    owner: Option<Uuid>,
    track: EntityId,
) {
    let folder = library.add_library_folder(dir, owner).await.unwrap();
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: format!("{}.mp3", track.uuid()),
                size: 1_000,
                mtime_ms: 0,
                format: Some("mpeg".to_owned()),
                codec: Some("mp3".to_owned()),
                bitrate_kbps: Some(320),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some(180_000),
                lossless: Some(false),
                replaygain: None,
                has_embedded_art: Some(false),
            },
        )
        .await
        .unwrap();
}

/// Reads a queue over the native API.
async fn get_queue(fixture: &Fixture, query: &str) -> (StatusCode, Value) {
    let response = bearer(
        fixture,
        Method::GET,
        &format!("/api/v1/library/queue?{query}"),
        None,
    )
    .await;
    let status = response.status();
    (status, read_json(response).await)
}

/// Asserts a queue answer is the expected success shape.
fn assert_queue(body: &Value, source: &str, count: Option<usize>) {
    assert_eq!(body["source"], source, "{body}");
    let items = body["items"].as_array().unwrap_or_else(|| panic!("{body}"));
    if let Some(count) = count {
        assert_eq!(items.len(), count, "{body}");
    }
}

/// Asserts a response is a problem document with the given status and code.
fn assert_problem(status: StatusCode, body: &Value, expected: StatusCode, code: &str) {
    assert_eq!(status, expected, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert_eq!(body["status"], expected.as_u16());
    assert_eq!(
        body["type"],
        format!("https://hearthsound.app/problems/{code}")
    );
}

/// Asserts the response content type is the problem media type.
fn assert_problem_media_type(content_type: &str) {
    assert_eq!(content_type, "application/problem+json");
}

/// Returns the item ids of an answer, in order.
fn item_ids(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The library the source tests share.
struct Seeded {
    /// The account whose queue is read.
    user: Uuid,
    /// The first album.
    alpha_album: EntityId,
    /// The artist of the first album.
    alpha_artist: EntityId,
    /// The first track of the first album, saved, liked and rated.
    alpha_one: EntityId,
    /// The second track of the first album, played twice.
    alpha_two: EntityId,
    /// The only track of the second album, saved.
    bravo_one: EntityId,
    /// A track in the account's own folder.
    folder_track: EntityId,
    /// A track on the other account's private playlist and folder.
    other_track: EntityId,
    /// The account's playlist, in reverse library order.
    mix: Uuid,
    /// The other account's private playlist.
    other_mix: Uuid,
}

/// Seeds the shared library through the running server's database.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `dir`: a directory the owned and unowned folder tracks live under.
async fn seed(fixture: &Fixture, dir: &Path) -> Seeded {
    let db = fixture.services.database();
    let library = LibraryRepository::new(db);
    let user = fixture.user_id;
    let other = insert_user(db, "other").await;

    let (alpha_album, alpha_artist) = seed_album(&library, "Alpha", "Alpha Album", 1999).await;
    let alpha_one = seed_track(&library, alpha_album, "Alpha", "Alpha One", 1).await;
    let alpha_two = seed_track(&library, alpha_album, "Alpha", "Alpha Two", 2).await;
    save(db, user, alpha_one).await;
    save(db, user, alpha_two).await;
    like(db, user, alpha_one, 10).await;
    rate(db, user, alpha_one, 4).await;
    play(db, user, alpha_two).await;
    play(db, user, alpha_two).await;

    let (bravo_album, _) = seed_album(&library, "Bravo", "Bravo Album", 2001).await;
    let bravo_one = seed_track(&library, bravo_album, "Bravo", "Bravo One", 1).await;
    save(db, user, bravo_one).await;

    let (folder_album, _) = seed_album(&library, "Folder", "Folder Album", 2003).await;
    let folder_track = seed_track(&library, folder_album, "Folder", "Mike Folder", 1).await;
    let owned = dir.join("owned");
    std::fs::create_dir_all(&owned).unwrap();
    own_in_folder(&library, &owned, Some(user), folder_track).await;

    let (other_album, _) = seed_album(&library, "Other", "Other Album", 2005).await;
    let other_track = seed_track(&library, other_album, "Other", "Oscar Other", 1).await;
    let theirs = dir.join("theirs");
    std::fs::create_dir_all(&theirs).unwrap();
    own_in_folder(&library, &theirs, Some(other), other_track).await;

    let mix = playlist(db, user, false).await;
    add_to_playlist(db, mix, alpha_two, 0).await;
    add_to_playlist(db, mix, alpha_one, 1).await;

    let other_mix = playlist(db, other, false).await;
    add_to_playlist(db, other_mix, other_track, 0).await;

    Seeded {
        user,
        alpha_album,
        alpha_artist,
        alpha_one,
        alpha_two,
        bravo_one,
        folder_track,
        other_track,
        mix,
        other_mix,
    }
}

/// The songs source answers the whole personal library in the Songs view's
/// order, with the item fields the player reads, and `from` names the start.
#[tokio::test]
async fn the_songs_queue_answers_the_library_in_order_and_starts_at_from() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;

    let (status, body) = get_queue(&fixture, "source=songs").await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, "songs", Some(4));
    assert_eq!(
        item_ids(&body),
        vec![
            seeded.alpha_one.to_string(),
            seeded.alpha_two.to_string(),
            seeded.bravo_one.to_string(),
            seeded.folder_track.to_string(),
        ],
        "sorted by the folded title; another account's track is absent"
    );
    assert_eq!(body["startIndex"], 0);

    let first = &body["items"][0];
    assert_eq!(first["title"], "Alpha One");
    assert_eq!(first["artist"], "Alpha");
    assert_eq!(first["album"], "Alpha Album");
    assert_eq!(first["albumId"], seeded.alpha_album.to_string());
    assert_eq!(first["artistId"], seeded.alpha_artist.to_string());
    assert_eq!(first["duration"], 180);
    assert_eq!(first["coverArt"], seeded.alpha_album.to_string());
    assert_eq!(first["availability"], "local");
    assert_eq!(first["rating"], 4);
    assert_eq!(first["liked"], true);
    assert_eq!(first["disliked"], false);

    let unrated = &body["items"][1];
    assert_eq!(unrated["rating"], Value::Null);
    assert_eq!(unrated["liked"], false);

    let (status, body) =
        get_queue(&fixture, &format!("source=songs&from={}", seeded.alpha_two)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["startIndex"], 1, "{body}");

    let (status, body) =
        get_queue(&fixture, "source=songs&from=tr_00000000000000000000000000").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-in-source");
}

/// Attaches one genre to a track, stored as the scan stores it.
async fn set_genre(db: &Database, track: EntityId, genre: &str) {
    let id = Uuid::now_v7();
    let name = genre.trim().to_lowercase();
    sqlx::query("INSERT INTO genres (id, name) VALUES (?, ?) ON CONFLICT (name) DO NOTHING")
        .bind(id.as_bytes().as_slice())
        .bind(&name)
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO track_genres (track_id, genre_id) \
         SELECT ?, id FROM genres WHERE name = ?",
    )
    .bind(track.uuid().as_bytes().as_slice())
    .bind(&name)
    .execute(db.writer())
    .await
    .unwrap();
}

/// The `genre` parameter narrows a songs queue exactly as the song listing
/// narrows it, and every other source ignores it.
#[tokio::test]
async fn the_songs_queue_narrows_to_one_genre() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;
    let db = fixture.services.database();
    set_genre(db, seeded.alpha_one, "Rock").await;
    set_genre(db, seeded.bravo_one, "Rock").await;
    set_genre(db, seeded.alpha_two, "Jazz").await;

    let (status, body) = get_queue(&fixture, "source=songs&genre=rock").await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, "songs", Some(2));
    assert_eq!(
        item_ids(&body),
        vec![seeded.alpha_one.to_string(), seeded.bravo_one.to_string()],
        "the folded name matches, and the title order still applies"
    );

    let (status, body) = get_queue(&fixture, "source=songs&genre=polka").await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, "songs", Some(0));

    let (status, body) = get_queue(&fixture, "source=liked&genre=polka").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        item_ids(&body),
        vec![seeded.alpha_one.to_string()],
        "a source with its own order ignores the genre"
    );
}

/// The liked and uploads sources name their own tracks in their own order.
#[tokio::test]
async fn the_liked_and_uploads_queues_list_their_own_tracks() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;
    let db = fixture.services.database();
    let library = LibraryRepository::new(db);
    let (upload_album, upload_artist) =
        seed_album(&library, "Uploader", "Upload Album", 2020).await;
    let upload_one = seed_track(&library, upload_album, "Uploader", "Yellow One", 1).await;
    let upload_two = seed_track(&library, upload_album, "Uploader", "Zulu Two", 2).await;
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'upload', 7)",
    )
    .bind(seeded.user.as_bytes().as_slice())
    .bind(upload_one.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'upload', 9)",
    )
    .bind(seeded.user.as_bytes().as_slice())
    .bind(upload_two.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    // An upload is a stored file that records the account as its uploader, so
    // each of the two tracks carries one.
    for (index, track) in [upload_one, upload_two].into_iter().enumerate() {
        library
            .insert_managed_media_file(&ManagedMediaFile {
                track_id: track,
                storage: StorageClass::Library,
                rel_path: format!("uploads/{index:02}.mp3"),
                size: 1_000,
                mtime_ms: 0,
                format: Some("mpeg".to_owned()),
                codec: Some("mp3".to_owned()),
                bitrate_kbps: Some(320),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some(180_000),
                lossless: Some(false),
                quality_tier: None,
                replaygain: None,
                has_embedded_art: Some(false),
                uploaded_by: Some(seeded.user),
                acquisition_id: None,
            })
            .await
            .unwrap();
    }
    like(db, seeded.user, seeded.folder_track, 30).await;

    let (status, body) = get_queue(&fixture, "source=liked").await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, "liked", Some(2));
    assert_eq!(
        item_ids(&body),
        vec![
            seeded.folder_track.to_string(),
            seeded.alpha_one.to_string()
        ],
        "the newest like first"
    );
    assert!(body["items"][0]["liked"].as_bool().unwrap());
    assert_eq!(body["items"][1]["rating"], 4);
    assert_eq!(
        body["items"][1]["albumId"],
        seeded.alpha_album.to_string(),
        "{body}"
    );
    assert_eq!(
        body["items"][1]["artistId"],
        seeded.alpha_artist.to_string(),
        "{body}"
    );

    let (status, body) = get_queue(&fixture, "source=uploads").await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, "uploads", Some(2));
    assert_eq!(
        item_ids(&body),
        vec![upload_two.to_string(), upload_one.to_string()],
        "the newest upload first"
    );
    assert_eq!(
        body["items"][0]["albumId"],
        upload_album.to_string(),
        "{body}"
    );
    assert_eq!(
        body["items"][0]["artistId"],
        upload_artist.to_string(),
        "{body}"
    );
}

/// A playlist queue follows the playlist's own positions, and a private
/// playlist of another account is not found.
#[tokio::test]
async fn the_playlist_queue_follows_playlist_order() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;

    let playlist_id = EntityId::from_uuid(IdPrefix::Playlist, seeded.mix);
    let (status, body) = get_queue(&fixture, &format!("source=playlist:{playlist_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_queue(&body, &format!("playlist:{playlist_id}"), Some(2));
    assert_eq!(
        item_ids(&body),
        vec![seeded.alpha_two.to_string(), seeded.alpha_one.to_string()],
        "the playlist's order, not the library's"
    );
    assert_eq!(
        body["items"][0]["albumId"],
        seeded.alpha_album.to_string(),
        "{body}"
    );
    assert_eq!(
        body["items"][0]["artistId"],
        seeded.alpha_artist.to_string(),
        "{body}"
    );

    let other_mix = EntityId::from_uuid(IdPrefix::Playlist, seeded.other_mix);
    let response = bearer(
        &fixture,
        Method::GET,
        &format!("/api/v1/library/queue?source=playlist:{other_mix}"),
        None,
    )
    .await;
    let content_type = response.headers()[CONTENT_TYPE]
        .to_str()
        .unwrap()
        .to_owned();
    let status = response.status();
    let body = read_json(response).await;
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-found");
    assert_problem_media_type(&content_type);
}

/// The artist queue uses its own order, and is limited to the account's
/// library.
#[tokio::test]
async fn the_artist_queue_uses_its_own_order() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;

    let (status, body) =
        get_queue(&fixture, &format!("source=artist:{}", seeded.alpha_artist)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        item_ids(&body),
        vec![seeded.alpha_two.to_string(), seeded.alpha_one.to_string()],
        "the played track comes first"
    );

    let (status, body) = get_queue(
        &fixture,
        &format!("source=artist-top:{}", seeded.alpha_artist),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(item_ids(&body), vec![seeded.alpha_two.to_string()]);

    let (status, body) = get_queue(
        &fixture,
        &format!("source=artist:{}", EntityId::new(IdPrefix::Artist)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-found");
}

/// An unknown source is a 400 problem, an unknown collection a 404, and a
/// `from` outside the source a 404 of its own.
#[tokio::test]
async fn the_error_cases_are_problem_documents() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;

    let (status, body) = get_queue(&fixture, "source=everything").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(status, &body, StatusCode::BAD_REQUEST, "unknown-source");

    let (status, body) = get_queue(&fixture, "source=kind:tr_00000000000000000000000000").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_problem(status, &body, StatusCode::BAD_REQUEST, "unknown-source");

    let (status, body) = get_queue(
        &fixture,
        &format!("source=playlist:{}", EntityId::new(IdPrefix::Playlist)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-found");

    // The bravo track exists, but it is not one of the account's likes.
    let (status, body) =
        get_queue(&fixture, &format!("source=liked&from={}", seeded.bravo_one)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-in-source");

    // Another account's track is not in this account's library either.
    let (status, body) = get_queue(
        &fixture,
        &format!("source=songs&from={}", seeded.other_track),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_problem(status, &body, StatusCode::NOT_FOUND, "not-in-source");
}

/// Another account's private playlist and folder content never appear in the
/// caller's queue.
#[tokio::test]
async fn another_accounts_collections_are_not_included() {
    let fixture = Fixture::new().await;
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(&fixture, dir.path()).await;

    let (status, body) = get_queue(&fixture, "source=songs").await;
    assert_eq!(status, StatusCode::OK);
    let ids = item_ids(&body);
    assert!(!ids.contains(&seeded.other_track.to_string()));
    assert_eq!(body["startIndex"], 0);
}

/// Bulk-inserts `count` tracks into the account's library, in reverse title
/// order, and returns their ids in insertion order.
///
/// # Parameters
///
/// - `db`: the running fixture's database.
/// - `user`: the account whose library the tracks join.
/// - `album`: the album every generated track belongs to.
/// - `count`: how many tracks to write.
async fn bulk_library(db: &Database, user: Uuid, album: EntityId, count: usize) -> Vec<EntityId> {
    let ids: Vec<EntityId> = (0..count)
        .rev()
        .map(|_| EntityId::new(IdPrefix::Track))
        .collect();
    let mut tx = db.writer().begin().await.unwrap();
    for (chunk_index, chunk) in ids.chunks(250).enumerate() {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, \
             track_number, duration_ms, availability, created_at, updated_at) ",
        );
        builder.push_values(chunk.iter().enumerate(), |mut row, (offset, id)| {
            let index = count - 1 - (chunk_index * 250 + offset);
            row.push_bind(id.uuid().as_bytes().to_vec())
                .push_bind(album.uuid().as_bytes().to_vec())
                .push_bind(format!("Bulk {index:05}"))
                .push_bind(format!("bulk {index:05}"))
                .push_bind("Bulk Artist")
                .push_bind(i64::try_from(index).unwrap())
                .push_bind(200_000_i64)
                .push_bind("local")
                .push_bind(0_i64)
                .push_bind(0_i64);
        });
        builder.build().execute(&mut *tx).await.unwrap();
    }
    for (chunk_index, chunk) in ids.chunks(500).enumerate() {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) ",
        );
        builder.push_values(chunk.iter().enumerate(), |mut row, (offset, id)| {
            row.push_bind(user.as_bytes().to_vec())
                .push_bind("track")
                .push_bind(id.uuid().as_bytes().to_vec())
                .push_bind("saved")
                .push_bind(i64::try_from(chunk_index * 500 + offset).unwrap());
        });
        builder.build().execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    ids
}

/// A six-thousand-track library answers every track, in order, in one request.
#[tokio::test]
async fn a_six_thousand_track_library_answers_in_one_request() {
    let fixture = Fixture::new().await;
    let db = fixture.services.database();
    let library = LibraryRepository::new(db);
    let (album, _) = seed_album(&library, "Bulk Artist", "Bulk Album", 2024).await;
    let ids = bulk_library(db, fixture.user_id, album, GENERATED).await;

    let started = Instant::now();
    let (status, body) = get_queue(&fixture, "source=songs").await;
    let elapsed = started.elapsed();
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    println!("queued {} tracks in {elapsed:?}", items.len());
    assert_eq!(items.len(), GENERATED);
    assert_eq!(body["startIndex"], 0);
    assert_eq!(items[0]["title"], "Bulk 00000");
    assert_eq!(
        items[0]["id"].as_str().unwrap(),
        ids.last().unwrap().to_string()
    );
    assert_eq!(
        items[GENERATED - 1]["title"],
        format!("Bulk {:05}", GENERATED - 1)
    );
    assert_eq!(
        items[GENERATED - 1]["id"].as_str().unwrap(),
        ids.first().unwrap().to_string()
    );
    for pair in items.windows(2) {
        assert!(
            pair[0]["title"].as_str().unwrap() < pair[1]["title"].as_str().unwrap(),
            "the items follow the library's sort order"
        );
    }
    assert!(
        elapsed < GENERATED_BUDGET,
        "a queue of {GENERATED} tracks over HTTP took {elapsed:?}"
    );
}
