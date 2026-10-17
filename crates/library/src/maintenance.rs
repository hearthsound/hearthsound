//! Storage maintenance: keeping a managed file where its track's pins say.
//!
//! The two managed roots mean two different things. `<storage>/library` holds
//! the files of pinned tracks - what a user saved, liked, put in a playlist, or
//! that lives in a folder they own - and eviction never touches it.
//! `<storage>/cache` holds the files of unpinned tracks, which is everything
//! the server stored on its own initiative, and eviction may remove them. A pin
//! change therefore changes where the file belongs, and the maintainer is what
//! acts on that: it moves the file to the root its pins imply, demotes it when
//! the last pin goes away, or deletes it, depending on `storage.unpin_action`.
//!
//! This is the storage side of the retention design: [`crate::retention`] owns
//! the pins and the availability they imply, and this module makes the
//! filesystem and the `media_files` rows agree with them.
//!
//! A pass never touches a file inside a scanned folder. Those files are the
//! user's own, Hearthsound never writes into a folder, and a track whose only
//! file is a folder copy has nothing to move.
//!
//! # How a move is recorded
//!
//! The file moves first and the row follows, because a row that names a path
//! the file is not at is worse than a file the row does not name yet: the file
//! is moved into place, and one short write transaction then updates the row and
//! re-derives the track's availability. A transaction that fails moves the file
//! back, so a failed pass leaves the pair exactly as it was.
//!
//! # Examples
//!
//! ```no_run
//! # use std::sync::Arc;
//! # async fn example(
//! #     db: &hearthsound_db::Database,
//! #     roots: hearthsound_library::StorageRoots,
//! #     settings: hearthsound_config::settings::Settings,
//! #     events: Arc<hearthsound_core::events::EventBus>,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! use hearthsound_library::maintenance::StorageMaintainer;
//!
//! let maintainer = StorageMaintainer::new(db, roots, settings, events);
//! let report = maintainer.relocate().await?;
//! println!("{} files promoted", report.promoted);
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{EventBus, ServerEvent, TrackAvailability};
use hearthsound_core::{Availability, EntityId, IdPrefix, StorageClass};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{StorageRoots, decode_entity, decode_uuid};
use crate::{READ_THEN_WRITE, media_files, placement, retention, tracks};

/// What one relocation pass changed.
///
/// Every count is a file that reached its new home *and* had its row updated;
/// a file whose move failed is in none of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelocationReport {
    /// Files moved from the evictable root to the permanent one, because their
    /// track is pinned.
    pub promoted: u64,
    /// Files moved from the permanent root to the evictable one, because their
    /// track lost its last pin and `storage.unpin_action` is `demote`.
    pub demoted: u64,
    /// Files removed from the permanent root, with their rows, because their
    /// track lost its last pin and `storage.unpin_action` is `delete`.
    pub deleted: u64,
}

/// Keeps the managed files of the two storage roots in step with the pins.
///
/// One maintainer serves one process: it owns no per-call state, so a clone
/// shares everything with the value it was cloned from, and a run reads the
/// current settings snapshot rather than a copy taken at startup.
///
/// # Examples
///
/// ```no_run
/// # use std::sync::Arc;
/// # async fn example(
/// #     db: &hearthsound_db::Database,
/// #     roots: hearthsound_library::StorageRoots,
/// #     settings: hearthsound_config::settings::Settings,
/// #     events: Arc<hearthsound_core::events::EventBus>,
/// # ) {
/// use hearthsound_library::maintenance::StorageMaintainer;
///
/// let maintainer = StorageMaintainer::new(db, roots, settings, events);
/// # let _ = maintainer;
/// # }
/// ```
#[derive(Clone)]
pub struct StorageMaintainer {
    /// The writer pool every row change goes through.
    writer: SqlitePool,
    /// The reader pool every query goes through.
    readers: SqlitePool,
    /// The two managed roots a file moves between.
    roots: StorageRoots,
    /// The settings service; every run reads its current snapshot.
    settings: Settings,
    /// The bus every availability change is published on.
    events: Arc<EventBus>,
}

impl std::fmt::Debug for StorageMaintainer {
    /// Reports the type without the pools, the settings, or the bus.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StorageMaintainer")
    }
}

impl StorageMaintainer {
    /// Builds the maintainer.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database whose `media_files` and `track_retention` rows
    ///   are reconciled.
    /// - `roots`: the managed storage roots a file moves between.
    /// - `settings`: the settings service; a run reads its current snapshot, so
    ///   `storage.unpin_action` takes effect without a restart.
    /// - `events`: the bus a `track.availability` change is published on.
    ///
    /// # Returns
    ///
    /// The maintainer.
    pub fn new(
        db: &Database,
        roots: StorageRoots,
        settings: Settings,
        events: Arc<EventBus>,
    ) -> StorageMaintainer {
        StorageMaintainer {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            roots,
            settings,
            events,
        }
    }

    /// Returns the managed roots the maintainer moves files between.
    pub fn roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Moves every managed file to the root its track's pins imply.
    ///
    /// The pass reads the files whose storage and pins disagree in one query,
    /// then handles each of them: a file of a pinned track in the evictable
    /// root is moved to the permanent one, and a file of an unpinned track in
    /// the permanent root is moved to the evictable one or deleted, depending
    /// on `storage.unpin_action`. The path inside the root is kept; a name
    /// something else already holds moves to the next ` (N)` collision suffix.
    /// The directories a move emptied under a managed root are removed.
    ///
    /// A file whose move fails is logged and left where it is, so one
    /// unwritable file does not stop the rest of the pass; a failure of the row
    /// update puts the file back and ends the pass with that error, because
    /// every later file would meet the same one.
    ///
    /// # Returns
    ///
    /// What the pass changed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a query fails, when a destination's row
    /// cannot be written, or when a setting cannot be read.
    pub async fn relocate(&self) -> Result<RelocationReport, LibraryError> {
        let action = self.unpin_action()?;
        let mut taken = self.managed_paths().await?;
        let files = self.mismatched_files().await?;
        let mut report = RelocationReport::default();
        for file in files {
            match self.relocate_one(&file, action, &mut taken).await? {
                Relocated::Promoted => report.promoted += 1,
                Relocated::Demoted => report.demoted += 1,
                Relocated::Deleted => report.deleted += 1,
                Relocated::Skipped => {}
            }
        }
        if report != RelocationReport::default() {
            tracing::debug!(
                promoted = report.promoted,
                demoted = report.demoted,
                deleted = report.deleted,
                "relocated managed files"
            );
        }
        Ok(report)
    }

    /// Reads `storage.unpin_action`.
    ///
    /// # Returns
    ///
    /// What happens to a file whose track lost its last pin.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Setting`] when the setting cannot be read and
    /// [`LibraryError::UnknownLiteral`] when it holds another value.
    fn unpin_action(&self) -> Result<UnpinAction, LibraryError> {
        let stored = self.settings.current().get(keys::STORAGE_UNPIN_ACTION)?;
        match stored.as_str() {
            "demote" => Ok(UnpinAction::Demote),
            "delete" => Ok(UnpinAction::Delete),
            other => Err(LibraryError::UnknownLiteral {
                kind: "unpin action",
                value: other.to_owned(),
            }),
        }
    }

    /// Relocates one file.
    ///
    /// # Parameters
    ///
    /// - `file`: the file whose storage and pins disagree.
    /// - `action`: what an unpinned file gets.
    /// - `taken`: the destinations this pass already picked, which a later file
    ///   of the same pass skips.
    ///
    /// # Returns
    ///
    /// What happened to the file.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a row cannot be read or written, and
    /// [`LibraryError::NoFreeName`] when every name at the destination is taken.
    async fn relocate_one(
        &self,
        file: &ManagedFile,
        action: UnpinAction,
        taken: &mut HashSet<PathBuf>,
    ) -> Result<Relocated, LibraryError> {
        if file.pinned {
            let source = self.roots.cache.join(&file.rel_path);
            return self
                .move_file(
                    file,
                    &source,
                    StorageClass::Library,
                    taken,
                    Relocated::Promoted,
                )
                .await;
        }
        let source = self.roots.library.join(&file.rel_path);
        match action {
            UnpinAction::Demote => {
                self.move_file(
                    file,
                    &source,
                    StorageClass::Cache,
                    taken,
                    Relocated::Demoted,
                )
                .await
            }
            UnpinAction::Delete => self.delete_file(file, &source).await,
        }
    }

    /// Moves one managed file into the other root and records the new path.
    ///
    /// # Parameters
    ///
    /// - `file`: the file to move.
    /// - `source`: its absolute path in the root it is in now.
    /// - `storage`: the root it moves to.
    /// - `taken`: the destinations this pass already picked.
    /// - `relocated`: what to report when the move succeeded.
    ///
    /// # Returns
    ///
    /// `relocated` when the file is at its new home and its row is updated,
    /// [`Relocated::Skipped`] when the file itself could not be moved.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the destination cannot be picked, when the
    /// row cannot be written, and when a failure cannot be undone.
    async fn move_file(
        &self,
        file: &ManagedFile,
        source: &std::path::Path,
        storage: StorageClass,
        taken: &mut HashSet<PathBuf>,
        relocated: Relocated,
    ) -> Result<Relocated, LibraryError> {
        let base = self.roots.root_for(storage).join(&file.rel_path);
        let destination = placement::free_destination(&base, taken).await?;
        taken.insert(destination.clone());
        if let Err(error) = placement::move_file(source, &destination).await {
            tracing::warn!(
                track = %file.track_id,
                storage = storage.as_str(),
                %error,
                "could not move a managed file; leaving it where it is"
            );
            return Ok(Relocated::Skipped);
        }
        let rel_path = placement::relative_path(&self.roots, storage, &destination);
        let changed = match self.record_move(file, storage, &rel_path).await {
            Ok(changed) => changed,
            Err(error) => {
                if let Err(back) = placement::move_file(&destination, source).await {
                    tracing::error!(
                        track = %file.track_id,
                        %back,
                        "could not put a managed file back after a failed row update"
                    );
                }
                return Err(error);
            }
        };
        self.prune(source, file.storage).await;
        if let Some(availability) = changed {
            self.publish_availability(file.track_id, availability);
        }
        tracing::debug!(
            track = %file.track_id,
            storage = storage.as_str(),
            rel_path = rel_path.as_str(),
            "moved a managed file"
        );
        Ok(relocated)
    }

    /// Removes one managed file and its row.
    ///
    /// # Parameters
    ///
    /// - `file`: the file to remove.
    /// - `source`: its absolute path.
    ///
    /// # Returns
    ///
    /// [`Relocated::Deleted`] when the file and its row are gone,
    /// [`Relocated::Skipped`] when the file itself could not be removed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the row cannot be removed.
    async fn delete_file(
        &self,
        file: &ManagedFile,
        source: &std::path::Path,
    ) -> Result<Relocated, LibraryError> {
        match tokio::fs::remove_file(source).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    track = %file.track_id,
                    path = %source.display(),
                    %error,
                    "could not remove an unpinned managed file; leaving it where it is"
                );
                return Ok(Relocated::Skipped);
            }
        }
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let before = self.availability(&mut tx, file.track_id).await?;
        media_files::delete_media_file(&mut tx, file.id).await?;
        let after = self.settle_availability(&mut tx, file.track_id).await?;
        tx.commit().await?;
        self.prune(source, file.storage).await;
        if let (Some(before), Some(after)) = (before, after)
            && before != after
        {
            self.publish_availability(file.track_id, after);
        }
        tracing::debug!(
            track = %file.track_id,
            rel_path = file.rel_path.as_str(),
            "deleted an unpinned managed file"
        );
        Ok(Relocated::Deleted)
    }

    /// Updates the row of a moved file and re-derives its track's availability.
    ///
    /// # Parameters
    ///
    /// - `file`: the file that moved.
    /// - `storage`: the root it is in now.
    /// - `rel_path`: its path relative to that root.
    ///
    /// # Returns
    ///
    /// The availability the track now has, when it changed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the row cannot be updated.
    async fn record_move(
        &self,
        file: &ManagedFile,
        storage: StorageClass,
        rel_path: &str,
    ) -> Result<Option<Availability>, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let before = self.availability(&mut tx, file.track_id).await?;
        media_files::move_managed_file(&mut tx, file.id, storage, rel_path).await?;
        let after = self.settle_availability(&mut tx, file.track_id).await?;
        tx.commit().await?;
        match (before, after) {
            (Some(before), Some(after)) if before != after => Ok(Some(after)),
            _ => Ok(None),
        }
    }

    /// Removes the directories a moved file emptied under its old root.
    ///
    /// A directory that cannot be removed is logged rather than fatal: an
    /// artwork file or another track may have kept it, and the next pass tries
    /// again.
    ///
    /// # Parameters
    ///
    /// - `source`: the path the file lived at.
    /// - `storage`: the root that path was under.
    async fn prune(&self, source: &std::path::Path, storage: StorageClass) {
        let root = self.roots.root_for(storage);
        match placement::prune_empty_parents(source, root).await {
            Ok(removed) if !removed.is_empty() => {
                tracing::debug!(
                    directories = removed.len(),
                    "removed the directories a moved file emptied"
                );
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "could not prune the directories a moved file emptied");
            }
        }
    }

    /// Reads a track's availability.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to read on.
    /// - `track_id`: the track to read.
    ///
    /// # Returns
    ///
    /// The availability, or `None` when the track does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when the query fails.
    async fn availability(
        &self,
        conn: &mut sqlx::SqliteConnection,
        track_id: EntityId,
    ) -> Result<Option<Availability>, LibraryError> {
        Ok(tracks::get_track(conn, track_id)
            .await?
            .map(|track| track.availability))
    }

    /// Brings a track's availability in step with the files it has left.
    ///
    /// A track with no file at all is `remote` again - the acquisition in
    /// flight, if there is one, keeps its own state - and every other track
    /// takes the availability its preferred file and its pins imply.
    ///
    /// # Parameters
    ///
    /// - `conn`: the transaction to read and write on.
    /// - `track_id`: the track whose files changed.
    ///
    /// # Returns
    ///
    /// The availability the track has now, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a read or a write fails.
    async fn settle_availability(
        &self,
        conn: &mut sqlx::SqliteConnection,
        track_id: EntityId,
    ) -> Result<Option<Availability>, LibraryError> {
        let Some(track) = tracks::get_track(conn, track_id).await? else {
            return Ok(None);
        };
        let files = media_files::list_media_files_for_track(conn, track_id).await?;
        if files.is_empty() {
            if track.availability == Availability::Acquiring {
                return Ok(Some(track.availability));
            }
            tracks::set_track_availability(conn, track_id, Availability::Remote, None).await?;
            return Ok(Some(Availability::Remote));
        }
        retention::recompute_availability(conn, track_id).await
    }

    /// Reads the managed files whose storage and pins disagree.
    ///
    /// A `cache` file of a pinned track has to be promoted and a `library` file
    /// of an unpinned track has to be demoted or deleted; a track with no
    /// `track_retention` row counts as unpinned.
    ///
    /// # Returns
    ///
    /// The files to relocate, by storage class and path.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the query fails or a stored literal or id
    /// cannot be decoded.
    async fn mismatched_files(&self) -> Result<Vec<ManagedFile>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query!(
            "SELECT mf.id AS id, mf.track_id AS track_id, mf.storage AS storage, \
                    mf.rel_path AS rel_path, COALESCE(tr.pinned, 0) AS \"pinned!: i64\" \
             FROM media_files mf \
             LEFT JOIN track_retention tr ON tr.track_id = mf.track_id \
             WHERE (mf.storage = 'cache' AND COALESCE(tr.pinned, 0) = 1) \
                OR (mf.storage = 'library' AND COALESCE(tr.pinned, 0) = 0) \
             ORDER BY mf.storage, mf.rel_path, mf.id",
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ManagedFile {
                    id: decode_uuid(&row.id)?,
                    track_id: decode_entity(IdPrefix::Track, &row.track_id)?,
                    storage: storage_class(&row.storage)?,
                    rel_path: row.rel_path,
                    pinned: row.pinned != 0,
                })
            })
            .collect()
    }

    /// Reads the absolute paths of every managed file.
    ///
    /// The set is what a relocation offers its destinations against, so two
    /// files never end up on one path even when the file a row names is not on
    /// disk at the moment the destination is picked.
    ///
    /// # Returns
    ///
    /// The absolute path of every `library` and `cache` row.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the query fails or a stored literal cannot
    /// be decoded.
    async fn managed_paths(&self) -> Result<HashSet<PathBuf>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query!(
            "SELECT storage, rel_path FROM media_files WHERE storage IN ('library', 'cache')",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut paths = HashSet::with_capacity(rows.len());
        for row in rows {
            let storage = storage_class(&row.storage)?;
            paths.insert(self.roots.root_for(storage).join(&row.rel_path));
        }
        Ok(paths)
    }

    /// Publishes a track's new availability.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track that changed.
    /// - `availability`: its new availability.
    fn publish_availability(&self, track_id: EntityId, availability: Availability) {
        self.events
            .publish(ServerEvent::TrackAvailability(TrackAvailability {
                track_id: track_id.to_string(),
                availability,
                quality: None,
            }));
    }
}

/// One managed file whose storage class and pins disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagedFile {
    /// The `media_files` row id.
    id: Uuid,
    /// The track the file belongs to.
    track_id: EntityId,
    /// The root the file is in now.
    storage: StorageClass,
    /// The path relative to that root.
    rel_path: String,
    /// Whether the track has at least one pin reason.
    pinned: bool,
}

/// What relocating one file did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relocated {
    /// The file moved into the permanent root.
    Promoted,
    /// The file moved into the evictable root.
    Demoted,
    /// The file and its row were removed.
    Deleted,
    /// The file could not be moved or removed and stayed where it was.
    Skipped,
}

/// What happens to a managed file whose track lost its last pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnpinAction {
    /// The file moves to the evictable root.
    Demote,
    /// The file and its row are removed.
    Delete,
}

/// Decodes a stored storage-class literal.
///
/// # Parameters
///
/// - `stored`: the column value.
///
/// # Returns
///
/// The storage class.
///
/// # Errors
///
/// Returns [`LibraryError::UnknownLiteral`] when the value is not one of the
/// known classes.
fn storage_class(stored: &str) -> Result<StorageClass, LibraryError> {
    stored
        .parse::<StorageClass>()
        .map_err(|_| LibraryError::UnknownLiteral {
            kind: "storage class",
            value: stored.to_owned(),
        })
}
