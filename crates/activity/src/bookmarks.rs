//! Bookmarks: the position a user left a track at.
//!
//! A bookmark belongs to one user and one track, so a resave replaces the row
//! rather than adding another, and a track may be bookmarked by any number of
//! users independently. Bookmarks are read newest first, which is the order a
//! client lists them in.

use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::now_millis;

/// A user's resume point in one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    /// The bookmarked track.
    pub track_id: EntityId,
    /// The position the user left off at, in milliseconds.
    pub position_ms: i64,
    /// The user's note about the position, when there is one.
    pub comment: Option<String>,
    /// When the bookmark was last saved, in unix milliseconds.
    pub updated_at: i64,
}

/// Returns a track's primary key bytes as an owned vector for a query bind.
///
/// Binding an owned vector rather than a slice of a temporary keeps the
/// compile-checked query macros free of borrow-lifetime noise.
///
/// # Parameters
///
/// - `id`: the track's client id.
///
/// # Returns
///
/// The 16 bytes the `BLOB` column holds.
fn entity_key(id: EntityId) -> Vec<u8> {
    id.uuid().as_bytes().to_vec()
}

/// One stored bookmark row.
#[derive(sqlx::FromRow)]
struct BookmarkRow {
    /// The stored track id.
    track_id: Vec<u8>,
    /// The stored position.
    position_ms: i64,
    /// The stored comment.
    comment: Option<String>,
    /// The stored instant.
    updated_at: i64,
}

/// The `bookmarks` table.
///
/// Cloning is cheap: every clone shares the same two pools.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// use hearthsound_activity::Bookmarks;
///
/// let bookmarks = Bookmarks::new(db);
/// # let _ = bookmarks;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Bookmarks {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl Bookmarks {
    /// Builds the repository over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    ///
    /// # Returns
    ///
    /// The repository.
    pub fn new(db: &Database) -> Bookmarks {
        Bookmarks {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Saves a user's position in a track, replacing an earlier one.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the bookmark belongs to.
    /// - `track_id`: the bookmarked track.
    /// - `position_ms`: the position to resume at, in milliseconds.
    /// - `comment`: the user's note, or `None` to leave the bookmark without
    ///   one.
    ///
    /// # Returns
    ///
    /// The bookmark as it was stored.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be written.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        track_id: EntityId,
        position_ms: i64,
        comment: Option<&str>,
    ) -> Result<Bookmark, ActivityError> {
        let now = now_millis();
        let mut conn = self.writer.acquire().await?;
        sqlx::query!(
            "INSERT INTO bookmarks (user_id, track_id, position_ms, comment, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (user_id, track_id) \
             DO UPDATE SET position_ms = excluded.position_ms, \
                           comment = excluded.comment, \
                           updated_at = excluded.updated_at",
            user_id.as_bytes().as_slice(),
            entity_key(track_id),
            position_ms,
            comment,
            now,
        )
        .execute(&mut *conn)
        .await?;
        Ok(Bookmark {
            track_id,
            position_ms,
            comment: comment.map(str::to_owned),
            updated_at: now,
        })
    }

    /// Removes a user's bookmark of a track.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the bookmark belongs to.
    /// - `track_id`: the bookmarked track.
    ///
    /// # Returns
    ///
    /// `true` when a bookmark was removed, `false` when there was none.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be removed.
    pub async fn delete(&self, user_id: Uuid, track_id: EntityId) -> Result<bool, ActivityError> {
        let mut conn = self.writer.acquire().await?;
        let removed = sqlx::query!(
            "DELETE FROM bookmarks WHERE user_id = ? AND track_id = ?",
            user_id.as_bytes().as_slice(),
            entity_key(track_id),
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
        Ok(removed > 0)
    }

    /// Lists a user's bookmarks, newest first.
    ///
    /// Two bookmarks saved within the same millisecond are ordered by the order
    /// their rows were written, so the last one saved is the first one listed.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose bookmarks are read.
    ///
    /// # Returns
    ///
    /// The bookmarks, most recently saved first.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read and
    /// [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
    pub async fn list(&self, user_id: Uuid) -> Result<Vec<Bookmark>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query_as!(
            BookmarkRow,
            "SELECT track_id, position_ms, comment, updated_at FROM bookmarks \
             WHERE user_id = ? ORDER BY updated_at DESC, rowid DESC",
            user_id.as_bytes().as_slice(),
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(Bookmark {
                    track_id: EntityId::from_uuid(
                        IdPrefix::Track,
                        Uuid::from_slice(&row.track_id)?,
                    ),
                    position_ms: row.position_ms,
                    comment: row.comment,
                    updated_at: row.updated_at,
                })
            })
            .collect::<Result<Vec<_>, uuid::Error>>()
            .map_err(ActivityError::from)
    }
}
