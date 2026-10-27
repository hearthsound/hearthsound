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
//! The edition label follows whoever states it, and only them: a scan derives the
//! label from the file it read and states it on every write, so a file that moves
//! out of a deluxe folder clears the label it used to have, while a provider that
//! carries no version leaves the stored label alone.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use hearthsound_core::providers::{ExternalId, MB_RELEASE, MB_RELEASE_GROUP};
use hearthsound_core::{EntityId, EntityKind, IdPrefix, new_uuid_v7};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::artists::upsert_artist;
use crate::entities;
use crate::error::LibraryError;
use crate::external_ids;
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
    pub(crate) edition: Option<String>,
    pub(crate) track_count: Option<i64>,
    pub(crate) file_tags: Option<String>,
    pub(crate) fetched_at: Option<i64>,
    pub(crate) page_fetched_at: Option<i64>,
    pub(crate) deep_indexed_at: Option<i64>,
    pub(crate) viewed_at: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

impl AlbumRow {
    /// Converts a stored row into an album.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when the primary key is not 16 bytes.
    pub(crate) fn into_album(self) -> Result<Album, LibraryError> {
        Ok(Album {
            id: decode_entity(IdPrefix::Album, &self.id)?,
            title: self.title,
            sort_title: self.sort_title,
            display_artist: self.display_artist,
            year: self.year,
            release_date: self.release_date,
            album_type: self.album_type,
            edition: self.edition,
            track_count: self.track_count,
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

/// One album row of an identity group, as the identity rule sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlbumIdentityRow {
    /// The row's raw primary key.
    pub(crate) id: Uuid,
    /// Whether the row carries any stored identifier.
    pub(crate) identified: bool,
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
/// are only written when the row has none. The edition label is the exception:
/// [`EditionInput`] states whether the writer derived a label at all, so a scan
/// that finds no edition clears a stale one and a provider that knows none leaves
/// it as it is. An album is enriched by whoever knows
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
/// use hearthsound_library::{AlbumInput, ArtistCredit};
/// use hearthsound_library::albums::upsert_album;
///
/// # async fn example(conn: &mut sqlx::SqliteConnection) -> Result<(), Box<dyn std::error::Error>> {
/// let album = AlbumInput::new("Cafe del Mar");
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

    let release = album
        .ids
        .iter()
        .find(|id| id.namespace == MB_RELEASE)
        .map(|id| id.value.clone());
    let by_identifier: Option<Uuid> = match release.as_deref() {
        Some(release) => {
            external_ids::owner_of(conn, EntityKind::Album, MB_RELEASE, release).await?
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
        .filter(|row| row.identified)
        .map(|row| row.id)
        .collect();
    let unidentified: Vec<Uuid> = group
        .iter()
        .filter(|row| !row.identified)
        .map(|row| row.id)
        .collect();

    // A row of the same title and primary artist that has no id yet is what an
    // input carrying a new id adopts, whatever year it stored: a re-tagged file
    // is the same album, and the year it was first scanned with stays.
    let candidates = if release.is_some() && by_identifier.is_none() {
        match artist_ids.first() {
            Some(primary) => adoption_candidates(conn, &sort_title, *primary).await?,
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let (target, absorbed): (Option<Uuid>, Vec<Uuid>) = match (release.is_some(), by_identifier) {
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

    let file_tags = album.file_tags.as_ref().map(serde_json::Value::to_string);
    let id = match target {
        Some(id) => {
            sqlx::query!(
                "UPDATE albums SET title = ?, sort_title = ?, \
                        display_artist = COALESCE(?, display_artist), \
                        year = COALESCE(year, ?), \
                        release_date = COALESCE(release_date, ?), \
                        album_type = COALESCE(?, album_type), \
                        track_count = COALESCE(?, track_count), \
                        file_tags = COALESCE(?, file_tags), \
                        edition = CASE WHEN ? THEN ? ELSE edition END, \
                        updated_at = ? WHERE id = ?",
                album.title,
                sort_title,
                album.display_artist,
                album.year,
                album.release_date,
                album.album_type,
                album.track_count,
                file_tags,
                album.edition.is_stated(),
                album.edition.label(),
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
            sqlx::query!(
                "INSERT INTO albums (id, title, sort_title, display_artist, year, release_date, \
                                     album_type, track_count, file_tags, edition, \
                                     created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                id.as_bytes().as_slice(),
                album.title,
                sort_title,
                album.display_artist,
                album.year,
                album.release_date,
                album.album_type,
                album.track_count,
                file_tags,
                album.edition.label(),
                now,
                now,
            )
            .execute(&mut *conn)
            .await?;
            id
        }
    };

    write_album_credits(conn, id, credits, &artist_ids).await?;
    // Every identifier the input carries is attached to the resolved row; one
    // another row already holds means the two rows are one album, and the merge
    // decides which survives.
    let mut survivor = id;
    let conflicts = external_ids::attach(conn, EntityKind::Album, survivor, &album.ids).await?;
    for conflict in conflicts {
        // A release group is a grouping key, never an album's identity: two
        // editions of one group are two rows, so the album that first claimed the
        // group's id keeps it and the other one keeps its own release id.
        if conflict.namespace == MB_RELEASE_GROUP {
            continue;
        }
        survivor = entities::merge(conn, EntityKind::Album, survivor, conflict.owner).await?;
    }
    Ok(EntityId::from_uuid(IdPrefix::Album, survivor))
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
        "SELECT a.id AS id, \
                EXISTS (SELECT 1 FROM external_ids e \
                        WHERE e.entity_kind = 'album' AND e.entity_id = a.id) \
                    AS \"identified: bool\" \
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
                identified: row.identified,
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
          WHERE a.sort_title = ? AND aa.artist_id = ? \
            AND NOT EXISTS (SELECT 1 FROM external_ids e \
                            WHERE e.entity_kind = 'album' AND e.entity_id = a.id) \
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
/// The survivor keeps an edition label or a track count only the loser had; the
/// loser's tracks, credits, identifiers, provider answers, images, genres,
/// acquisitions, likes, dislikes, saves and ratings move to it, and the loser row
/// is deleted. The identity includes the year, so the survivor's year, title and
/// display artist are left to the caller.
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
             edition = COALESCE(edition, (SELECT edition FROM albums WHERE id = ?)), \
             track_count = COALESCE(track_count, \
                 (SELECT track_count FROM albums WHERE id = ?)), \
             updated_at = ? \
         WHERE id = ?",
    )
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
    for statement in [
        "UPDATE external_ids SET entity_id = ? WHERE entity_kind = 'album' AND entity_id = ?",
        "UPDATE entity_sources SET entity_id = ? WHERE entity_kind = 'album' AND entity_id = ?",
        "UPDATE entity_images SET entity_id = ? WHERE entity_kind = 'album' AND entity_id = ?",
        "UPDATE index_queue SET entity_id = ? WHERE entity_kind = 'album' AND entity_id = ?",
        "UPDATE album_genres SET album_id = ? WHERE album_id = ? AND genre_id NOT IN (\
             SELECT genre_id FROM album_genres WHERE album_id = ?)",
    ] {
        if statement.contains("album_genres") {
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
    // The loser's credits move to the survivor; a duplicate of an artist it
    // already credits is dropped.
    crate::entities::move_album_credits(conn, from, into).await?;
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
/// An album stored from the album reference of a single track can arrive
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
    let key = album.uuid();
    let mut ids = vec![ExternalId {
        namespace: MB_RELEASE.to_owned(),
        value: release.to_owned(),
    }];
    if let Some(group) = release_group {
        ids.push(ExternalId {
            namespace: MB_RELEASE_GROUP.to_owned(),
            value: group.to_owned(),
        });
    }
    let conflicts = external_ids::attach(conn, EntityKind::Album, key, &ids).await?;
    let mut survivor = key;
    for conflict in conflicts {
        survivor = entities::merge(conn, EntityKind::Album, survivor, conflict.owner).await?;
    }
    Ok(EntityId::from_uuid(IdPrefix::Album, survivor))
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
        "SELECT id, title, sort_title, display_artist, year, release_date, album_type, edition, \
                track_count, file_tags, fetched_at, page_fetched_at, deep_indexed_at, \
                viewed_at, created_at, updated_at \
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
        "SELECT id, title, sort_title, display_artist, year, release_date, album_type, edition, \
                track_count, file_tags, fetched_at, page_fetched_at, deep_indexed_at, \
                viewed_at, created_at, updated_at \
         FROM albums WHERE id = (SELECT entity_id FROM external_ids \
                                 WHERE entity_kind = 'album' AND namespace = 'mb-release' \
                                   AND value = ? LIMIT 1)",
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
                al.album_type, al.edition, al.track_count, al.file_tags, al.fetched_at, \
                al.page_fetched_at, al.deep_indexed_at, al.viewed_at, al.created_at, \
                al.updated_at \
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
