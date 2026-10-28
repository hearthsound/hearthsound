//! The index queue: which entities need provider data fetched, and how far.
//!
//! One row per entity, kept up to date by whoever learns that the entity exists
//! or changed: a scan, an upload, an import, a save, a playlist add, a viewer
//! whose stored data went stale, and the periodic sweep. The indexer claims rows,
//! one at a time per worker, walks the depth a row asks for, and records when it
//! last did.
//!
//! Depth is [`Depth::Head`] for the entity's own fields, [`Depth::Page`] for a
//! page's worth of data (an album's tracklist, an artist's discography) and
//! [`Depth::Deep`] for the recursion that also indexes everything the page
//! points at. A row records the furthest depth asked for and the highest urgency
//! ever asked for, so a deep request is not lost by a later shallow one and a
//! viewer's refresh is not lost by a background sweep.
//!
//! Priority is lower for more urgent work, and the constants below name the rungs:
//! [`PRIORITY_VIEW`] for an administrator's refresh or a viewer staring at an
//! empty page, [`PRIORITY_LIBRARY`] for a library album's page,
//! [`PRIORITY_CREDIT`] for an artist that album credits, [`PRIORITY_TRACK`] for a
//! library track's own head, [`PRIORITY_PREFETCH`] for a cover or discography
//! nobody is looking at yet, and [`PRIORITY_SWEEP`] for the periodic refresh.

use hearthsound_core::EntityKind;
use sqlx::{Connection as _, SqliteConnection};
use uuid::Uuid;

use crate::error::LibraryError;
use crate::external_ids::kind_literal;
use crate::model::decode_uuid;
use crate::now_millis;

/// The urgency of work an administrator asked for or a viewer is waiting on.
pub const PRIORITY_VIEW: i64 = 0;

/// The urgency of a library album's page, which carries its tracks' data.
pub const PRIORITY_LIBRARY: i64 = 1;

/// The urgency of an artist a library album credits.
pub const PRIORITY_CREDIT: i64 = 2;

/// The urgency of a library track's own head walk.
pub const PRIORITY_TRACK: i64 = 3;

/// The urgency of a cover or page nobody is looking at yet.
pub const PRIORITY_PREFETCH: i64 = 4;

/// The urgency of the periodic refresh.
pub const PRIORITY_SWEEP: i64 = 5;

/// How long a claim keeps a row out of every other worker's reach.
///
/// A worker that dies mid-walk leaves its row claimed; the row becomes due again
/// once this window passes, so a crash costs one lease rather than the row.
pub const CLAIM_LEASE_MS: i64 = 600_000;

/// How much of an entity the indexer should fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Depth {
    /// The entity's own fields.
    Head,
    /// The entity's page: an album's tracklist, an artist's discography.
    Page,
    /// The page and everything it points at, recursively.
    Deep,
}

impl Depth {
    /// Returns the literal stored in `index_queue.depth`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Depth::Head => "head",
            Depth::Page => "page",
            Depth::Deep => "deep",
        }
    }

    /// Parses a stored depth literal.
    ///
    /// # Parameters
    ///
    /// - `value`: the stored literal.
    ///
    /// # Returns
    ///
    /// The depth, or `None` for an unknown literal.
    pub fn parse(value: &str) -> Option<Depth> {
        match value {
            "head" => Some(Depth::Head),
            "page" => Some(Depth::Page),
            "deep" => Some(Depth::Deep),
            _ => None,
        }
    }
}

/// One claimed queue row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// The entity kind.
    pub kind: EntityKind,
    /// The row's raw primary key.
    pub entity: Uuid,
    /// The depth to walk.
    pub depth: Depth,
    /// The urgency the row was queued with.
    pub priority: i64,
    /// How many times this row has been attempted.
    pub attempts: i64,
}

/// Queues an entity, keeping the deepest depth and the highest urgency.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `depth`: how far to walk.
/// - `priority`: the urgency, lower being more urgent.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub async fn enqueue(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    depth: Depth,
    priority: i64,
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let rank = match depth {
        Depth::Head => 0,
        Depth::Page => 1,
        Depth::Deep => 2,
    };
    let now = now_millis();
    sqlx::query(
        "INSERT INTO index_queue (entity_kind, entity_id, depth, priority, state, attempts, \
                                  run_at, enqueued_at) \
         VALUES (?, ?, ?, ?, 'queued', 0, ?, ?) \
         ON CONFLICT (entity_kind, entity_id) DO UPDATE SET \
             depth = CASE WHEN (CASE index_queue.depth WHEN 'head' THEN 0 WHEN 'page' THEN 1 \
                                                       ELSE 2 END) >= ? \
                          THEN index_queue.depth ELSE excluded.depth END, \
             priority = MIN(index_queue.priority, excluded.priority), \
             state = 'queued', \
             run_at = MIN(index_queue.run_at, excluded.run_at)",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .bind(depth.as_str())
    .bind(priority)
    .bind(now)
    .bind(now)
    .bind(rank)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Moves a merged entity's queue row to the row that survives it.
///
/// A merge deletes the loser's row and every table that names it, so moving the
/// queue row with a plain `UPDATE` collided with the survivor's own row: the
/// primary key is `(entity_kind, entity_id)`, and both rows are commonly queued.
/// This reads the loser's depth and priority, deletes the row, and enqueues the
/// survivor with them, which keeps the deeper depth and the lower priority the
/// survivor already asked for.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `from`: the row that goes away.
/// - `into`: the row that survives.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub(crate) async fn absorb(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    from: Uuid,
    into: Uuid,
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT depth, priority FROM index_queue WHERE entity_kind = ? AND entity_id = ?",
    )
    .bind(kind_literal)
    .bind(from.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM index_queue WHERE entity_kind = ? AND entity_id = ?")
        .bind(kind_literal)
        .bind(from.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    let Some((depth, priority)) = row else {
        return Ok(());
    };
    let Some(depth) = Depth::parse(&depth) else {
        return Err(LibraryError::UnknownLiteral {
            kind: "index_queue.depth",
            value: depth,
        });
    };
    enqueue(conn, kind, into, depth, priority).await
}

/// Claims up to `limit` due rows, each leased for [`CLAIM_LEASE_MS`].
///
/// The select and the lease are one transaction, so two workers never receive
/// the same row: a claim pushes `run_at` past the lease along with the urgency
/// order, which is why a leased row sorts behind everything still due. A worker
/// that dies mid-walk leaves its rows leased; they become due again when the
/// lease runs out.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `limit`: the most rows to take.
///
/// # Returns
///
/// The claimed rows, most urgent first, or an empty list when nothing is due.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a statement fails,
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded, and
/// [`LibraryError::UnknownLiteral`] for a stored kind or depth that is unknown.
pub async fn claim_many(
    conn: &mut SqliteConnection,
    limit: usize,
) -> Result<Vec<QueueItem>, LibraryError> {
    let now = now_millis();
    let mut tx = conn.begin().await?;
    let rows: Vec<(String, Vec<u8>, String, i64, i64)> = sqlx::query_as(
        "SELECT entity_kind, entity_id, depth, priority, attempts FROM index_queue \
         WHERE state = 'queued' AND run_at <= ? \
         ORDER BY priority, run_at, enqueued_at LIMIT ?",
    )
    .bind(now)
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(&mut *tx)
    .await?;
    let mut claimed = Vec::with_capacity(rows.len());
    for (kind, entity, depth, priority, attempts) in rows {
        sqlx::query("UPDATE index_queue SET run_at = ? WHERE entity_kind = ? AND entity_id = ?")
            .bind(now + CLAIM_LEASE_MS)
            .bind(&kind)
            .bind(&entity)
            .execute(&mut *tx)
            .await?;
        claimed.push(QueueItem {
            kind: match kind.as_str() {
                "track" => EntityKind::Track,
                "album" => EntityKind::Album,
                "artist" => EntityKind::Artist,
                other => {
                    return Err(LibraryError::UnknownLiteral {
                        kind: "entity",
                        value: other.to_owned(),
                    });
                }
            },
            entity: decode_uuid(&entity)?,
            depth: Depth::parse(&depth).ok_or_else(|| LibraryError::UnknownLiteral {
                kind: "index depth",
                value: depth.clone(),
            })?,
            priority,
            attempts,
        });
    }
    tx.commit().await?;
    Ok(claimed)
}

/// Removes a processed row.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `item`: the claimed row.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub async fn complete(conn: &mut SqliteConnection, item: &QueueItem) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(item.kind)?;
    sqlx::query(
        "DELETE FROM index_queue WHERE entity_kind = ? AND entity_id = ? AND priority >= ?",
    )
    .bind(kind_literal)
    .bind(item.entity.as_bytes().as_slice())
    .bind(item.priority)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Puts a claimed row back for later, unchanged otherwise.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `item`: the claimed row.
/// - `run_at`: the instant it becomes due again, in unix milliseconds.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub async fn defer(
    conn: &mut SqliteConnection,
    item: &QueueItem,
    run_at: i64,
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(item.kind)?;
    sqlx::query("UPDATE index_queue SET run_at = ? WHERE entity_kind = ? AND entity_id = ?")
        .bind(run_at)
        .bind(kind_literal)
        .bind(item.entity.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Records a failed attempt and when to retry it.
///
/// A row that has failed [`MAX_ATTEMPTS`] times is marked `failed` and left for
/// the admin overview; the sweep resets it once its failure is old enough.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `item`: the claimed row.
/// - `error`: the message to keep.
/// - `retry_at`: when to retry, or `None` to give up.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub async fn fail(
    conn: &mut SqliteConnection,
    item: &QueueItem,
    error: &str,
    retry_at: Option<i64>,
) -> Result<(), LibraryError> {
    let kind_literal = kind_literal(item.kind)?;
    let attempts = item.attempts + 1;
    let state = match retry_at {
        Some(_) if attempts < MAX_ATTEMPTS => "queued",
        _ => "failed",
    };
    sqlx::query(
        "UPDATE index_queue SET attempts = ?, state = ?, run_at = ?, last_error = ? \
         WHERE entity_kind = ? AND entity_id = ?",
    )
    .bind(attempts)
    .bind(state)
    .bind(retry_at.unwrap_or_else(now_millis))
    .bind(error)
    .bind(kind_literal)
    .bind(item.entity.as_bytes().as_slice())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// How many attempts one row gets before it is marked failed.
pub const MAX_ATTEMPTS: i64 = 5;

/// Returns the state one queued entity is in.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Returns
///
/// The stored `state` - `queued` or `failed` - or `None` when the entity is not
/// queued.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails and
/// [`LibraryError::UnknownLiteral`] for a kind the queue does not index.
pub async fn state_of(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<Option<String>, LibraryError> {
    let kind_literal = kind_literal(kind)?;
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM index_queue WHERE entity_kind = ? AND entity_id = ?")
            .bind(kind_literal)
            .bind(entity.as_bytes().as_slice())
            .fetch_optional(&mut *conn)
            .await?;
    Ok(state)
}

/// Returns how many rows wait, and how many are marked failed.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// `(queued, failed)`.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn counts(conn: &mut SqliteConnection) -> Result<(i64, i64), LibraryError> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(state = 'queued'), 0) AS queued, \
                COALESCE(SUM(state = 'failed'), 0) AS failed FROM index_queue",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(row)
}

/// Returns rows that have not succeeded or failed recently.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `older_than`: failures enqueued before this instant are reset.
///
/// # Returns
///
/// How many rows were reset.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails.
pub async fn reset_failed(
    conn: &mut SqliteConnection,
    older_than: i64,
) -> Result<u64, LibraryError> {
    let affected = sqlx::query(
        "UPDATE index_queue SET state = 'queued', attempts = 0, run_at = ?, last_error = NULL \
         WHERE state = 'failed' AND enqueued_at < ?",
    )
    .bind(now_millis())
    .bind(older_than)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected)
}

/// Queues a library entity for the deep index at library-change urgency.
///
/// A track is queued behind its album: the album's page carries the tracklist,
/// its recordings, ISRCs and credits, so walking it first answers the track's own
/// queue row without a provider request, and the track's row only has to queue
/// the artists it credits. A track whose row is gone or that belongs to no album
/// is queued on its own.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Errors
///
/// Returns the errors of [`enqueue`].
pub async fn enqueue_library(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<(), LibraryError> {
    if kind != EntityKind::Track {
        return enqueue(conn, kind, entity, Depth::Deep, PRIORITY_LIBRARY).await;
    }
    let album: Option<Vec<u8>> = sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
        .bind(entity.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if let Some(album) = album {
        enqueue(
            conn,
            EntityKind::Album,
            decode_uuid(&album)?,
            Depth::Deep,
            PRIORITY_LIBRARY,
        )
        .await?;
    }
    enqueue(conn, kind, entity, Depth::Deep, PRIORITY_TRACK).await
}

/// Lists the entities of one kind the library holds, for the periodic sweep.
///
/// "Held" is the same rule everywhere: a track the server stores a file for, a
/// pinned track, a saved or liked track, or a track of a saved playlist; an album
/// holding such a track or saved or liked itself; an artist credited on such an
/// album or track, or saved or liked. A row whose stored data is older than
/// `cutoff` is included, so the sweep refreshes what the setting says is stale.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `cutoff`: the instant before which stored data counts as stale, in unix
///   milliseconds.
/// - `limit`: the most rows to return.
///
/// # Returns
///
/// The rows that need the deep index.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails, and
/// [`LibraryError::CorruptId`] when a stored id cannot be decoded.
pub async fn library_candidates(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    cutoff: i64,
    limit: i64,
) -> Result<Vec<Uuid>, LibraryError> {
    let rows: Vec<Vec<u8>> = match kind {
        EntityKind::Track => {
            sqlx::query_scalar(
                "SELECT t.id FROM tracks t \
                  WHERE (t.deep_indexed_at IS NULL OR t.deep_indexed_at < ?) AND ( \
                      EXISTS (SELECT 1 FROM media_files mf WHERE mf.track_id = t.id \
                               AND mf.storage IN ('folder', 'library')) \
                      OR EXISTS (SELECT 1 FROM track_retention tr WHERE tr.track_id = t.id \
                                  AND tr.pinned = 1) \
                      OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'track' \
                                  AND ul.entity_id = t.id) \
                      OR EXISTS (SELECT 1 FROM user_feedback uf WHERE uf.entity_kind = 'track' \
                                  AND uf.entity_id = t.id AND uf.value = 'like') \
                      OR EXISTS (SELECT 1 FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                                  WHERE pt.track_id = t.id AND p.is_temporary = 0)) \
                  ORDER BY t.deep_indexed_at IS NOT NULL, t.deep_indexed_at, t.id LIMIT ?",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await?
        }
        EntityKind::Album => {
            sqlx::query_scalar(
                "SELECT a.id FROM albums a \
                  WHERE (a.deep_indexed_at IS NULL OR a.deep_indexed_at < ?) AND ( \
                      EXISTS (SELECT 1 FROM tracks t JOIN media_files mf ON mf.track_id = t.id \
                               WHERE t.album_id = a.id AND mf.storage IN ('folder', 'library')) \
                      OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'album' \
                                  AND ul.entity_id = a.id) \
                      OR EXISTS (SELECT 1 FROM user_feedback uf WHERE uf.entity_kind = 'album' \
                                  AND uf.entity_id = a.id AND uf.value = 'like')) \
                  ORDER BY a.deep_indexed_at IS NOT NULL, a.deep_indexed_at, a.id LIMIT ?",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await?
        }
        _ => {
            sqlx::query_scalar(
                "SELECT ar.id FROM artists ar \
                  WHERE (ar.deep_indexed_at IS NULL OR ar.deep_indexed_at < ?) AND ( \
                      EXISTS (SELECT 1 FROM album_artists aa JOIN tracks t ON t.album_id = aa.album_id \
                               JOIN media_files mf ON mf.track_id = t.id \
                               WHERE aa.artist_id = ar.id AND mf.storage IN ('folder', 'library')) \
                      OR EXISTS (SELECT 1 FROM track_artists ta JOIN tracks t ON t.id = ta.track_id \
                                  JOIN media_files mf ON mf.track_id = t.id \
                                  WHERE ta.artist_id = ar.id AND ta.role = 'main' \
                                    AND mf.storage IN ('folder', 'library')) \
                      OR EXISTS (SELECT 1 FROM user_library ul WHERE ul.entity_kind = 'artist' \
                                  AND ul.entity_id = ar.id) \
                      OR EXISTS (SELECT 1 FROM user_feedback uf WHERE uf.entity_kind = 'artist' \
                                  AND uf.entity_id = ar.id AND uf.value = 'like')) \
                  ORDER BY ar.deep_indexed_at IS NOT NULL, ar.deep_indexed_at, ar.id LIMIT ?",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&mut *conn)
            .await?
        }
    };
    rows.iter()
        .map(|bytes| decode_uuid(bytes).map_err(LibraryError::CorruptId))
        .collect()
}
