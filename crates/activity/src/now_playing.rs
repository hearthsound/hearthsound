//! The current playback entry per user and client.
//!
//! A client that reports playback keeps one row per user and client: the track
//! it is playing, where it started, how far in it is, and the state it last
//! reported. A row that is older than the track plus a grace period is stale -
//! the client stopped reporting without saying so - and is left out of every
//! listing, so a crashed client cannot show as playing forever.
//!
//! Who may be listed is the caller's decision: the accounts whose now playing is
//! visible to the viewer are resolved against `privacy.now_playing_visible` by
//! the API layer, which owns the settings of every account, and this module
//! answers with the fresh rows of the accounts it is given.

use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ActivityError;

/// The client name used when a request carries none.
///
/// `now_playing` keys a row by user and client, and a request without a client
/// identifier still has to land somewhere, so it lands here rather than being
/// dropped.
pub const DEFAULT_CLIENT: &str = "unknown";

/// How long after a track's end a row is still shown, in milliseconds.
///
/// A track that is shorter than its listing should be is not unusual, so the
/// grace period is generous; a client that stops reporting is dropped after it.
/// The queries below add this to the track's duration.
pub const PLAYING_GRACE_MS: i64 = 10 * 60 * 1000;

/// The grace period for a track whose duration the server does not know, in
/// milliseconds.
pub const PLAYING_GRACE_UNKNOWN_MS: i64 = 30 * 60 * 1000;

/// One account playing one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NowPlayingEntry {
    /// The account the entry belongs to.
    pub user_id: Uuid,
    /// The account's login name.
    pub username: String,
    /// The client that reported it.
    pub client: String,
    /// The track being played.
    pub track_id: EntityId,
    /// When the playback started, in unix milliseconds.
    pub started_at: i64,
}

/// One stored now-playing row.
#[derive(sqlx::FromRow)]
struct NowPlayingRow {
    /// The stored account id.
    user_id: Vec<u8>,
    /// The account's login name.
    username: String,
    /// The stored client name.
    client: String,
    /// The stored track id.
    track_id: Vec<u8>,
    /// The stored start instant.
    started_at: i64,
}

/// One stored account id.
#[derive(sqlx::FromRow)]
struct UserRow {
    /// The stored account id.
    user_id: Vec<u8>,
}

/// The `now_playing` table.
///
/// Cloning is cheap: every clone shares the same two pools.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// use hearthsound_activity::NowPlaying;
///
/// let playing = NowPlaying::new(db);
/// # let _ = playing;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct NowPlaying {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl NowPlaying {
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
    pub fn new(db: &Database) -> NowPlaying {
        NowPlaying {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Records what one client is playing, replacing what it reported before.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that is playing.
    /// - `client`: the client that reported it.
    /// - `track_id`: the track being played.
    /// - `started_at`: when the playback started, in unix milliseconds.
    /// - `position_ms`: how far into the track the client is.
    /// - `state`: the playback state the client reported.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be written.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        client: &str,
        track_id: EntityId,
        started_at: i64,
        position_ms: i64,
        state: &str,
    ) -> Result<(), ActivityError> {
        // A row that starts in the future would stay visible for as long as it
        // claimed, so no caller can write one: the instant is clamped to now.
        let started_at = started_at.min(crate::now_millis());
        let mut conn = self.writer.acquire().await?;
        sqlx::query!(
            "INSERT INTO now_playing (user_id, client, track_id, started_at, position_ms, state) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT (user_id, client) \
             DO UPDATE SET track_id = excluded.track_id, \
                           started_at = excluded.started_at, \
                           position_ms = excluded.position_ms, \
                           state = excluded.state",
            user_id.as_bytes().as_slice(),
            client,
            entity_key(track_id),
            started_at,
            position_ms,
            state,
        )
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Clears what one client reported.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that stopped playing.
    /// - `client`: the client that stopped reporting.
    ///
    /// # Returns
    ///
    /// `true` when a row was removed.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be removed.
    pub async fn remove(&self, user_id: Uuid, client: &str) -> Result<bool, ActivityError> {
        let mut conn = self.writer.acquire().await?;
        let removed = sqlx::query!(
            "DELETE FROM now_playing WHERE user_id = ? AND client = ?",
            user_id.as_bytes().as_slice(),
            client,
        )
        .execute(&mut *conn)
        .await?
        .rows_affected();
        Ok(removed > 0)
    }

    /// Lists the accounts whose now playing is still fresh.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant the listing is for, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The accounts that are playing a track that has not run out, in any
    /// order.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read and
    /// [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
    pub async fn players(&self, now: i64) -> Result<Vec<Uuid>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query_as!(
            UserRow,
            "SELECT DISTINCT np.user_id AS user_id FROM now_playing np \
             LEFT JOIN tracks t ON t.id = np.track_id \
             WHERE np.started_at + COALESCE(t.duration_ms, 0) \
                   + (CASE WHEN t.duration_ms IS NULL THEN 1800000 ELSE 600000 END) > ?",
            now,
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| Uuid::from_slice(&row.user_id).map_err(ActivityError::from))
            .collect()
    }

    /// Lists the fresh now-playing rows of the named accounts.
    ///
    /// # Parameters
    ///
    /// - `users`: the accounts that may be listed, which the caller has already
    ///   checked the viewer may see.
    /// - `now`: the instant the listing is for, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The rows, most recently started first.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read and
    /// [`ActivityError::CorruptId`] when a stored id is not 16 bytes.
    pub async fn visible(
        &self,
        users: &[Uuid],
        now: i64,
    ) -> Result<Vec<NowPlayingEntry>, ActivityError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query_as!(
            NowPlayingRow,
            "SELECT np.user_id, u.username, np.client, np.track_id, np.started_at \
             FROM now_playing np \
             JOIN users u ON u.id = np.user_id \
             LEFT JOIN tracks t ON t.id = np.track_id \
             WHERE np.started_at + COALESCE(t.duration_ms, 0) \
                   + (CASE WHEN t.duration_ms IS NULL THEN 1800000 ELSE 600000 END) > ? \
             ORDER BY np.started_at DESC, np.user_id, np.client",
            now,
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .filter_map(|row| {
                let user_id = match Uuid::from_slice(&row.user_id) {
                    Ok(user_id) => user_id,
                    Err(error) => return Some(Err(ActivityError::from(error))),
                };
                if !users.contains(&user_id) {
                    return None;
                }
                Some(
                    Uuid::from_slice(&row.track_id)
                        .map_err(ActivityError::from)
                        .map(|track| NowPlayingEntry {
                            user_id,
                            username: row.username,
                            client: row.client,
                            track_id: EntityId::from_uuid(IdPrefix::Track, track),
                            started_at: row.started_at,
                        }),
                )
            })
            .collect::<Result<Vec<_>, ActivityError>>()
    }
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
