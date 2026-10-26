//! The advisory lock a serving process holds on its data directory.
//!
//! Two processes must not migrate one database. The server applies the pending
//! migrations when it builds its services, and a command-line tool that would
//! migrate the same database has to know whether a server is running: a newer
//! binary moving the schema under a running server is exactly what the
//! forward-only migration rule forbids.
//!
//! The server takes `<data>/server.lock` for its lifetime. A command compares
//! the applied schema version with the version its binary bundles, and only when
//! it would migrate does it take the same lock: a command that needs no
//! migration takes no lock and keeps working beside a running server.
//!
//! The lock is an operating-system advisory lock, so it dies with the process
//! that holds it; a stale file left behind by a crash holds nothing.

use std::fs::File;
use std::fs::TryLockError;
use std::path::{Path, PathBuf};

use crate::error::ServerError;

/// The file name of the lock inside the data directory.
const LOCK_FILE: &str = "server.lock";

/// Returns the path of the lock file inside a data directory.
///
/// # Parameters
///
/// - `data`: the data directory.
pub fn lock_path(data: &Path) -> PathBuf {
    data.join(LOCK_FILE)
}

/// An advisory lock a serving process holds for its lifetime.
///
/// The lock is released when the value is dropped, and by the operating system
/// when the process dies.
#[derive(Debug)]
pub struct ServeLock {
    /// The open handle; closing it releases the lock.
    _file: File,
    /// The file the handle locked, for error messages.
    path: PathBuf,
}

impl ServeLock {
    /// Takes the lock, or reports that another process holds it.
    ///
    /// # Parameters
    ///
    /// - `data`: the data directory the lock file lives in.
    ///
    /// # Returns
    ///
    /// The held lock, or `None` when another process holds it.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::LockFile`] when the lock file cannot be opened or
    /// the operating system refuses the lock for a reason other than another
    /// holder.
    pub fn try_acquire(data: &Path) -> Result<Option<ServeLock>, ServerError> {
        let path = lock_path(data);
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| ServerError::LockFile {
                path: path.clone(),
                source,
            })?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ServeLock { _file: file, path })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(ServerError::LockFile { path, source }),
        }
    }

    /// Returns the path of the lock file this value holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_attempt_to_hold_the_lock_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let held = ServeLock::try_acquire(dir.path()).unwrap();
        let held = held.expect("the first attempt takes the lock");
        assert_eq!(held.path(), dir.path().join("server.lock"));

        let second = ServeLock::try_acquire(dir.path()).unwrap();
        assert!(
            second.is_none(),
            "a lock a process holds is not taken twice"
        );

        drop(held);
        let third = ServeLock::try_acquire(dir.path()).unwrap();
        assert!(third.is_some(), "the lock is free once its holder is gone");
    }
}
