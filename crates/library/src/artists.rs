//! Reading and writing the `artists` table.
//!
//! An artist row is created by a scan, an upload, or the catalog layer. Local
//! artists are deduped before insert: a scan that sees the same artist in two
//! files must produce one row, so [`upsert_artist`] looks an artist up by its
//! MusicBrainz artist id when one is known, and otherwise by its
//! case-insensitive name, and only inserts when neither matches.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;

use crate::error::LibraryError;
use crate::model::{Artist, ArtistInput, decode_entity, decode_uuid, entity_key, sort_key};
use crate::now_millis;

/// One row of the `artists` table as it is stored.
#[derive(sqlx::FromRow)]
pub(crate) struct ArtistRow {
    pub(crate) id: Vec<u8>,
    pub(crate) name: String,
    pub(crate) sort_name: String,
    pub(crate) mb_artist_id: Option<String>,
    pub(crate) origin: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl ArtistRow {
    /// Converts a stored row into an artist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when the primary key is not 16 bytes
    /// and [`LibraryError::Origin`] when the `origin` column is not a known
    /// literal.
    pub(crate) fn into_artist(self) -> Result<Artist, LibraryError> {
        Ok(Artist {
            id: decode_entity(IdPrefix::Artist, &self.id)?,
            name: self.name,
            sort_name: self.sort_name,
            mb_artist_id: self.mb_artist_id,
            origin: self.origin.parse()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Inserts an artist, or refreshes the one that already matches.
///
/// The dedupe rule is: an artist with the same MusicBrainz artist id, when the
/// input carries one, otherwise an artist with the same name compared
/// case-insensitively. A matching row has its name, sort key, and `updated_at`
/// refreshed and its id returned, so a rescan of an unchanged folder does not
/// grow the table. A MusicBrainz id the input does not carry is left in place,
/// so a caller that knows less never clears a column the table already holds,
/// and the `origin` of an existing row is never rewritten: an artist a scan
/// created stays local when the catalog layer later resolves it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `artist`: the artist's fields. An omitted sort key is derived from the
///   name with [`sort_key`], and the origin is written only by the insert, so
///   a catalog-materialized artist must supply [`Origin::Catalog`](crate::Origin).
///
/// # Returns
///
/// The client id of the stored artist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn upsert_artist(
    conn: &mut SqliteConnection,
    artist: &ArtistInput,
) -> Result<EntityId, LibraryError> {
    let now = now_millis();
    let sort_name = artist
        .sort_name
        .clone()
        .unwrap_or_else(|| sort_key(&artist.name));

    let existing: Option<Vec<u8>> = match artist.mb_artist_id.as_deref() {
        Some(mbid) => {
            sqlx::query_scalar!(
                "SELECT id FROM artists WHERE mb_artist_id = ? LIMIT 1",
                mbid
            )
            .fetch_optional(&mut *conn)
            .await?
        }
        None => {
            sqlx::query_scalar!(
                "SELECT id FROM artists WHERE name = ? COLLATE NOCASE LIMIT 1",
                artist.name
            )
            .fetch_optional(&mut *conn)
            .await?
        }
    };

    if let Some(id) = existing {
        let id = decode_uuid(&id)?;
        sqlx::query!(
            "UPDATE artists SET name = ?, sort_name = ?, \
                    mb_artist_id = COALESCE(?, mb_artist_id), updated_at = ? WHERE id = ?",
            artist.name,
            sort_name,
            artist.mb_artist_id,
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&mut *conn)
        .await?;
        return Ok(EntityId::from_uuid(IdPrefix::Artist, id));
    }

    let id = new_uuid_v7();
    let origin = artist.origin.as_str();
    sqlx::query!(
        "INSERT INTO artists (id, name, sort_name, mb_artist_id, origin, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        id.as_bytes().as_slice(),
        artist.name,
        sort_name,
        artist.mb_artist_id,
        origin,
        now,
        now,
    )
    .execute(&mut *conn)
    .await?;
    Ok(EntityId::from_uuid(IdPrefix::Artist, id))
}

/// Reads an artist by its client id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the artist's client id.
///
/// # Returns
///
/// The artist, or `None` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn get_artist(
    conn: &mut SqliteConnection,
    id: EntityId,
) -> Result<Option<Artist>, LibraryError> {
    let row = sqlx::query_as!(
        ArtistRow,
        "SELECT id, name, sort_name, mb_artist_id, origin, created_at, updated_at \
         FROM artists WHERE id = ?",
        entity_key(id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(ArtistRow::into_artist).transpose()
}

/// Reads an artist by its MusicBrainz artist id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `mbid`: the MusicBrainz artist id.
///
/// # Returns
///
/// The artist, or `None` when no row carries that MBID.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_artist_by_mb_id(
    conn: &mut SqliteConnection,
    mbid: &str,
) -> Result<Option<Artist>, LibraryError> {
    let row = sqlx::query_as!(
        ArtistRow,
        "SELECT id, name, sort_name, mb_artist_id, origin, created_at, updated_at \
         FROM artists WHERE mb_artist_id = ? LIMIT 1",
        mbid,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(ArtistRow::into_artist).transpose()
}

/// Reads an artist by its case-insensitive name.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `name`: the artist's display name.
///
/// # Returns
///
/// The first artist whose name matches, or `None`.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_artist_by_name(
    conn: &mut SqliteConnection,
    name: &str,
) -> Result<Option<Artist>, LibraryError> {
    let row = sqlx::query_as!(
        ArtistRow,
        "SELECT id, name, sort_name, mb_artist_id, origin, created_at, updated_at \
         FROM artists WHERE name = ? COLLATE NOCASE LIMIT 1",
        name,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(ArtistRow::into_artist).transpose()
}

/// Lists every artist, ordered by sort key.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// Every artist, ordered by `sort_name` with the id as a tie breaker.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_artists(conn: &mut SqliteConnection) -> Result<Vec<Artist>, LibraryError> {
    let rows = sqlx::query_as!(
        ArtistRow,
        "SELECT id, name, sort_name, mb_artist_id, origin, created_at, updated_at \
         FROM artists ORDER BY sort_name, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(ArtistRow::into_artist).collect()
}

/// Lists the artists credited on a track, in credit order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track's client id.
///
/// # Returns
///
/// The credited artists ordered by their credit position.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_track_artists(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<Artist>, LibraryError> {
    let rows = sqlx::query_as!(
        ArtistRow,
        "SELECT ar.id, ar.name, ar.sort_name, ar.mb_artist_id, ar.origin, ar.created_at, \
                ar.updated_at \
         FROM track_artists ta JOIN artists ar ON ar.id = ta.artist_id \
         WHERE ta.track_id = ? ORDER BY ta.position",
        entity_key(track_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(ArtistRow::into_artist).collect()
}

/// Lists the artists credited on an album, in credit order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_id`: the album's client id.
///
/// # Returns
///
/// The credited artists ordered by their credit position.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_album_artists(
    conn: &mut SqliteConnection,
    album_id: EntityId,
) -> Result<Vec<Artist>, LibraryError> {
    let rows = sqlx::query_as!(
        ArtistRow,
        "SELECT ar.id, ar.name, ar.sort_name, ar.mb_artist_id, ar.origin, ar.created_at, \
                ar.updated_at \
         FROM album_artists aa JOIN artists ar ON ar.id = aa.artist_id \
         WHERE aa.album_id = ? ORDER BY aa.position",
        entity_key(album_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(ArtistRow::into_artist).collect()
}
