//! Reading and writing the `artists` table.
//!
//! An artist row is created by a scan, an upload, or the catalog layer, and the
//! identity rule is what keeps a scan that sees one act in twenty files from
//! writing twenty rows:
//!
//! - rows that share an identifier are one artist. An identifier is the
//!   MusicBrainz artist id or any row of `external_ids` for the artist, which is
//!   where the catalog layer stores a provider's own id for it.
//! - a name identifies an artist only when it is unambiguous. A file whose tags
//!   carry no id credits the one identified artist of that name; when two or
//!   more identified artists share the name, every untagged file of that name
//!   shares one separate row, so two acts called "Alex G" do not become one page
//!   and neither absorbs an untagged rip; two untagged rows of one name are one
//!   artist.
//!
//! A row that turns out to be a twin of another is merged into it, which moves
//! its credits, aliases, likes, saves and ratings; see [`merge_artist`]. A caller
//! that resolved the artist itself - the catalog layer, which resolves a
//! credited artist through the provider id the alias table holds - names the row
//! on the input, and a resolved MusicBrainz artist id is attached to the row the
//! resolution named with [`identify_artist`] rather than creating a second row
//! for the act. The one-time `library.reidentity` pass applies the same rule to
//! rows that already exist (see [`crate::identity`]).
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

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

/// One artist row of a name group, as the identity rule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameGroupRow {
    /// The row's raw primary key.
    pub(crate) id: Uuid,
    /// The row's MusicBrainz artist id.
    pub(crate) mb_artist_id: Option<String>,
    /// Whether the row carries an `external_ids` alias, which identifies it too.
    pub(crate) aliased: bool,
}

impl NameGroupRow {
    /// Reports whether the row is identified, by its own id or by an alias.
    ///
    /// # Returns
    ///
    /// `true` when the row carries a MusicBrainz artist id or an alias.
    pub(crate) fn identified(&self) -> bool {
        self.mb_artist_id.is_some() || self.aliased
    }
}

/// Inserts an artist, or refreshes the one that already matches.
///
/// The identity rule is documented on this module, and it resolves to one row
/// like this - `by_identifier` is the row the input's identifier belongs to, and
/// "aside" counts the other identified artists of the same name:
///
/// | input | `by_identifier` | identified aside | target | absorbed |
/// |---|---|---|---|---|
/// | names a row | - | - | that row | none |
/// | carries an id | a row | 0 | that row | every untagged row |
/// | carries an id | a row | 1 or more | that row | none |
/// | carries an id | none | 0 | the oldest untagged row (absent: insert) | the other untagged rows |
/// | carries an id | none | 1 or more | none (insert) | none |
/// | no id | - | exactly 1 | that artist | every untagged row |
/// | no id | - | 0 or 2 or more | the oldest untagged row (absent: insert) | the other untagged rows |
///
/// Every absorbed row is merged into the target after the target is refreshed,
/// so a scan cannot leave two rows for one artist. Two artists with different
/// identifiers stay apart whatever their names. A MusicBrainz id the input does
/// not carry is left in place, so a caller that knows less never clears a column
/// the table already holds, and the `origin` of an existing row is never
/// rewritten: an artist a scan created stays local when the catalog layer later
/// resolves it.
///
/// An input that names a row ([`ArtistInput::row`]) skips the rule entirely: the
/// caller resolved the artist through an identifier of its own, which is what
/// the catalog layer does with a provider id, so the name cannot be allowed to
/// move the credit to a namesake. Such an input never absorbs a twin, and the
/// MusicBrainz id it carries is attached to the named row with
/// [`identify_artist`], which merges the row away when a twin already holds it.
/// A named row that has since been deleted falls back to the rule.
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
///
/// # Examples
///
/// ```no_run
/// use hearthsound_library::ArtistInput;
/// use hearthsound_library::artists::upsert_artist;
///
/// # async fn example(conn: &mut sqlx::SqliteConnection) -> Result<(), Box<dyn std::error::Error>> {
/// let input = ArtistInput::new("Energy 52").mb_artist_id("c2f4e6f0-8a5a-4b6c-9a54-7c4d8a0e1f21");
/// let id = upsert_artist(conn, &input).await?;
/// println!("{id}");
/// # Ok(())
/// # }
/// ```
pub async fn upsert_artist(
    conn: &mut SqliteConnection,
    artist: &ArtistInput,
) -> Result<EntityId, LibraryError> {
    let now = now_millis();
    let sort_name = artist
        .sort_name
        .clone()
        .unwrap_or_else(|| sort_key(&artist.name));

    let by_identifier = match artist.mb_artist_id.as_deref() {
        Some(mbid) => row_for_mbid(conn, mbid).await?,
        None => None,
    };
    // A row the caller resolved itself is the artist, whatever the name rule
    // would say about the name: the caller knows which provider id, and so which
    // row, this credit is. It is not refreshed into a twin either, because the
    // resolution is what identifies the row.
    if let Some(resolved) = artist.row
        && row_exists(conn, resolved.uuid()).await?
    {
        let id = resolved.uuid();
        let id = match artist.mb_artist_id.as_deref() {
            Some(mbid) => identify_artist(conn, resolved, mbid).await?.uuid(),
            None => {
                sqlx::query!(
                    "UPDATE artists SET name = ?, sort_name = ?, updated_at = ? WHERE id = ?",
                    artist.name,
                    sort_name,
                    now,
                    id.as_bytes().as_slice(),
                )
                .execute(&mut *conn)
                .await?;
                id
            }
        };
        return Ok(EntityId::from_uuid(IdPrefix::Artist, id));
    }
    let group = name_group(conn, &artist.name).await?;
    let identified: Vec<Uuid> = group
        .iter()
        .filter(|row| row.identified())
        .map(|row| row.id)
        .collect();
    let unidentified: Vec<Uuid> = group
        .iter()
        .filter(|row| !row.identified())
        .map(|row| row.id)
        .collect();

    let (target, absorbed): (Option<Uuid>, Vec<Uuid>) =
        match (artist.mb_artist_id.is_some(), by_identifier) {
            // An identifier the table already knows is the artist, and it absorbs
            // the untagged rows of its name only while no other artist is identified.
            (_, Some(row)) => {
                let aside = identified.iter().filter(|id| **id != row).count();
                let absorbed = if aside == 0 {
                    unidentified.clone()
                } else {
                    Vec::new()
                };
                (Some(row), absorbed)
            }
            // An identifier no row holds: the untagged rows of the name become it,
            // unless an identified artist already owns the name.
            (true, None) => {
                if identified.is_empty() {
                    (
                        unidentified.first().copied(),
                        unidentified.iter().skip(1).copied().collect(),
                    )
                } else {
                    (None, Vec::new())
                }
            }
            // No identifier: one identified artist of the name is the artist,
            // otherwise the untagged rows are one artist of their own.
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

    if let Some(id) = target {
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
        for twin in absorbed {
            merge_artist(conn, twin, id).await?;
        }
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

/// Reads every row whose name matches, oldest first.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `name`: the artist name to match, case-insensitively.
///
/// # Returns
///
/// The rows of that name, oldest first.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub(crate) async fn name_group(
    conn: &mut SqliteConnection,
    name: &str,
) -> Result<Vec<NameGroupRow>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT a.id AS id, a.mb_artist_id AS mb_artist_id, \
                EXISTS (SELECT 1 FROM external_ids e \
                        WHERE e.entity_kind = 'artist' AND e.entity_id = a.id) AS \"aliased: bool\" \
         FROM artists a WHERE a.name = ? COLLATE NOCASE ORDER BY a.id",
        name,
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(NameGroupRow {
                id: decode_uuid(&row.id)?,
                mb_artist_id: row.mb_artist_id,
                aliased: row.aliased,
            })
        })
        .collect()
}

/// Finds the row an identifier belongs to.
///
/// The MusicBrainz column wins over the alias table, because a row that carries
/// the id outright is the one the catalog layer resolved to.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `mbid`: the MusicBrainz artist id.
///
/// # Returns
///
/// The row's raw primary key, or `None` when no row holds the id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub(crate) async fn row_for_mbid(
    conn: &mut SqliteConnection,
    mbid: &str,
) -> Result<Option<Uuid>, LibraryError> {
    let by_column: Option<Vec<u8>> = sqlx::query_scalar!(
        "SELECT id FROM artists WHERE mb_artist_id = ? LIMIT 1",
        mbid
    )
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = by_column {
        return Ok(Some(decode_uuid(&id)?));
    }
    let by_alias: Option<Vec<u8>> = sqlx::query_scalar!(
        "SELECT entity_id FROM external_ids \
         WHERE namespace = 'mb-artist' AND entity_kind = 'artist' AND value = ? LIMIT 1",
        mbid,
    )
    .fetch_optional(&mut *conn)
    .await?;
    match by_alias {
        Some(id) => Ok(Some(decode_uuid(&id)?)),
        None => Ok(None),
    }
}

/// Merges one artist row into another, moving everything that names it.
///
/// The survivor keeps an identifier only the loser had; the loser's credits,
/// aliases, likes, dislikes, saves, uploads and ratings move to it, and the loser
/// row is deleted. A credit the survivor already has on the same track or album
/// is not duplicated, and a like, save or rating the same user has on both rows
/// keeps the survivor's. The survivor's name, sort name and origin are left to
/// the caller, which owns the write that explained why the rows are one.
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
pub(crate) async fn merge_artist(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let now = now_millis();
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    sqlx::query(
        "UPDATE artists SET mb_artist_id = COALESCE(mb_artist_id, \
                (SELECT mb_artist_id FROM artists WHERE id = ?)), updated_at = ? WHERE id = ?",
    )
    .bind(from_key.as_slice())
    .bind(now)
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE track_artists SET artist_id = ? WHERE artist_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM track_artists WHERE artist_id = ? AND rowid NOT IN (\
             SELECT MIN(rowid) FROM track_artists WHERE artist_id = ? GROUP BY track_id)",
    )
    .bind(into_key.as_slice())
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE album_artists SET artist_id = ? WHERE artist_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "DELETE FROM album_artists WHERE artist_id = ? AND rowid NOT IN (\
             SELECT MIN(rowid) FROM album_artists WHERE artist_id = ? GROUP BY album_id)",
    )
    .bind(into_key.as_slice())
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE external_ids SET entity_id = ? WHERE entity_kind = 'artist' AND entity_id = ?",
    )
    .bind(into_key.as_slice())
    .bind(from_key.as_slice())
    .execute(&mut *conn)
    .await?;
    for (insert, delete) in [
        (
            "INSERT OR IGNORE INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
             SELECT user_id, entity_kind, ?, value, at FROM user_feedback \
             WHERE entity_kind = 'artist' AND entity_id = ?",
            "DELETE FROM user_feedback WHERE entity_kind = 'artist' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
             SELECT user_id, entity_kind, ?, origin, added_at FROM user_library \
             WHERE entity_kind = 'artist' AND entity_id = ?",
            "DELETE FROM user_library WHERE entity_kind = 'artist' AND entity_id = ?",
        ),
        (
            "INSERT OR IGNORE INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
             SELECT user_id, entity_kind, ?, rating, at FROM user_ratings \
             WHERE entity_kind = 'artist' AND entity_id = ?",
            "DELETE FROM user_ratings WHERE entity_kind = 'artist' AND entity_id = ?",
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
    sqlx::query("DELETE FROM artists WHERE id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Reports whether an artist row exists.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the row's raw primary key.
///
/// # Returns
///
/// `true` when a row has that key.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
async fn row_exists(conn: &mut SqliteConnection, id: Uuid) -> Result<bool, LibraryError> {
    let found: Option<i64> = sqlx::query_scalar!(
        "SELECT 1 AS \"found: i64\" FROM artists WHERE id = ?",
        id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(found.is_some())
}

/// Attaches a MusicBrainz artist id to the row an artist is, merging a twin.
///
/// A resolved identity names the act, not a name: the row a caller resolved is
/// the artist the track or the album credits, and when that row carries no
/// MusicBrainz id yet, the id is written to it. When another row already holds
/// the id, the two are one artist and are merged with [`merge_artist`], the row
/// that holds the id surviving because that is the row every other identifier of
/// the act resolves to.
///
/// This is what identity resolution applies, and what keeps a resolution from
/// creating a second row for an artist the library already stores under the
/// provider's own id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `artist`: the artist row the caller resolved.
/// - `mbid`: the MusicBrainz artist id the resolution established.
///
/// # Returns
///
/// The surviving row: `artist` itself, or the row that already held the id and
/// absorbed it.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
///
/// # Examples
///
/// ```no_run
/// use hearthsound_core::EntityId;
/// use hearthsound_library::artists::identify_artist;
///
/// # async fn example(conn: &mut sqlx::SqliteConnection, artist: EntityId) -> Result<(), Box<dyn std::error::Error>> {
/// let artist = identify_artist(conn, artist, "c2f4e6f0-8a5a-4b6c-9a54-7c4d8a0e1f21").await?;
/// println!("{artist}");
/// # Ok(())
/// # }
/// ```
pub async fn identify_artist(
    conn: &mut SqliteConnection,
    artist: EntityId,
    mbid: &str,
) -> Result<EntityId, LibraryError> {
    let now = now_millis();
    let key = artist.uuid();
    match row_for_mbid(conn, mbid).await? {
        Some(holder) if holder == key => {
            sqlx::query!(
                "UPDATE artists SET updated_at = ? WHERE id = ?",
                now,
                key.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            Ok(artist)
        }
        Some(holder) => {
            merge_artist(conn, key, holder).await?;
            Ok(EntityId::from_uuid(IdPrefix::Artist, holder))
        }
        None => {
            sqlx::query!(
                "UPDATE artists SET mb_artist_id = ?, updated_at = ? WHERE id = ?",
                mbid,
                now,
                key.as_bytes().as_slice(),
            )
            .execute(&mut *conn)
            .await?;
            Ok(artist)
        }
    }
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
