//! Placing managed files under a managed storage root.
//!
//! A managed file lives at a path relative to one of the two managed roots, and
//! two files never share one path: `media_files` enforces that with a unique
//! index on `(storage, rel_path)`. Every writer that puts a file under a root -
//! the acquisition import and the storage maintainer - therefore has to offer
//! the rendered name first and walk to a ` (N)` collision suffix when something
//! already holds it, and this module is that one implementation.
//!
//! The module also holds the two filesystem primitives a relocation needs: a
//! move that is atomic inside one filesystem and durable across two, and the
//! pruning of the directories a move emptied. Both are blocking work and run on
//! a blocking thread, so a maintainer pass never stalls the runtime.
//!
//! Nothing here touches the database: a caller picks a destination, places the
//! file, and then records the new path in a short write transaction.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use hearthsound_core::StorageClass;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::StorageRoots;

/// How many collision suffixes a placement tries before it gives up.
///
/// The rendered name is the first candidate, then ` (2)`, ` (3)`, and so on up
/// to this many, which is more names than any one directory of a music library
/// can hold.
pub const MAX_COLLISIONS: u32 = 10_000;

/// Returns whether a path exists.
///
/// # Parameters
///
/// - `path`: the path to look at.
///
/// # Returns
///
/// `true` when something exists at the path, `false` when nothing does.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] for any filesystem error other than "not
/// found".
pub async fn path_exists(path: &Path) -> Result<bool, LibraryError> {
    match tokio::fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(path, source)),
    }
}

/// Returns the first free name for a rendered destination.
///
/// The rendered name is offered first, then its ` (N)` collision suffixes. A
/// name counts as held when a file already exists at it or when the caller has
/// already lost it, which is what keeps a retry from picking the same name
/// again.
///
/// # Parameters
///
/// - `destination`: the absolute path a naming template rendered, or the path a
///   file already lives at.
/// - `taken`: the names the caller has already lost; they are skipped even when
///   nothing is there.
///
/// # Returns
///
/// The free path.
///
/// # Errors
///
/// Returns [`LibraryError::NoFreeName`] when every suffix is taken, and
/// [`LibraryError::Io`] when a path cannot be stat-ed.
pub async fn free_destination(
    destination: &Path,
    taken: &HashSet<PathBuf>,
) -> Result<PathBuf, LibraryError> {
    if !taken.contains(destination) && !path_exists(destination).await? {
        return Ok(destination.to_path_buf());
    }
    let parent = destination
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let stem = destination
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("file")
        .to_owned();
    let extension = destination
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| format!(".{extension}"))
        .unwrap_or_default();
    for suffix in 2..=MAX_COLLISIONS {
        let candidate = parent.join(format!("{stem} ({suffix}){extension}"));
        if !taken.contains(&candidate) && !path_exists(&candidate).await? {
            return Ok(candidate);
        }
    }
    Err(LibraryError::NoFreeName {
        path: destination.to_path_buf(),
    })
}

/// Returns the part of a path that is relative to its storage root.
///
/// # Parameters
///
/// - `roots`: the managed storage roots.
/// - `storage`: the root the path is expected to be under.
/// - `path`: the absolute path.
///
/// # Returns
///
/// The path relative to the root, as text, or the whole path when it is not
/// under the root.
pub fn relative_path(roots: &StorageRoots, storage: StorageClass, path: &Path) -> String {
    path.strip_prefix(roots.root_for(storage))
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Moves a file to a destination the caller has picked free.
///
/// Inside one filesystem the move is a `rename`, which is atomic: a reader sees
/// either the old or the new name, never a partial file. Across two filesystems,
/// which two managed roots on separate mounts are, the bytes are copied to a
/// temporary file beside the destination, flushed to disk, and renamed into
/// place, so a crash never exposes a partial file at the destination; the source
/// is removed only after that rename succeeded. The destination's parent
/// directories are created first.
///
/// # Parameters
///
/// - `source`: the file that moves.
/// - `destination`: the path it moves to, which the caller has picked free.
///
/// # Returns
///
/// `()` once the file is at the destination and the source is gone.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] when a directory cannot be created, the file
/// cannot be copied, the copy cannot be flushed, the rename fails, or the
/// source cannot be removed, and [`LibraryError::Join`] when the blocking task
/// cannot be joined.
pub async fn move_file(source: &Path, destination: &Path) -> Result<(), LibraryError> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || move_file_blocking(&source, &destination)).await?
}

/// Moves one file, on a blocking thread.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] for the step that failed.
fn move_file_blocking(source: &Path, destination: &Path) -> Result<(), LibraryError> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
    }
    match std::fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
        Err(error) => return Err(io_error(destination, error)),
    }
    copy_durable_blocking(source, destination)?;
    std::fs::remove_file(source).map_err(|error| io_error(source, error))
}

/// Copies a file durably to a destination on another filesystem.
///
/// The bytes go to a temporary file beside the destination, are flushed to
/// disk, and are renamed into place.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] when a step fails.
fn copy_durable_blocking(source: &Path, destination: &Path) -> Result<(), LibraryError> {
    let temporary = match destination.parent() {
        Some(parent) => parent.join(format!(".{}.tmp", Uuid::now_v7())),
        None => destination.with_extension("tmp"),
    };
    std::fs::copy(source, &temporary).map_err(|error| io_error(&temporary, error))?;
    let file = std::fs::File::open(&temporary).map_err(|error| io_error(&temporary, error))?;
    file.sync_all()
        .map_err(|error| io_error(&temporary, error))?;
    drop(file);
    std::fs::rename(&temporary, destination).map_err(|error| io_error(destination, error))
}

/// Removes the directories a move emptied, deepest first.
///
/// A managed library keeps one directory tree per artist and album under each
/// root, and a move out of a leaf can leave a whole chain of empty directories
/// behind. The walk stops at the first directory that is not empty, at the root
/// itself, and at any path outside the root, so the two managed roots are never
/// removed and nothing outside them is ever touched.
///
/// # Parameters
///
/// - `path`: the path the file lived at, which has already moved away.
/// - `root`: the managed root the path belongs to.
///
/// # Returns
///
/// The directories that were removed, deepest first.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] when a directory exists, is inside the root, and
/// cannot be removed for a reason other than being non-empty, and
/// [`LibraryError::Join`] when the blocking task cannot be joined.
pub async fn prune_empty_parents(path: &Path, root: &Path) -> Result<Vec<PathBuf>, LibraryError> {
    let path = path.to_path_buf();
    let root = root.to_path_buf();
    tokio::task::spawn_blocking(move || prune_empty_parents_blocking(&path, &root)).await?
}

/// Removes the directories a move emptied, on a blocking thread.
///
/// # Errors
///
/// Returns [`LibraryError::Io`] for a directory that cannot be removed.
fn prune_empty_parents_blocking(path: &Path, root: &Path) -> Result<Vec<PathBuf>, LibraryError> {
    let mut removed = Vec::new();
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root || !directory.starts_with(root) {
            break;
        }
        match std::fs::remove_dir(directory) {
            Ok(()) => removed.push(directory.to_path_buf()),
            // A directory that is not empty is where the walk stops: the
            // directories above it still hold something.
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(directory, error)),
        }
        current = directory.parent();
    }
    Ok(removed)
}

/// Wraps a filesystem error with the path it happened on.
///
/// # Parameters
///
/// - `path`: the path the operation failed on.
/// - `source`: the error the filesystem reported.
///
/// # Returns
///
/// The library error for it.
fn io_error(path: &Path, source: std::io::Error) -> LibraryError {
    LibraryError::Io {
        path: path.to_path_buf(),
        source,
    }
}
