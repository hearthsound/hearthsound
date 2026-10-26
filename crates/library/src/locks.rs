//! One lock per library folder, shared by every writer into that folder.
//!
//! A folder has two kinds of writer: the scanner, which indexes what is on
//! disk, and a save, which writes a user's file into the folder and records the
//! row that keeps the next scan from indexing it as a second track. Both have
//! to see each other: a scan that runs between a save's file write and its row
//! insert would index the file as a new track, and a save that runs while a scan
//! walks the same directory could write over a path the scan has already
//! recorded.
//!
//! The lock is keyed by the folder's row id, so every writer that was handed the
//! same [`FolderLocks`] serialises on the same folder and a writer in another
//! process still needs the database's own serialisation. A lock is never
//! removed once asked for, which keeps the map small: a library has one folder
//! per mounted directory, not one per scan.
//!
//! # Examples
//!
//! ```no_run
//! use hearthsound_library::locks::FolderLocks;
//! # async fn example(folder_id: uuid::Uuid) {
//! let locks = FolderLocks::new();
//! let held = locks.lock(folder_id).await;
//! // A scan of the folder waits here until `held` is dropped.
//! drop(held);
//! # }
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

/// The lock of every folder one process writes into.
///
/// Cloning is not provided on purpose: the instance is shared through an
/// [`Arc`], so two services that must not interleave hold the same map.
#[derive(Default)]
pub struct FolderLocks {
    /// One lock per folder that has been asked for, created on first use.
    locks: Mutex<HashMap<Uuid, Arc<AsyncMutex<()>>>>,
}

impl std::fmt::Debug for FolderLocks {
    /// Reports the type without its entries.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FolderLocks")
    }
}

impl FolderLocks {
    /// Builds a set with no locks taken.
    ///
    /// # Returns
    ///
    /// The lock set.
    pub fn new() -> FolderLocks {
        FolderLocks::default()
    }

    /// Waits for the folder's lock and returns the guard.
    ///
    /// The guard is owned, so the caller can hold it across an `await` and drop
    /// it wherever the write finishes; the lock itself stays in the set, which
    /// is why two callers that ask for the same folder share one lock.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder whose lock is taken.
    ///
    /// # Returns
    ///
    /// The held lock. Every other writer of the same folder waits until it is
    /// dropped.
    pub async fn lock(&self, folder_id: Uuid) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(locks.entry(folder_id).or_default())
        };
        lock.lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_writers_of_one_folder_serialise_and_other_folders_do_not() {
        let locks = Arc::new(FolderLocks::new());
        let folder = Uuid::now_v7();
        let other = Uuid::now_v7();

        let held = locks.lock(folder).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), locks.lock(folder))
                .await
                .is_err(),
            "a second writer of the same folder waits"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), locks.lock(other))
                .await
                .is_ok(),
            "another folder is a different lock"
        );
        drop(held);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), locks.lock(folder))
                .await
                .is_ok(),
            "the lock is free once the guard is dropped"
        );
    }
}
