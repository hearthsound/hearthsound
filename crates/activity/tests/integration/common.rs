//! Helpers shared by the activity integration tests.
//!
//! Every test runs against a temporary migrated database, so the statements
//! under test are the ones the server will run. The rows a test needs - an
//! account, a catalog album with its tracks - are created through the
//! repositories the server writes them with, which is what makes a pin or an
//! availability visible in the same shape the server sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{Availability, EntityId, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, TrackInput};
use uuid::Uuid;

/// Opens a fresh temporary database with the server schema applied.
///
/// # Parameters
///
/// - `dir`: the temporary directory the database file lives in.
///
/// # Returns
///
/// The open database.
pub async fn open_database(dir: &Path) -> Database {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    database
        .migrate(&hearthsound_db::MIGRATOR, &dir.join("backups"))
        .await
        .unwrap();
    database
}

/// Inserts an account and returns its id.
///
/// # Parameters
///
/// - `database`: the open database.
pub async fn insert_user(database: &Database) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at)
         VALUES (?, ?, 'Test User', 'not-a-hash', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .execute(database.writer())
    .await
    .unwrap();
    id
}

/// Inserts a remote album with two tracks.
///
/// A catalog track has no file of its own, which is what makes the difference
/// between a pin and an acquisition visible.
///
/// # Parameters
///
/// - `library`: the repository to write through.
///
/// # Returns
///
/// The album's id and the ids of its two tracks.
pub async fn insert_album(library: &LibraryRepository) -> (Uuid, Vec<Uuid>) {
    let mut album = AlbumInput::new("Remote Album");
    album.display_artist = Some("Remote Artist".to_owned());
    let album_id = library
        .upsert_album(&album, &[ArtistCredit::new("Remote Artist", "main")])
        .await
        .unwrap();
    let mut tracks = Vec::new();
    for (index, title) in ["First", "Second"].iter().enumerate() {
        let mut input = TrackInput::new(*title, Availability::Remote);
        input.album_id = Some(album_id);
        input.track_number = Some(index as i64 + 1);
        let track = library
            .insert_track(&input, &[ArtistCredit::new("Remote Artist", "main")], &[])
            .await
            .unwrap();
        tracks.push(track.uuid());
    }
    (album_id.uuid(), tracks)
}

/// Returns the client id of a track.
///
/// # Parameters
///
/// - `track`: the track's id.
pub fn track_id(track: Uuid) -> EntityId {
    EntityId::from_uuid(IdPrefix::Track, track)
}

/// Adds a remote track to an album.
///
/// # Parameters
///
/// - `library`: the repository to write through.
/// - `album`: the album's id.
/// - `title`: the track title.
/// - `number`: the track number within the album.
///
/// # Returns
///
/// The new track's id.
pub async fn insert_track(
    library: &LibraryRepository,
    album: Uuid,
    title: &str,
    number: i64,
) -> Uuid {
    let mut input = TrackInput::new(title, Availability::Remote);
    input.album_id = Some(EntityId::from_uuid(IdPrefix::Album, album));
    input.track_number = Some(number);
    library
        .insert_track(&input, &[ArtistCredit::new("Remote Artist", "main")], &[])
        .await
        .unwrap()
        .uuid()
}

/// Returns whether a track carries any pin.
///
/// # Parameters
///
/// - `library`: the repository to read through.
/// - `track`: the track's id.
pub async fn is_pinned(library: &LibraryRepository, track: Uuid) -> bool {
    library.track_is_pinned(track_id(track)).await.unwrap()
}
