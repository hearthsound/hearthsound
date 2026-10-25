//! Reading and writing the `library_folders` table.
//!
//! A library folder is a directory the admin added to the scan set. Its `path`
//! is canonicalized when it is added, so the unique index on the path merges two
//! spellings of the same directory, and its `owner_user_id` is the association
//! the first-run prompt asks about: a non-NULL owner puts the folder's content
//! in that user's personal library and pins it with the `folder` reason, while a
//! NULL owner leaves it generic and adoptable by anyone.
//!
//! Every function takes a [`sqlx::SqliteConnection`] so a caller can run it
//! inside its own transaction; the pool-level convenience is on
//! [`crate::LibraryRepository`].

use std::path::{Path, PathBuf};

use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{LibraryFolder, decode_uuid};
use crate::now_millis;

/// One row of the `library_folders` table as it is stored.
#[derive(sqlx::FromRow)]
struct FolderRow {
    id: Vec<u8>,
    path: String,
    name: String,
    owner_user_id: Option<Vec<u8>>,
    watch: i64,
    created_at: i64,
    last_scan_at: Option<i64>,
    last_scan_error: Option<String>,
}

impl FolderRow {
    /// Converts a stored row into a library folder.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::CorruptId`] when an id is not 16 bytes.
    fn into_folder(self) -> Result<LibraryFolder, LibraryError> {
        Ok(LibraryFolder {
            id: decode_uuid(&self.id)?,
            path: PathBuf::from(self.path),
            name: self.name,
            owner_user_id: self.owner_user_id.as_deref().map(decode_uuid).transpose()?,
            watch: self.watch != 0,
            created_at: self.created_at,
            last_scan_at: self.last_scan_at,
            last_scan_error: self.last_scan_error,
        })
    }
}

/// Adds a folder to the scan set.
///
/// The path is canonicalized, so a symlinked or relative spelling resolves to
/// the same directory and a second add of the same directory returns the
/// existing row unchanged. The folder's display name is the directory's file
/// name.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `path`: the directory to add; it must exist and be a directory.
/// - `owner_user_id`: the user whose personal library the content belongs to,
///   or `None` for a generic folder.
///
/// # Returns
///
/// The stored folder. When a folder with the same canonical path already exists
/// it is returned as it is, and `owner_user_id` is not applied; use
/// [`set_library_folder_owner`] for that.
///
/// # Errors
///
/// Returns [`LibraryError::FolderPath`] when the path cannot be canonicalized or
/// is not a directory, and [`LibraryError::Database`] when the insert fails.
pub async fn add_library_folder(
    conn: &mut SqliteConnection,
    path: &Path,
    owner_user_id: Option<Uuid>,
) -> Result<LibraryFolder, LibraryError> {
    let canonical = path
        .canonicalize()
        .map_err(|source| LibraryError::FolderPath {
            path: path.to_path_buf(),
            source,
        })?;
    if !canonical.is_dir() {
        return Err(LibraryError::FolderPath {
            path: canonical,
            source: std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "library folder is not a directory",
            ),
        });
    }
    let stored_path = canonical.to_string_lossy().into_owned();

    if let Some(existing) = find_library_folder_by_path(conn, &stored_path).await? {
        return Ok(existing);
    }

    let name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| stored_path.clone());
    let id = Uuid::now_v7();
    let now = now_millis();
    sqlx::query!(
        "INSERT INTO library_folders (id, path, name, owner_user_id, watch, created_at) \
         VALUES (?, ?, ?, ?, 1, ?)",
        id.as_bytes().as_slice(),
        stored_path,
        name,
        owner_user_id.map(|user| user.as_bytes().to_vec()),
        now,
    )
    .execute(&mut *conn)
    .await?;

    Ok(LibraryFolder {
        id,
        path: canonical,
        name,
        owner_user_id,
        watch: true,
        created_at: now,
        last_scan_at: None,
        last_scan_error: None,
    })
}

/// Reads a folder by its id.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `id`: the folder's id.
///
/// # Returns
///
/// The folder, or `None` when no row has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn get_library_folder(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<Option<LibraryFolder>, LibraryError> {
    let row = sqlx::query_as!(
        FolderRow,
        "SELECT id, path, name, owner_user_id, watch, created_at, last_scan_at, \
                last_scan_error \
         FROM library_folders WHERE id = ?",
        id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(FolderRow::into_folder).transpose()
}

/// Reads a folder by its canonical path.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `path`: the canonical path text, exactly as it is stored.
///
/// # Returns
///
/// The folder, or `None` when no row has that path.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn find_library_folder_by_path(
    conn: &mut SqliteConnection,
    path: &str,
) -> Result<Option<LibraryFolder>, LibraryError> {
    let row = sqlx::query_as!(
        FolderRow,
        "SELECT id, path, name, owner_user_id, watch, created_at, last_scan_at, \
                last_scan_error \
         FROM library_folders WHERE path = ?",
        path,
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(FolderRow::into_folder).transpose()
}

/// Lists every library folder, ordered by name.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// Every folder, ordered by name with the id as a tie breaker.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn list_library_folders(
    conn: &mut SqliteConnection,
) -> Result<Vec<LibraryFolder>, LibraryError> {
    let rows = sqlx::query_as!(
        FolderRow,
        "SELECT id, path, name, owner_user_id, watch, created_at, last_scan_at, \
                last_scan_error \
         FROM library_folders ORDER BY name, id",
    )
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(FolderRow::into_folder).collect()
}

/// Renames a library folder.
///
/// The name is the folder's display name only; the scanned path never changes.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the folder to rename.
/// - `name`: the new display name.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the folder does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn rename_library_folder(
    conn: &mut SqliteConnection,
    id: Uuid,
    name: &str,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE library_folders SET name = ? WHERE id = ?",
        name,
        id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Deletes a library folder row.
///
/// The row's file records and any track only it held are removed by the caller
/// in the same transaction, so no media file is left pointing at a folder that
/// no longer exists. Nothing inside the folder on disk is touched.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the folder's UUIDv7 primary key.
///
/// # Returns
///
/// `true` when a row was deleted, `false` when no folder has that id.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the write fails.
pub async fn delete_library_folder(
    conn: &mut SqliteConnection,
    id: Uuid,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "DELETE FROM library_folders WHERE id = ?",
        id.as_bytes().as_slice()
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Associates a library folder with a user, or makes it generic.
///
/// This writes the ownership column only. The `folder` pin reason and the
/// availability of the folder's tracks follow the column, and are re-derived by
/// [`crate::retention`] in the same transaction.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the folder to change.
/// - `owner_user_id`: the new owner, or `None` to make the folder generic.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the folder does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn set_library_folder_owner(
    conn: &mut SqliteConnection,
    id: Uuid,
    owner_user_id: Option<Uuid>,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE library_folders SET owner_user_id = ? WHERE id = ?",
        owner_user_id.map(|user| user.as_bytes().to_vec()),
        id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Records whether a folder is watched for changes.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the folder to update.
/// - `watch`: whether a watcher should watch the folder.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the folder does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn set_library_folder_watch(
    conn: &mut SqliteConnection,
    id: Uuid,
    watch: bool,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE library_folders SET watch = ? WHERE id = ?",
        watch,
        id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// Records the outcome of a folder's last scan.
///
/// One statement, so the scan instant and the problem never disagree: a scan
/// that finished writes both, a scan that failed writes the message and leaves
/// the instant alone, and a clean scan clears the message.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `id`: the folder that was scanned.
/// - `at`: when the scan finished, in unix milliseconds, or `None` to leave the
///   stored instant as it is.
/// - `error`: why the scan failed or ended with a warning, or `None` for a clean
///   scan.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the folder does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn set_library_folder_scan_outcome(
    conn: &mut SqliteConnection,
    id: Uuid,
    at: Option<i64>,
    error: Option<&str>,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE library_folders SET last_scan_at = COALESCE(?, last_scan_at), \
                last_scan_error = ? WHERE id = ?",
        at,
        error,
        id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}
