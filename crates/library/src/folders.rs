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

/// The server's own files and directories.
///
/// A library folder may hold nothing but music, so a path that is, contains, or
/// sits inside one of these is refused: scanning the data directory would index
/// the database, the thumbnails and the plugin files, and a folder around it
/// would index them on every scan.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use hearthsound_library::folders::ServerPaths;
///
/// let paths = ServerPaths {
///     data: PathBuf::from("/var/lib/hearthsound"),
///     storage: PathBuf::from("/srv/music"),
///     database: PathBuf::from("/var/lib/hearthsound/hearthsound.db"),
/// };
/// assert_eq!(paths.database, PathBuf::from("/var/lib/hearthsound/hearthsound.db"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerPaths {
    /// `paths.data`: the database, artwork, thumbnails and staging.
    pub data: PathBuf,
    /// `paths.storage`: the managed library and cache roots.
    pub storage: PathBuf,
    /// `database.path`: the SQLite file.
    pub database: PathBuf,
}

/// Why a path cannot be a library folder.
///
/// The [`Display`](std::fmt::Display) text is the sentence the API answers as
/// its problem detail and the CLI prints, so an administrator reads the same
/// explanation wherever the path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderPathRejection {
    /// The path is `/`, which would scan the whole filesystem.
    Root,
    /// The path is the server's own directory or database file.
    Equals {
        /// The refused path.
        path: PathBuf,
        /// What the path is, for example `data directory`.
        label: &'static str,
    },
    /// The path contains the server's own directory or database file, so a scan
    /// would descend into it.
    Contains {
        /// The refused path.
        path: PathBuf,
        /// The protected path it contains.
        protected: PathBuf,
        /// What the protected path is, for example `database file`.
        label: &'static str,
    },
    /// The path is inside the server's own directory, so the scan would index
    /// the server's files.
    Inside {
        /// The refused path.
        path: PathBuf,
        /// The protected path it sits in.
        protected: PathBuf,
        /// What the protected path is, for example `storage directory`.
        label: &'static str,
    },
}

impl std::fmt::Display for FolderPathRejection {
    /// Writes the sentence the API and the CLI show.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FolderPathRejection::Root => f.write_str(
                "`/` cannot be a library folder: it would scan the whole server filesystem. \
                 Choose the directory that holds your music.",
            ),
            FolderPathRejection::Equals { path, label } => write!(
                f,
                "`{path}` is the server's own {label}; it cannot be a library folder.",
                path = path.display(),
            ),
            FolderPathRejection::Contains {
                path,
                protected,
                label,
            } => write!(
                f,
                "`{path}` contains `{protected}`, which the server uses for its own {label}. \
                 Choose a directory that does not contain it.",
                path = path.display(),
                protected = protected.display(),
            ),
            FolderPathRejection::Inside {
                path,
                protected,
                label,
            } => write!(
                f,
                "`{path}` is inside `{protected}`, which the server uses for its own {label}. \
                 Choose a directory outside it.",
                path = path.display(),
                protected = protected.display(),
            ),
        }
    }
}

impl std::error::Error for FolderPathRejection {}

/// Checks that a path may be a library folder.
///
/// The path and each of the server's own paths are canonicalized first, so a
/// symlink to the data directory is refused like the directory itself. A path
/// that cannot be canonicalized is checked as it was given; a caller that needs
/// an existing directory has already canonicalized it.
///
/// # Parameters
///
/// - `path`: the directory to check.
/// - `server`: the server's own files and directories.
///
/// # Returns
///
/// The canonical path to store, which is `path` with every symlink resolved.
///
/// # Errors
///
/// Returns the [`FolderPathRejection`] that says why the path cannot be used.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use hearthsound_library::folders::{FolderPathRejection, ServerPaths, check_folder_path};
///
/// let server = ServerPaths {
///     data: PathBuf::from("/var/lib/hearthsound"),
///     storage: PathBuf::from("/srv/music"),
///     database: PathBuf::from("/var/lib/hearthsound/hearthsound.db"),
/// };
/// assert_eq!(
///     check_folder_path(PathBuf::from("/").as_path(), &server),
///     Err(FolderPathRejection::Root)
/// );
/// ```
pub fn check_folder_path(
    path: &Path,
    server: &ServerPaths,
) -> Result<PathBuf, FolderPathRejection> {
    let canonical = canonical_or_same(path);
    if canonical.parent().is_none() {
        return Err(FolderPathRejection::Root);
    }
    let protected = [
        (server.data.as_path(), "data directory"),
        (server.storage.as_path(), "storage directory"),
        (server.database.as_path(), "database file"),
    ];
    let protected: Vec<(PathBuf, &'static str)> = protected
        .iter()
        .map(|(path, label)| (canonical_or_same(path), *label))
        .collect();
    for (path_of_server, label) in &protected {
        if canonical == *path_of_server {
            return Err(FolderPathRejection::Equals {
                path: canonical,
                label,
            });
        }
    }
    for (path_of_server, label) in &protected {
        if path_of_server.starts_with(&canonical) {
            return Err(FolderPathRejection::Contains {
                path: canonical,
                protected: path_of_server.clone(),
                label,
            });
        }
    }
    for (path_of_server, label) in &protected {
        if canonical.starts_with(path_of_server) {
            return Err(FolderPathRejection::Inside {
                path: canonical,
                protected: path_of_server.clone(),
                label,
            });
        }
    }
    Ok(canonical)
}

/// Returns a path with its symlinks resolved, or the path itself when it cannot
/// be resolved.
///
/// # Parameters
///
/// - `path`: the path to canonicalize.
///
/// # Returns
///
/// The canonical path, or `path` unchanged.
fn canonical_or_same(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Returns the library folders strictly inside another one.
///
/// Overlapping folders are allowed: a file belongs to the deepest folder that
/// contains it, so an outer folder's scan skips the roots of the folders inside
/// it and a file is indexed exactly once. Stored folder paths are canonical, so
/// a plain component-wise comparison is enough.
///
/// # Parameters
///
/// - `folders`: every library folder.
/// - `folder`: the folder whose scan is about to run.
///
/// # Returns
///
/// The canonical paths of the other folders inside `folder`, sorted.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use hearthsound_library::LibraryFolder;
/// use hearthsound_library::folders::nested_roots;
/// use uuid::Uuid;
///
/// let folder = |id: u128, path: &str| LibraryFolder {
///     id: Uuid::from_u128(id),
///     path: PathBuf::from(path),
///     name: path.to_owned(),
///     owner_user_id: None,
///     watch: true,
///     created_at: 0,
///     last_scan_at: None,
///     last_scan_error: None,
/// };
/// let folders = vec![folder(1, "/music"), folder(2, "/music/Alex G")];
/// assert_eq!(
///     nested_roots(&folders, &folders[0]),
///     vec![PathBuf::from("/music/Alex G")]
/// );
/// ```
pub fn nested_roots(folders: &[LibraryFolder], folder: &LibraryFolder) -> Vec<PathBuf> {
    let mut nested: Vec<PathBuf> = folders
        .iter()
        .filter(|other| other.id != folder.id)
        .filter(|other| other.path != folder.path)
        .filter(|other| other.path.starts_with(&folder.path))
        .map(|other| other.path.clone())
        .collect();
    nested.sort();
    nested.dedup();
    nested
}

/// Returns the library folders that strictly contain another one.
///
/// Overlapping folders are allowed: a file belongs to the deepest folder that
/// contains it, so the folder inside another one takes over the rows the
/// enclosing folder holds for the files it indexes, and the enclosing folder's
/// scan leaves those rows alone. Stored folder paths are canonical, so a plain
/// component-wise comparison is enough.
///
/// # Parameters
///
/// - `folders`: every library folder.
/// - `folder`: the folder whose scan is about to run.
///
/// # Returns
///
/// The other folders that contain `folder`, the deepest first, so the nearest
/// enclosing folder is the first candidate for a file's row.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use hearthsound_library::LibraryFolder;
/// use hearthsound_library::folders::enclosing_folders;
/// use uuid::Uuid;
///
/// let folder = |id: u128, path: &str| LibraryFolder {
///     id: Uuid::from_u128(id),
///     path: PathBuf::from(path),
///     name: path.to_owned(),
///     owner_user_id: None,
///     watch: true,
///     created_at: 0,
///     last_scan_at: None,
///     last_scan_error: None,
/// };
/// let folders = vec![
///     folder(1, "/music"),
///     folder(2, "/music/Alex G"),
///     folder(3, "/music/Alex G/live"),
///     folder(4, "/other"),
/// ];
/// let enclosing = enclosing_folders(&folders, &folders[2]);
/// assert_eq!(
///     enclosing
///         .iter()
///         .map(|folder| folder.path.display().to_string())
///         .collect::<Vec<_>>(),
///     vec!["/music/Alex G".to_owned(), "/music".to_owned()]
/// );
/// ```
pub fn enclosing_folders(folders: &[LibraryFolder], folder: &LibraryFolder) -> Vec<LibraryFolder> {
    let mut enclosing: Vec<LibraryFolder> = folders
        .iter()
        .filter(|other| other.id != folder.id)
        .filter(|other| other.path != folder.path)
        .filter(|other| folder.path.starts_with(&other.path))
        .cloned()
        .collect();
    enclosing.sort_by(|left, right| {
        right
            .path
            .components()
            .count()
            .cmp(&left.path.components().count())
            .then_with(|| left.path.cmp(&right.path))
    });
    enclosing.dedup_by(|left, right| left.id == right.id);
    enclosing
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

/// Reads a folder when it exists and belongs to `owner`.
///
/// A save location is per account, so the only folder it may name is one that
/// account owns: a folder owned by someone else, or a generic folder, never
/// receives a save.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `owner`: the account the folder has to belong to.
/// - `folder_id`: the folder to read.
///
/// # Returns
///
/// The folder, or `None` when no row has that id or its owner is someone else.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails and
/// [`LibraryError::CorruptId`] when a stored id is not 16 bytes.
pub async fn owned_folder(
    conn: &mut SqliteConnection,
    owner: Uuid,
    folder_id: Uuid,
) -> Result<Option<LibraryFolder>, LibraryError> {
    let row = sqlx::query_as!(
        FolderRow,
        "SELECT id, path, name, owner_user_id, watch, created_at, last_scan_at, \
                last_scan_error \
         FROM library_folders WHERE id = ? AND owner_user_id = ?",
        folder_id.as_bytes().as_slice(),
        owner.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(FolderRow::into_folder).transpose()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The server's own paths, rooted in a temporary directory.
    fn server(dir: &Path) -> ServerPaths {
        ServerPaths {
            data: dir.join("data"),
            storage: dir.join("storage"),
            database: dir.join("data/hearthsound.db"),
        }
    }

    #[test]
    fn the_filesystem_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let rejection = check_folder_path(Path::new("/"), &server(dir.path())).unwrap_err();
        assert_eq!(rejection, FolderPathRejection::Root);
        assert_eq!(
            rejection.to_string(),
            "`/` cannot be a library folder: it would scan the whole server filesystem. Choose the \
             directory that holds your music."
        );
    }

    #[test]
    fn the_servers_own_paths_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path());
        std::fs::create_dir_all(&server.storage).unwrap();
        std::fs::create_dir_all(&server.data).unwrap();
        std::fs::write(&server.database, b"db").unwrap();

        for (path, label) in [
            (server.data.clone(), "data directory"),
            (server.storage.clone(), "storage directory"),
            (server.database.clone(), "database file"),
        ] {
            let rejection = check_folder_path(&path, &server).unwrap_err();
            assert_eq!(
                rejection,
                FolderPathRejection::Equals {
                    path: path.canonicalize().unwrap(),
                    label
                }
            );
        }
    }

    #[test]
    fn a_directory_around_a_server_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path());
        std::fs::create_dir_all(&server.storage).unwrap();
        std::fs::create_dir_all(&server.data).unwrap();
        std::fs::write(&server.database, b"db").unwrap();

        let rejection = check_folder_path(&server.data, &server).unwrap_err();
        assert!(matches!(rejection, FolderPathRejection::Equals { .. }));

        let rejection = check_folder_path(dir.path(), &server).unwrap_err();
        match &rejection {
            FolderPathRejection::Contains {
                protected, label, ..
            } => {
                assert_eq!(*protected, server.data.canonicalize().unwrap());
                assert_eq!(*label, "data directory");
            }
            other => panic!("expected a containing path, got {other:?}"),
        }
        let message = rejection.to_string();
        assert!(message.contains("contains"), "{message}");
        assert!(message.contains("data directory"), "{message}");
    }

    #[test]
    fn a_directory_inside_a_server_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path());
        std::fs::create_dir_all(server.storage.join("library")).unwrap();

        let rejection = check_folder_path(&server.storage.join("library"), &server).unwrap_err();
        match &rejection {
            FolderPathRejection::Inside {
                protected, label, ..
            } => {
                assert_eq!(*protected, server.storage.canonicalize().unwrap());
                assert_eq!(*label, "storage directory");
            }
            other => panic!("expected an inner path, got {other:?}"),
        }
        assert!(rejection.to_string().contains("is inside"));
    }

    #[test]
    fn a_music_directory_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path());
        std::fs::create_dir_all(&server.storage).unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir(&music).unwrap();

        let accepted = check_folder_path(&music, &server).unwrap();
        assert_eq!(accepted, music.canonicalize().unwrap());
    }

    #[test]
    fn a_symlink_to_a_server_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let server = server(dir.path());
        std::fs::create_dir_all(&server.data).unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir(&music).unwrap();
        std::os::unix::fs::symlink(&server.data, music.join("link")).unwrap();

        let rejection = check_folder_path(&music.join("link"), &server).unwrap_err();
        assert!(matches!(rejection, FolderPathRejection::Equals { .. }));
    }
}
