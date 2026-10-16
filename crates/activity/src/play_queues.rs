//! Saved play queues, one row per user and client.
//!
//! A client saves the queue it is playing so the same or another client can
//! pick it up later. The row holds the client ids as the client sent them, so
//! the server does not have to resolve them while saving, and the most recently
//! saved row is what `getPlayQueue` answers: a user who plays on a phone and
//! then on a desktop finds the queue the desktop saved, whoever asked last.
//!
//! Saving an empty queue is not a saved queue: it deletes the row, which is how
//! a client clears what it saved.

use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::now_millis;

/// The play queue one client saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedQueue {
    /// The client that saved it.
    pub client: String,
    /// The track ids the client sent, in queue order.
    pub track_ids: Vec<String>,
    /// The zero-based index of the entry the client was on.
    pub current_index: i64,
    /// How far into the current entry the client was, in milliseconds.
    pub position_ms: i64,
    /// When it was saved, in unix milliseconds.
    pub updated_at: i64,
}

/// The `play_queues` table.
///
/// Cloning is cheap: every clone shares the same two pools.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// use hearthsound_activity::PlayQueues;
///
/// let queues = PlayQueues::new(db);
/// # let _ = queues;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct PlayQueues {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl PlayQueues {
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
    pub fn new(db: &Database) -> PlayQueues {
        PlayQueues {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Saves the queue one client is playing.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that saved the queue.
    /// - `client`: the client that saved it.
    /// - `track_ids`: the track ids, in queue order, as the client sent them.
    ///   An empty list clears the saved queue.
    /// - `current_index`: the zero-based index of the entry the client is on.
    /// - `position_ms`: how far into that entry the client was.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::InvalidQueueIndex`] when the index names no
    /// entry of a non-empty queue, [`ActivityError::Json`] when the ids cannot
    /// be encoded, and [`ActivityError::Database`] when the row cannot be
    /// written.
    pub async fn save(
        &self,
        user_id: Uuid,
        client: &str,
        track_ids: &[String],
        current_index: usize,
        position_ms: i64,
    ) -> Result<(), ActivityError> {
        let mut conn = self.writer.acquire().await?;
        if track_ids.is_empty() {
            sqlx::query!(
                "DELETE FROM play_queues WHERE user_id = ? AND client = ?",
                user_id.as_bytes().as_slice(),
                client,
            )
            .execute(&mut *conn)
            .await?;
            return Ok(());
        }
        if current_index >= track_ids.len() {
            return Err(ActivityError::InvalidQueueIndex {
                index: current_index,
                len: track_ids.len(),
            });
        }
        let ids = serde_json::to_string(track_ids)?;
        sqlx::query!(
            "INSERT INTO play_queues (user_id, client, current_index, position_ms, track_ids, \
                                      updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id, client) \
             DO UPDATE SET current_index = excluded.current_index, \
                           position_ms = excluded.position_ms, \
                           track_ids = excluded.track_ids, \
                           updated_at = excluded.updated_at",
            user_id.as_bytes().as_slice(),
            client,
            current_index as i64,
            position_ms,
            ids,
            now_millis(),
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Returns the queue the user saved most recently.
    ///
    /// Two queues saved within the same millisecond are ordered by the order
    /// their rows were written, so the last save wins.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose queue is read.
    ///
    /// # Returns
    ///
    /// The most recently saved queue, or `None` when the user has none.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Json`] when a stored list cannot be decoded and
    /// [`ActivityError::Database`] when the row cannot be read.
    pub async fn latest(&self, user_id: Uuid) -> Result<Option<SavedQueue>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let row = sqlx::query!(
            "SELECT client, current_index, position_ms, track_ids, updated_at \
             FROM play_queues WHERE user_id = ? \
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            user_id.as_bytes().as_slice(),
        )
        .fetch_optional(&mut *conn)
        .await?;
        match row {
            Some(row) => Ok(Some(SavedQueue {
                client: row.client,
                track_ids: serde_json::from_str(&row.track_ids)?,
                current_index: row.current_index,
                position_ms: row.position_ms,
                updated_at: row.updated_at,
            })),
            None => Ok(None),
        }
    }
}
