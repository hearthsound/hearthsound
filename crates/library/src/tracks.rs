//! Reading and writing the `tracks`, `track_artists`, `genres`, and
//! `track_genres` tables.
//!
//! Tracks are not deduped here. A track row is created either by a scan, which
//! creates exactly one per recognised file, or by the catalog layer, which
//! resolves a recording's identity through the metadata providers before it
//! writes; this module offers the identity lookups
//! ([`find_track_by_mb_recording_id`], [`find_track_by_isrc`]) that resolution
//! needs but never merges two rows itself.
//!
//! Genre names are case-folded before insert, so `Rock` and `rock` share one
//! `genres` row and the unique index on the name actually merges duplicates.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::{Availability, EntityId, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::artists::upsert_artist;
use crate::error::LibraryError;
use crate::model::{
    ArtistCredit, Credit, Genre, Track, TrackInput, decode_entity, entity_key, sort_key,
};
use crate::now_millis;

/// One row of the `tracks` table as it is stored.
#[derive(sqlx::FromRow)]
pub(crate) struct TrackRow {
    pub(crate) id: Vec<u8>,
    pub(crate) album_id: Option<Vec<u8>>,
    pub(crate) title: String,
    pub(crate) sort_title: String,
    pub(crate) display_artist: Option<String>,
    pub(crate) disc_number: Option<i64>,
    pub(crate) track_number: Option<i64>,
    pub(crate) duration_ms: Option<i64>,
    pub(crate) isrc: Option<String>,
    pub(crate) mb_recording_id: Option<String>,
    pub(crate) explicit: Option<i64>,
    pub(crate) origin: String,
    pub(crate) availability: String,
    pub(crate) unavailable_until: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl TrackRow {
    /// Converts a stored row into a track.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when a primary key is not 16 bytes,
    /// [`LibraryError::Origin`] when the `origin` column is unknown, and
    /// [`LibraryError::UnknownLiteral`] when `availability` is unknown.
    pub(crate) fn into_track(self) -> Result<Track, LibraryError> {
        let album_id = match self.album_id.as_deref() {
            Some(bytes) => Some(decode_entity(IdPrefix::Album, bytes)?),
            None => None,
        };
        let availability = self.availability.parse::<Availability>().map_err(|_| {
            LibraryError::UnknownLiteral {
                kind: "availability",
                value: self.availability,
            }
        })?;
        Ok(Track {
            id: decode_entity(IdPrefix::Track, &self.id)?,
            album_id,
            title: self.title,
            sort_title: self.sort_title,
            display_artist: self.display_artist,
            disc_number: self.disc_number,
            track_number: self.track_number,
            duration_ms: self.duration_ms,
            isrc: self.isrc,
            mb_recording_id: self.mb_recording_id,
            explicit: self.explicit.map(|value| value != 0),
            origin: self.origin.parse()?,
            availability,
            unavailable_until: self.unavailable_until,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Inserts a track and its credits and genres.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track`: the track's fields. An omitted sort key is derived from the title
///   with [`sort_key`].
/// - `credits`: the ordered artist credits.
/// - `genres`: the genre names; each is case-folded and upserted.
///
/// # Returns
///
/// The client id of the stored track.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn insert_track(
    conn: &mut SqliteConnection,
    track: &TrackInput,
    credits: &[ArtistCredit],
    genres: &[String],
) -> Result<EntityId, LibraryError> {
    let now = now_millis();
    let sort_title = track
        .sort_title
        .clone()
        .unwrap_or_else(|| sort_key(&track.title));
    let id = new_uuid_v7();
    let origin = track.origin.as_str();
    let availability = track.availability.as_str();
    let album_id = track.album_id.map(|album| album.uuid());

    sqlx::query!(
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, disc_number, \
                             track_number, duration_ms, isrc, mb_recording_id, explicit, origin, \
                             availability, unavailable_until, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        id.as_bytes().as_slice(),
        album_id.map(|album| album.as_bytes().to_vec()),
        track.title,
        sort_title,
        track.display_artist,
        track.disc_number,
        track.track_number,
        track.duration_ms,
        track.isrc,
        track.mb_recording_id,
        track.explicit,
        origin,
        availability,
        track.unavailable_until,
        now,
        now,
    )
    .execute(&mut *conn)
    .await?;

    let track_id = EntityId::from_uuid(IdPrefix::Track, id);
    write_track_credits(conn, id, credits).await?;
    write_track_genres(conn, id, genres).await?;
    Ok(track_id)
}

/// Refreshes a track's mutable fields, credits, and genres.
///
/// The identity columns (`id`, `origin`, `created_at`) and the availability
/// columns are left alone; availability is written by
/// [`set_track_availability`] so a rescan of a changed file does not undo a
/// pin-driven transition.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to update.
/// - `track`: the track's fields.
/// - `credits`: the ordered artist credits.
/// - `genres`: the genre names.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn update_track(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    track: &TrackInput,
    credits: &[ArtistCredit],
    genres: &[String],
) -> Result<bool, LibraryError> {
    let now = now_millis();
    let sort_title = track
        .sort_title
        .clone()
        .unwrap_or_else(|| sort_key(&track.title));
    let album_id = track.album_id.map(|album| album.uuid());

    let affected = sqlx::query!(
        "UPDATE tracks SET album_id = ?, title = ?, sort_title = ?, display_artist = ?, \
                          disc_number = ?, track_number = ?, duration_ms = ?, isrc = ?, \
                          mb_recording_id = ?, explicit = ?, updated_at = ? WHERE id = ?",
        album_id.map(|album| album.as_bytes().to_vec()),
        track.title,
        sort_title,
        track.display_artist,
        track.disc_number,
        track.track_number,
        track.duration_ms,
        track.isrc,
        track.mb_recording_id,
        track.explicit,
        now,
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    if affected == 0 {
        return Ok(false);
    }
    write_track_credits(conn, track_id.uuid(), credits).await?;
    write_track_genres(conn, track_id.uuid(), genres).await?;
    Ok(true)
}

/// Replaces a track's artist credits.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track's raw primary key.
/// - `credits`: the credits to write.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
async fn write_track_credits(
    conn: &mut SqliteConnection,
    track_id: uuid::Uuid,
    credits: &[ArtistCredit],
) -> Result<(), LibraryError> {
    sqlx::query!(
        "DELETE FROM track_artists WHERE track_id = ?",
        track_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?;
    for (position, credit) in credits.iter().enumerate() {
        let artist_id = upsert_artist(conn, &credit.artist).await?;
        let position = position as i64;
        sqlx::query!(
            "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
            track_id.as_bytes().as_slice(),
            entity_key(artist_id),
            position,
            credit.role,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Replaces a track's genres.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track's raw primary key.
/// - `genres`: the genre names.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
async fn write_track_genres(
    conn: &mut SqliteConnection,
    track_id: uuid::Uuid,
    genres: &[String],
) -> Result<(), LibraryError> {
    sqlx::query!(
        "DELETE FROM track_genres WHERE track_id = ?",
        track_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?;
    for name in genres {
        let genre_id = upsert_genre(conn, name).await?;
        sqlx::query!(
            "INSERT OR IGNORE INTO track_genres (track_id, genre_id) VALUES (?, ?)",
            track_id.as_bytes().as_slice(),
            genre_id.as_bytes().to_vec(),
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Inserts a genre, or returns the row that already holds the folded name.
///
/// The name is trimmed and Unicode-lowercased before the lookup and the insert,
/// so `Rock`, `rock`, and `ROCK` are one row and the stored name is the folded
/// form.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `name`: the genre name as the tags or the catalog gave it.
///
/// # Returns
///
/// The stored genre's id. An empty or whitespace-only name folds to the empty
/// string and is still stored, so callers that care should filter empty names
/// out.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn upsert_genre(conn: &mut SqliteConnection, name: &str) -> Result<Uuid, LibraryError> {
    let folded = fold_genre(name);
    let existing: Option<Vec<u8>> =
        sqlx::query_scalar!("SELECT id FROM genres WHERE name = ? LIMIT 1", folded)
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(bytes) = existing {
        return Ok(crate::model::decode_uuid(&bytes)?);
    }
    let id = Uuid::now_v7();
    sqlx::query!(
        "INSERT INTO genres (id, name) VALUES (?, ?)",
        id.as_bytes().as_slice(),
        folded,
    )
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

/// Folds a genre name to the stored, case-insensitive form.
///
/// # Parameters
///
/// - `name`: the genre name.
///
/// # Returns
///
/// The trimmed, lowercased name.
fn fold_genre(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Reads a track by its client id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the track's client id.
///
/// # Returns
///
/// The track, or `None` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn get_track(
    conn: &mut SqliteConnection,
    id: EntityId,
) -> Result<Option<Track>, LibraryError> {
    let row = sqlx::query_as!(
        TrackRow,
        "SELECT id, album_id, title, sort_title, display_artist, disc_number, track_number, \
                duration_ms, isrc, mb_recording_id, explicit, origin, availability, \
                unavailable_until, created_at, updated_at FROM tracks WHERE id = ?",
        entity_key(id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(TrackRow::into_track).transpose()
}

/// Reads a track by its MusicBrainz recording id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `mbid`: the MusicBrainz recording id.
///
/// # Returns
///
/// The track, or `None` when no row carries that recording id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_track_by_mb_recording_id(
    conn: &mut SqliteConnection,
    mbid: &str,
) -> Result<Option<Track>, LibraryError> {
    let row = sqlx::query_as!(
        TrackRow,
        "SELECT id, album_id, title, sort_title, display_artist, disc_number, track_number, \
                duration_ms, isrc, mb_recording_id, explicit, origin, availability, \
                unavailable_until, created_at, updated_at FROM tracks \
         WHERE mb_recording_id = ? LIMIT 1",
        mbid,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(TrackRow::into_track).transpose()
}

/// Reads a track by its ISRC.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `isrc`: the International Standard Recording Code.
///
/// # Returns
///
/// The first track carrying that ISRC, or `None`.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_track_by_isrc(
    conn: &mut SqliteConnection,
    isrc: &str,
) -> Result<Option<Track>, LibraryError> {
    let row = sqlx::query_as!(
        TrackRow,
        "SELECT id, album_id, title, sort_title, display_artist, disc_number, track_number, \
                duration_ms, isrc, mb_recording_id, explicit, origin, availability, \
                unavailable_until, created_at, updated_at FROM tracks \
         WHERE isrc = ? LIMIT 1",
        isrc,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(TrackRow::into_track).transpose()
}

/// Lists an album's tracks in playing order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_id`: the album's client id.
///
/// # Returns
///
/// The tracks ordered by disc number, then track number, then title.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_tracks_for_album(
    conn: &mut SqliteConnection,
    album_id: EntityId,
) -> Result<Vec<Track>, LibraryError> {
    let rows = sqlx::query_as!(
        TrackRow,
        "SELECT id, album_id, title, sort_title, display_artist, disc_number, track_number, \
                duration_ms, isrc, mb_recording_id, explicit, origin, availability, \
                unavailable_until, created_at, updated_at FROM tracks WHERE album_id = ? \
         ORDER BY disc_number IS NULL, disc_number, track_number IS NULL, track_number, \
                  sort_title, id",
        entity_key(album_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(TrackRow::into_track).collect()
}

/// Lists a track's artist credits in order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track's client id.
///
/// # Returns
///
/// The credits ordered by position.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_track_credits(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<Credit>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT artist_id, position, role FROM track_artists WHERE track_id = ? ORDER BY position",
        entity_key(track_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Credit {
                artist_id: decode_entity(IdPrefix::Artist, &row.artist_id)?,
                position: row.position,
                role: row.role,
            })
        })
        .collect()
}

/// Lists a track's genres.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track's client id.
///
/// # Returns
///
/// The genres ordered by name.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_track_genres(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<Genre>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT g.id, g.name FROM track_genres tg JOIN genres g ON g.id = tg.genre_id \
         WHERE tg.track_id = ? ORDER BY g.name",
        entity_key(track_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(Genre {
                id: crate::model::decode_uuid(&row.id)?,
                name: row.name,
            })
        })
        .collect()
}

/// Writes a track's availability.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to update.
/// - `availability`: the new availability.
/// - `unavailable_until`: the retry instant for an `unavailable` track.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn set_track_availability(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    availability: Availability,
    unavailable_until: Option<i64>,
) -> Result<bool, LibraryError> {
    let now = now_millis();
    let affected = sqlx::query!(
        "UPDATE tracks SET availability = ?, unavailable_until = ?, updated_at = ? WHERE id = ?",
        availability.as_str(),
        unavailable_until,
        now,
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Writes a track's MusicBrainz recording id.
///
/// Identity resolution learns the canonical recording id after the track row
/// exists, so it needs a setter narrower than [`update_track`], which rewrites
/// every mutable column and the track's credits with it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to update.
/// - `mbid`: the MusicBrainz recording id.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn set_track_mb_recording_id(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    mbid: &str,
) -> Result<bool, LibraryError> {
    let now = now_millis();
    let affected = sqlx::query!(
        "UPDATE tracks SET mb_recording_id = ?, updated_at = ? WHERE id = ?",
        mbid,
        now,
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Deletes a track and everything that hangs off it.
///
/// The `ON DELETE CASCADE` clauses remove the track's credits, genres,
/// retention row, and media files, and the delete trigger removes its search
/// index entry.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to delete.
///
/// # Returns
///
/// `true` when a row was deleted, `false` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the delete fails.
pub async fn delete_track(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!("DELETE FROM tracks WHERE id = ?", entity_key(track_id),)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(affected > 0)
}
