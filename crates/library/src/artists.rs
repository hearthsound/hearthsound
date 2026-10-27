//! Reading and writing the `artists` table.
//!
//! An artist row is created by a scan, an upload, or the catalog layer, and the
//! identity rule is what keeps a scan that sees one act in twenty files from
//! writing twenty rows:
//!
//! - rows that share an identifier are one artist. An identifier is the
//!   MusicBrainz artist id or any row of `external_ids` for the artist, which is
//!   where the catalog layer stores a provider's own id for it.
//! - a row is *identified* when it holds a MusicBrainz artist id, which is the
//!   identifier a provider resolves an act by. A provider's own id does not
//!   identify a row.
//! - a name identifies an artist only when it is unambiguous. A file whose tags
//!   carry no id credits the one identified artist of that name; when two or
//!   more identified artists share the name, every untagged file of that name
//!   shares one separate row, so two acts called "Alex G" do not become one page
//!   and neither absorbs an untagged rip; two untagged rows of one name are one
//!   artist.
//! - names are compared by [`crate::identity_key`], which folds case, whitespace
//!   and the hyphen and quote characters providers spell differently, so
//!   `alt-J` and `alt\u{2010}J` are the same name.
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

use hearthsound_core::providers::{ExternalId, MB_ARTIST};
use hearthsound_core::{EntityId, EntityKind, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::entities;
use crate::error::LibraryError;
use crate::external_ids;
use crate::model::{
    Artist, ArtistInput, decode_entity, decode_uuid, entity_key, identity_key, sort_key,
};
use crate::now_millis;

/// One row of the `artists` table as it is stored.
#[derive(sqlx::FromRow)]
pub(crate) struct ArtistRow {
    pub(crate) id: Vec<u8>,
    pub(crate) name: String,
    pub(crate) sort_name: String,
    pub(crate) biography: Option<String>,
    pub(crate) links: String,
    pub(crate) album_count: Option<i64>,
    pub(crate) file_tags: Option<String>,
    pub(crate) fetched_at: Option<i64>,
    pub(crate) page_fetched_at: Option<i64>,
    pub(crate) deep_indexed_at: Option<i64>,
    pub(crate) viewed_at: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl ArtistRow {
    /// Converts a stored row into an artist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when the primary key is not 16 bytes.
    pub(crate) fn into_artist(self) -> Result<Artist, LibraryError> {
        Ok(Artist {
            id: decode_entity(IdPrefix::Artist, &self.id)?,
            name: self.name,
            sort_name: self.sort_name,
            biography: self.biography,
            links: self.links,
            album_count: self.album_count,
            file_tags: self.file_tags,
            fetched_at: self.fetched_at,
            page_fetched_at: self.page_fetched_at,
            deep_indexed_at: self.deep_indexed_at,
            viewed_at: self.viewed_at,
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
    /// Whether the row holds a MusicBrainz artist id.
    pub(crate) aliased: bool,
}

impl NameGroupRow {
    /// Reports whether the row is identified.
    ///
    /// "Identified" means the row holds a MusicBrainz artist id, the identifier
    /// a catalog provider resolves an act by. A row that holds only a provider's
    /// own id - a Deezer artist id, say - is not identified: that id says which
    /// row one provider's answer belongs to, not which act the row is, so it must
    /// not stop the row from being adopted when the MusicBrainz id for the same
    /// act arrives.
    ///
    /// # Returns
    ///
    /// `true` when the row holds an `mb-artist` id.
    pub(crate) fn identified(&self) -> bool {
        self.aliased
    }
}

/// Inserts an artist, or refreshes the one that already matches.
///
/// The identity rule is documented on this module, and it resolves to one row
/// like this - `by_identifier` is the row the input's identifier belongs to, and
/// "aside" counts the other identified artists of the same name, where
/// identified means the row holds a MusicBrainz artist id:
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
///   name with [`sort_key`].
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
/// let input = ArtistInput::new("Energy 52");
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
    let match_key = identity_key(&artist.name);

    let mbid = artist
        .ids
        .iter()
        .find(|id| id.namespace == hearthsound_core::providers::MB_ARTIST)
        .map(|id| id.value.clone());
    let by_identifier = match mbid.as_deref() {
        Some(mbid) => external_ids::owner_of(conn, EntityKind::Artist, MB_ARTIST, mbid).await?,
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
        sqlx::query(
            "UPDATE artists SET name = ?, sort_name = ?, match_key = ?, \
                    file_tags = COALESCE(?, file_tags), updated_at = ? WHERE id = ?",
        )
        .bind(&artist.name)
        .bind(&sort_name)
        .bind(&match_key)
        .bind(artist.file_tags.as_ref().map(serde_json::Value::to_string))
        .bind(now)
        .bind(id.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
        let survivor = match mbid.as_deref() {
            Some(mbid) => identify_artist(conn, resolved, mbid).await?.uuid(),
            None => id,
        };
        return Ok(EntityId::from_uuid(IdPrefix::Artist, survivor));
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

    let (target, absorbed): (Option<Uuid>, Vec<Uuid>) = match (mbid.is_some(), by_identifier) {
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

    let id = match target {
        Some(id) => {
            sqlx::query(
                "UPDATE artists SET name = ?, sort_name = ?, match_key = ?, \
                        file_tags = COALESCE(?, file_tags), updated_at = ? WHERE id = ?",
            )
            .bind(&artist.name)
            .bind(&sort_name)
            .bind(&match_key)
            .bind(artist.file_tags.as_ref().map(serde_json::Value::to_string))
            .bind(now)
            .bind(id.as_bytes().as_slice())
            .execute(&mut *conn)
            .await?;
            for twin in absorbed {
                merge_artist(conn, twin, id).await?;
            }
            id
        }
        None => {
            let id = new_uuid_v7();
            sqlx::query(
                "INSERT INTO artists (id, name, sort_name, match_key, file_tags, created_at, \
                                     updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id.as_bytes().as_slice())
            .bind(&artist.name)
            .bind(&sort_name)
            .bind(&match_key)
            .bind(artist.file_tags.as_ref().map(serde_json::Value::to_string))
            .bind(now)
            .bind(now)
            .execute(&mut *conn)
            .await?;
            id
        }
    };

    // The MusicBrainz id the rule matched on is the one identifier the artist
    // module itself records; every other identifier travels with the caller,
    // which attaches it and merges what it already names.
    let mut survivor = match mbid.as_deref() {
        Some(mbid) => identify_artist(conn, EntityId::from_uuid(IdPrefix::Artist, id), mbid)
            .await?
            .uuid(),
        None => id,
    };
    let conflicts = external_ids::attach(conn, EntityKind::Artist, survivor, &artist.ids).await?;
    for conflict in conflicts {
        survivor = entities::merge(conn, EntityKind::Artist, survivor, conflict.owner).await?;
    }
    Ok(EntityId::from_uuid(IdPrefix::Artist, survivor))
}

/// Reads every row whose name matches, oldest first.
///
/// The match is on [`identity_key`], not on the stored name: `alt-J`,
/// `alt\u{2010}J` and `ALT-J` are one name, because providers and tags spell the
/// punctuation differently. A row whose key was never written - none exist after
/// the baseline, which makes the column `NOT NULL` - does not match.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `name`: the artist name to match.
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
        "SELECT a.id AS id, \
                EXISTS (SELECT 1 FROM external_ids e \
                        WHERE e.entity_kind = 'artist' AND e.entity_id = a.id \
                          AND e.namespace = 'mb-artist') AS \"aliased: bool\" \
         FROM artists a WHERE a.match_key = ? ORDER BY a.id",
        identity_key(name),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(NameGroupRow {
                id: decode_uuid(&row.id)?,
                aliased: row.aliased,
            })
        })
        .collect()
}

/// Finds the row a MusicBrainz artist id belongs to.
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
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
#[allow(dead_code)]
pub(crate) async fn row_for_mbid(
    conn: &mut SqliteConnection,
    mbid: &str,
) -> Result<Option<Uuid>, LibraryError> {
    external_ids::owner_of(conn, EntityKind::Artist, MB_ARTIST, mbid).await
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
        "UPDATE artists SET biography = COALESCE(biography, \
                (SELECT biography FROM artists WHERE id = ?)), \
             album_count = COALESCE(album_count, (SELECT album_count FROM artists WHERE id = ?)), \
             updated_at = ? WHERE id = ?",
    )
    .bind(from_key.as_slice())
    .bind(from_key.as_slice())
    .bind(now)
    .bind(into_key.as_slice())
    .execute(&mut *conn)
    .await?;
    // A credit the survivor already has at that position stays; the loser's
    // rows for those positions then go, which is what keeps the primary keys
    // of one (track or album, position) unique.
    crate::entities::move_track_artist_credits(conn, from, into).await?;
    crate::entities::move_album_artist_credits(conn, from, into).await?;
    for statement in [
        "UPDATE external_ids SET entity_id = ? WHERE entity_kind = 'artist' AND entity_id = ?",
        "UPDATE OR IGNORE entity_sources SET entity_id = ? \
          WHERE entity_kind = 'artist' AND entity_id = ?",
        "UPDATE OR IGNORE entity_images SET entity_id = ? \
          WHERE entity_kind = 'artist' AND entity_id = ?",
        "UPDATE artist_top_tracks SET artist_id = ? WHERE artist_id = ? AND position NOT IN ( \
             SELECT position FROM artist_top_tracks WHERE artist_id = ?)",
        "UPDATE artist_similar SET artist_id = ? WHERE artist_id = ? AND position NOT IN ( \
             SELECT position FROM artist_similar WHERE artist_id = ?)",
    ] {
        if statement.contains("NOT IN") {
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
    // The survivor's answer and its image slot win; whatever could not move is
    // dropped with the loser below.
    sqlx::query("DELETE FROM entity_sources WHERE entity_kind = 'artist' AND entity_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM entity_images WHERE entity_kind = 'artist' AND entity_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    crate::index_queue::absorb(conn, EntityKind::Artist, from, into).await?;
    // A similar-artist entry the survivor already lists is dropped with the
    // loser's row; the survivor's own position wins.
    sqlx::query("UPDATE OR IGNORE artist_similar SET similar_id = ? WHERE similar_id = ?")
        .bind(into_key.as_slice())
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM artist_similar WHERE similar_id = ?")
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
    let key = artist.uuid();
    let id = ExternalId {
        namespace: MB_ARTIST.to_owned(),
        value: mbid.to_owned(),
    };
    let conflicts =
        external_ids::attach(conn, EntityKind::Artist, key, std::slice::from_ref(&id)).await?;
    let survivor = match conflicts.first() {
        Some(conflict) => entities::merge(conn, EntityKind::Artist, key, conflict.owner).await?,
        None => key,
    };
    Ok(EntityId::from_uuid(IdPrefix::Artist, survivor))
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
        "SELECT id, name, sort_name, biography, links, album_count, file_tags, fetched_at, \
                page_fetched_at, deep_indexed_at, viewed_at, created_at, updated_at \
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
        "SELECT id, name, sort_name, biography, links, album_count, file_tags, fetched_at, \
                page_fetched_at, deep_indexed_at, viewed_at, created_at, updated_at \
         FROM artists WHERE id = (SELECT entity_id FROM external_ids \
                                  WHERE entity_kind = 'artist' AND namespace = 'mb-artist' \
                                    AND value = ? LIMIT 1)",
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
        "SELECT id, name, sort_name, biography, links, album_count, file_tags, fetched_at, \
                page_fetched_at, deep_indexed_at, viewed_at, created_at, updated_at \
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
        "SELECT id, name, sort_name, biography, links, album_count, file_tags, fetched_at, \
                page_fetched_at, deep_indexed_at, viewed_at, created_at, updated_at \
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
        "SELECT ar.id, ar.name, ar.sort_name, ar.biography, ar.links, ar.album_count, \
                ar.file_tags, ar.fetched_at, ar.page_fetched_at, ar.deep_indexed_at, \
                ar.viewed_at, ar.created_at, \
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
        "SELECT ar.id, ar.name, ar.sort_name, ar.biography, ar.links, ar.album_count, \
                ar.file_tags, ar.fetched_at, ar.page_fetched_at, ar.deep_indexed_at, \
                ar.viewed_at, ar.created_at, \
                ar.updated_at \
         FROM album_artists aa JOIN artists ar ON ar.id = aa.artist_id \
         WHERE aa.album_id = ? ORDER BY aa.position",
        entity_key(album_id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(ArtistRow::into_artist).collect()
}
