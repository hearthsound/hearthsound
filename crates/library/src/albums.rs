//! Reading and writing the `albums` and `album_artists` tables.
//!
//! An album is identified the same way an artist is, and its identity is
//! `(folded title, primary artist, year)`:
//!
//! - a MusicBrainz release id identifies an album outright, and rows that share
//!   one are one album;
//! - without an id, the folded title together with the credit at position 0 and
//!   the year is the identity. `NULL` is a year like any other, so an untagged
//!   file is its own album, and two albums of one title by one artist with
//!   different years stay apart;
//! - a release id is adopted by an id-less row of the same identity, so
//!   re-tagging a file with an id does not create a second album.
//!
//! The year and the release date are written once: a later scan or provider that
//! knows a different year never replaces them. The credit list is rewritten
//! wholesale on every upsert, so an album's credits always match the input that
//! last wrote it.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

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

/// One album row of an identity group, as the identity rule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlbumIdentityRow {
    /// The row's raw primary key.
    pub(crate) id: Uuid,
    /// The row's MusicBrainz release id.
    pub(crate) mb_release_id: Option<String>,
}

/// Inserts an album, or refreshes the one that already matches.
///
/// The credited artists are upserted first, so the returned album always has its
/// credits in the database and the primary credit is known. The album is then
/// resolved by the identity rule documented on this module:
///
/// 1. an input whose release id an existing row holds is that row;
/// 2. an input whose release id no row holds adopts the id-less rows of its
///    identity when no identified album of that identity exists yet, and
///    otherwise inserts a new row;
/// 3. an input without an id is the single identified album of its identity when
///    exactly one exists, and otherwise shares one id-less row with the other
///    id-less albums of that identity.
///
/// An adopted row has its fields refreshed and its id-less twins merged into it.
/// The optional columns are merged, not overwritten: a column the input leaves
/// empty keeps the value the row already holds, and the year and the release date
/// are only written when the row has none. An album is enriched by whoever knows
/// more (a scan that read tags, a provider that resolved a release) and is never
/// degraded by a caller that knows less. The title and its sort key are always
/// written, because every caller supplies them.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album`: the album's fields.
/// - `credits`: the ordered artist credits; the first one is the primary artist
///   used by the identity.
///
/// # Returns
///
/// The client id of the stored album.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
///
/// # Examples
///
/// ```no_run
/// use hearthsound_library::{AlbumInput, ArtistCredit, Origin};
/// use hearthsound_library::albums::upsert_album;
///
/// # async fn example(conn: &mut sqlx::SqliteConnection) -> Result<(), Box<dyn std::error::Error>> {
/// let album = AlbumInput::new("Cafe del Mar", Origin::Local);
/// let credits = [ArtistCredit::new("Energy 52", "main")];
/// let id = upsert_album(conn, &album, &credits).await?;
/// println!("{id}");
/// # Ok(())
/// # }
/// ```
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

    let by_identifier: Option<Uuid> = match album.mb_release_id.as_deref() {
        Some(mbid) => {
            let found: Option<Vec<u8>> = sqlx::query_scalar!(
                "SELECT id FROM albums WHERE mb_release_id = ? LIMIT 1",
                mbid
            )
            .fetch_optional(&mut *conn)
            .await?;
            found.as_deref().map(decode_uuid).transpose()?
        }
        None => None,
    };
    // Without a credit there is no primary artist, so the identity has nothing
    // to match on beyond the release id.
    let group = match artist_ids.first() {
        Some(primary) => identity_group(conn, &sort_title, *primary, album.year).await?,
        None => Vec::new(),
    };
    let identified: Vec<Uuid> = group
        .iter()
        .filter(|row| row.mb_release_id.is_some())
        .map(|row| row.id)
        .collect();
    let unidentified: Vec<Uuid> = group
        .iter()
        .filter(|row| row.mb_release_id.is_none())
        .map(|row| row.id)
        .collect();

    // A row of the same title and primary artist that has no id yet is what an
    // input carrying a new id adopts, whatever year it stored: a re-tagged file
    // is the same album, and the year it was first scanned with stays.
    let candidates = if album.mb_release_id.is_some() && by_identifier.is_none() {
        match artist_ids.first() {
            Some(primary) => adoption_candidates(conn, &sort_title, *primary).await?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let (target, absorbed): (Option<Uuid>, Vec<Uuid>) =
        match (album.mb_release_id.is_some(), by_identifier) {
            (_, Some(row)) => {
                let aside = identified.iter().filter(|id| **id != row).count();
                let absorbed = if aside == 0 {
                    unidentified.clone()
                } else {
                    Vec::new()
                };
                (Some(row), absorbed)
            }
            (true, None) => {
                if identified.is_empty() {
                    (
                        candidates.first().copied(),
                        candidates.iter().skip(1).copied().collect(),
                    )
                } else {
                    (None, Vec::new())
                }
            }
            (false, None) => {
                if identified.len() == 1 {
                    (identified.first().copied(), unidentified.clone())
                } else {
                    (
                        unidentified.first().copied(),
                        unidentified.iter().skip(1).copied().collect(),
                    )
                }
            }
        };

    let id = match target {
        Some(id) => {
            sqlx::query!(
                "UPDATE albums SET title = ?, sort_title = ?, \
                        display_artist = COALESCE(?, display_artist), \
                        year = COALESCE(year, ?), \
                        release_date = COALESCE(release_date, ?), \
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
            for twin in absorbed {
                merge_album(conn, twin, id).await?;
            }
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

/// Reads the albums of one identity: folded title, primary artist, year.
///
/// The year is part of the identity, and `NULL` is a value of it rather than a
/// wildcard, so an untagged file is its own album.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `sort_title`: the folded title.
/// - `primary`: the artist credited at position 0.
/// - `year`: the year, which may be `None`.
///
/// # Returns
///
/// The rows of that identity, oldest first.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub(crate) async fn identity_group(
    conn: &mut SqliteConnection,
    sort_title: &str,
    primary: EntityId,
    year: Option<i64>,
) -> Result<Vec<AlbumIdentityRow>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT a.id AS id, a.mb_release_id AS mb_release_id \
           FROM albums a JOIN album_artists aa ON aa.album_id = a.id AND aa.position = 0 \
          WHERE a.sort_title = ? AND aa.artist_id = ? AND a.year IS ? \
          ORDER BY a.id",
        sort_title,
        entity_key(primary),
        year,
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(AlbumIdentityRow {
                id: decode_uuid(&row.id)?,
                mb_release_id: row.mb_release_id,
            })
        })
        .collect()
}

/// Lists the id-less rows of a title and primary artist, oldest first.
///
/// These are the rows an album that arrives carrying a release id may adopt: the
/// identity includes the year, so the adoption deliberately ignores it - a file
/// re-tagged with an id and a different year is still the album that was scanned
/// before.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `sort_title`: the folded title.
/// - `primary`: the artist credited at position 0.
///
/// # Returns
///
/// The raw primary keys of the id-less rows, oldest first.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
async fn adoption_candidates(
    conn: &mut SqliteConnection,
    sort_title: &str,
    primary: EntityId,
) -> Result<Vec<Uuid>, LibraryError> {
    let rows = sqlx::query_scalar!(
        "SELECT a.id FROM albums a \
           JOIN album_artists aa ON aa.album_id = a.id AND aa.position = 0 \
          WHERE a.sort_title = ? AND aa.artist_id = ? AND a.mb_release_id IS NULL \
          ORDER BY a.id",
        sort_title,
        entity_key(primary),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|id| decode_uuid(id).map_err(LibraryError::CorruptId))
        .collect()
}

/// Merges one album row into another, moving everything that names it.
///
/// The survivor keeps a release id, a release-group id or a UPC that only the
/// loser had; the loser's tracks, acquisitions, aliases, likes, dislikes, saves
/// and ratings move to it, and the loser row is deleted. The identity includes
/// the year, so the survivor's year, title and display artist are left to the
/// caller.
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
pub(crate) async fn merge_album(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let now = now_millis();
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    sqlx::query(
        "UPDATE albums SET \
             mb_release_id = COALESCE(mb_release_id, (SELECT mb_release_id FROM albums WHERE id = ?)), \
             mb_release_group_id = COALESCE(mb_release_group_id, \
                 (SELECT mb_release_group_id FROM albums WHERE id = ?)), \
             upc = COALESCE(upc, (SELECT upc FROM albums WHERE id = ?)), \
             updated_at = ? \
         WHERE id = ?",
    )
    .bind(from_key.as_slice())
    .bind(from_key.as_slice())
    .bind(from_key.as_slice())
    .bind(now)
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE tracks SET album_id = ? WHERE album_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE acquisitions SET album_id = ? WHERE album_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE external_ids SET entity_id = ? WHERE entity_kind = 'album' AND entity_id = ?",
    )
    .bind(into_key.as_slice())
    .bind(from_key.as_slice())
    .execute(&mut *conn)
    .await?;
    for (insert, delete) in [
        (
            "INSERT OR IGNORE INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
             SELECT user_id, entity_kind, ?, value, at FROM user_feedback \
             WHERE entity_kind = 'album' AND entity_id = ?",
            "DELETE FROM user_feedback WHERE entity_kind = 'album' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
             SELECT user_id, entity_kind, ?, origin, added_at FROM user_library \
             WHERE entity_kind = 'album' AND entity_id = ?",
            "DELETE FROM user_library WHERE entity_kind = 'album' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
             SELECT user_id, entity_kind, ?, rating, at FROM user_ratings \
             WHERE entity_kind = 'album' AND entity_id = ?",
            "DELETE FROM user_ratings WHERE entity_kind = 'album' AND entity_id = ?",
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
    sqlx::query("DELETE FROM albums WHERE id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
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

/// Attaches a MusicBrainz release id to the album row an album is, merging a
/// twin.
///
/// A resolved identity names the release, not the title: the row a caller
/// resolved is the album a track belongs to, and when that row carries no
/// release id yet, the id is written to it, and the release-group id is filled
/// in the same way. When another row already holds the release id, the two are
/// one album and are merged with [`merge_album`], the row that holds the id
/// surviving because that is the row every other identifier of the release
/// resolves to.
///
/// The release-group id is only written while the row has none, so two rows of
/// one release group that a resolution reports separately are not merged: the
/// release id is the identity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album`: the album row the caller resolved.
/// - `release`: the MusicBrainz release id the resolution established.
/// - `release_group`: the MusicBrainz release-group id, when the resolution
///   reported one.
///
/// # Returns
///
/// The surviving row: `album` itself, or the row that already held the release
/// id and absorbed it.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
///
/// # Examples
///
/// ```no_run
/// use hearthsound_core::EntityId;
/// use hearthsound_library::albums::identify_album;
///
/// # async fn example(conn: &mut sqlx::SqliteConnection, album: EntityId) -> Result<(), Box<dyn std::error::Error>> {
/// let album = identify_album(conn, album, "b6c5dd0e-1c55-4b1f-8b7e-9d1c2f0a3b44", None).await?;
/// println!("{album}");
/// # Ok(())
/// # }
/// ```
pub async fn identify_album(
    conn: &mut SqliteConnection,
    album: EntityId,
    release: &str,
    release_group: Option<&str>,
) -> Result<EntityId, LibraryError> {
    let now = now_millis();
    let key = album.uuid();
    let holder: Option<Vec<u8>> = sqlx::query_scalar!(
        "SELECT id FROM albums WHERE mb_release_id = ? LIMIT 1",
        release,
    )
    .fetch_optional(&mut *conn)
    .await?;
    let holder = match holder {
        Some(id) => Some(decode_uuid(&id)?),
        None => None,
    };
    match holder {
        Some(holder) if holder == key => {
            sqlx::query!(
                "UPDATE albums SET mb_release_group_id = COALESCE(mb_release_group_id, ?), \
                        updated_at = ? WHERE id = ?",
                release_group,
                now,
                key.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            Ok(album)
        }
        Some(holder) => {
            merge_album(conn, key, holder).await?;
            if let Some(group) = release_group {
                sqlx::query!(
                    "UPDATE albums SET mb_release_group_id = COALESCE(mb_release_group_id, ?), \
                            updated_at = ? WHERE id = ?",
                    group,
                    now,
                    holder.as_bytes().as_slice(),
                )
                .execute(&mut *conn)
                .await?;
            }
            Ok(EntityId::from_uuid(IdPrefix::Album, holder))
        }
        None => {
            sqlx::query!(
                "UPDATE albums SET mb_release_id = ?, \
                        mb_release_group_id = COALESCE(mb_release_group_id, ?), \
                        updated_at = ? WHERE id = ?",
                release,
                release_group,
                now,
                key.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            Ok(album)
        }
    }
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
