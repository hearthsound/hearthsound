//! Entity identity: the redirects a merge leaves and the one-id rule.
//!
//! Two rows that turn out to be the same item become one: the older row (the
//! smaller UUIDv7) survives, everything that named the loser moves to it, and
//! the loser's id keeps resolving through `entity_redirects`. That table is why
//! a client that holds an id the server has since merged away is answered with
//! the survivor instead of an error, and why the server never emits the loser
//! again.
//!
//! A row is only ever merged into another row of the same kind. The merge itself
//! is per kind, because what has to move differs: a track carries files and a
//! retention row, an album carries its tracks and credits, an artist carries
//! credits and similar artists.

use std::collections::BTreeSet;

use hearthsound_core::EntityKind;
use sqlx::{Connection as _, SqliteConnection};
use uuid::Uuid;

use crate::albums::merge_album;
use crate::artists::merge_artist;
use crate::error::LibraryError;
use crate::external_ids::{self, kind_literal};
use crate::model::decode_uuid;
use crate::now_millis;
use crate::tracks::merge_track;

/// Resolves an id to the row that stands for it now.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind the id names.
/// - `id`: the row's raw primary key, which may be one a merge retired.
///
/// # Returns
///
/// The row itself when it exists, the row it was merged into when it does not,
/// and `None` when neither exists.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn canonical(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    id: Uuid,
) -> Result<Option<Uuid>, LibraryError> {
    let found: Option<i64> = match kind {
        EntityKind::Track => {
            sqlx::query_scalar("SELECT 1 AS \"found: i64\" FROM tracks WHERE id = ?")
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Album => {
            sqlx::query_scalar("SELECT 1 AS \"found: i64\" FROM albums WHERE id = ?")
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Artist => {
            sqlx::query_scalar("SELECT 1 AS \"found: i64\" FROM artists WHERE id = ?")
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        EntityKind::Playlist => {
            sqlx::query_scalar("SELECT 1 AS \"found: i64\" FROM playlists WHERE id = ?")
                .bind(id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
    };
    if found.is_some() {
        return Ok(Some(id));
    }
    let kind_literal = kind_literal(kind)?;
    let target: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT new_id FROM entity_redirects WHERE entity_kind = ? AND old_id = ?",
    )
    .bind(kind_literal)
    .bind(id.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    match target {
        Some(bytes) => Ok(Some(decode_uuid(&bytes)?)),
        None => Ok(None),
    }
}

/// Merges two rows of one kind into the older of them.
///
/// Everything that named either row ends up naming the survivor: identifiers,
/// credits, saved items, likes, ratings, provider answers, images and queued
/// index work. The retired id is recorded in `entity_redirects`, and any earlier
/// redirect that pointed at it is repointed at the survivor, so a chain of merges
/// always resolves in one step.
///
/// The whole merge is one transaction - a savepoint when the caller already
/// holds one - so a statement that fails partway leaves both rows as they were
/// rather than half-merged.
///
/// Two artists that each hold a different MusicBrainz artist id are not merged:
/// each id names a distinct act, so the rows are two acts whatever their names
/// suggest. The survivor is returned and a warning is logged.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind both rows are.
/// - `a`: one row's raw primary key.
/// - `b`: the other row's raw primary key.
///
/// # Returns
///
/// The surviving row's raw primary key. When `a` and `b` are the same row it is
/// returned unchanged and nothing is written.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn merge(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    a: Uuid,
    b: Uuid,
) -> Result<Uuid, LibraryError> {
    if a == b {
        return Ok(a);
    }
    let (survivor, loser) = if a < b { (a, b) } else { (b, a) };
    if kind == EntityKind::Artist {
        let held = external_ids::held_mb_artist(conn, survivor).await?;
        let other = external_ids::held_mb_artist(conn, loser).await?;
        if let (Some(held), Some(other)) = (held, other)
            && held != other
        {
            tracing::warn!(
                survivor = %survivor,
                loser = %loser,
                held = %held,
                other = %other,
                "refused to merge two artists with different MusicBrainz ids"
            );
            return Ok(survivor);
        }
    }
    let mut tx = conn.begin().await?;
    match kind {
        EntityKind::Artist => merge_artist(&mut tx, loser, survivor).await?,
        EntityKind::Album => merge_album(&mut tx, loser, survivor).await?,
        EntityKind::Track => merge_track(&mut tx, loser, survivor).await?,
        EntityKind::Playlist => {
            return Err(LibraryError::UnknownLiteral {
                kind: "mergeable entity",
                value: "playlist".to_owned(),
            });
        }
    }
    let kind_literal = kind_literal(kind)?;
    sqlx::query("UPDATE entity_redirects SET new_id = ? WHERE entity_kind = ? AND new_id = ?")
        .bind(survivor.as_bytes().as_slice())
        .bind(kind_literal)
        .bind(loser.as_bytes().as_slice())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO entity_redirects (entity_kind, old_id, new_id, merged_at) \
         VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
    )
    .bind(kind_literal)
    .bind(loser.as_bytes().as_slice())
    .bind(survivor.as_bytes().as_slice())
    .bind(now_millis())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(survivor)
}

/// Merges every row an entity name resolves to that turns out to be one row.
///
/// The helper a caller with several candidate rows uses: it merges them in
/// ascending id order, so the oldest row survives, and returns the survivor.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `candidates`: the rows.
///
/// # Returns
///
/// The surviving row, or `None` when `candidates` is empty.
///
/// # Errors
///
/// Returns the errors of [`merge`].
pub async fn merge_all(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    candidates: &[Uuid],
) -> Result<Option<Uuid>, LibraryError> {
    let ordered: BTreeSet<Uuid> = candidates.iter().copied().collect();
    let mut survivor = None;
    for id in ordered {
        survivor = Some(match survivor {
            Some(survivor) => merge(conn, kind, survivor, id).await?,
            None => id,
        });
    }
    Ok(survivor)
}

/// Moves one artist's credits from a losing artist row to the survivor.
///
/// A credit the survivor already carries on the same track or album is dropped;
/// the rest keep their position when it is free and are appended after the
/// survivor's last credit when it is not, so moving a credit never collides with
/// the primary key of `(owner, position)`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `from`: the artist row that goes away.
/// - `into`: the artist row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub(crate) async fn move_track_artist_credits(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    move_artist_credits_track_artists(conn, from, into).await
}

/// Moves one artist's credits in `album_artists` to the survivor.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `from`: the artist row that goes away.
/// - `into`: the artist row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub(crate) async fn move_album_artist_credits(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    move_artist_credits_album_artists(conn, from, into).await
}

/// Moves one track's credits from a losing track row to the survivor.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `from`: the track row that goes away.
/// - `into`: the track row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub(crate) async fn move_track_credits(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    move_owner_credits_track_artists(conn, from, into).await
}

/// Moves one album's credits from a losing album row to the survivor.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `from`: the album row that goes away.
/// - `into`: the album row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails.
pub(crate) async fn move_album_credits(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    move_owner_credits_album_artists(conn, from, into).await
}

/// Moves one artist's credits in `track_artists` to the survivor.
async fn move_artist_credits_track_artists(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    let moved: Vec<(Vec<u8>, i64, String)> =
        sqlx::query_as("SELECT track_id, position, role FROM track_artists WHERE artist_id = ? ORDER BY track_id, position")
            .bind(from_key.as_slice())
            .fetch_all(&mut *conn)
            .await?;
    for (row, position, role) in moved {
        let credited: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM track_artists WHERE artist_id = ? AND track_id = ?",
        )
        .bind(into_key.as_slice())
        .bind(row.as_slice())
        .fetch_optional(&mut *conn)
        .await?;
        if credited.is_some() {
            continue;
        }
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM track_artists WHERE track_id = ? AND position = ?",
        )
        .bind(row.as_slice())
        .bind(position)
        .fetch_optional(&mut *conn)
        .await?;
        let position = match taken {
            Some(_) => {
                let last: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(position) FROM track_artists WHERE track_id = ?",
                )
                .bind(row.as_slice())
                .fetch_one(&mut *conn)
                .await?;
                last.unwrap_or(-1) + 1
            }
            None => position,
        };
        sqlx::query(
            "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
        )
        .bind(row.as_slice())
        .bind(into_key.as_slice())
        .bind(position)
        .bind(&role)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM track_artists WHERE artist_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Moves one artist's credits in `album_artists` to the survivor.
async fn move_artist_credits_album_artists(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    let moved: Vec<(Vec<u8>, i64, String)> =
        sqlx::query_as("SELECT album_id, position, role FROM album_artists WHERE artist_id = ? ORDER BY album_id, position")
            .bind(from_key.as_slice())
            .fetch_all(&mut *conn)
            .await?;
    for (row, position, role) in moved {
        let credited: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM album_artists WHERE artist_id = ? AND album_id = ?",
        )
        .bind(into_key.as_slice())
        .bind(row.as_slice())
        .fetch_optional(&mut *conn)
        .await?;
        if credited.is_some() {
            continue;
        }
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM album_artists WHERE album_id = ? AND position = ?",
        )
        .bind(row.as_slice())
        .bind(position)
        .fetch_optional(&mut *conn)
        .await?;
        let position = match taken {
            Some(_) => {
                let last: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(position) FROM album_artists WHERE album_id = ?",
                )
                .bind(row.as_slice())
                .fetch_one(&mut *conn)
                .await?;
                last.unwrap_or(-1) + 1
            }
            None => position,
        };
        sqlx::query(
            "INSERT INTO album_artists (album_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
        )
        .bind(row.as_slice())
        .bind(into_key.as_slice())
        .bind(position)
        .bind(&role)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM album_artists WHERE artist_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Moves one owner's credits in `track_artists` to the survivor.
async fn move_owner_credits_track_artists(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    let moved: Vec<(Vec<u8>, i64, String)> = sqlx::query_as(
        "SELECT artist_id, position, role FROM track_artists WHERE track_id = ? ORDER BY position",
    )
    .bind(from_key.as_slice())
    .fetch_all(&mut *conn)
    .await?;
    for (artist, position, role) in moved {
        let credited: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM track_artists WHERE track_id = ? AND artist_id = ?",
        )
        .bind(into_key.as_slice())
        .bind(artist.as_slice())
        .fetch_optional(&mut *conn)
        .await?;
        if credited.is_some() {
            continue;
        }
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM track_artists WHERE track_id = ? AND position = ?",
        )
        .bind(into_key.as_slice())
        .bind(position)
        .fetch_optional(&mut *conn)
        .await?;
        let position = match taken {
            Some(_) => {
                let last: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(position) FROM track_artists WHERE track_id = ?",
                )
                .bind(into_key.as_slice())
                .fetch_one(&mut *conn)
                .await?;
                last.unwrap_or(-1) + 1
            }
            None => position,
        };
        sqlx::query(
            "INSERT INTO track_artists (track_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
        )
        .bind(into_key.as_slice())
        .bind(artist.as_slice())
        .bind(position)
        .bind(&role)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM track_artists WHERE track_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Moves one owner's credits in `album_artists` to the survivor.
async fn move_owner_credits_album_artists(
    conn: &mut SqliteConnection,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let from_key = from.as_bytes().to_vec();
    let into_key = into.as_bytes().to_vec();
    let moved: Vec<(Vec<u8>, i64, String)> = sqlx::query_as(
        "SELECT artist_id, position, role FROM album_artists WHERE album_id = ? ORDER BY position",
    )
    .bind(from_key.as_slice())
    .fetch_all(&mut *conn)
    .await?;
    for (artist, position, role) in moved {
        let credited: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM album_artists WHERE album_id = ? AND artist_id = ?",
        )
        .bind(into_key.as_slice())
        .bind(artist.as_slice())
        .fetch_optional(&mut *conn)
        .await?;
        if credited.is_some() {
            continue;
        }
        let taken: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM album_artists WHERE album_id = ? AND position = ?",
        )
        .bind(into_key.as_slice())
        .bind(position)
        .fetch_optional(&mut *conn)
        .await?;
        let position = match taken {
            Some(_) => {
                let last: Option<i64> = sqlx::query_scalar(
                    "SELECT MAX(position) FROM album_artists WHERE album_id = ?",
                )
                .bind(into_key.as_slice())
                .fetch_one(&mut *conn)
                .await?;
                last.unwrap_or(-1) + 1
            }
            None => position,
        };
        sqlx::query(
            "INSERT INTO album_artists (album_id, artist_id, position, role) VALUES (?, ?, ?, ?)",
        )
        .bind(into_key.as_slice())
        .bind(artist.as_slice())
        .bind(position)
        .bind(&role)
        .execute(&mut *conn)
        .await?;
    }
    sqlx::query("DELETE FROM album_artists WHERE album_id = ?")
        .bind(from_key.as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}
