//! The open playback session of each user and client, and its row.
//!
//! A playback report describes one client's progress through one track: when it
//! started, how much of it was listened to, whether it is playing or paused, and
//! whether the client asked for no play to be recorded. The server needs that
//! state between two reports, and it must survive a restart: a client that
//! listened to most of a track, lost the server to a container restart, and then
//! stopped must still earn its play.
//!
//! So the session lives in two places. The copy in memory is authoritative for
//! the pass, because a report is decided against the newest state. The row in
//! `listen_sessions` is a throttled shadow of it: it is written when a session
//! opens, when its state changes, when it crosses the play threshold, and at
//! most every [`LISTEN_SESSION_PERSIST_MS`] while it keeps playing. A restart
//! reads those rows back with [`ListenSessions::all`] and
//! [`hearthsound_activity::InMemoryReports::recover`] decides what to do with
//! each: a session whose client is still within its track goes back into
//! memory, and one that went quiet is closed, recording the play it earned.
//!
//! The row is best effort. A write that fails is logged and never fails the
//! report it came from: losing one shadow of a session costs at most the
//! listening time since the previous write, while failing a report would lose
//! the play the client did report.

use hearthsound_core::EntityId;
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::reports::PlaybackState;

/// The fastest playback rate a client may report.
///
/// Eight times normal speed. A rate above it is a client bug rather than a
/// player setting, and without a bound a client can earn a play in milliseconds;
/// the session clamps what it counts so a wrong report cannot inflate the
/// listening time even when it reaches the layer.
pub const MAX_PLAYBACK_RATE: f64 = 8.0;

/// How often a session in progress is written, in milliseconds.
///
/// Five seconds. A restart loses at most this much listening time, and a client
/// that reports every second still writes its session once per interval rather
/// than on every report.
pub const LISTEN_SESSION_PERSIST_MS: i64 = 5_000;

/// One client's open playback session.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::{ListenSession, PlaybackState};
///
/// let session = ListenSession {
///     track_id: hearthsound_core::EntityId::new(hearthsound_core::IdPrefix::Track),
///     duration_ms: 180_000,
///     state: PlaybackState::Playing,
///     started_at: 0,
///     last_at: 0,
///     listened_ms: 0,
///     ignore_scrobble: false,
///     persisted_at: 0,
/// };
/// assert!(!session.reached_threshold());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ListenSession {
    /// The track the session is for.
    pub track_id: EntityId,
    /// How long the track lasts, in milliseconds.
    pub duration_ms: i64,
    /// The state the client last reported.
    pub state: PlaybackState,
    /// The instant the session's first report arrived, in unix milliseconds.
    pub started_at: i64,
    /// The instant the last report arrived, in unix milliseconds.
    pub last_at: i64,
    /// How much listening time has accumulated, in milliseconds.
    pub listened_ms: i64,
    /// Whether the client asked for no play to be recorded.
    pub ignore_scrobble: bool,
    /// When the session was last written, in unix milliseconds; `0` when it was
    /// never written.
    pub persisted_at: i64,
}

impl ListenSession {
    /// Adds the time that passed since the last report while the client said it
    /// was playing.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant of the new report, in unix milliseconds.
    /// - `rate`: the playback rate the client reported.
    pub fn accumulate(&mut self, now: i64, rate: f64) {
        if self.state == PlaybackState::Playing {
            let elapsed = (now - self.last_at).max(0) as f64;
            self.listened_ms += (elapsed * rate.clamp(0.0, MAX_PLAYBACK_RATE)) as i64;
        }
        self.last_at = now;
    }

    /// Reports whether the session has listened to enough to be a play.
    ///
    /// The threshold is half the track, or four minutes of it, whichever comes
    /// first, on a track of at least [`crate::MIN_TRACK_MS`]. It answers about
    /// the listening alone: whether the client asked to be ignored is
    /// [`ListenSession::earned`]'s business, so a session can cross the threshold
    /// and still record nothing.
    ///
    /// # Returns
    ///
    /// `true` when the listened time reached the threshold.
    pub fn reached_threshold(&self) -> bool {
        self.duration_ms >= crate::MIN_TRACK_MS
            && self.listened_ms >= (self.duration_ms / 2).min(crate::LISTENED_HALF_MS)
    }

    /// Reports whether the session earned a play.
    ///
    /// # Returns
    ///
    /// `true` when the client did not ask to be ignored and the session reached
    /// the play threshold.
    pub fn earned(&self) -> bool {
        !self.ignore_scrobble && self.reached_threshold()
    }
}

/// One stored session, with the account and client it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSession {
    /// The account that is playing.
    pub user_id: Uuid,
    /// The client that reported.
    pub client: String,
    /// The session.
    pub session: ListenSession,
}

/// The `listen_sessions` table.
///
/// Cloning is cheap: every clone shares the same two pools.
#[derive(Debug, Clone)]
pub struct ListenSessions {
    /// The writer pool every write goes through.
    writer: SqlitePool,
    /// The reader pool the rows are read on.
    readers: SqlitePool,
}

impl ListenSessions {
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
    pub fn new(db: &Database) -> ListenSessions {
        ListenSessions {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Writes one session, replacing the row of the same account and client.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that is playing.
    /// - `client`: the client that reported.
    /// - `session`: the session as memory has it.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be written.
    pub async fn upsert(
        &self,
        user_id: Uuid,
        client: &str,
        session: &ListenSession,
    ) -> Result<(), ActivityError> {
        let track_id = *session.track_id.uuid().as_bytes();
        sqlx::query!(
            "INSERT INTO listen_sessions (
                 user_id, client, track_id, duration_ms, state, started_at, last_at,
                 listened_ms, ignore_scrobble, persisted_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (user_id, client) DO UPDATE SET
                 track_id = excluded.track_id,
                 duration_ms = excluded.duration_ms,
                 state = excluded.state,
                 started_at = excluded.started_at,
                 last_at = excluded.last_at,
                 listened_ms = excluded.listened_ms,
                 ignore_scrobble = excluded.ignore_scrobble,
                 persisted_at = excluded.persisted_at",
            user_id.as_bytes().as_slice(),
            client,
            track_id.as_slice(),
            session.duration_ms,
            session.state.as_str(),
            session.started_at,
            session.last_at,
            session.listened_ms,
            i64::from(session.ignore_scrobble),
            session.persisted_at,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Removes the stored session of one account and client.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that is playing.
    /// - `client`: the client that reported.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be removed.
    pub async fn delete(&self, user_id: Uuid, client: &str) -> Result<(), ActivityError> {
        sqlx::query!(
            "DELETE FROM listen_sessions WHERE user_id = ? AND client = ?",
            user_id.as_bytes().as_slice(),
            client,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Returns every stored session, oldest report first.
    ///
    /// # Returns
    ///
    /// One entry per open session.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the rows cannot be read or an
    /// id does not decode.
    pub async fn all(&self) -> Result<Vec<StoredSession>, ActivityError> {
        let rows = sqlx::query!(
            "SELECT user_id, client, track_id, duration_ms, state, started_at, last_at,
                    listened_ms, ignore_scrobble, persisted_at
             FROM listen_sessions ORDER BY last_at",
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter()
            .map(|row| {
                let state = PlaybackState::parse(&row.state).ok_or_else(|| {
                    ActivityError::Database(sqlx::Error::Protocol(format!(
                        "unknown listen session state `{}`",
                        row.state
                    )))
                })?;
                Ok(StoredSession {
                    user_id: Uuid::from_slice(&row.user_id)?,
                    client: row.client,
                    session: ListenSession {
                        track_id: EntityId::from_uuid(
                            hearthsound_core::IdPrefix::Track,
                            Uuid::from_slice(&row.track_id)?,
                        ),
                        duration_ms: row.duration_ms,
                        state,
                        started_at: row.started_at,
                        last_at: row.last_at,
                        listened_ms: row.listened_ms,
                        ignore_scrobble: row.ignore_scrobble != 0,
                        persisted_at: row.persisted_at,
                    },
                })
            })
            .collect()
    }
}
