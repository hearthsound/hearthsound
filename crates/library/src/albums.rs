//! Reading and writing the `albums` and `album_artists` tables.
//!
//! An album is deduped before insert the same way an artist is: by its
//! MusicBrainz release id when the input carries one, and otherwise by the pair
//! of its folded title and its primary artist (the credit at position 0). The
//! credit list is rewritten wholesale on every upsert, so an album's credits
//! always match the input that last wrote it.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;

use crate::artists::upsert_artist;
use crate::error::LibraryError;
use crate::model::{
    Album, AlbumInput, ArtistCredit, Credit, decode_entity, decode_uuid, entity_key, sort_key,
};
use crate::now_millis;

/// One row of the `albums` table as it is stored.
#[derive(sqlx::FromRow)]
pub(crate) struct AlbumRow {
    pub(crate) id: Vec<u8>,
    pub(crate) title: String,
    pub(crate) sort_title: String,
    pub(crate) display_artist: Option<String>,
    pub(crate) year: Option<i64>,
    pub(crate) release_date: Option<String>,
    pub(crate) album_type: Option<String>,
    pub(crate) mb_release_id: Option<String>,
    pub(crate) mb_release_group_id: Option<String>,
    pub(crate) upc: Option<String>,
    pub(crate) origin: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl AlbumRow {
    /// Converts a stored row into an album.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when the primary key is not 16 bytes
    /// and [`LibraryError::Origin`] when the `origin` column is not a known
    /// literal.
    pub(crate) fn into_album(self) -> Result<Album, LibraryError> {
        Ok(Album {
            id: decode_entity(IdPrefix::Album, &self.id)?,
            title: self.title,
            sort_title: self.sort_title,
            display_artist: self.display_artist,
            year: self.year,
            release_date: self.release_date,
            album_type: self.album_type,
            mb_release_id: self.mb_release_id,
            mb_release_group_id: self.mb_release_group_id,
            upc: self.upc,
            origin: self.origin.parse()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Inserts an album, or refreshes the one that already matches.
///
/// The credited artists are upserted first, so the returned album always has its
/// credits in the database. The album itself is deduped by MusicBrainz release
/// id when the input carries one, and otherwise by the folded title together
/// with the credit at position 0 (its primary artist); a matching row has its
/// fields refreshed rather than duplicated.
///
/// The optional columns are merged, not overwritten: a column the input leaves
/// empty keeps the value the row already holds. An album is enriched by whoever
/// knows more (a scan that read tags, a provider that resolved a release) and is
/// never degraded by a caller that knows less. The title and its sort key are
/// always written, because every caller supplies them.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album`: the album's fields.
/// - `credits`: the ordered artist credits; the first one is the primary artist
///   used by the dedupe.
///
/// # Returns
///
/// The client id of the stored album.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn upsert_album(
    conn: &mut SqliteConnection,
    album: &AlbumInput,
    credits: &[ArtistCredit],
) -> Result<EntityId, LibraryError> {
    let mut artist_ids = Vec::with_capacity(credits.len());
    for credit in credits {
        artist_ids.push(upsert_artist(conn, &credit.artist).await?);
    }

    let now = now_millis();
    let sort_title = album
        .sort_title
        .clone()
        .unwrap_or_else(|| sort_key(&album.title));

    let existing: Option<Vec<u8>> = match album.mb_release_id.as_deref() {
        Some(mbid) => {
            sqlx::query_scalar!(
                "SELECT id FROM albums WHERE mb_release_id = ? LIMIT 1",
                mbid
            )
            .fetch_optional(&mut *conn)
            .await?
        }
        None => match artist_ids.first() {
            Some(primary) => {
                sqlx::query_scalar!(
                    "SELECT a.id FROM albums a \
                     JOIN album_artists aa ON aa.album_id = a.id AND aa.position = 0 \
                     WHERE a.sort_title = ? AND aa.artist_id = ? LIMIT 1",
                    sort_title,
                    entity_key(*primary),
                )
                .fetch_optional(&mut *conn)
                .await?
            }
            None => None,
        },
    };

    let id = match existing {
        Some(bytes) => {
            let id = decode_uuid(&bytes)?;
            sqlx::query!(
                "UPDATE albums SET title = ?, sort_title = ?, \
                        display_artist = COALESCE(?, display_artist), year = COALESCE(?, year), \
                        release_date = COALESCE(?, release_date), \
                        album_type = COALESCE(?, album_type), \
                        mb_release_id = COALESCE(?, mb_release_id), \
                        mb_release_group_id = COALESCE(?, mb_release_group_id), \
                        upc = COALESCE(?, upc), updated_at = ? WHERE id = ?",
                album.title,
                sort_title,
                album.display_artist,
                album.year,
                album.release_date,
                album.album_type,
                album.mb_release_id,
                album.mb_release_group_id,
                album.upc,
                now,
                id.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            id
        }
        None => {
            let id = new_uuid_v7();
            let origin = album.origin.as_str();
            sqlx::query!(
                "INSERT INTO albums (id, title, sort_title, display_artist, year, release_date, \
                                     album_type, mb_release_id, mb_release_group_id, upc, origin, \
                                     created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                id.as_bytes().as_slice(),
                album.title,
                sort_title,
                album.display_artist,
                album.year,
                album.release_date,
                album.album_type,
                album.mb_release_id,
                album.mb_release_group_id,
                album.upc,
                origin,
                now,
                now,
            )
            .execute(&mut *conn)
            .await?;
            id
        }
    };

    write_album_credits(conn, id, credits, &artist_ids).await?;
    Ok(EntityId::from_uuid(IdPrefix::Album, id))
}

/// Adds an album's artist credits when it has none.
///
/// An album materialized from the album reference of a single track can arrive
/// without credits when the provider left the reference's artist list empty.
/// The track's own credits are then the best evidence of who the album is by,
/// and this writes them so the album is listed under those artists.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album_id`: the album's client id.
/// - `credits`: the credits to write when the album has none.
/// - `display_artist`: the display artist to fill in when the row has none.
///
/// # Returns
///
/// `true` when credits were written, `false` when the album already had some or
/// none were supplied.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub async fn add_album_credits_if_missing(
    conn: &mut SqliteConnection,
    album_id: EntityId,
    credits: &[ArtistCredit],
    display_artist: Option<&str>,
) -> Result<bool, LibraryError> {
    if credits.is_empty() {
        return Ok(false);
    }
    let album_key = entity_key(album_id);
    let existing: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM album_artists WHERE album_id = ?",
        album_key,
    )
    .fetch_one(&mut *conn)
    .await?;
    if existing > 0 {
        return Ok(false);
    }
    let mut artist_ids = Vec::with_capacity(credits.len());
    for credit in credits {
        artist_ids.push(upsert_artist(conn, &credit.artist).await?);
    }
    write_album_credits(conn, album_id.uuid(), credits, &artist_ids).await?;
    if let Some(display_artist) = display_artist {
        sqlx::query!(
            "UPDATE albums SET display_artist = COALESCE(display_artist, ?), updated_at = ? \
             WHERE id = ?",
            display_artist,
            now_millis(),
            album_key,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(true)
}

/// Replaces an album's artist credits.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album_id`: the album's raw primary key.
/// - `credits`: the credits as the caller supplied them.
/// - `artist_ids`: the resolved artist ids, in the same order as `credits`.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
async fn write_album_credits(
    conn: &mut SqliteConnection,
    album_id: uuid::Uuid,
    credits: &[ArtistCredit],
    artist_ids: &[EntityId],
) -> Result<(), LibraryError> {
    sqlx::query!(
        "DELETE FROM album_artists WHERE album_id = ?",
        album_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?;
    for (position, (credit, artist_id)) in credits.iter().zip(artist_ids).enumerate() {
        let position = position as i64;
        sqlx::query!(
            "INSERT INTO album_artists (album_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
            album_id.as_bytes().as_slice(),
            entity_key(*artist_id),
            position,
            credit.role,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Reads an album by its client id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the album's client id.
///
/// # Returns
///
/// The album, or `None` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn get_album(
    conn: &mut SqliteConnection,
    id: EntityId,
) -> Result<Option<Album>, LibraryError> {
    let row = sqlx::query_as!(
        AlbumRow,
        "SELECT id, title, sort_title, display_artist, year, release_date, album_type, \
                mb_release_id, mb_release_group_id, upc, origin, created_at, updated_at \
         FROM albums WHERE id = ?",
        entity_key(id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AlbumRow::into_album).transpose()
}

/// Reads an album by its MusicBrainz release id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `mbid`: the MusicBrainz release id.
///
/// # Returns
///
/// The album, or `None` when no row carries that release id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_album_by_mb_release_id(
    conn: &mut SqliteConnection,
    mbid: &str,
) -> Result<Option<Album>, LibraryError> {
    let row = sqlx::query_as!(
        AlbumRow,
        "SELECT id, title, sort_title, display_artist, year, release_date, album_type, \
                mb_release_id, mb_release_group_id, upc, origin, created_at, updated_at \
         FROM albums WHERE mb_release_id = ? LIMIT 1",
        mbid,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AlbumRow::into_album).transpose()
}

/// Lists the albums a primary artist is credited on.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `artist_id`: the artist's client id.
///
/// # Returns
///
/// The albums ordered by release year and title, oldest first; an album with no
/// year sorts before one with a year.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_albums_for_artist(
    conn: &mut SqliteConnection,
    artist_id: EntityId,
) -> Result<Vec<Album>, LibraryError> {
    let rows = sqlx::query_as!(
        AlbumRow,
        "SELECT al.id, al.title, al.sort_title, al.display_artist, al.year, al.release_date, \
                al.album_type, al.mb_release_id, al.mb_release_group_id, al.upc, al.origin, \
                al.created_at, al.updated_at \
         FROM albums al \
         WHERE (EXISTS (SELECT 1 FROM album_artists aa \
                        WHERE aa.album_id = al.id AND aa.artist_id = ?) \
             OR EXISTS (SELECT 1 FROM tracks t JOIN track_artists ta ON ta.track_id = t.id \
                        WHERE t.album_id = al.id AND ta.artist_id = ?)) \
         ORDER BY al.year IS NULL, al.year, al.sort_title, al.id",
        entity_key(artist_id),
        entity_key(artist_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(AlbumRow::into_album).collect()
}

/// Lists an album's artist credits in order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_id`: the album's client id.
///
/// # Returns
///
/// The credits ordered by position.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_album_credits(
    conn: &mut SqliteConnection,
    album_id: EntityId,
) -> Result<Vec<Credit>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT artist_id, position, role FROM album_artists WHERE album_id = ? ORDER BY position",
        entity_key(album_id),
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
