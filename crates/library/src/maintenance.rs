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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{EventBus, ServerEvent, TrackAvailability};
use hearthsound_core::{Availability, EntityId, IdPrefix, PinReason, StorageClass};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler, Shutdown};
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

/// What one play-count pin pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutopinReport {
    /// Tracks that reached `storage.autopin.play_count` and gained the
    /// `policy` pin.
    pub pinned: u64,
    /// Tracks that lost the `policy` pin because they are below the threshold
    /// or the threshold is zero.
    pub released: u64,
}

/// What one eviction pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// How many cache files were removed.
    pub files: u64,
    /// How many bytes those files held.
    pub bytes: i64,
}

/// The files and bytes of one storage class.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassUsage {
    /// How many files the class holds.
    pub files: u64,
    /// How many bytes those files hold.
    pub bytes: i64,
}

/// The size and free space of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemSpace {
    /// The filesystem's total size in bytes.
    pub total_bytes: i64,
    /// The bytes an unprivileged process may still write.
    pub available_bytes: i64,
}

/// What the managed roots and the scanned folders hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageUsage {
    /// The managed permanent files under `<storage>/library`.
    pub library: ClassUsage,
    /// The managed evictable files under `<storage>/cache`.
    pub cache: ClassUsage,
    /// The files inside scanned folders.
    pub folders: ClassUsage,
    /// How many tracks carry at least one pin reason.
    pub pinned_tracks: u64,
    /// The filesystem under the cache root, when its space can be read.
    pub filesystem: Option<FilesystemSpace>,
}

/// Reads how much space a filesystem has.
///
/// The maintainer asks it about the cache root before an eviction pass. The
/// default, [`StatvfsProbe`], asks the operating system; tests supply their own.
pub trait SpaceProbe: Send + Sync {
    /// Returns the space of the filesystem a path is on.
    ///
    /// # Parameters
    ///
    /// - `path`: a path on the filesystem.
    ///
    /// # Returns
    ///
    /// The space, or `None` when it cannot be read.
    fn space(&self, path: &Path) -> Option<FilesystemSpace>;
}

/// Reads filesystem space with `statvfs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct StatvfsProbe;

impl SpaceProbe for StatvfsProbe {
    /// Returns the space `statvfs` reports, or `None` when the call fails or a
    /// value does not fit.
    fn space(&self, path: &Path) -> Option<FilesystemSpace> {
        let stat = rustix::fs::statvfs(path).ok()?;
        let block = stat.f_frsize;
        let total = stat.f_blocks.checked_mul(block)?;
        let available = stat.f_bavail.checked_mul(block)?;
        Some(FilesystemSpace {
            total_bytes: i64::try_from(total).ok()?,
            available_bytes: i64::try_from(available).ok()?,
        })
    }
}

/// The job kind of a scheduled maintenance pass.
pub const MAINTENANCE_JOB_KIND: &str = "storage.maintain";

/// The cron expression of the scheduled maintenance pass: every 15 minutes.
///
/// The pass is a safety net behind the wake-ups, which handle a pin change or
/// an import within a second.
pub const MAINTENANCE_SCHEDULE: &str = "*/15 * * * *";

/// How long the worker waits after a wake-up before it runs, so a burst of
/// changes (an album liked track by track) becomes one pass.
const WAKE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// A wake-up call for the maintenance worker.
///
/// Whatever changes a pin, records a play, or adds a cache file calls
/// [`MaintenanceSignal::notify`]; the worker waiting in
/// [`StorageMaintainer::run`] wakes, waits briefly so related changes arrive
/// together, and runs one pass. A notification while nobody waits is kept, so
/// a change made during a pass causes one more pass afterwards. Clones share
/// one signal.
///
/// # Examples
///
/// ```
/// use hearthsound_library::maintenance::MaintenanceSignal;
///
/// let signal = MaintenanceSignal::new();
/// signal.clone().notify();
/// ```
#[derive(Debug, Clone, Default)]
pub struct MaintenanceSignal {
    /// The notification the worker waits on.
    notify: Arc<tokio::sync::Notify>,
}

impl MaintenanceSignal {
    /// Builds a signal nobody waits on yet.
    pub fn new() -> MaintenanceSignal {
        MaintenanceSignal::default()
    }

    /// Asks the worker for a pass.
    pub fn notify(&self) {
        self.notify.notify_one();
    }

    /// Waits until a pass is asked for.
    pub async fn notified(&self) {
        self.notify.notified().await;
    }
}

/// What one maintenance pass changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// The play-count pins that changed.
    pub autopin: AutopinReport,
    /// The files that moved between the roots or were deleted.
    pub relocation: RelocationReport,
    /// The cache files that were evicted.
    pub eviction: EvictionReport,
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
    /// Reads the free space of the filesystem under the cache root.
    space: Arc<dyn SpaceProbe>,
    /// Serializes passes: a wake-up, the schedule and an administrator may
    /// ask at the same time, and two passes must never move the same file.
    running: Arc<tokio::sync::Mutex<()>>,
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
            space: Arc::new(StatvfsProbe),
            running: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Replaces how the free space of the cache filesystem is read.
    ///
    /// The default asks the operating system (`statvfs`); a test passes a probe
    /// that reports a chosen amount so the free-space limit can be exercised
    /// without filling a disk.
    ///
    /// # Parameters
    ///
    /// - `space`: the probe to use.
    ///
    /// # Returns
    ///
    /// The maintainer, using that probe.
    pub fn with_space_probe(mut self, space: Arc<dyn SpaceProbe>) -> StorageMaintainer {
        self.space = space;
        self
    }

    /// Returns the managed roots the maintainer moves files between.
    pub fn roots(&self) -> &StorageRoots {
        &self.roots
    }

    /// Runs one full pass: play-count pins, then relocation, then eviction.
    ///
    /// The order matters: a track that just reached the play-count threshold is
    /// pinned before relocation decides where its file belongs, and relocation
    /// demotes files into the cache before eviction measures it. Passes never
    /// overlap; a call while another runs waits for it.
    ///
    /// # Returns
    ///
    /// What the pass changed.
    ///
    /// # Errors
    ///
    /// Returns the first [`LibraryError`] of [`StorageMaintainer::autopin`],
    /// [`StorageMaintainer::relocate`] or [`StorageMaintainer::evict`]; the
    /// steps after a failing one do not run.
    pub async fn run_once(&self) -> Result<MaintenanceReport, LibraryError> {
        let _running = self.running.lock().await;
        let autopin = self.autopin().await?;
        let relocation = self.relocate().await?;
        let eviction = self.evict().await?;
        Ok(MaintenanceReport {
            autopin,
            relocation,
            eviction,
        })
    }

    /// Runs a pass whenever the signal asks for one, until shutdown.
    ///
    /// One pass runs at startup, so changes made while the server was stopped
    /// are acted on. A failed pass is logged and the worker keeps waiting.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal the writers notify.
    /// - `shutdown`: ends the loop between passes.
    pub async fn run(&self, signal: MaintenanceSignal, shutdown: Shutdown) {
        loop {
            if let Err(error) = self.run_once().await {
                tracing::warn!(%error, "storage maintenance failed");
            }
            tokio::select! {
                () = shutdown.wait() => return,
                () = signal.notified() => {}
            }
            tokio::select! {
                () = shutdown.wait() => return,
                () = tokio::time::sleep(WAKE_DEBOUNCE) => {}
            }
        }
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

    /// Brings the `policy` pins in step with `storage.autopin.play_count`.
    ///
    /// With a threshold N above zero, a track with at least N recorded plays,
    /// counted over every user, carries the `policy` pin reason, and a track
    /// below N loses it; a threshold of zero releases every `policy` pin. Each
    /// change is its own short transaction through [`retention`], so the
    /// track's availability follows, and a change of availability is
    /// published. A pass after a threshold change therefore corrects the pins
    /// in both directions.
    ///
    /// # Returns
    ///
    /// How many tracks gained and lost the pin.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a query or a pin write fails, or the
    /// setting cannot be read.
    pub async fn autopin(&self) -> Result<AutopinReport, LibraryError> {
        let threshold = self
            .settings
            .current()
            .get(keys::STORAGE_AUTOPIN_PLAY_COUNT)?;
        let mut report = AutopinReport::default();
        let (to_pin, to_release) = {
            let mut conn = self.readers.acquire().await?;
            let to_pin = if threshold > 0 {
                sqlx::query_scalar!(
                    "SELECT p.track_id AS \"track_id!: Vec<u8>\" FROM plays p \
                     GROUP BY p.track_id \
                     HAVING COUNT(*) >= ? AND NOT EXISTS ( \
                         SELECT 1 FROM track_retention tr, json_each(tr.pin_reasons) j \
                         WHERE tr.track_id = p.track_id AND j.value = 'policy')",
                    threshold,
                )
                .fetch_all(&mut *conn)
                .await?
            } else {
                Vec::new()
            };
            // A threshold of zero releases every policy pin.
            let release_below = if threshold > 0 { threshold } else { i64::MAX };
            let to_release = sqlx::query_scalar!(
                "SELECT tr.track_id AS \"track_id!: Vec<u8>\" \
                 FROM track_retention tr, json_each(tr.pin_reasons) j \
                 WHERE j.value = 'policy' \
                   AND (SELECT COUNT(*) FROM plays p WHERE p.track_id = tr.track_id) < ?",
                release_below,
            )
            .fetch_all(&mut *conn)
            .await?;
            (to_pin, to_release)
        };
        for stored in to_pin {
            let track_id = decode_entity(IdPrefix::Track, &stored)?;
            if self.change_policy_pin(track_id, true).await? {
                report.pinned += 1;
            }
        }
        for stored in to_release {
            let track_id = decode_entity(IdPrefix::Track, &stored)?;
            if self.change_policy_pin(track_id, false).await? {
                report.released += 1;
            }
        }
        if report != AutopinReport::default() {
            tracing::debug!(
                pinned = report.pinned,
                released = report.released,
                threshold,
                "updated play-count pins"
            );
        }
        Ok(report)
    }

    /// Adds or removes the `policy` pin of one track.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track.
    /// - `pin`: `true` to add the reason, `false` to remove it.
    ///
    /// # Returns
    ///
    /// `true` when the reason changed.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the write fails.
    async fn change_policy_pin(&self, track_id: EntityId, pin: bool) -> Result<bool, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let before = self.availability(&mut tx, track_id).await?;
        let changed = if pin {
            retention::add_pin_reason(&mut tx, track_id, PinReason::Policy).await?
        } else {
            retention::remove_pin_reason(&mut tx, track_id, PinReason::Policy).await?
        };
        let after = self.availability(&mut tx, track_id).await?;
        tx.commit().await?;
        if let (Some(before), Some(after)) = (before, after)
            && before != after
        {
            self.publish_availability(track_id, after);
        }
        Ok(changed)
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
    /// The row goes first, in a transaction that confirms the file is still in
    /// the root it was read from and its track is still unpinned: a pin that
    /// arrived since the candidates were read wins, and the file stays. The file
    /// itself is removed once that transaction committed, so a row never names a
    /// file that is already gone.
    ///
    /// # Parameters
    ///
    /// - `file`: the file to remove.
    /// - `source`: its absolute path.
    ///
    /// # Returns
    ///
    /// [`Relocated::Deleted`] when the row is gone, [`Relocated::Skipped`] when
    /// the track was pinned or the row moved in the meantime.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the row cannot be read or removed.
    async fn delete_file(
        &self,
        file: &ManagedFile,
        source: &std::path::Path,
    ) -> Result<Relocated, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        if !still_unpinned(&mut tx, file).await? {
            return Ok(Relocated::Skipped);
        }
        let before = self.availability(&mut tx, file.track_id).await?;
        media_files::delete_media_file(&mut tx, file.id).await?;
        let after = self.settle_availability(&mut tx, file.track_id).await?;
        tx.commit().await?;
        match tokio::fs::remove_file(source).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(
                    track = %file.track_id,
                    path = %source.display(),
                    %error,
                    "removed the row of an unpinned managed file but not the file itself"
                );
            }
        }
        self.prune(source, file.storage).await;
        if let (Some(before), Some(after)) = (before, after)
            && before != after
        {
            self.publish_availability(file.track_id, after);
        }
        tracing::debug!(
            track = %file.track_id,
            storage = file.storage.as_str(),
            rel_path = file.rel_path.as_str(),
            "deleted an unpinned managed file"
        );
        Ok(Relocated::Deleted)
    }

    /// Evicts unpinned cache files until the cache is within its limits.
    ///
    /// Only `cache` files of unpinned tracks are candidates; a file in a
    /// scanned folder or in `<storage>/library`, and the file of any pinned
    /// track, is never one. When `storage.cache.max_age_days` is above zero,
    /// every candidate last used (or, never streamed, added) longer ago than
    /// that goes first, whatever the pressure. The rest go in the order
    /// `storage.cache.eviction` names - `lru` by last use, `lfu` by recorded
    /// plays then last use, `fifo` by when the file was added - while the cache
    /// holds more than `storage.cache.max_bytes` or the filesystem under it has
    /// less than `storage.cache.min_free_bytes` available. A filesystem whose
    /// free space cannot be read leaves only the size limit in force.
    ///
    /// An evicted track that has no other file becomes `remote` again.
    ///
    /// # Returns
    ///
    /// How many files and bytes were evicted.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a query fails, a row cannot be removed, or
    /// a setting cannot be read.
    pub async fn evict(&self) -> Result<EvictionReport, LibraryError> {
        let snapshot = self.settings.current();
        let max_bytes = snapshot.get(keys::STORAGE_CACHE_MAX_BYTES)?;
        let min_free = snapshot.get(keys::STORAGE_CACHE_MIN_FREE_BYTES)?;
        let max_age_days = snapshot.get(keys::STORAGE_CACHE_MAX_AGE_DAYS)?;
        let policy = EvictionPolicy::parse(&snapshot.get(keys::STORAGE_CACHE_EVICTION)?)?;
        drop(snapshot);

        let mut candidates = self.eviction_candidates().await?;
        policy.order(&mut candidates);
        let mut cache_bytes = self.class_usage(StorageClass::Cache).await?.bytes;
        let mut available = self
            .space
            .space(&self.roots.cache)
            .map(|space| space.available_bytes);
        let mut report = EvictionReport::default();

        let mut remaining = Vec::with_capacity(candidates.len());
        if max_age_days > 0 {
            let cutoff = crate::now_millis() - max_age_days.saturating_mul(MILLIS_PER_DAY);
            for candidate in candidates {
                if candidate.used_at < cutoff {
                    if self.evict_one(&candidate).await? {
                        report.files += 1;
                        report.bytes += candidate.size;
                        cache_bytes -= candidate.size;
                        available = available.map(|bytes| bytes.saturating_add(candidate.size));
                    }
                } else {
                    remaining.push(candidate);
                }
            }
        } else {
            remaining = candidates;
        }

        for candidate in remaining {
            let over_size = cache_bytes > max_bytes;
            let under_free = available.is_some_and(|bytes| bytes < min_free);
            if !over_size && !under_free {
                break;
            }
            if self.evict_one(&candidate).await? {
                report.files += 1;
                report.bytes += candidate.size;
                cache_bytes -= candidate.size;
                available = available.map(|bytes| bytes.saturating_add(candidate.size));
            }
        }
        if report.files > 0 {
            tracing::info!(
                files = report.files,
                bytes = report.bytes,
                policy = policy.as_str(),
                "evicted cache files"
            );
        }
        Ok(report)
    }

    /// Evicts one candidate.
    ///
    /// # Parameters
    ///
    /// - `candidate`: the cache file to remove.
    ///
    /// # Returns
    ///
    /// `true` when the file was evicted, `false` when its track was pinned in
    /// the meantime.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the row cannot be read or removed.
    async fn evict_one(&self, candidate: &EvictionCandidate) -> Result<bool, LibraryError> {
        let source = self.roots.cache.join(&candidate.file.rel_path);
        let outcome = self.delete_file(&candidate.file, &source).await?;
        Ok(outcome == Relocated::Deleted)
    }

    /// Reports what the managed roots and the scanned folders hold.
    ///
    /// # Returns
    ///
    /// The file counts and bytes per storage class, the number of pinned
    /// tracks, and the space of the filesystem under the cache root when it can
    /// be read.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when a query fails.
    pub async fn usage(&self) -> Result<StorageUsage, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query!(
            "SELECT storage, COUNT(*) AS \"files!: i64\", COALESCE(SUM(size), 0) AS \"bytes!: i64\" \
             FROM media_files GROUP BY storage",
        )
        .fetch_all(&mut *conn)
        .await?;
        let mut usage = StorageUsage {
            library: ClassUsage::default(),
            cache: ClassUsage::default(),
            folders: ClassUsage::default(),
            pinned_tracks: 0,
            filesystem: self.space.space(&self.roots.cache),
        };
        for row in rows {
            let class = ClassUsage {
                files: u64::try_from(row.files).unwrap_or(0),
                bytes: row.bytes,
            };
            match storage_class(&row.storage)? {
                StorageClass::Library => usage.library = class,
                StorageClass::Cache => usage.cache = class,
                StorageClass::Folder => usage.folders = class,
            }
        }
        let pinned = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"pinned!: i64\" FROM track_retention WHERE pinned = 1"
        )
        .fetch_one(&mut *conn)
        .await?;
        usage.pinned_tracks = u64::try_from(pinned).unwrap_or(0);
        Ok(usage)
    }

    /// Reads the file count and bytes of one storage class.
    ///
    /// # Parameters
    ///
    /// - `storage`: the class to measure.
    ///
    /// # Returns
    ///
    /// Its files and bytes.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when the query fails.
    async fn class_usage(&self, storage: StorageClass) -> Result<ClassUsage, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        let literal = storage.as_str();
        let row = sqlx::query!(
            "SELECT COUNT(*) AS \"files!: i64\", COALESCE(SUM(size), 0) AS \"bytes!: i64\" \
             FROM media_files WHERE storage = ?",
            literal,
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(ClassUsage {
            files: u64::try_from(row.files).unwrap_or(0),
            bytes: row.bytes,
        })
    }

    /// Reads every cache file whose track is unpinned, with what the eviction
    /// orders sort by.
    ///
    /// # Returns
    ///
    /// The candidates, in no particular order.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the query fails or a stored id cannot be
    /// decoded.
    async fn eviction_candidates(&self) -> Result<Vec<EvictionCandidate>, LibraryError> {
        let mut conn = self.readers.acquire().await?;
        let rows = sqlx::query!(
            "SELECT mf.id AS id, mf.track_id AS track_id, mf.rel_path AS rel_path, \
                    mf.size AS size, mf.added_at AS added_at, \
                    COALESCE(mf.last_accessed_at, mf.added_at) AS \"used_at!: i64\", \
                    (SELECT COUNT(*) FROM plays p WHERE p.track_id = mf.track_id) AS \"plays!: i64\" \
             FROM media_files mf \
             LEFT JOIN track_retention tr ON tr.track_id = mf.track_id \
             WHERE mf.storage = 'cache' AND COALESCE(tr.pinned, 0) = 0",
        )
        .fetch_all(&mut *conn)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(EvictionCandidate {
                    file: ManagedFile {
                        id: decode_uuid(&row.id)?,
                        track_id: decode_entity(IdPrefix::Track, &row.track_id)?,
                        storage: StorageClass::Cache,
                        rel_path: row.rel_path,
                        pinned: false,
                    },
                    size: row.size,
                    added_at: row.added_at,
                    used_at: row.used_at,
                    plays: row.plays,
                })
            })
            .collect()
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

/// Milliseconds in one day, for `storage.cache.max_age_days`.
const MILLIS_PER_DAY: i64 = 86_400_000;

/// One cache file an eviction pass may remove, with what the orders sort by.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EvictionCandidate {
    /// The file.
    file: ManagedFile,
    /// Its size in bytes.
    size: i64,
    /// When it was added, in unix milliseconds.
    added_at: i64,
    /// When it was last streamed, or added when it never was.
    used_at: i64,
    /// How many plays of its track are recorded, over every user.
    plays: i64,
}

/// The order `storage.cache.eviction` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictionPolicy {
    /// Least recently used first.
    Lru,
    /// Least played first, then least recently used.
    Lfu,
    /// Oldest addition first.
    Fifo,
}

impl EvictionPolicy {
    /// Parses the setting's literal.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::UnknownLiteral`] for any other value.
    fn parse(stored: &str) -> Result<EvictionPolicy, LibraryError> {
        match stored {
            "lru" => Ok(EvictionPolicy::Lru),
            "lfu" => Ok(EvictionPolicy::Lfu),
            "fifo" => Ok(EvictionPolicy::Fifo),
            other => Err(LibraryError::UnknownLiteral {
                kind: "eviction policy",
                value: other.to_owned(),
            }),
        }
    }

    /// Returns the setting's literal.
    fn as_str(self) -> &'static str {
        match self {
            EvictionPolicy::Lru => "lru",
            EvictionPolicy::Lfu => "lfu",
            EvictionPolicy::Fifo => "fifo",
        }
    }

    /// Sorts candidates so the first one is evicted first.
    ///
    /// Ties fall back to the row id, which is a UUIDv7 and so follows insertion
    /// order, keeping a pass deterministic.
    fn order(self, candidates: &mut [EvictionCandidate]) {
        match self {
            EvictionPolicy::Lru => candidates.sort_by_key(|c| (c.used_at, c.file.id)),
            EvictionPolicy::Lfu => candidates.sort_by_key(|c| (c.plays, c.used_at, c.file.id)),
            EvictionPolicy::Fifo => candidates.sort_by_key(|c| (c.added_at, c.file.id)),
        }
    }
}

/// Confirms a file is still where it was read and its track still unpinned.
///
/// # Parameters
///
/// - `conn`: the transaction the removal runs in.
/// - `file`: the file about to be removed.
///
/// # Returns
///
/// `true` when the row is still in the same storage class and the track has
/// no pin.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
async fn still_unpinned(
    conn: &mut sqlx::SqliteConnection,
    file: &ManagedFile,
) -> Result<bool, LibraryError> {
    let id = file.id.as_bytes().as_slice();
    let row = sqlx::query!(
        "SELECT mf.storage AS storage, COALESCE(tr.pinned, 0) AS \"pinned!: i64\" \
         FROM media_files mf \
         LEFT JOIN track_retention tr ON tr.track_id = mf.track_id \
         WHERE mf.id = ?",
        id,
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.is_some_and(|row| row.pinned == 0 && row.storage == file.storage.as_str()))
}

/// Runs a maintenance pass for the `storage.maintain` job.
///
/// The server registers it under [`MAINTENANCE_JOB_KIND`] and schedules it at
/// [`MAINTENANCE_SCHEDULE`], so the storage limits hold even when no wake-up
/// arrives, for example after a setting changed.
#[derive(Debug, Clone)]
pub struct MaintenanceJobHandler {
    /// The maintainer the job runs.
    maintainer: Arc<StorageMaintainer>,
}

impl MaintenanceJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `maintainer`: the maintainer the job runs.
    ///
    /// # Returns
    ///
    /// The handler.
    pub fn new(maintainer: Arc<StorageMaintainer>) -> MaintenanceJobHandler {
        MaintenanceJobHandler { maintainer }
    }
}

#[async_trait]
impl JobHandler for MaintenanceJobHandler {
    /// Runs one pass.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the pass fails, so the queue retries it.
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        self.maintainer
            .run_once()
            .await
            .map(|_| ())
            .map_err(|error| JobError::new(error.to_string()))
    }
}
