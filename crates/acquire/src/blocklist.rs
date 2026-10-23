//! The `source_blocklist` quarantine table.
//!
//! A file that failed fingerprint verification is not a file the server should
//! be offered again in a hurry: the source either returned a different recording
//! or a corrupted one, and asking it for the same file again wastes a download
//! and can hand the wrong audio to a verification plugin twice. The file's
//! source key and its own name are therefore written to `source_blocklist` with
//! the reason and an expiry, and the search stage drops any candidate file the
//! table names until that expiry passes. The expiry keeps the list from becoming
//! a permanent ban: `acquisition.verify.quarantine_days` decides how long one
//! lasts, and the storage maintenance pass deletes the rows that have lapsed.
//!
//! The table is keyed by the instance that offered the file, because the same
//! source key means nothing to another backend.

use std::collections::HashSet;

use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::AcquireError;
use crate::orchestrator::AcquisitionOrchestrator;

/// The milliseconds in one day.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// One blocked file: the source key and the file name the candidate reported.
pub(crate) type BlockedFile = (String, String);

/// Reads the unexpired blocklist entries of one instance.
///
/// # Parameters
///
/// - `conn`: the reader connection to query.
/// - `instance_id`: the instance whose entries are read.
/// - `now`: the current unix time in milliseconds; an entry whose `expires_at`
///   is at or before it has lapsed and is left out.
///
/// # Returns
///
/// The source keys and file names that are still quarantined.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the query fails.
pub(crate) async fn blocked_files(
    conn: &mut SqliteConnection,
    instance_id: Uuid,
    now: i64,
) -> Result<HashSet<BlockedFile>, AcquireError> {
    let rows = sqlx::query!(
        "SELECT source_key, file_key FROM source_blocklist \
         WHERE instance_id = ? AND (expires_at IS NULL OR expires_at > ?)",
        instance_id.as_bytes().as_slice(),
        now,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.source_key, row.file_key))
        .collect())
}

/// Writes one quarantine entry, replacing an earlier one for the same file.
///
/// # Parameters
///
/// - `conn`: the write transaction to insert on.
/// - `instance_id`: the instance that offered the file.
/// - `source_key`: the backend's key for the source.
/// - `file_key`: the file name the candidate reported.
/// - `reason`: why the file was quarantined.
/// - `created_at`: when the quarantine began, in unix milliseconds.
/// - `quarantine_days`: how many days the entry lasts; values below one are
///   treated as one.
///
/// # Returns
///
/// The instant the entry expires, in unix milliseconds.
///
/// # Errors
///
/// Returns [`AcquireError::Database`] when the write fails.
pub(crate) async fn block(
    conn: &mut SqliteConnection,
    instance_id: Uuid,
    source_key: &str,
    file_key: &str,
    reason: &str,
    created_at: i64,
    quarantine_days: i64,
) -> Result<i64, AcquireError> {
    let days = quarantine_days.max(1);
    let expires_at = created_at.saturating_add(days.saturating_mul(MILLIS_PER_DAY));
    sqlx::query!(
        "INSERT INTO source_blocklist (instance_id, source_key, file_key, reason, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (instance_id, source_key, file_key) DO UPDATE SET \
             reason = excluded.reason, created_at = excluded.created_at, expires_at = excluded.expires_at",
        instance_id.as_bytes().as_slice(),
        source_key,
        file_key,
        reason,
        created_at,
        expires_at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(expires_at)
}

impl AcquisitionOrchestrator {
    /// Returns the unexpired quarantine entries of one instance.
    ///
    /// A read failure is logged and treated as an empty list: a search must not
    /// fail because the quarantine table could not be read, and the worst case
    /// is that a blocklisted file is offered once more.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the instance whose entries are read.
    ///
    /// # Returns
    ///
    /// The source keys and file names that are still quarantined.
    pub(crate) async fn blocked_files(&self, instance_id: Uuid) -> HashSet<BlockedFile> {
        let mut conn = match self.database().readers().acquire().await {
            Ok(conn) => conn,
            Err(error) => {
                tracing::warn!(%error, "the source blocklist could not be read");
                return HashSet::new();
            }
        };
        match blocked_files(&mut conn, instance_id, crate::now_millis()).await {
            Ok(blocked) => blocked,
            Err(error) => {
                tracing::warn!(%error, "the source blocklist could not be read");
                HashSet::new()
            }
        }
    }
}
