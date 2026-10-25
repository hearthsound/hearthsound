//! Watching library folders for changes and scanning what changed.
//!
//! A folder whose `watch` column is set gets one filesystem watcher per
//! directory it holds: a recursive watch gives up on the first directory the
//! process may not read, which would leave a whole folder unwatched, while a
//! directory that cannot be watched on its own only loses itself. A directory
//! created while the watcher runs is watched from its creation event onward.
//! Changes are debounced - a burst of writes from one copy or one tagger is
//! reported once, after the quiet period in `library.watch.debounce_ms` - and
//! each batch turns into one incremental scan of the folders that changed, never
//! a full walk of the library.
//!
//! The watcher engine is chosen by the `notify` crate: inotify on Linux, FSEvents
//! on macOS, and whatever the platform offers elsewhere. An engine only reports
//! changes it can see, so a change made on the remote side of a network mount is
//! not reported; run a scan for those.
//!
//! Only events that create, modify or remove an entry turn into a scan. Engines
//! also report the reads a scan performs - inotify subscribes to `IN_OPEN`, so
//! walking a folder and probing its files arrives as an access event - and a
//! scan that acted on those would request the next scan for as long as it runs.
//!
//! Watchers are reconciled rather than restarted: [`LibraryWatcher::reconcile`]
//! starts a watcher for a folder that gained one, drops the watcher of a folder
//! that lost one, disappeared, or whose path is gone, and rebuilds the watchers
//! when the debounce period changes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_db::Database;
use hearthsound_jobs::JobQueue;
use notify::RecursiveMode;
use notify::event::{AccessKind, AccessMode, EventKind};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};
use tokio::sync::mpsc;

use crate::error::LibraryError;
use crate::folders;
use crate::model::LibraryFolder;
use crate::scan::{LibraryScanner, ScanRequest};

/// The paths one folder's debouncer reported in one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WatchBatch {
    /// The folder the paths belong to.
    folder_id: uuid::Uuid,
    /// The folder's root, which the paths are made relative to.
    root: PathBuf,
    /// The nested library folders, which are not watched by this folder.
    skip: Vec<PathBuf>,
    /// The paths the filesystem reported.
    paths: Vec<PathBuf>,
}

/// One folder's live watcher.
struct FolderWatcher {
    /// The debouncer the folder is watched through.
    debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    /// The debounce period the watcher was built with.
    debounce: Duration,
    /// The path the watcher was started for.
    root: PathBuf,
}

/// Watches library folders and scans what changed.
///
/// # Examples
///
/// ```no_run
/// # use std::sync::Arc;
/// # use hearthsound_config::settings::Settings;
/// # use hearthsound_db::Database;
/// # use hearthsound_jobs::JobQueue;
/// # use hearthsound_library::scan::LibraryScanner;
/// # use hearthsound_library::watch::LibraryWatcher;
/// # async fn example(
/// #     db: Database,
/// #     scanner: Arc<LibraryScanner>,
/// #     queue: Arc<JobQueue>,
/// #     settings: Settings,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// let watcher = LibraryWatcher::new(&db, scanner, queue, settings);
/// watcher.reconcile().await?;
/// # Ok(())
/// # }
/// ```
pub struct LibraryWatcher {
    /// The reader pool the folder list is read through.
    readers: sqlx::SqlitePool,
    /// The settings service the debounce period is read from.
    settings: Settings,
    /// One watcher per watched folder, keyed by folder id.
    ///
    /// Shared with the draining task, so a directory that appears after the
    /// folder was armed is watched from its creation event onward.
    watchers: Arc<Mutex<HashMap<uuid::Uuid, FolderWatcher>>>,
    /// The sender the debouncer threads forward their batches on.
    sender: mpsc::UnboundedSender<WatchBatch>,
}

impl LibraryWatcher {
    /// Builds a watcher and starts the task that turns batches into scans.
    ///
    /// The task drains the batches for as long as a watcher exists, so it ends
    /// when [`LibraryWatcher::stop`] drops them all.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `scanner`: the scanner a reported change is scanned through.
    /// - `queue`: the queue a scan is requested on.
    /// - `settings`: the settings service; the debounce period is read on every
    ///   reconcile.
    ///
    /// # Returns
    ///
    /// The watcher, watching nothing until [`LibraryWatcher::reconcile`] runs.
    pub fn new(
        db: &Database,
        scanner: Arc<LibraryScanner>,
        queue: Arc<JobQueue>,
        settings: Settings,
    ) -> LibraryWatcher {
        let (sender, mut receiver) = mpsc::unbounded_channel::<WatchBatch>();
        let watchers: Arc<Mutex<HashMap<uuid::Uuid, FolderWatcher>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let drain = Drain {
            scanner: Arc::clone(&scanner),
            queue: Arc::clone(&queue),
            watchers: Arc::clone(&watchers),
        };
        tokio::spawn(async move {
            while let Some(batch) = receiver.recv().await {
                drain.handle(batch).await;
            }
        });
        LibraryWatcher {
            readers: db.readers().clone(),
            settings,
            watchers,
            sender,
        }
    }

    /// Brings the running watchers in line with the watched folders.
    ///
    /// A folder that should be watched but is not gets a watcher, one that
    /// should not be watched or whose path is gone loses its watcher, and the
    /// rest are rebuilt when the debounce period changed.
    ///
    /// # Returns
    ///
    /// `Ok(())` once every folder that can be watched is watched. A folder whose
    /// watcher cannot be started is logged and left unwatched; it does not fail
    /// the reconcile.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when the folder list cannot be read
    /// and [`LibraryError::Setting`] when the debounce setting is missing.
    pub async fn reconcile(&self) -> Result<(), LibraryError> {
        let debounce = self
            .settings
            .current()
            .get(keys::LIBRARY_WATCH_DEBOUNCE_MS)?;
        let debounce = Duration::from_millis(debounce.max(1) as u64);
        let mut read = self.readers.acquire().await?;
        let folders = folders::list_library_folders(&mut read).await?;
        drop(read);

        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let wanted: HashSet<uuid::Uuid> = folders
            .iter()
            .filter(|folder| folder.watch)
            .map(|folder| folder.id)
            .collect();
        watchers.retain(|folder_id, watcher| {
            let keep = wanted.contains(folder_id)
                && watcher.debounce == debounce
                && folders
                    .iter()
                    .any(|folder| folder.id == *folder_id && folder.path == watcher.root);
            if !keep {
                tracing::debug!(folder = %folder_id, "stopped watching a library folder");
            }
            keep
        });

        for folder in &folders {
            if !folder.watch || watchers.contains_key(&folder.id) {
                continue;
            }
            let skip = folders::nested_roots(&folders, folder);
            match self.start(folder, debounce, &skip) {
                Ok(watcher) => {
                    tracing::info!(
                        folder = %folder.id,
                        path = %folder.path.display(),
                        "watching a library folder"
                    );
                    watchers.insert(folder.id, watcher);
                }
                Err(error) => tracing::warn!(
                    folder = %folder.id,
                    path = %folder.path.display(),
                    %error,
                    "could not watch a library folder"
                ),
            }
        }
        Ok(())
    }

    /// Stops every watcher.
    ///
    /// The debouncer threads stop between ticks, and the batch task ends once
    /// the last sender is gone.
    pub fn stop(&self) {
        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (folder_id, watcher) in watchers.drain() {
            tracing::debug!(folder = %folder_id, "stopped watching a library folder");
            watcher.debouncer.stop_nonblocking();
        }
    }

    /// Returns how many folders are watched.
    ///
    /// # Returns
    ///
    /// The number of live watchers.
    pub fn watched_folders(&self) -> usize {
        self.watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    /// Starts the watcher of one folder.
    ///
    /// Watches are armed per directory rather than recursively: the recursive
    /// mode of the inotify backend walks the tree and gives up on the first
    /// directory it may not watch, which would leave a folder with one closed
    /// subdirectory unwatched. A directory that cannot be read or watched is
    /// logged and skipped instead, and the next scan still finds its files.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder to watch; its path must be a directory.
    /// - `debounce`: the quiet period before a batch is reported.
    /// - `skip`: the nested library folders, which this folder does not watch.
    ///
    /// # Returns
    ///
    /// The running watcher.
    ///
    /// # Errors
    ///
    /// Returns [`notify::Error`] when the debouncer cannot be built or the folder
    /// root itself cannot be watched, which is what an exhausted inotify budget
    /// looks like.
    fn start(
        &self,
        folder: &LibraryFolder,
        debounce: Duration,
        skip: &[PathBuf],
    ) -> Result<FolderWatcher, notify::Error> {
        let root = folder.path.clone();
        let folder_id = folder.id;
        let sender = self.sender.clone();
        let handler_root = root.clone();
        let handler_skip = skip.to_vec();
        let events = move |result: DebounceEventResult| {
            let paths = match result {
                Ok(events) => paths_of(&events),
                Err(errors) => {
                    for error in errors {
                        tracing::warn!(%error, "a filesystem watcher reported an error");
                    }
                    return;
                }
            };
            if paths.is_empty() {
                return;
            }
            // The receiving task is the only reader; a closed channel means the
            // watcher was stopped, so dropping the batch is right.
            let _ = sender.send(WatchBatch {
                folder_id,
                root: handler_root.clone(),
                skip: handler_skip.clone(),
                paths,
            });
        };
        let mut debouncer = new_debouncer(debounce, None, events)?;
        // A root that cannot be watched still fails the folder, as before.
        debouncer.watch(&root, RecursiveMode::NonRecursive)?;
        let (directories, unreadable) = readable_directories(&root, skip);
        let unwatched = register_directories(&mut debouncer, &directories);
        tracing::info!(
            folder = %folder.id,
            path = %folder.path.display(),
            watched = directories.len(),
            unreadable,
            unwatched,
            "watching a library folder"
        );
        Ok(FolderWatcher {
            debouncer,
            debounce,
            root,
        })
    }
}

/// Collects every directory the watcher should arm.
///
/// The walk reads each directory, follows symlinks the way a scan does, and
/// stops where it has been before, so a link cycle cannot loop. A directory it
/// cannot read is counted and logged: its files are found by the next scan, and
/// the rest of the folder stays watched.
///
/// # Parameters
///
/// - `root`: the folder root.
/// - `skip`: the nested library folders, which are not descended into.
///
/// # Returns
///
/// The directories to arm, and how many could not be read.
fn readable_directories(root: &Path, skip: &[PathBuf]) -> (Vec<PathBuf>, u64) {
    let mut directories: Vec<PathBuf> = Vec::new();
    let mut unreadable = 0_u64;
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let canonical = match std::fs::canonicalize(&directory) {
            Ok(canonical) => canonical,
            Err(error) => {
                tracing::warn!(
                    path = %directory.display(),
                    %error,
                    "could not read a directory while arming the watcher; its changes are \
                     picked up by the next scan"
                );
                unreadable += 1;
                continue;
            }
        };
        if skip.iter().any(|nested| canonical == *nested) {
            continue;
        }
        if !visited.insert(canonical) {
            continue;
        }
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    path = %directory.display(),
                    %error,
                    "could not read a directory while arming the watcher; its changes are \
                     picked up by the next scan"
                );
                unreadable += 1;
                continue;
            }
        };
        directories.push(directory);
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    (directories, unreadable)
}

/// Arms one non-recursive watch per directory.
///
/// # Parameters
///
/// - `debouncer`: the folder's debouncer.
/// - `directories`: the directories to watch.
///
/// # Returns
///
/// How many directories could not be watched.
fn register_directories(
    debouncer: &mut Debouncer<notify::RecommendedWatcher, RecommendedCache>,
    directories: &[PathBuf],
) -> u64 {
    let mut failed = 0_u64;
    for directory in directories {
        if let Err(error) = debouncer.watch(directory, RecursiveMode::NonRecursive) {
            failed += 1;
            tracing::warn!(
                path = %directory.display(),
                %error,
                "could not watch a directory while arming the watcher; its changes are picked \
                 up by the next scan"
            );
        }
    }
    failed
}

/// Turns one debounced batch into scans.
struct Drain {
    /// The scanner a batch is scanned through.
    scanner: Arc<LibraryScanner>,
    /// The queue a scan is requested on.
    queue: Arc<JobQueue>,
    /// The live watchers, shared with the area that started them.
    watchers: Arc<Mutex<HashMap<uuid::Uuid, FolderWatcher>>>,
}

impl Drain {
    /// Arms the watcher on every reported directory it does not watch yet.
    ///
    /// A directory that appears after the folder was armed has no watch of its
    /// own, so without this a file written into a brand-new album folder would
    /// wait for the next scan. The scan of the reported subtree is what indexes
    /// the files, so a directory that cannot be watched still loses nothing.
    ///
    /// # Parameters
    ///
    /// - `batch`: the paths one folder's debouncer reported.
    fn arm_directories(&self, batch: &WatchBatch) {
        let directories: Vec<PathBuf> = batch
            .paths
            .iter()
            .filter(|path| path.is_dir())
            .filter(|path| {
                !batch.skip.iter().any(|nested| {
                    *path == nested || std::fs::canonicalize(path).is_ok_and(|real| real == *nested)
                })
            })
            .cloned()
            .collect();
        if directories.is_empty() {
            return;
        }
        let mut watchers = self
            .watchers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(watcher) = watchers.get_mut(&batch.folder_id) {
            register_directories(&mut watcher.debouncer, &directories);
        }
    }

    /// Requests one scan for the directories a batch reported.
    ///
    /// # Parameters
    ///
    /// - `batch`: the paths one folder's debouncer reported.
    async fn handle(&self, batch: WatchBatch) {
        self.arm_directories(&batch);
        let subtrees = relative_subtrees(&batch.root, &batch.paths);
        let request = ScanRequest {
            folder_id: Some(batch.folder_id),
            full: false,
            paths: subtrees,
        };
        match self.scanner.request_scan(&self.queue, &request).await {
            Ok(job) => tracing::debug!(
                folder = %batch.folder_id,
                job = %job.id,
                paths = ?request.paths,
                "requested a scan for a watched change"
            ),
            Err(error) => tracing::warn!(
                folder = %batch.folder_id,
                %error,
                "could not request a scan for a watched change"
            ),
        }
    }
}

/// Collects the paths of a debounced batch.
///
/// Only events that can have changed the folder's content are collected. See
/// [`changes_the_library`].
///
/// # Parameters
///
/// - `events`: the events of the batch.
///
/// # Returns
///
/// The event paths of the changes, in the order they were reported.
fn paths_of(events: &[DebouncedEvent]) -> Vec<PathBuf> {
    events
        .iter()
        .filter(|event| changes_the_library(&event.event.kind))
        .flat_map(|event| event.event.paths.iter().cloned())
        .collect()
}

/// Reports whether an event can have changed what a scan reads.
///
/// `Create`, `Modify` and `Remove` are the changes the watcher exists for. A
/// write close is accepted as well, in case an engine reports a write that way
/// and not as a modify. Opens, reads and read closes are not changes: a scan
/// opens every directory it walks and every file it probes, so acting on them
/// would make each scan request the next one. Events of no particular kind have
/// nothing to act on that a scan could find.
///
/// # Parameters
///
/// - `kind`: the kind of the event.
///
/// # Returns
///
/// `true` when the event says an entry appeared, changed or went away.
fn changes_the_library(kind: &EventKind) -> bool {
    match kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => true,
        EventKind::Access(access) => matches!(access, AccessKind::Close(AccessMode::Write)),
        EventKind::Any | EventKind::Other => false,
    }
}

/// Maps reported paths onto the directories a scan should walk.
///
/// A file maps to its directory, a directory maps to itself, and a path with a
/// hidden component is ignored, because the walk skips hidden entries anyway. A
/// directory that contains another reported directory is dropped, so a batch
/// that reports both an album and a file inside it scans the album once.
///
/// # Parameters
///
/// - `root`: the watched folder's root.
/// - `paths`: the paths the filesystem reported.
///
/// # Returns
///
/// The folder-relative directories to scan, sorted; empty means the whole
/// folder, which is what a change directly inside the root maps to, and also
/// what a batch with nothing usable maps to.
fn relative_subtrees(root: &Path, paths: &[PathBuf]) -> Vec<String> {
    let mut subtrees: Vec<String> = Vec::new();
    for path in paths {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        if relative
            .components()
            .any(|component| component.as_os_str().as_encoded_bytes().first() == Some(&b'.'))
        {
            continue;
        }
        let directory = if path.is_dir() {
            relative.to_path_buf()
        } else {
            relative.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let directory = directory.to_string_lossy().replace('\\', "/");
        let directory = directory.trim_matches('/').to_owned();
        if directory.is_empty() {
            // Something directly inside the root changed, so the whole folder
            // is in scope.
            return Vec::new();
        }
        if !subtrees.contains(&directory) {
            subtrees.push(directory);
        }
    }
    subtrees.sort();
    let mut collapsed: Vec<String> = Vec::new();
    for subtree in subtrees {
        let nested = collapsed.iter().any(|kept| {
            subtree
                .strip_prefix(kept)
                .is_some_and(|rest| rest.starts_with('/'))
        });
        if !nested {
            collapsed.push(subtree);
        }
    }
    collapsed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_maps_to_its_directory() {
        let root = Path::new("/music");
        let paths = vec![PathBuf::from("/music/Album/01 - Song.flac")];
        assert_eq!(relative_subtrees(root, &paths), ["Album"]);
    }

    #[test]
    fn a_change_inside_the_root_scans_the_whole_folder() {
        let root = Path::new("/music");
        let paths = vec![PathBuf::from("/music/loose.flac")];
        assert_eq!(relative_subtrees(root, &paths), Vec::<String>::new());
    }

    #[test]
    fn hidden_paths_are_ignored() {
        let root = Path::new("/music");
        let paths = vec![
            PathBuf::from("/music/.Trashes/01.flac"),
            PathBuf::from("/music/Album/.hidden.flac"),
        ];
        assert_eq!(relative_subtrees(root, &paths), Vec::<String>::new());
    }

    #[test]
    fn nested_directories_collapse_to_the_topmost() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Album/Disc 1")).unwrap();
        std::fs::create_dir_all(root.join("Other")).unwrap();
        let paths = vec![
            root.join("Album/Disc 1/01.flac"),
            root.join("Album"),
            root.join("Other/02.flac"),
        ];
        assert_eq!(relative_subtrees(root, &paths), ["Album", "Other"]);
    }

    #[test]
    fn a_directory_maps_to_itself_and_a_file_to_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Album")).unwrap();
        std::fs::create_dir(root.join("Removed")).unwrap();
        std::fs::remove_dir(root.join("Removed")).unwrap();
        let paths = vec![root.join("Album"), root.join("Removed/01.flac")];
        assert_eq!(relative_subtrees(root, &paths), ["Album", "Removed"]);
    }

    #[test]
    fn a_path_outside_the_root_is_ignored() {
        let root = Path::new("/music");
        let paths = vec![PathBuf::from("/elsewhere/01.flac")];
        assert_eq!(relative_subtrees(root, &paths), Vec::<String>::new());
    }

    /// Builds one debounced event for a path.
    fn debounced(kind: EventKind, path: &str) -> DebouncedEvent {
        DebouncedEvent::new(
            notify::Event::new(kind).add_path(PathBuf::from(path)),
            std::time::Instant::now(),
        )
    }

    #[test]
    fn the_reads_a_scan_performs_are_not_collected() {
        let events = vec![
            debounced(
                EventKind::Access(AccessKind::Open(AccessMode::Any)),
                "/music/Album",
            ),
            debounced(
                EventKind::Access(AccessKind::Open(AccessMode::Any)),
                "/music/Album/01 - Song.flac",
            ),
            debounced(
                EventKind::Access(AccessKind::Close(AccessMode::Read)),
                "/music/Album/01 - Song.flac",
            ),
        ];
        assert_eq!(paths_of(&events), Vec::<PathBuf>::new());
    }

    #[test]
    fn created_modified_and_removed_paths_are_collected() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};

        let events = vec![
            debounced(EventKind::Create(CreateKind::File), "/music/A/01.flac"),
            debounced(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                "/music/A/02.flac",
            ),
            debounced(
                EventKind::Access(AccessKind::Close(AccessMode::Write)),
                "/music/A/03.flac",
            ),
            debounced(EventKind::Remove(RemoveKind::File), "/music/A/04.flac"),
            debounced(EventKind::Other, "/music/A/05.flac"),
        ];
        assert_eq!(
            paths_of(&events),
            [
                "/music/A/01.flac",
                "/music/A/02.flac",
                "/music/A/03.flac",
                "/music/A/04.flac"
            ]
            .map(PathBuf::from)
        );
    }
}
