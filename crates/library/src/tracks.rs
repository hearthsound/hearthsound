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

use hearthsound_core::providers::{ExternalId, MB_ARTIST, MB_RECORDING};
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::artists::upsert_artist;
use crate::entities;
use crate::error::LibraryError;
use crate::external_ids;
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
    pub(crate) explicit: Option<i64>,
    pub(crate) file_tags: Option<String>,
    pub(crate) fetched_at: Option<i64>,
    pub(crate) deep_indexed_at: Option<i64>,
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
    /// and [`LibraryError::UnknownLiteral`] when `availability` is unknown.
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
            explicit: self.explicit.map(|value| value != 0),
            file_tags: self.file_tags,
            fetched_at: self.fetched_at,
            deep_indexed_at: self.deep_indexed_at,
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
    let availability = track.availability.as_str();
    let album_id = track.album_id.map(|album| album.uuid());

    sqlx::query!(
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, disc_number, \
                             track_number, duration_ms, explicit, file_tags, availability, \
                             unavailable_until, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        id.as_bytes().as_slice(),
        album_id.map(|album| album.as_bytes().to_vec()),
        track.title,
        sort_title,
        track.display_artist,
        track.disc_number,
        track.track_number,
        track.duration_ms,
        track.explicit,
        track.file_tags.as_ref().map(serde_json::Value::to_string),
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
/// The identity columns (`id`, `created_at`) and the availability
/// columns are left alone; availability is written by
/// [`set_track_availability`] so a rescan of a changed file does not undo a
/// pin-driven transition.
///
/// A credit that names no identifier keeps the artist row the track already
/// credits when the names agree, so a rescan of a file whose tags did not change
/// its artist never moves the track to a namesake of that artist; see
/// [`kept_credits`].
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
                          disc_number = ?, track_number = ?, duration_ms = ?, explicit = ?, \
                          file_tags = COALESCE(?, file_tags), updated_at = ? WHERE id = ?",
        album_id.map(|album| album.as_bytes().to_vec()),
        track.title,
        sort_title,
        track.display_artist,
        track.disc_number,
        track.track_number,
        track.duration_ms,
        track.explicit,
        track.file_tags.as_ref().map(serde_json::Value::to_string),
        now,
        entity_key(track_id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();

    if affected == 0 {
        return Ok(false);
    }
    let credits = kept_credits(conn, track_id, credits).await?;
    write_track_credits(conn, track_id.uuid(), &credits).await?;
    write_track_genres(conn, track_id.uuid(), genres).await?;
    Ok(true)
}

/// Keeps the credits a track already has on the rows it already credits.
///
/// A rewritten row (a rescan of a file whose tags changed, or the identity pass)
/// reads the same artist names back, and the name rule cannot tell which of two
/// identified artists of one name the file is by. The row the track already
/// credits is the answer: when a new credit names no identifier of its own and
/// its name is the name of a row the track credits, the credit is pinned to that
/// row, so a file whose tags and stored identity agree never moves to a
/// namesake.
///
/// A credit that carries an identifier is left alone, because that is where the
/// tags and the stored row disagree and the identity rule decides.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track whose credits are about to be replaced.
/// - `credits`: the credits the caller wants to write.
///
/// # Returns
///
/// The credits to write, with the unnamed ones pinned where they match.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
async fn kept_credits(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    credits: &[ArtistCredit],
) -> Result<Vec<ArtistCredit>, LibraryError> {
    if credits.iter().all(|credit| {
        credit.artist.row.is_some() || credit.artist.ids.iter().any(|id| id.namespace == MB_ARTIST)
    }) {
        return Ok(credits.to_vec());
    }
    let rows = sqlx::query!(
        "SELECT ar.id AS id, ar.name AS name \
           FROM track_artists ta JOIN artists ar ON ar.id = ta.artist_id \
          WHERE ta.track_id = ? ORDER BY ta.position",
        entity_key(track_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    let existing: Vec<(EntityId, String)> = rows
        .into_iter()
        .map(|row| Ok((decode_entity(IdPrefix::Artist, &row.id)?, row.name)))
        .collect::<Result<Vec<_>, LibraryError>>()?;
    if existing.is_empty() {
        return Ok(credits.to_vec());
    }
    Ok(credits
        .iter()
        .enumerate()
        .map(|(position, credit)| {
            if credit.artist.row.is_some()
                || credit.artist.ids.iter().any(|id| id.namespace == MB_ARTIST)
            {
                return credit.clone();
            }
            let folded = sort_key(&credit.artist.name);
            // The credit at the same position is the strongest evidence: a
            // featured artist who moved up the list keeps its row either way.
            let pinned = existing
                .get(position)
                .filter(|entry| sort_key(&entry.1) == folded)
                .or_else(|| existing.iter().find(|entry| sort_key(&entry.1) == folded));
            match pinned {
                Some((row, _)) => ArtistCredit {
                    artist: credit.artist.clone().row(*row),
                    role: credit.role.clone(),
                },
                None => credit.clone(),
            }
        })
        .collect())
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
                duration_ms, explicit, file_tags, fetched_at, deep_indexed_at, availability, \
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
                duration_ms, explicit, file_tags, fetched_at, deep_indexed_at, availability, \
                unavailable_until, created_at, updated_at FROM tracks \
         WHERE id = (SELECT entity_id FROM external_ids \
                     WHERE entity_kind = 'track' AND namespace = 'mb-recording' \
                       AND value = ? LIMIT 1)",
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
                duration_ms, explicit, file_tags, fetched_at, deep_indexed_at, availability, \
                unavailable_until, created_at, updated_at FROM tracks \
         WHERE id = (SELECT entity_id FROM external_ids \
                     WHERE entity_kind = 'track' AND namespace = 'isrc' \
                       AND value = ? LIMIT 1)",
        isrc,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(TrackRow::into_track).transpose()
}

/// A stored track with the title of the album it belongs to.
///
/// The catalog layer compares a track it wants to materialize against the rows
/// the server already holds, and the album title is part of that comparison.
/// One query returns both, so a candidate read does not need a second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackCandidate {
    /// The stored track.
    pub track: Track,
    /// The title of the track's album, or `None` when it has no album.
    pub album_title: Option<String>,
}

/// Reads the stored tracks whose sort title is `sort_title`.
///
/// The catalog layer uses this as a bounded candidate set before it compares
/// normalized titles: a sort title folds case and whitespace, so this returns
/// the rows worth comparing without scanning the whole table.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `sort_title`: the folder key of the title, as [`crate::sort_key`] builds it.
///
/// # Returns
///
/// One candidate per matching row, oldest first.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_tracks_by_sort_title(
    conn: &mut SqliteConnection,
    sort_title: &str,
) -> Result<Vec<TrackCandidate>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT t.id, t.album_id, t.title, t.sort_title, t.display_artist, t.disc_number, \
                t.track_number, t.duration_ms, t.explicit, t.file_tags, \
                t.fetched_at, t.deep_indexed_at, t.availability, \
                t.unavailable_until, t.created_at, t.updated_at, \
                a.title AS album_title \
         FROM tracks t LEFT JOIN albums a ON a.id = t.album_id \
         WHERE t.sort_title = ? ORDER BY t.created_at, t.id",
        sort_title,
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut candidates = Vec::with_capacity(rows.len());
    for row in rows {
        let track = TrackRow {
            id: row.id,
            album_id: row.album_id,
            title: row.title,
            sort_title: row.sort_title,
            display_artist: row.display_artist,
            disc_number: row.disc_number,
            track_number: row.track_number,
            duration_ms: row.duration_ms,
            explicit: row.explicit,
            file_tags: row.file_tags,
            fetched_at: row.fetched_at,
            deep_indexed_at: row.deep_indexed_at,
            availability: row.availability,
            unavailable_until: row.unavailable_until,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
        .into_track()?;
        candidates.push(TrackCandidate {
            track,
            album_title: row.album_title,
        });
    }
    Ok(candidates)
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
                duration_ms, explicit, file_tags, fetched_at, deep_indexed_at, availability, \
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
    let id = ExternalId {
        namespace: MB_RECORDING.to_owned(),
        value: mbid.to_owned(),
    };
    let conflicts = external_ids::attach(
        conn,
        EntityKind::Track,
        track_id.uuid(),
        std::slice::from_ref(&id),
    )
    .await?;
    let mut survivor = track_id.uuid();
    for conflict in conflicts {
        survivor = entities::merge(conn, EntityKind::Track, survivor, conflict.owner).await?;
    }
    let now = now_millis();
    sqlx::query("UPDATE tracks SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(survivor.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(true)
}

/// Merges one track row into another, moving everything that names it.
///
/// The survivor takes the loser's files, retention reasons, credits, genres,
/// acquisitions, saved items, likes, ratings, provider answers, images and
/// playlist positions; a playlist that would then hold the same track twice
/// keeps one entry, and a draft queue whose saved ids named the loser is
/// rewritten to the survivor. Availability is recomputed from the surviving
/// files, and the loser row is deleted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `from`: the row that goes away.
/// - `into`: the row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub(crate) async fn merge_track(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    for (insert, delete) in [
        (
            "INSERT OR IGNORE INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
             SELECT user_id, entity_kind, ?, value, at FROM user_feedback \
             WHERE entity_kind = 'track' AND entity_id = ?",
            "DELETE FROM user_feedback WHERE entity_kind = 'track' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
             SELECT user_id, entity_kind, ?, origin, added_at FROM user_library \
             WHERE entity_kind = 'track' AND entity_id = ?",
            "DELETE FROM user_library WHERE entity_kind = 'track' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
             SELECT user_id, entity_kind, ?, rating, at FROM user_ratings \
             WHERE entity_kind = 'track' AND entity_id = ?",
            "DELETE FROM user_ratings WHERE entity_kind = 'track' AND entity_id = ?",
        ),
    ] {
        sqlx::query(insert)
            .bind(into_key.as_slice())
            .bind(from_key.as_slice())
            .execute(&mut *conn)
            .await?;
        sqlx::query(delete)
            .bind(from_key.as_slice())
            .execute(&mut *conn)
            .await?;
    }
    for statement in [
        "UPDATE external_ids SET entity_id = ? WHERE entity_kind = 'track' AND entity_id = ?",
        "UPDATE entity_sources SET entity_id = ? WHERE entity_kind = 'track' AND entity_id = ?",
        "UPDATE entity_images SET entity_id = ? WHERE entity_kind = 'track' AND entity_id = ?",
        "UPDATE index_queue SET entity_id = ? WHERE entity_kind = 'track' AND entity_id = ?",
        "UPDATE media_files SET track_id = ? WHERE track_id = ?",
        "UPDATE track_genres SET track_id = ? WHERE track_id = ? AND genre_id NOT IN (\
             SELECT genre_id FROM track_genres WHERE track_id = ?)",
        "UPDATE acquisitions SET track_id = ? WHERE track_id = ?",
        "UPDATE bookmarks SET track_id = ? WHERE track_id = ? AND track_id NOT IN (\
             SELECT track_id FROM bookmarks WHERE user_id = bookmarks.user_id AND track_id = ?)",
        "UPDATE plays SET track_id = ? WHERE track_id = ?",
        "UPDATE listen_sessions SET track_id = ? WHERE track_id = ?",
        "UPDATE playlist_tracks SET track_id = ? WHERE track_id = ?",
        "UPDATE artist_top_tracks SET track_id = ? WHERE track_id = ?",
    ] {
        if statement.contains("track_genres") || statement.contains("bookmarks") {
            sqlx::query(statement)
                .bind(into_key.as_slice())
                .bind(from_key.as_slice())
                .bind(into_key.as_slice())
                .execute(&mut *conn)
                .await?;
            continue;
        }
        sqlx::query(statement)
            .bind(into_key.as_slice())
            .bind(from_key.as_slice())
            .execute(&mut *conn)
            .await?;
    }
    // Credits move to free positions on the survivor; a duplicate of one it
    // already credits is dropped.
    crate::entities::move_track_credits(conn, from, into).await?;
    sqlx::query(
        "DELETE FROM playlist_tracks WHERE track_id = ? AND rowid NOT IN (\
             SELECT MIN(rowid) FROM playlist_tracks WHERE track_id = ? GROUP BY playlist_id)",
    )
    .bind(into_key.as_slice())
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE OR IGNORE track_retention SET track_id = ? WHERE track_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM track_retention WHERE track_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM tracks WHERE id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Deletes a track and everything that hangs off it.
///
/// The `ON DELETE CASCADE` clauses remove the track's credits, genres,
/// retention row, media files, plays and playlist entries, and the delete
/// triggers remove its search index entry and the rows that name it without a
/// foreign key: its `external_ids` aliases, its `user_library` saves, its
/// `user_feedback` likes and dislikes, and its `user_ratings`. Nothing is left
/// pointing at an id that no longer exists, whatever path deleted the row.
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

/// Counts what deleting one folder's tracks would take with it.
///
/// The predicate is exactly the delete condition of
/// [`crate::LibraryRepository::remove_library_folder`] and of the scanner's
/// pruning - a locally created track that has a file in this folder and no file
/// anywhere else - so the preview and the delete cannot disagree. The likes and
/// playlist entries are counted among those tracks only, because they are the
/// rows the delete takes with them.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `folder_id`: the folder a removal is being previewed for.
///
/// # Returns
///
/// The tracks, likes and playlist entries a removal would delete.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn removal_impact(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
) -> Result<crate::model::FolderRemovalImpact, LibraryError> {
    let folder = folder_id.as_bytes().as_slice();
    let row = sqlx::query!(
        "WITH doomed AS ( \
             SELECT t.id FROM tracks t \
              WHERE NOT EXISTS (SELECT 1 FROM external_ids e \
                                 WHERE e.entity_kind = 'track' AND e.entity_id = t.id) \
                AND EXISTS (SELECT 1 FROM media_files mf \
                             WHERE mf.track_id = t.id AND mf.folder_id = ?) \
                AND NOT EXISTS (SELECT 1 FROM media_files mf \
                                 WHERE mf.track_id = t.id \
                                   AND (mf.folder_id IS NULL OR mf.folder_id <> ?)) \
         ) \
         SELECT (SELECT COUNT(*) FROM doomed) AS tracks, \
                (SELECT COUNT(*) FROM user_feedback f JOIN doomed d ON d.id = f.entity_id \
                  WHERE f.entity_kind = 'track' AND f.value = 'like') AS likes, \
                (SELECT COUNT(*) FROM playlist_tracks pt JOIN doomed d ON d.id = pt.track_id) \
                    AS playlist_entries",
        folder,
        folder,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(crate::model::FolderRemovalImpact {
        tracks: row.tracks,
        likes: row.likes,
        playlist_entries: row.playlist_entries,
    })
}
