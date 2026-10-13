//! The `plays` table: play history and play counts.
//!
//! A row records one play: who listened, which track, the client that reported
//! it, when playback started, how long the client listened, and whether a
//! stream session delivered song audio. [`Plays::record_play`] is the
//! pool-level entry point; [`Plays::record_play_in`] takes the caller's
//! connection so an insert can join a transaction started by another crate,
//! which is why the statement is not wrapped in a transaction of its own.

use std::collections::HashMap;

use hearthsound_core::new_uuid_v7;
use hearthsound_db::Database;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;
use uuid::Uuid;

use crate::error::ActivityError;

/// The `plays` table.
///
/// Cloning is cheap: every clone shares the same two pools.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// let plays = hearthsound_activity::Plays::new(db);
/// # let _ = plays;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Plays {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl Plays {
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
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(db: &hearthsound_db::Database) {
    /// let plays = hearthsound_activity::Plays::new(db);
    /// # let _ = plays;
    /// # }
    /// ```
    pub fn new(db: &Database) -> Plays {
        Plays {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Records a play through the writer pool.
    ///
    /// This is a convenience over [`Plays::record_play_in`]: it takes the
    /// writer connection for the insert on its own. A caller that already holds
    /// a transaction calls [`Plays::record_play_in`] instead.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that listened.
    /// - `track_id`: the track that was listened to.
    /// - `client`: the client identifier the request carried, when there was
    ///   one.
    /// - `started_at`: when playback started, in unix milliseconds.
    /// - `listened_ms`: how long the client listened, in milliseconds.
    /// - `audio_delivered`: whether a stream session delivered song audio.
    ///
    /// # Returns
    ///
    /// The new row's UUIDv7 primary key.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the insert fails, for example
    /// when the user or the track does not exist.
    pub async fn record_play(
        &self,
        user_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        started_at: i64,
        listened_ms: i64,
        audio_delivered: bool,
    ) -> Result<Uuid, ActivityError> {
        let mut connection = self.writer.acquire().await?;
        Plays::record_play_in(
            &mut connection,
            user_id,
            track_id,
            client,
            started_at,
            listened_ms,
            audio_delivered,
        )
        .await
    }

    /// Records a play on a caller-supplied connection or transaction.
    ///
    /// The insert is one statement and no transaction of its own, so a caller
    /// that is already inside a transaction keeps this row in it.
    ///
    /// # Parameters
    ///
    /// - `connection`: the write connection or transaction to insert on. It
    ///   must be a write connection; a reader connection rejects the statement.
    /// - `user_id`: the account that listened.
    /// - `track_id`: the track that was listened to.
    /// - `client`: the client identifier the request carried, when there was
    ///   one.
    /// - `started_at`: when playback started, in unix milliseconds.
    /// - `listened_ms`: how long the client listened, in milliseconds.
    /// - `audio_delivered`: whether a stream session delivered song audio.
    ///
    /// # Returns
    ///
    /// The new row's UUIDv7 primary key.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the insert fails, for example
    /// when the user or the track does not exist or the connection is
    /// read-only.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async fn example(
    /// #     db: &hearthsound_db::Database,
    /// #     user_id: uuid::Uuid,
    /// #     track_id: uuid::Uuid,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut transaction = db.writer().begin().await?;
    /// hearthsound_activity::Plays::record_play_in(
    ///     &mut transaction,
    ///     user_id,
    ///     track_id,
    ///     Some("dsub"),
    ///     0,
    ///     180_000,
    ///     true,
    /// )
    /// .await?;
    /// transaction.commit().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn record_play_in(
        connection: &mut SqliteConnection,
        user_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        started_at: i64,
        listened_ms: i64,
        audio_delivered: bool,
    ) -> Result<Uuid, ActivityError> {
        let id = new_uuid_v7();
        sqlx::query!(
            "INSERT INTO plays (
                 id, user_id, track_id, client, started_at, listened_ms, audio_delivered
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
            id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
            track_id.as_bytes().as_slice(),
            client,
            started_at,
            listened_ms,
            audio_delivered,
        )
        .execute(&mut *connection)
        .await?;
        Ok(id)
    }

    /// Counts a user's plays of one track.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account to count for.
    /// - `track_id`: the track to count.
    ///
    /// # Returns
    ///
    /// How many plays of that track the user has, `0` when there are none.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the query fails.
    pub async fn play_count(&self, user_id: Uuid, track_id: Uuid) -> Result<i64, ActivityError> {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM plays WHERE user_id = ? AND track_id = ?",
            user_id.as_bytes().as_slice(),
            track_id.as_bytes().as_slice(),
        )
        .fetch_one(&self.readers)
        .await?;
        Ok(count)
    }

    /// Counts a user's plays of several tracks.
    ///
    /// This is what a `playCount` field is built from. Each id is one indexed
    /// lookup on `(user_id, track_id)`.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account to count for.
    /// - `track_ids`: the tracks to count; a duplicate counts once.
    ///
    /// # Returns
    ///
    /// A count per track the user has played. A track the user has never played
    /// has no entry, so an absent track means `playCount = 0`.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a query fails.
    pub async fn play_counts(
        &self,
        user_id: Uuid,
        track_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, i64>, ActivityError> {
        let mut counts = HashMap::new();
        for track_id in track_ids {
            let count = self.play_count(user_id, *track_id).await?;
            if count > 0 {
                counts.insert(*track_id, count);
            }
        }
        Ok(counts)
    }
}
