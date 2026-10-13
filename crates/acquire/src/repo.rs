//! The `acquisitions` and `acquisition_log` tables.
//!
//! This module is the only place that reads or writes the acquisition ledger.
//! Every function takes a [`SqliteConnection`], which may be a transaction, so a
//! caller that has to keep several writes atomic - admitting an acquisition and
//! marking its track `acquiring`, for instance - can pass its own transaction.
//! [`AcquisitionRepository`] is the pool-level convenience over the same
//! functions.
//!
//! Every state change goes through [`transition`] (or [`fail`], which is a
//! transition to `failed` with a reason), and each of them writes the
//! `acquisition_log` row in the same statement sequence. The log can therefore
//! never disagree with the ledger, which is what the admin view and the
//! diagnostics read.
//!
//! The state machine itself lives in `hearthsound-core`: a transition is only
//! written when [`AcquisitionState::can_transition_to`] allows it, so an
//! acquisition can never reach a state its diagram does not contain.

use hearthsound_core::{
    AcquisitionState, EntityId, FailureReason, Granularity, IdPrefix, Priority,
};
use hearthsound_db::Database;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::error::AcquireError;

/// The states an acquisition occupies while it still holds work.
///
/// Exactly one acquisition per track may be in one of these states at a time;
/// [`active_for_track`] is the lookup that enforces it.
pub const ACTIVE_STATES: [AcquisitionState; 6] = [
    AcquisitionState::Queued,
    AcquisitionState::Resolving,
    AcquisitionState::Searching,
    AcquisitionState::Downloading,
    AcquisitionState::Verifying,
    AcquisitionState::Importing,
];

/// The row one [`insert`] writes.
///
/// The id is chosen by the caller so the staging directory
/// `<storage>/staging/<acquisition-id>/` can be created before the row exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAcquisition {
    /// The acquisition's id.
    pub id: Uuid,
    /// The track the acquisition is for, for a track-granular acquisition.
    pub track_id: Option<EntityId>,
    /// The album the acquisition is for, for an album-granular acquisition.
    pub album_id: Option<EntityId>,
    /// Whether the acquisition targets a track or an album.
    pub granularity: Granularity,
    /// The priority the acquisition was admitted with.
    pub priority: Priority,
    /// Why the acquisition was requested.
    pub reason: String,
    /// The user whose action created the acquisition, when there was one.
    pub requested_by: Option<Uuid>,
}

/// One row of the `acquisitions` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionRow {
    /// The acquisition's id.
    pub id: Uuid,
    /// The track the acquisition is for, when it is track-granular.
    pub track_id: Option<EntityId>,
    /// The album the acquisition is for, when it is album-granular.
    pub album_id: Option<EntityId>,
    /// Whether the acquisition targets a track or an album.
    pub granularity: Granularity,
    /// The acquisition's current priority.
    pub priority: Priority,
    /// Why the acquisition was requested.
    pub reason: String,
    /// The acquisition's state.
    pub state: AcquisitionState,
    /// The user whose action created the acquisition, when there was one.
    pub requested_by: Option<Uuid>,
    /// The provider instance currently serving the acquisition, when one is.
    pub instance_id: Option<Uuid>,
    /// The JSON of the candidate being downloaded, when one is selected.
    pub candidate: Option<String>,
    /// The plugin's opaque token for the in-flight search or download.
    pub plugin_token: Option<String>,
    /// The JSON progress snapshot, when the download has reported one.
    pub progress: Option<String>,
    /// How many candidates the acquisition has attempted.
    pub attempts: i64,
    /// Why the acquisition failed, when it has failed.
    pub failure_reason: Option<FailureReason>,
    /// The backend's own detail of the failure, when there is one.
    pub error_detail: Option<String>,
    /// The media file the import produced, once it has completed.
    pub media_file_id: Option<Uuid>,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

/// One row of the `acquisition_log` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// When the entry was written, in unix milliseconds.
    pub at: i64,
    /// The state the acquisition entered.
    pub state: AcquisitionState,
    /// The free-text detail of the transition, when there is one.
    pub message: Option<String>,
}

/// The stored form of one ledger row.
#[derive(sqlx::FromRow)]
struct AcquisitionDbRow {
    id: Vec<u8>,
    track_id: Option<Vec<u8>>,
    album_id: Option<Vec<u8>>,
    granularity: String,
    priority: String,
    reason: String,
    state: String,
    requested_by: Option<Vec<u8>>,
    instance_id: Option<Vec<u8>>,
    candidate: Option<String>,
    plugin_token: Option<String>,
    progress: Option<String>,
    attempts: i64,
    failure_reason: Option<String>,
    error_detail: Option<String>,
    media_file_id: Option<Vec<u8>>,
    created_at: i64,
    updated_at: i64,
}

impl AcquisitionDbRow {
    /// Converts a stored row into an acquisition.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::CorruptId`] when an id column is not 16 bytes and
    /// [`AcquireError::UnknownLiteral`] when a literal column holds a value
    /// outside its enum.
    fn decode(self) -> Result<AcquisitionRow, AcquireError> {
        Ok(AcquisitionRow {
            id: uuid_of(&self.id)?,
            track_id: self
                .track_id
                .as_deref()
                .map(|bytes| entity(IdPrefix::Track, bytes))
                .transpose()?,
            album_id: self
                .album_id
                .as_deref()
                .map(|bytes| entity(IdPrefix::Album, bytes))
                .transpose()?,
            granularity: decode_literal("granularity", &self.granularity)?,
            priority: decode_literal("priority", &self.priority)?,
            reason: self.reason,
            state: decode_literal("state", &self.state)?,
            requested_by: self.requested_by.as_deref().map(uuid_of).transpose()?,
            instance_id: self.instance_id.as_deref().map(uuid_of).transpose()?,
            candidate: self.candidate,
            plugin_token: self.plugin_token,
            progress: self.progress,
            attempts: self.attempts,
            failure_reason: self
                .failure_reason
                .as_deref()
                .map(|value| decode_literal("failure reason", value))
                .transpose()?,
            error_detail: self.error_detail,
            media_file_id: self.media_file_id.as_deref().map(uuid_of).transpose()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Writes one acquisition row, in `queued`, and the log entry that records it.
///
/// The caller chooses the id and is expected to have created the acquisition's
/// staging directory before calling, so a row never exists without the
/// directory its download writes into.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `new`: the row to write.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when a statement fails.
pub async fn insert(conn: &mut SqliteConnection, new: &NewAcquisition) -> Result<(), AcquireError> {
    let now = crate::now_millis();
    let track_id = new.track_id.map(|id| id.uuid().as_bytes().to_vec());
    let album_id = new.album_id.map(|id| id.uuid().as_bytes().to_vec());
    let requested_by = new.requested_by.map(|id| id.as_bytes().to_vec());
    sqlx::query!(
        "INSERT INTO acquisitions (id, track_id, album_id, granularity, priority, reason, state, \
                                   requested_by, instance_id, candidate, plugin_token, progress, \
                                   attempts, failure_reason, error_detail, media_file_id, \
                                   created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, 'queued', ?, NULL, NULL, NULL, NULL, 0, NULL, NULL, NULL, ?, ?)",
        uuid_key(new.id),
        track_id,
        album_id,
        new.granularity.as_str(),
        new.priority.as_str(),
        new.reason.as_str(),
        requested_by,
        now,
        now,
    )
    .execute(&mut *conn)
    .await?;
    write_log(conn, new.id, AcquisitionState::Queued, Some(&new.reason)).await
}

/// Appends one entry to the acquisition log.
///
/// The entry carries the state the acquisition entered, so the log reads as the
/// sequence of states the acquisition passed through.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition the entry belongs to.
/// - `state`: the state the acquisition is in after the write being logged.
/// - `message`: the free-text detail, when there is one.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the insert fails.
pub async fn write_log(
    conn: &mut SqliteConnection,
    id: Uuid,
    state: AcquisitionState,
    message: Option<&str>,
) -> Result<(), AcquireError> {
    sqlx::query!(
        "INSERT INTO acquisition_log (acquisition_id, at, state, message) VALUES (?, ?, ?, ?)",
        uuid_key(id),
        crate::now_millis(),
        state.as_str(),
        message,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reads one acquisition.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the acquisition to read.
///
/// # Returns
///
/// The row, or `None` when no acquisition has that id.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails.
pub async fn get(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<Option<AcquisitionRow>, AcquireError> {
    let row = sqlx::query_as!(
        AcquisitionDbRow,
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions WHERE id = ?",
        uuid_key(id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AcquisitionDbRow::decode).transpose()
}

/// Reads the acquisition that currently holds a track, if any.
///
/// At most one acquisition per track is active at a time, so this is the lookup
/// a new request uses to decide between joining an existing job and admitting a
/// new one. The most recent row wins when a completed or failed acquisition
/// still references the track.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track to look up.
///
/// # Returns
///
/// The active acquisition, or `None` when the track has none.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails.
pub async fn active_for_track(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Option<AcquisitionRow>, AcquireError> {
    let row = sqlx::query_as!(
        AcquisitionDbRow,
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions \
         WHERE track_id = ? AND state IN ('queued', 'resolving', 'searching', 'downloading', \
                                          'verifying', 'importing') \
         ORDER BY created_at DESC LIMIT 1",
        track_id.uuid().as_bytes().to_vec(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AcquisitionDbRow::decode).transpose()
}

/// Reads the acquisition that runs next.
///
/// The queue is ordered by priority, most urgent first, and then by age, so a
/// `play` request that has been waiting is always taken before any other queued
/// work and equal priorities are served in the order they were admitted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// The most urgent queued acquisition, or `None` when nothing is queued.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails.
pub async fn next_queued(
    conn: &mut SqliteConnection,
) -> Result<Option<AcquisitionRow>, AcquireError> {
    let row = sqlx::query_as!(
        AcquisitionDbRow,
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions WHERE state = 'queued' \
         ORDER BY CASE priority WHEN 'play' THEN 0 WHEN 'next' THEN 1 WHEN 'queue' THEN 2 \
                                 WHEN 'background' THEN 3 ELSE 4 END, created_at, rowid \
         LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AcquisitionDbRow::decode).transpose()
}

/// Returns an acquisition's current state.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the acquisition to read.
///
/// # Errors
///
/// Returns [`AcquireError::UnknownAcquisition`] when no row has that id and
/// [`AcquireError::UnknownLiteral`] when the stored state is not a known one.
pub async fn state(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<AcquisitionState, AcquireError> {
    let stored = sqlx::query_scalar!("SELECT state FROM acquisitions WHERE id = ?", uuid_key(id))
        .fetch_optional(&mut *conn)
        .await?
        .ok_or(AcquireError::UnknownAcquisition(id))?;
    decode_literal("state", &stored)
}

/// Moves an acquisition to `to` and logs it.
///
/// The transition is refused with [`AcquireError::IllegalTransition`] when the
/// state machine has no such edge, so a caller cannot skip a state. The log
/// entry is written in the same connection, which makes the ledger and the log
/// atomic when the caller passes a transaction.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to move.
/// - `to`: the state to move it to.
/// - `message`: the free-text detail of the transition.
///
/// # Errors
///
/// Returns [`AcquireError::IllegalTransition`] when the edge does not exist and
/// [`AcquireError::UnknownAcquisition`] when the row is missing.
pub async fn transition(
    conn: &mut SqliteConnection,
    id: Uuid,
    to: AcquisitionState,
    message: Option<&str>,
) -> Result<(), AcquireError> {
    let from = state(conn, id).await?;
    if !from.can_transition_to(to) {
        return Err(AcquireError::IllegalTransition { from, to });
    }
    sqlx::query!(
        "UPDATE acquisitions SET state = ?, updated_at = ? WHERE id = ?",
        to.as_str(),
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    write_log(conn, id, to, message).await
}

/// Fails an acquisition with a reason and logs it.
///
/// This is [`transition`] to [`AcquisitionState::Failed`] plus the
/// `failure_reason` and `error_detail` columns, all in one statement sequence.
/// The failure reason is one of the fixed literals of
/// [`FailureReason`], so the API layer can map it to an error code.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to fail.
/// - `reason`: why it failed.
/// - `detail`: the backend's own message, when the failure has one.
///
/// # Errors
///
/// Returns [`AcquireError::IllegalTransition`] when the current state has no
/// edge to `failed`.
pub async fn fail(
    conn: &mut SqliteConnection,
    id: Uuid,
    reason: FailureReason,
    detail: Option<&str>,
) -> Result<(), AcquireError> {
    let from = state(conn, id).await?;
    if !from.can_transition_to(AcquisitionState::Failed) {
        return Err(AcquireError::IllegalTransition {
            from,
            to: AcquisitionState::Failed,
        });
    }
    sqlx::query!(
        "UPDATE acquisitions SET state = 'failed', failure_reason = ?, error_detail = ?, \
                                updated_at = ? WHERE id = ?",
        reason.as_str(),
        detail,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    write_log(
        conn,
        id,
        AcquisitionState::Failed,
        Some(detail.unwrap_or(reason.as_str())),
    )
    .await
}

/// Raises an acquisition's priority.
///
/// The priority is only ever raised: a lower-urgency request whose track is
/// already being acquired must not slow the job down. The log entry carries the
/// new priority so the admin view can show when a job was promoted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to update.
/// - `priority`: the priority to set.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the acquisition does not exist.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when a statement fails.
pub async fn set_priority(
    conn: &mut SqliteConnection,
    id: Uuid,
    priority: Priority,
) -> Result<bool, AcquireError> {
    let now = crate::now_millis();
    let affected = sqlx::query!(
        "UPDATE acquisitions SET priority = ?, updated_at = ? WHERE id = ? AND priority <> ?",
        priority.as_str(),
        now,
        uuid_key(id),
        priority.as_str(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if affected == 0 {
        return Ok(false);
    }
    let current = state(conn, id).await?;
    write_log(
        conn,
        id,
        current,
        Some(&format!("priority raised to {}", priority.as_str())),
    )
    .await?;
    Ok(true)
}

/// Claims the most urgent queued acquisition for a worker.
///
/// The claim is a guarded update, so two workers racing for the same row cannot
/// both take it. The claim moves the acquisition to
/// [`AcquisitionState::Resolving`], which is also the first step of the
/// pipeline.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to claim.
///
/// # Returns
///
/// `true` when this caller claimed the row, `false` when it was no longer
/// queued.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when a statement fails.
pub async fn claim(conn: &mut SqliteConnection, id: Uuid) -> Result<bool, AcquireError> {
    let affected = sqlx::query!(
        "UPDATE acquisitions SET state = 'resolving', updated_at = ? WHERE id = ? AND state = 'queued'",
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if affected == 0 {
        return Ok(false);
    }
    write_log(
        conn,
        id,
        AcquisitionState::Resolving,
        Some("claimed by a worker slot"),
    )
    .await?;
    Ok(true)
}

/// Records the instance, candidate and token of a download that just started.
///
/// The three columns are written together, in the same statement, because they
/// describe one attempt: the token is only meaningful with the instance that
/// issued it, and the candidate JSON says which files that token is fetching.
/// Persisting them is what makes a download recoverable after a restart.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition being downloaded.
/// - `instance_id`: the instance serving the download.
/// - `candidate`: the candidate as JSON text.
/// - `plugin_token`: the plugin's token for the download.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the acquisition does not exist.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the statement fails.
pub async fn set_download(
    conn: &mut SqliteConnection,
    id: Uuid,
    instance_id: Option<Uuid>,
    candidate: Option<&str>,
    plugin_token: Option<&str>,
) -> Result<bool, AcquireError> {
    let affected = sqlx::query!(
        "UPDATE acquisitions SET instance_id = ?, candidate = ?, plugin_token = ?, updated_at = ? \
         WHERE id = ?",
        instance_id.map(|instance| instance.as_bytes().to_vec()),
        candidate,
        plugin_token,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Records a download's latest progress snapshot.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition being downloaded.
/// - `progress`: the progress as JSON text, or `None` to clear it.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the acquisition does not exist.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the statement fails.
pub async fn set_progress(
    conn: &mut SqliteConnection,
    id: Uuid,
    progress: Option<&str>,
) -> Result<bool, AcquireError> {
    let affected = sqlx::query!(
        "UPDATE acquisitions SET progress = ?, updated_at = ? WHERE id = ?",
        progress,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Records how many candidates an acquisition has attempted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to update.
/// - `attempts`: the number of candidates tried so far.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the acquisition does not exist.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the statement fails.
pub async fn set_attempts(
    conn: &mut SqliteConnection,
    id: Uuid,
    attempts: i64,
) -> Result<bool, AcquireError> {
    let affected = sqlx::query!(
        "UPDATE acquisitions SET attempts = ?, updated_at = ? WHERE id = ?",
        attempts,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Reads an acquisition's log, oldest first.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the acquisition whose log to read.
///
/// # Returns
///
/// Every entry, in the order it was written.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails and
/// [`AcquireError::UnknownLiteral`] when an entry holds an unknown state.
pub async fn log(conn: &mut SqliteConnection, id: Uuid) -> Result<Vec<LogEntry>, AcquireError> {
    let rows = sqlx::query!(
        "SELECT at, state, message FROM acquisition_log WHERE acquisition_id = ? ORDER BY at, rowid",
        uuid_key(id),
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(LogEntry {
                at: row.at,
                state: decode_literal("state", &row.state)?,
                message: row.message,
            })
        })
        .collect()
}

/// The pool-level view of the acquisition ledger.
///
/// Each method wraps the matching free function in its own transaction, which
/// is what the pipeline stages use when a write is a single logical change. A
/// caller that has to make several writes atomic, such as admitting an
/// acquisition and marking its track `acquiring`, uses the free functions with
/// its own transaction instead.
#[derive(Debug, Clone)]
pub struct AcquisitionRepository {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
}

impl AcquisitionRepository {
    /// Builds the repository over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    pub fn new(db: &Database) -> AcquisitionRepository {
        AcquisitionRepository {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
        }
    }

    /// Reads one acquisition, in its own read transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn get(&self, id: Uuid) -> Result<Option<AcquisitionRow>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        get(&mut conn, id).await
    }

    /// Reads the track's active acquisition, in its own read transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn active_for_track(
        &self,
        track_id: EntityId,
    ) -> Result<Option<AcquisitionRow>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        active_for_track(&mut conn, track_id).await
    }

    /// Reads the next queued acquisition, in its own read transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn next_queued(&self) -> Result<Option<AcquisitionRow>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        next_queued(&mut conn).await
    }

    /// Reads an acquisition's log, in its own read transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn log(&self, id: Uuid) -> Result<Vec<LogEntry>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        log(&mut conn, id).await
    }

    /// Moves an acquisition to `to` in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::IllegalTransition`] when the edge does not exist.
    pub async fn transition(
        &self,
        id: Uuid,
        to: AcquisitionState,
        message: Option<&str>,
    ) -> Result<(), AcquireError> {
        let mut tx = self.writer.begin().await?;
        transition(&mut tx, id, to, message).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fails an acquisition in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::IllegalTransition`] when the edge does not exist.
    pub async fn fail(
        &self,
        id: Uuid,
        reason: FailureReason,
        detail: Option<&str>,
    ) -> Result<(), AcquireError> {
        let mut tx = self.writer.begin().await?;
        fail(&mut tx, id, reason, detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Raises an acquisition's priority in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when a statement fails.
    pub async fn set_priority(&self, id: Uuid, priority: Priority) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let changed = set_priority(&mut tx, id, priority).await?;
        tx.commit().await?;
        Ok(changed)
    }

    /// Records a download's instance, candidate and token in its own
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the write fails.
    pub async fn set_download(
        &self,
        id: Uuid,
        instance_id: Option<Uuid>,
        candidate: Option<&str>,
        plugin_token: Option<&str>,
    ) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let updated = set_download(&mut tx, id, instance_id, candidate, plugin_token).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Records a download's progress snapshot in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the write fails.
    pub async fn set_progress(
        &self,
        id: Uuid,
        progress: Option<&str>,
    ) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let updated = set_progress(&mut tx, id, progress).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Records how many candidates an acquisition has attempted, in its own
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the write fails.
    pub async fn set_attempts(&self, id: Uuid, attempts: i64) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let updated = set_attempts(&mut tx, id, attempts).await?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Claims an acquisition for a worker in its own transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when a statement fails.
    pub async fn claim(&self, id: Uuid) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let claimed = claim(&mut tx, id).await?;
        tx.commit().await?;
        Ok(claimed)
    }
}

/// Reads a 16-byte id column.
///
/// # Errors
///
/// Returns [`AcquireError::CorruptId`] when the bytes are not a UUID.
fn uuid_of(bytes: &[u8]) -> Result<Uuid, AcquireError> {
    Ok(Uuid::from_slice(bytes)?)
}

/// Reads an id column as the client id of `prefix`.
///
/// # Errors
///
/// Returns [`AcquireError::CorruptId`] when the bytes are not a UUID.
fn entity(prefix: IdPrefix, bytes: &[u8]) -> Result<EntityId, AcquireError> {
    Ok(EntityId::from_uuid(prefix, uuid_of(bytes)?))
}

/// The stored bytes of a UUID.
fn uuid_key(id: Uuid) -> Vec<u8> {
    id.as_bytes().to_vec()
}

/// Parses a stored literal into an enum.
///
/// # Errors
///
/// Returns [`AcquireError::UnknownLiteral`] when the value is outside the
/// enum's literals.
fn decode_literal<T: std::str::FromStr>(
    kind: &'static str,
    value: &str,
) -> Result<T, AcquireError> {
    value
        .parse::<T>()
        .map_err(|_| AcquireError::UnknownLiteral {
            kind,
            value: value.to_owned(),
        })
}
