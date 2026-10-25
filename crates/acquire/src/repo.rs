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

use std::collections::HashMap;

use hearthsound_core::{
    AcquisitionState, EntityId, FailureReason, Granularity, IdPrefix, ParkReason, Priority,
};
use hearthsound_db::Database;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};
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

/// The statement a transaction that reads before it writes starts with.
///
/// A deferred transaction that reads first holds a read snapshot, and upgrading
/// it to a write can only wait on `busy_timeout` when no other connection has
/// written since; SQLite answers `SQLITE_BUSY_SNAPSHOT` instead when one has.
/// That is what a running server does while the command line cancels an
/// acquisition, so those transactions start as a write: the writer lock is taken
/// up front and the wait is the one `busy_timeout` covers.
const READ_THEN_WRITE: &str = "BEGIN IMMEDIATE";

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
    /// Why a queued acquisition is waiting for a source, when it is parked.
    pub park_reason: Option<ParkReason>,
    /// How many times the acquisition has parked in a row.
    ///
    /// The counter is reset by a real attempt and by a registry change, and it
    /// sets the backoff between two retries.
    pub park_count: i64,
    /// When the scheduler may claim a parked row again, in unix milliseconds.
    ///
    /// `None` on a parked row means "as soon as a source exists", which is how
    /// a [`ParkReason::NoSource`] park is written.
    pub retry_at: Option<i64>,
    /// The backend's own detail of the failure, when there is one.
    pub error_detail: Option<String>,
    /// The media file the import produced, once it has completed.
    pub media_file_id: Option<Uuid>,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

impl AcquisitionRow {
    /// Returns the row's download progress as a whole percentage.
    ///
    /// The percentage is decoded from the last snapshot the download reported,
    /// which is the same number the event stream and the administrative listing
    /// carry.
    ///
    /// # Returns
    ///
    /// The percentage, or `None` when the download has reported none yet and
    /// when it does not know the total size.
    pub fn percent(&self) -> Option<u8> {
        crate::download::stored_progress(self.progress.as_deref())
            .and_then(|progress| progress.percent())
    }

    /// Returns the estimated number of seconds left in the download.
    ///
    /// # Returns
    ///
    /// The estimate, or `None` when the download has not reported both a total
    /// size and a speed.
    pub fn eta_secs(&self) -> Option<u32> {
        crate::download::stored_progress(self.progress.as_deref())
            .and_then(|progress| progress.eta_secs())
    }
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
    park_reason: Option<String>,
    park_count: i64,
    retry_at: Option<i64>,
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
            park_reason: self
                .park_reason
                .as_deref()
                .map(|value| decode_literal("park reason", value))
                .transpose()?,
            park_count: self.park_count,
            retry_at: self.retry_at,
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
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
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
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
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

/// Reads the active album-granular acquisition of an album.
///
/// An album has at most one such row in an [`ACTIVE_STATES`] state, so a track
/// request for one of the album's tracks can attach to it instead of starting a
/// second job for the same release.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_id`: the album to look up.
///
/// # Returns
///
/// The album's active acquisition, or `None` when it has none.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails, or a decode error when
/// a stored row does not decode.
pub async fn active_for_album(
    conn: &mut SqliteConnection,
    album_id: EntityId,
) -> Result<Option<AcquisitionRow>, AcquireError> {
    let row = sqlx::query_as!(
        AcquisitionDbRow,
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions \
         WHERE album_id = ? AND granularity = 'album' \
           AND state IN ('queued', 'resolving', 'searching', 'downloading', \
                         'verifying', 'importing') \
         ORDER BY created_at DESC LIMIT 1",
        album_id.uuid().as_bytes().to_vec(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AcquisitionDbRow::decode).transpose()
}

/// The number of track ids one batch query binds.
///
/// Keeps every statement well under SQLite's bind-parameter limit; longer lists
/// are read in chunks of this size.
const MAX_IDS_PER_QUERY: usize = 500;

/// Reads the active acquisition of each of the given tracks.
///
/// This is the set form of [`active_for_track`]: a track has at most one row
/// in an [`ACTIVE_STATES`] state, and that row is the one returned for it. Ids
/// are read in chunks of 500.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_ids`: the tracks to look up, in any order.
///
/// # Returns
///
/// The active acquisition of each track that has one, keyed by track; a track
/// without one is absent.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when a read fails, or a decode error when
/// a stored row does not decode.
pub async fn active_for_tracks(
    conn: &mut SqliteConnection,
    track_ids: &[EntityId],
) -> Result<HashMap<EntityId, AcquisitionRow>, AcquireError> {
    latest_rows_for_tracks(conn, track_ids, true).await
}

/// Reads the most recent acquisition of each of the given tracks, in any state.
///
/// This is the set form of "the latest row for a track": the same ordering
/// [`active_for_track`] uses (newest `created_at` first), but without the state
/// filter, so a completed, failed or cancelled row answers too. Ties on
/// `created_at` resolve to the row inserted last. Ids are read in chunks of 500.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_ids`: the tracks to look up, in any order.
///
/// # Returns
///
/// The most recent acquisition of each track that has one, keyed by track; a
/// track with no acquisition is absent.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when a read fails, or a decode error when
/// a stored row does not decode.
pub async fn latest_for_tracks(
    conn: &mut SqliteConnection,
    track_ids: &[EntityId],
) -> Result<HashMap<EntityId, AcquisitionRow>, AcquireError> {
    latest_rows_for_tracks(conn, track_ids, false).await
}

/// Reads the newest acquisition of each track, optionally restricted to the
/// [`ACTIVE_STATES`].
///
/// The first row per track in newest-first order wins, so each track keeps the
/// row [`active_for_track`] or the latest-row lookup would return.
async fn latest_rows_for_tracks(
    conn: &mut SqliteConnection,
    track_ids: &[EntityId],
    active_only: bool,
) -> Result<HashMap<EntityId, AcquisitionRow>, AcquireError> {
    let mut rows_by_track: HashMap<EntityId, AcquisitionRow> = HashMap::new();
    for chunk in track_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new(
            "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                    instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                    park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
             FROM acquisitions WHERE track_id IN (",
        );
        let mut ids = builder.separated(", ");
        for id in chunk {
            ids.push_bind(uuid_key(id.uuid()));
        }
        ids.push_unseparated(")");
        if active_only {
            builder.push(" AND state IN (");
            let mut states = builder.separated(", ");
            for state in ACTIVE_STATES {
                states.push_bind(state.as_str());
            }
            states.push_unseparated(")");
        }
        builder.push(" ORDER BY created_at DESC, rowid DESC");
        let rows = builder
            .build_query_as::<AcquisitionDbRow>()
            .fetch_all(&mut *conn)
            .await?;
        for row in rows {
            let row = row.decode()?;
            if let Some(track) = row.track_id {
                rows_by_track.entry(track).or_insert(row);
            }
        }
    }
    Ok(rows_by_track)
}

/// Counts the acquisitions a user started in a window and the bytes they hold.
///
/// This is what a quota is judged against: [`admitted_since`] counts the rows
/// the user's own requests created, so joining someone else's acquisition - which
/// writes no row - costs the caller nothing, and a refused request - which
/// writes nothing at all - is not counted either.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user`: the account whose acquisitions to count.
/// - `since`: the start of the window, in unix milliseconds; a row created
///   exactly then is counted.
///
/// # Returns
///
/// The number of acquisitions and the total size in bytes of the media files
/// they produced, which is zero for an acquisition that has not imported yet.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails.
pub async fn admitted_since(
    conn: &mut SqliteConnection,
    user: Uuid,
    since: i64,
) -> Result<(i64, i64), AcquireError> {
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM acquisitions WHERE requested_by = ? AND created_at >= ?",
        uuid_key(user),
        since,
    )
    .fetch_one(&mut *conn)
    .await?;

    let bytes: i64 = sqlx::query_scalar!(
        "SELECT COALESCE(SUM(m.size), 0) FROM media_files m \
         WHERE m.acquisition_id IN (SELECT id FROM acquisitions \
                                    WHERE requested_by = ? AND created_at >= ?)",
        uuid_key(user),
        since,
    )
    .fetch_one(&mut *conn)
    .await?;

    Ok((count, bytes))
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
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions WHERE state = 'queued' \
           AND (retry_at IS NULL OR retry_at <= ?) \
         ORDER BY CASE priority WHEN 'play' THEN 0 WHEN 'next' THEN 1 WHEN 'queue' THEN 2 \
                                 WHEN 'background' THEN 3 ELSE 4 END, created_at, rowid \
         LIMIT 1",
        crate::now_millis(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(AcquisitionDbRow::decode).transpose()
}

/// Reads every acquisition that still holds work, oldest first.
///
/// The rows are exactly the ones in [`ACTIVE_STATES`]. A process that starts
/// resumes the acquisitions this returns, so the order matters: oldest first is
/// the order the queue would have served them in, and it keeps the resumed jobs
/// of one track in the order they were admitted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// Every non-terminal acquisition, oldest first.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails and
/// [`AcquireError::UnknownLiteral`] when a stored literal is not a known one.
pub async fn active(conn: &mut SqliteConnection) -> Result<Vec<AcquisitionRow>, AcquireError> {
    let rows = sqlx::query_as!(
        AcquisitionDbRow,
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at \
         FROM acquisitions \
         WHERE state IN ('queued', 'resolving', 'searching', 'downloading', 'verifying', \
                         'importing') \
         ORDER BY created_at, rowid",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(AcquisitionDbRow::decode).collect()
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
                                park_reason = NULL, retry_at = NULL, park_count = 0, \
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

/// Completes an acquisition and records the media file it produced.
///
/// This is [`transition`] to [`AcquisitionState::Completed`] plus the
/// `media_file_id` column, in one statement sequence, so the ledger never shows
/// a completed acquisition without the file it imported.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to complete.
/// - `media_file_id`: the `media_files` row the import produced.
/// - `message`: the free-text detail of the transition.
///
/// # Errors
///
/// Returns [`AcquireError::IllegalTransition`] when the current state has no
/// edge to `completed` and [`AcquireError::UnknownAcquisition`] when the row is
/// missing.
pub async fn complete(
    conn: &mut SqliteConnection,
    id: Uuid,
    media_file_id: Uuid,
    message: Option<&str>,
) -> Result<(), AcquireError> {
    let from = state(conn, id).await?;
    if !from.can_transition_to(AcquisitionState::Completed) {
        return Err(AcquireError::IllegalTransition {
            from,
            to: AcquisitionState::Completed,
        });
    }
    sqlx::query!(
        "UPDATE acquisitions SET state = 'completed', media_file_id = ?, \
                                park_reason = NULL, retry_at = NULL, park_count = 0, updated_at = ? \
         WHERE id = ?",
        media_file_id.as_bytes().to_vec(),
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    write_log(conn, id, AcquisitionState::Completed, message).await
}

/// Completes an album-granular acquisition.
///
/// An album acquisition imports several files, so no single `media_files` row is
/// its product and the column stays empty; the files still name the acquisition
/// they were imported for.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to complete.
/// - `message`: the free-text detail of the transition.
///
/// # Errors
///
/// Returns [`AcquireError::IllegalTransition`] when the current state has no
/// edge to `completed` and [`AcquireError::UnknownAcquisition`] when the row is
/// missing.
pub async fn complete_album(
    conn: &mut SqliteConnection,
    id: Uuid,
    message: Option<&str>,
) -> Result<(), AcquireError> {
    let from = state(conn, id).await?;
    if !from.can_transition_to(AcquisitionState::Completed) {
        return Err(AcquireError::IllegalTransition {
            from,
            to: AcquisitionState::Completed,
        });
    }
    sqlx::query!(
        "UPDATE acquisitions SET state = 'completed', media_file_id = NULL, \
                                park_reason = NULL, retry_at = NULL, park_count = 0, \
                                updated_at = ? \
         WHERE id = ?",
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    write_log(conn, id, AcquisitionState::Completed, message).await
}

/// Ends an acquisition as `cancelled` and logs it.
///
/// The cancellation is written as the same kind of terminal row a failure
/// writes: the state, the `failure_reason` literal the design's failure table
/// gives cancellations, the optional detail and the log entry. The caller is
/// what undoes the acquisition's side effects, because only it knows the staging
/// directory and the backend call that are still live.
///
/// The architecture's state diagram only lets an acquisition be cancelled from
/// the states that hold backend work - `searching`, `downloading`, `verifying`
/// and `importing` - so a row that has not reached one of them is moved through
/// the states that lead to `searching` first. Every cancellation therefore ends
/// in `cancelled` rather than being refused because the job was still queued or
/// resolving.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to cancel.
/// - `message`: why it was cancelled, when the caller knows.
///
/// # Returns
///
/// `true` when the row was cancelled, `false` when it had already ended and
/// nothing was written.
///
/// # Errors
///
/// Returns [`AcquireError::UnknownAcquisition`] when no row has that id and
/// [`AcquireError::Database`] when a statement fails.
pub async fn cancel(
    conn: &mut SqliteConnection,
    id: Uuid,
    message: Option<&str>,
) -> Result<bool, AcquireError> {
    let mut from = state(conn, id).await?;
    if from.is_terminal() {
        return Ok(false);
    }
    // Cancellation is only defined from the states that hold backend work, so a
    // row that has not reached one of them walks to `searching` first.
    if from == AcquisitionState::Queued {
        transition(
            conn,
            id,
            AcquisitionState::Resolving,
            Some("cancelling the acquisition"),
        )
        .await?;
        from = AcquisitionState::Resolving;
    }
    if from == AcquisitionState::Resolving {
        transition(
            conn,
            id,
            AcquisitionState::Searching,
            Some("cancelling the acquisition"),
        )
        .await?;
        from = AcquisitionState::Searching;
    }
    debug_assert!(
        from.can_transition_to(AcquisitionState::Cancelled),
        "every remaining active state may be cancelled"
    );
    sqlx::query!(
        "UPDATE acquisitions SET state = 'cancelled', failure_reason = ?, error_detail = ?, \
                                park_reason = NULL, retry_at = NULL, park_count = 0, \
                                updated_at = ? WHERE id = ?",
        FailureReason::Cancelled.as_str(),
        message,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    write_log(
        conn,
        id,
        AcquisitionState::Cancelled,
        message.or(Some("cancelled")),
    )
    .await?;
    Ok(true)
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
        "UPDATE acquisitions SET state = 'resolving', park_reason = NULL, retry_at = NULL, \
                                updated_at = ? WHERE id = ? AND state = 'queued'",
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
    // A real attempt starts with a token, and it is what resets the park
    // backoff: the retry that reached a download must not inherit the wait of
    // the parks before it.
    let started = i64::from(plugin_token.is_some());
    let affected = sqlx::query!(
        "UPDATE acquisitions SET instance_id = ?, candidate = ?, plugin_token = ?, \
                                park_count = CASE WHEN ? THEN 0 ELSE park_count END, \
                                updated_at = ? \
         WHERE id = ?",
        instance_id.map(|instance| instance.as_bytes().to_vec()),
        candidate,
        plugin_token,
        started,
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

/// Parks an acquisition until a source can be attempted.
///
/// A park is not a failure: the row goes back to [`AcquisitionState::Queued`]
/// through [`transition`], which writes the log entry, and the three park
/// columns say why it waits and when it may be claimed again. The track is not
/// touched, so it keeps the availability admission gave it.
///
/// The caller has already cleared the attempt's instance, candidate, token and
/// progress, because a parked row holds no backend work.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the acquisition to park.
/// - `reason`: why it is waiting.
/// - `park_count`: how many times it has parked in a row, including this one;
///   it sets the backoff between retries.
/// - `retry_at`: when the scheduler may claim it again, in unix milliseconds,
///   or `None` when it may be claimed as soon as a source exists.
/// - `message`: why the attempt ended, for the log.
///
/// # Errors
///
/// Returns [`AcquireError::IllegalTransition`] when the current state has no
/// edge to `queued` - only `searching` and `downloading` park - and
/// [`AcquireError::Database`] when a statement fails.
pub async fn park(
    conn: &mut SqliteConnection,
    id: Uuid,
    reason: ParkReason,
    park_count: i64,
    retry_at: Option<i64>,
    message: &str,
) -> Result<(), AcquireError> {
    transition(conn, id, AcquisitionState::Queued, Some(message)).await?;
    sqlx::query!(
        "UPDATE acquisitions SET park_reason = ?, park_count = ?, retry_at = ?, updated_at = ? \
         WHERE id = ?",
        reason.as_str(),
        park_count,
        retry_at,
        crate::now_millis(),
        uuid_key(id),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Clears the retry schedule of every parked acquisition.
///
/// A provider registry change - an instance added, enabled, disabled, removed
/// or reordered - is what a parked row is waiting for, so the schedules the
/// rows built up are dropped and they become claimable at once. The rows stay
/// parked with their reason until the scheduler picks them up, so a change that
/// does not help them parks them again and their backoff starts over.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
///
/// # Returns
///
/// How many rows the schedule was cleared for.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the statement fails.
pub async fn reset_park_schedule(conn: &mut SqliteConnection) -> Result<u64, AcquireError> {
    let affected = sqlx::query!(
        "UPDATE acquisitions SET retry_at = NULL, park_count = 0, updated_at = ? \
         WHERE state = 'queued' AND park_reason IS NOT NULL",
        crate::now_millis(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected)
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

/// How [`list`] selects the rows it returns.
///
/// The administrative listing reads the ledger for a person rather than for the
/// pipeline, so it is filtered by state and by whether the acquisition still
/// holds work, and it is capped.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::repo::ListFilter;
/// use hearthsound_core::AcquisitionState;
///
/// let filter = ListFilter {
///     state: Some(AcquisitionState::Failed),
///     active: false,
///     limit: Some(20),
/// };
/// assert_eq!(filter.state, Some(AcquisitionState::Failed));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// Only rows in this state, when one is set.
    pub state: Option<AcquisitionState>,
    /// Only rows that still hold work, when set.
    ///
    /// Ignored when [`ListFilter::state`] is set: an explicit state is narrower
    /// than "active".
    pub active: bool,
    /// At most this many rows, when set.
    pub limit: Option<u32>,
}

/// One acquisition as an administrative listing shows it.
///
/// The row carries what a person needs to recognize the job - its client id, its
/// state, its priority and the track it targets - plus the display fields the
/// ledger does not hold itself: the track's title and artist, and the slug of the
/// instance serving it. The percentage is the one the last progress snapshot
/// allows, which is the same number the event stream carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionSummary {
    /// The acquisition's primary key.
    pub id: Uuid,
    /// The acquisition's client id, `ac_...`.
    pub client_id: String,
    /// The acquisition's state.
    pub state: AcquisitionState,
    /// The acquisition's priority.
    pub priority: Priority,
    /// The track the acquisition is for, when it is track-granular.
    pub track_id: Option<EntityId>,
    /// The track's title, when the track still exists.
    pub track_title: Option<String>,
    /// The track's display artist, when the track knows one.
    pub track_artist: Option<String>,
    /// The slug of the instance serving the acquisition, when one is named and
    /// the registry still knows it.
    pub instance: Option<String>,
    /// The download progress as a whole percentage, when the last progress
    /// snapshot allows one.
    pub percent: Option<u8>,
    /// Why the acquisition failed, when it failed or was cancelled.
    pub failure_reason: Option<FailureReason>,
    /// Why a queued acquisition is waiting for a source, when it is parked.
    pub park_reason: Option<ParkReason>,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

/// The stored form of one listed acquisition.
#[derive(sqlx::FromRow)]
struct ListDbRow {
    id: Vec<u8>,
    track_id: Option<Vec<u8>>,
    priority: String,
    state: String,
    progress: Option<String>,
    failure_reason: Option<String>,
    park_reason: Option<String>,
    created_at: i64,
    updated_at: i64,
    track_title: Option<String>,
    track_artist: Option<String>,
    instance_slug: Option<String>,
}

impl ListDbRow {
    /// Converts a stored row into a summary.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::CorruptId`] when an id column is not 16 bytes and
    /// [`AcquireError::UnknownLiteral`] when a literal column holds a value
    /// outside its enum.
    fn decode(self) -> Result<AcquisitionSummary, AcquireError> {
        let id = uuid_of(&self.id)?;
        Ok(AcquisitionSummary {
            id,
            client_id: EntityId::from_uuid(IdPrefix::Acquisition, id).to_string(),
            state: decode_literal("state", &self.state)?,
            priority: decode_literal("priority", &self.priority)?,
            track_id: self
                .track_id
                .as_deref()
                .map(|bytes| entity(IdPrefix::Track, bytes))
                .transpose()?,
            track_title: self.track_title,
            track_artist: self.track_artist,
            instance: self.instance_slug,
            percent: crate::download::stored_progress(self.progress.as_deref())
                .and_then(|stored| stored.percent()),
            failure_reason: self
                .failure_reason
                .as_deref()
                .map(|value| decode_literal("failure reason", value))
                .transpose()?,
            park_reason: self
                .park_reason
                .as_deref()
                .map(|value| decode_literal("park reason", value))
                .transpose()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Lists acquisitions, newest first, with the display fields of their track.
///
/// The track's title and artist are joined in, as is the slug of the instance
/// serving the acquisition, so a caller that only prints the listing does not
/// have to look either up. A row whose track or instance has been deleted keeps
/// its place with the missing field left `None`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `filter`: which rows to return and how many.
///
/// # Returns
///
/// The matching rows, newest first.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails and
/// [`AcquireError::UnknownLiteral`] when a stored literal is not a known one.
pub async fn list(
    conn: &mut SqliteConnection,
    filter: &ListFilter,
) -> Result<Vec<AcquisitionSummary>, AcquireError> {
    let state = filter.state.map(AcquisitionState::as_str);
    // An explicit state is narrower than "only the acquisitions that still hold
    // work", so the two filters are never combined: `--state failed` lists the
    // failed ones even though they have ended.
    let active = i64::from(filter.active && filter.state.is_none());
    let limit = i64::from(filter.limit.unwrap_or(u32::MAX));
    let rows = sqlx::query_as!(
        ListDbRow,
        "SELECT a.id, a.track_id, a.priority, a.state, a.progress, a.failure_reason, \
                a.park_reason, a.created_at, a.updated_at, \
                t.title AS track_title, t.display_artist AS track_artist, \
                i.slug AS instance_slug \
         FROM acquisitions a \
         LEFT JOIN tracks t ON t.id = a.track_id \
         LEFT JOIN provider_instances i ON i.id = a.instance_id \
         WHERE (? IS NULL OR a.state = ?) \
           AND (? = 0 OR a.state IN ('queued', 'resolving', 'searching', 'downloading', \
                                     'verifying', 'importing')) \
         ORDER BY a.created_at DESC, a.rowid DESC \
         LIMIT ?",
        state,
        state,
        active,
        limit,
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(ListDbRow::decode).collect()
}

/// Which part of the acquisition ledger a listing reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerWindow {
    /// The acquisitions that still hold work, in any of the
    /// [`ACTIVE_STATES`].
    Active,
    /// The acquisitions that ended at or after a cutoff instant.
    Recent {
        /// The earliest `updated_at` a row may have, in unix milliseconds. A
        /// row last written exactly then is included.
        since: i64,
    },
    /// Both, newest first.
    All,
}

/// One page of the acquisition ledger.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::repo::{LedgerQuery, LedgerWindow};
///
/// let query = LedgerQuery {
///     window: LedgerWindow::Recent { since: 0 },
///     requested_by: None,
///     limit: 50,
///     offset: 0,
/// };
/// assert_eq!(query.limit, 50);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerQuery {
    /// Which part of the ledger to read.
    pub window: LedgerWindow,
    /// Only the rows this account requested, when set; every account's rows
    /// when `None`.
    pub requested_by: Option<Uuid>,
    /// At most this many rows.
    pub limit: u32,
    /// How many matching rows to skip, for the page after this one.
    pub offset: u32,
}

/// Lists one page of the acquisition ledger, newest first.
///
/// This is the listing a client-facing view reads: it selects whole rows rather
/// than the display columns [`list`] joins in, so a caller can render the target
/// of an album-granular row as well as a track-granular one, and it takes the
/// window, the requester and the page in one query.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `query`: which rows to return, and how many.
///
/// # Returns
///
/// The matching rows, newest first: by creation, and by insertion order within
/// one millisecond, so a page is stable across calls.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the read fails, or a decode error
/// when a stored row does not decode.
pub async fn list_window(
    conn: &mut SqliteConnection,
    query: &LedgerQuery,
) -> Result<Vec<AcquisitionRow>, AcquireError> {
    let mut builder = QueryBuilder::<Sqlite>::new(
        "SELECT id, track_id, album_id, granularity, priority, reason, state, requested_by, \
                instance_id, candidate, plugin_token, progress, attempts, failure_reason, \
                park_reason, park_count, retry_at, error_detail, media_file_id, created_at, updated_at FROM acquisitions \
         WHERE 1 = 1",
    );
    match query.window {
        LedgerWindow::Active => push_state_filter(&mut builder, true, None),
        LedgerWindow::Recent { since } => push_state_filter(&mut builder, false, Some(since)),
        LedgerWindow::All => {}
    }
    if let Some(user) = query.requested_by {
        builder
            .push(" AND requested_by = ")
            .push_bind(uuid_key(user));
    }
    builder
        .push(" ORDER BY created_at DESC, rowid DESC LIMIT ")
        .push_bind(i64::from(query.limit))
        .push(" OFFSET ")
        .push_bind(i64::from(query.offset));
    let rows = builder
        .build_query_as::<AcquisitionDbRow>()
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(AcquisitionDbRow::decode).collect()
}

/// Adds a window's state predicate to a ledger listing.
///
/// # Parameters
///
/// - `builder`: the statement being built.
/// - `active`: whether the rows that still hold work are the ones wanted; the
///   other case selects the rows that ended.
/// - `since`: the cutoff a terminal row is held to, when the caller has one.
fn push_state_filter(builder: &mut QueryBuilder<Sqlite>, active: bool, since: Option<i64>) {
    let keyword = if active {
        " AND state IN ("
    } else {
        " AND state NOT IN ("
    };
    builder.push(keyword);
    let mut states = builder.separated(", ");
    for state in ACTIVE_STATES {
        states.push_bind(state.as_str());
    }
    states.push_unseparated(")");
    if let Some(since) = since {
        builder.push(" AND updated_at >= ").push_bind(since);
    }
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

    /// Reads every acquisition that still holds work, in its own read
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn active(&self) -> Result<Vec<AcquisitionRow>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        active(&mut conn).await
    }

    /// Lists one page of the acquisition ledger, in its own read transaction.
    ///
    /// # Parameters
    ///
    /// - `query`: which rows to return, and how many.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn list_window(
        &self,
        query: &LedgerQuery,
    ) -> Result<Vec<AcquisitionRow>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        list_window(&mut conn, query).await
    }

    /// Lists acquisitions, in its own read transaction.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn list(&self, filter: &ListFilter) -> Result<Vec<AcquisitionSummary>, AcquireError> {
        let mut conn = self.readers.acquire().await?;
        list(&mut conn, filter).await
    }

    /// Cancels an acquisition in its own transaction.
    ///
    /// The cancellation reads the row's state before it writes, so the
    /// transaction starts as a write and a running server that commits to the
    /// same file cannot make it fail with a busy snapshot.
    ///
    /// # Returns
    ///
    /// `true` when the row was cancelled, `false` when it had already ended.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when no row has that id.
    pub async fn cancel(&self, id: Uuid, message: Option<&str>) -> Result<bool, AcquireError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let cancelled = cancel(&mut tx, id, message).await?;
        tx.commit().await?;
        Ok(cancelled)
    }

    /// Moves an acquisition to `to` in its own transaction.
    ///
    /// The transition reads the row's state before it writes, so the transaction
    /// starts as a write; see [`AcquisitionRepository::cancel`].
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
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        transition(&mut tx, id, to, message).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fails an acquisition in its own transaction.
    ///
    /// The failure reads the row's state before it writes, so the transaction
    /// starts as a write; see [`AcquisitionRepository::cancel`].
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
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        fail(&mut tx, id, reason, detail).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Completes an acquisition in its own transaction.
    ///
    /// The completion reads the row's state before it writes, so the transaction
    /// starts as a write; see [`AcquisitionRepository::cancel`].
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::IllegalTransition`] when the edge does not exist.
    pub async fn complete(
        &self,
        id: Uuid,
        media_file_id: Uuid,
        message: Option<&str>,
    ) -> Result<(), AcquireError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        complete(&mut tx, id, media_file_id, message).await?;
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

    /// Clears the retry schedule of every parked acquisition in its own
    /// transaction.
    ///
    /// This is what a provider registry change does to the parked rows: they
    /// become claimable at once, so a source that was just added or enabled is
    /// tried without waiting for the rows' timers.
    ///
    /// # Returns
    ///
    /// How many rows the schedule was cleared for.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the write fails.
    pub async fn reset_park_schedule(&self) -> Result<u64, AcquireError> {
        let mut tx = self.writer.begin().await?;
        let cleared = reset_park_schedule(&mut tx).await?;
        tx.commit().await?;
        Ok(cleared)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_park_reason_round_trips_through_its_literal() {
        for reason in ParkReason::ALL {
            let decoded: ParkReason = decode_literal("park reason", reason.as_str()).unwrap();
            assert_eq!(decoded, reason);
        }
    }

    #[test]
    fn an_unknown_park_reason_is_refused() {
        let decoded = decode_literal::<ParkReason>("park reason", "no-sources");
        assert!(matches!(
            decoded,
            Err(AcquireError::UnknownLiteral {
                kind: "park reason",
                ..
            })
        ));
    }
}
