//! Scanning library folders and indexing their tags.
//!
//! A scan walks a folder, recognises the audio files in it, probes each one for
//! its format and tags, and writes the artists, albums, tracks, and media files
//! it finds. The scan never writes into the folder - the only writer into a
//! scanned folder is a save into the folder its owner chose as their save
//! location - so a scan changes only the database rows describing the folder.
//!
//! The scan is incremental. A file whose size and modification time match the
//! stored media file is skipped without probing, a file whose size or
//! modification time changed has its probe columns and its track refreshed, and
//! a file that disappeared has its media file row removed. A track left with no
//! files at all is deleted when it was created by scanning and demoted to
//! `remote` when it came from the catalog.
//!
//! Overlapping folders are allowed, and each file belongs to the deepest folder
//! that contains it. A folder's walk skips the roots of the folders inside it,
//! its scan takes over the rows an enclosing folder holds for the files under
//! its root - re-pointing them instead of indexing the same file again, so the
//! track keeps its id and everything that points at it - and its prune leaves
//! the rows under a nested folder's root alone, because those are that folder's
//! to take over or remove.
//!
//! Probing never happens inside a write transaction. The walk is blocking work
//! and runs on a blocking thread; the probes are asynchronous and run outside
//! any transaction; the writes are grouped into short transactions so the
//! shared writer's window stays small.
//!
//! A folder with an owner pins its tracks with the `folder` reason in the same
//! transaction that writes them, which is what makes an owned folder's content
//! permanent in its owner's personal library.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use futures_util::StreamExt as _;
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{
    Audience, EventBus, LibraryChange, LibraryChanged, ScanProgress, ServerEvent,
};
use hearthsound_core::{Availability, EntityId, EntityKind, PinReason};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler, JobQueue, NewJob};
use hearthsound_media::ProbeResult;
use serde::{Deserialize, Serialize};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::artists_split::split_artists;
use crate::error::LibraryError;
use crate::locks::FolderLocks;
use crate::model::{
    AlbumInput, ArtistCredit, ArtistInput, LibraryFolder, Origin, ReplayGain, ScannedMediaFile,
    TrackInput,
};
use crate::{
    DELETE_BATCH, READ_THEN_WRITE, albums, folders, media_files, now_millis, retention, tracks,
};

/// The job kind a library scan is enqueued under.
pub const SCAN_JOB_KIND: &str = "library.scan";

/// How many files one write transaction covers.
///
/// Small enough that the writer's window stays short, large enough that a scan
/// of a big folder does not commit once per file.
const WRITE_BATCH: usize = 64;

/// The album artist a compilation without one is credited to.
const VARIOUS_ARTISTS: &str = "Various Artists";

/// The `albums.album_type` literal a compilation is stored with.
const COMPILATION_ALBUM_TYPE: &str = "compilation";

/// The file extensions the scanner treats as audio.
const AUDIO_EXTENSIONS: [&str; 14] = [
    "aac", "aif", "aiff", "ape", "flac", "m4a", "mp3", "mp4", "mpc", "oga", "ogg", "opus", "wav",
    "wv",
];

/// The JSON payload of a `library.scan` job.
///
/// A request names the folder to scan, or all of them; whether to re-probe
/// every file instead of trusting size and modification time; and the subtrees
/// to restrict the scan to. The fields a queue payload may leave out have
/// defaults, so a payload enqueued by an older version still parses.
///
/// # Examples
///
/// ```
/// use hearthsound_library::scan::ScanRequest;
///
/// let request = ScanRequest::all();
/// assert_eq!(request.folder_id, None);
/// assert!(!request.full);
/// assert_eq!(
///     serde_json::to_string(&request).unwrap(),
///     "{\"folderId\":null,\"full\":false,\"paths\":[]}"
/// );
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRequest {
    /// The folder to scan, or `None` to scan every folder.
    pub folder_id: Option<Uuid>,
    /// Whether to probe every file, ignoring the stored size and modification
    /// time.
    #[serde(default)]
    pub full: bool,
    /// The directories to restrict the scan to, relative to the folder root,
    /// `/`-separated and without `..`. An empty list scans the whole folder.
    #[serde(default)]
    pub paths: Vec<String>,
}

impl ScanRequest {
    /// Builds a request that scans every folder.
    pub fn all() -> ScanRequest {
        ScanRequest::default()
    }

    /// Builds a request that scans one folder.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    pub fn folder(folder_id: Uuid) -> ScanRequest {
        ScanRequest {
            folder_id: Some(folder_id),
            ..ScanRequest::default()
        }
    }

    /// Restricts the request to subtrees of its folder.
    ///
    /// # Parameters
    ///
    /// - `paths`: the directories relative to the folder root.
    pub fn paths<I, S>(mut self, paths: I) -> ScanRequest
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.paths = paths.into_iter().map(Into::into).collect();
        self
    }

    /// Marks the request as a full scan.
    pub fn full(mut self) -> ScanRequest {
        self.full = true;
        self
    }
}

/// What the scanner is doing right now.
///
/// # Examples
///
/// ```
/// use hearthsound_library::scan::ScanStatus;
///
/// let idle = ScanStatus::default();
/// assert!(!idle.scanning);
/// assert_eq!(idle.files_processed, 0);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanStatus {
    /// Whether a scan is queued or running.
    pub scanning: bool,
    /// How many files the running scans have dealt with.
    pub files_processed: u64,
}

/// The counters behind [`ScanStatus`].
#[derive(Debug, Default)]
struct ScanCounters {
    /// Requests enqueued but not started.
    queued: AtomicUsize,
    /// Scans running right now.
    active: AtomicUsize,
    /// Files the running scans have dealt with.
    files_processed: AtomicU64,
}

/// Marks one scan as running for as long as it is held.
///
/// Dropping the guard ends the scan, so a scan that fails, panics, or is
/// cancelled is never left counted as running.
struct RunningScan<'a> {
    /// The counters the scan is counted in.
    counters: &'a ScanCounters,
}

impl Drop for RunningScan<'_> {
    fn drop(&mut self) {
        self.counters.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What one folder scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// The scanned folder.
    pub folder_id: Uuid,
    /// How many audio files the walk found.
    pub files_seen: u64,
    /// How many files were probed and written.
    pub files_indexed: u64,
    /// How many files were skipped because size and modification time matched.
    pub files_skipped: u64,
    /// How many files could not be read as audio and were left out of the
    /// index, for example a damaged file or one whose content does not match
    /// its extension.
    pub files_failed: u64,
    /// How many media files were removed because the file is gone.
    pub files_removed: u64,
    /// How many locally created tracks were removed because they lost their last
    /// file.
    pub tracks_removed: u64,
    /// How many entries the walk could not read and skipped: a directory the
    /// process has no permission for, an entry that vanished mid-walk, a link
    /// that does not resolve, or a file name that is not valid UTF-8. They are
    /// counted rather than stored, so the next scan can find them.
    pub unreadable: u64,
    /// The first entry the walk could not read, which the problem message names
    /// as an example.
    unreadable_path: Option<PathBuf>,
    /// Whether the scan refused to remove anything because the folder root held
    /// no entry at all, which is what an unmounted disk looks like.
    pub prune_refused: bool,
}

impl ScanReport {
    /// The problem the scan ended with, when it ended with one.
    ///
    /// A clean scan reports `None`. A scan that finished but could not do its
    /// job reports the sentence the folder's `last_scan_error` stores and the
    /// terminal `scan.progress` event carries, so the administrator sees one
    /// wording everywhere.
    ///
    /// # Returns
    ///
    /// The message, or `None` when the scan had nothing to report.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_library::scan::ScanReport;
    ///
    /// let clean = ScanReport::default();
    /// assert_eq!(clean.problem(), None);
    ///
    /// // A scan that refused to prune, or that skipped entries it could not
    /// // read, reports one sentence naming the reason; a clean scan reports
    /// // nothing, and that is what clears the folder's `last_scan_error`.
    /// ```
    pub fn problem(&self) -> Option<String> {
        if self.prune_refused {
            return Some(PRUNE_REFUSED.to_owned());
        }
        if self.unreadable == 0 {
            return None;
        }
        let example = match &self.unreadable_path {
            Some(path) => format!(" (for example \"{}\")", path.display()),
            None => String::new(),
        };
        Some(format!(
            "{} entries could not be read and were skipped{example}.",
            self.unreadable
        ))
    }
}

/// What the scan writes into `last_scan_error` when it refused to prune.
const PRUNE_REFUSED: &str =
    "Nothing was removed: the folder root is empty, so the disk may not be mounted.";

/// An audio file the walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundFile {
    /// The path relative to the folder root.
    rel_path: String,
    /// The absolute path on disk.
    absolute: PathBuf,
    /// The file size in bytes.
    size: i64,
    /// The file modification time in unix milliseconds.
    mtime_ms: i64,
}

/// A media file row an enclosing folder holds under a folder's root.
///
/// A file belongs to the deepest folder that contains it, so such a row is the
/// scanned folder's to keep: its scan re-points it instead of indexing the same
/// file again, which is what keeps the track and everything hanging off it.
#[derive(Debug, Clone)]
struct InheritedFile {
    /// The path relative to the scanned folder's root.
    rel_path: String,
    /// The row as the enclosing folder stores it.
    state: media_files::StoredFileState,
}

/// A file waiting to be written, with its probe result.
#[derive(Debug)]
struct PendingFile {
    /// The file the walk found.
    found: FoundFile,
    /// The probe result.
    probe: ProbeResult,
    /// The track the file already belongs to, when the row exists.
    track_id: Option<EntityId>,
}

/// The track, album, credits, and genres a probe implies.
#[derive(Debug)]
pub(crate) struct IndexedTags {
    /// The album to upsert, when the tags name one.
    pub(crate) album: Option<AlbumInput>,
    /// The album's credits.
    pub(crate) album_credits: Vec<ArtistCredit>,
    /// The track to insert or update.
    pub(crate) track: TrackInput,
    /// The track's credits.
    pub(crate) credits: Vec<ArtistCredit>,
    /// The track's genres.
    pub(crate) genres: Vec<String>,
}

/// Scans library folders into the database.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use hearthsound_core::events::EventBus;
/// use hearthsound_db::Database;
/// use hearthsound_library::locks::FolderLocks;
/// use hearthsound_library::scan::LibraryScanner;
///
/// # async fn example(db: &Database, folder_id: uuid::Uuid) -> Result<(), Box<dyn std::error::Error>> {
/// let settings = hearthsound_config::settings::Settings::load(
///     db.writer().clone(),
///     hearthsound_config::SettingsRegistry::builtin(),
///     hearthsound_config::SettingSources::default(),
/// )
/// .await?;
/// let scanner = LibraryScanner::new(
///     db,
///     Arc::new(EventBus::new()),
///     settings,
///     Arc::new(FolderLocks::new()),
/// );
/// let report = scanner.scan_folder(folder_id).await?;
/// println!("indexed {} files", report.files_indexed);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct LibraryScanner {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The reader pool the lookup queries go through.
    readers: SqlitePool,
    /// The bus scan progress and library changes are published on.
    events: Arc<EventBus>,
    /// The settings service the scan reads its tunables from.
    settings: Settings,
    /// One lock per folder, shared with every other writer into the folder, so
    /// a scan and a save never overlap.
    folder_locks: Arc<FolderLocks>,
    /// The counters [`LibraryScanner::status`] reports.
    counters: Arc<ScanCounters>,
}

impl std::fmt::Debug for LibraryScanner {
    /// Reports the type without the pools or the settings.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LibraryScanner")
    }
}

impl LibraryScanner {
    /// Builds a scanner over a database, an event bus, and the settings.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `events`: the bus `scan.progress` and `library.changed` are published
    ///   on.
    /// - `settings`: the settings service; a scan reads its tunables from the
    ///   snapshot in effect when it starts.
    /// - `folder_locks`: the per-folder locks every writer into a scanned folder
    ///   shares, so a scan never runs while a save writes into the same folder.
    ///
    /// # Returns
    ///
    /// The scanner.
    pub fn new(
        db: &Database,
        events: Arc<EventBus>,
        settings: Settings,
        folder_locks: Arc<FolderLocks>,
    ) -> LibraryScanner {
        LibraryScanner {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            events,
            settings,
            folder_locks,
            counters: Arc::new(ScanCounters::default()),
        }
    }

    /// Returns what the scanner is doing right now.
    ///
    /// The counters cover this process only: a scan enqueued by another
    /// connection, such as a command-line invocation, is not counted, and a
    /// restart clears them.
    ///
    /// # Returns
    ///
    /// Whether a scan is queued or running and how many files the running scans
    /// have dealt with.
    pub fn status(&self) -> ScanStatus {
        let queued = self.counters.queued.load(Ordering::SeqCst);
        let active = self.counters.active.load(Ordering::SeqCst);
        ScanStatus {
            scanning: queued + active > 0,
            files_processed: self.counters.files_processed.load(Ordering::SeqCst),
        }
    }

    /// Enqueues a scan and returns the queued job.
    ///
    /// The request counts as queued from before the job can be claimed until a
    /// [`ScanJobHandler`] starts it, so [`LibraryScanner::status`] reports a
    /// scan from the moment it is requested.
    ///
    /// # Parameters
    ///
    /// - `queue`: the queue to enqueue on.
    /// - `request`: the folders, subtrees, and depth to scan.
    ///
    /// # Returns
    ///
    /// The enqueued job.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Queue`] when the job cannot be written.
    pub async fn request_scan(
        &self,
        queue: &JobQueue,
        request: &ScanRequest,
    ) -> Result<Job, LibraryError> {
        // Counted before the job is visible to the runner, so a handler that
        // claims it at once always finds its request to release.
        self.counters.queued.fetch_add(1, Ordering::SeqCst);
        match enqueue_scan(queue, request).await {
            Ok(job) => Ok(job),
            Err(error) => {
                self.request_started();
                Err(error)
            }
        }
    }

    /// Scans every folder, or the one a request names.
    ///
    /// The whole request counts as one running scan, from before the folders
    /// are listed until the last one is done, so the status never reads idle
    /// between two folders. Scans of one folder never overlap: a request that
    /// arrives while its folder is being scanned waits for the running scan to
    /// finish.
    ///
    /// # Parameters
    ///
    /// - `request`: the folders, subtrees, and depth to scan.
    ///
    /// # Returns
    ///
    /// One report per folder that scanned, in the order the folders are listed.
    ///
    /// # Errors
    ///
    /// Returns the first folder's error. Every other folder is still scanned:
    /// one disk that is not mounted must not cost the rest of the library its
    /// scan, and the folder that failed carries its own `last_scan_error`. A
    /// scan is idempotent, so the caller can retry it after fixing what made the
    /// folder unreadable.
    pub async fn scan(&self, request: &ScanRequest) -> Result<Vec<ScanReport>, LibraryError> {
        let _running = self.begin_scan();
        match request.folder_id {
            Some(folder_id) => Ok(vec![self.scan_one(folder_id, request).await?]),
            None => {
                let mut read = self.readers.acquire().await?;
                let folders = folders::list_library_folders(&mut read).await?;
                drop(read);
                let mut reports = Vec::with_capacity(folders.len());
                let mut first_error: Option<LibraryError> = None;
                for folder in folders {
                    match self.scan_one(folder.id, request).await {
                        Ok(report) => reports.push(report),
                        Err(error) => {
                            tracing::warn!(
                                folder = %folder.id,
                                path = %folder.path.display(),
                                %error,
                                "a folder scan failed"
                            );
                            if first_error.is_none() {
                                first_error = Some(error);
                            }
                        }
                    }
                }
                match first_error {
                    Some(error) => Err(error),
                    None => Ok(reports),
                }
            }
        }
    }

    /// Scans one library folder.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    ///
    /// # Returns
    ///
    /// What the scan did.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::UnknownFolder`] when no folder has that id,
    /// [`LibraryError::Scan`] when the folder cannot be walked or a file cannot
    /// be read. A file that cannot be probed as audio does not fail the scan: it
    /// is logged, counted in [`ScanReport::files_failed`], and left out. A file
    /// that was indexed by an earlier scan and fails to probe now keeps its
    /// stored row.
    pub async fn scan_folder(&self, folder_id: Uuid) -> Result<ScanReport, LibraryError> {
        let _running = self.begin_scan();
        self.scan_one(folder_id, &ScanRequest::folder(folder_id))
            .await
    }

    /// Scans one folder under its lock.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    /// - `request`: the scan to run; its `folder_id` is ignored.
    ///
    /// # Returns
    ///
    /// What the scan did.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::UnknownFolder`] when no folder has that id,
    /// [`LibraryError::Scan`] when the folder cannot be walked or a file cannot
    /// be read, and [`LibraryError::Setting`] when a scan tunable is not
    /// registered.
    async fn scan_one(
        &self,
        folder_id: Uuid,
        request: &ScanRequest,
    ) -> Result<ScanReport, LibraryError> {
        let _guard = self.folder_locks.lock(folder_id).await;
        self.scan_locked(folder_id, request).await
    }

    /// Records that a queued request is starting.
    ///
    /// A request enqueued by another process, or a job reclaimed after a crash,
    /// has no counter to release, so a release that would go below zero is
    /// dropped.
    fn request_started(&self) {
        let _ = self
            .counters
            .queued
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |queued| {
                queued.checked_sub(1)
            });
    }

    /// Records that a scan started, until the returned guard is dropped.
    ///
    /// The file counter is cleared when the first scan of a run starts, so
    /// [`LibraryScanner::status`] reports what the current run has done.
    ///
    /// # Returns
    ///
    /// The guard that ends the scan when dropped.
    fn begin_scan(&self) -> RunningScan<'_> {
        if self.counters.active.fetch_add(1, Ordering::SeqCst) == 0 {
            self.counters.files_processed.store(0, Ordering::SeqCst);
        }
        RunningScan {
            counters: &self.counters,
        }
    }

    /// Records that one file was dealt with.
    ///
    /// # Parameters
    ///
    /// - `files`: how many files were dealt with.
    fn count_files(&self, files: u64) {
        self.counters
            .files_processed
            .fetch_add(files, Ordering::SeqCst);
    }

    /// Scans one folder under its lock and records how it ended.
    ///
    /// The folder's row and the terminal `scan.progress` event always say how
    /// the scan ended: a clean scan clears the stored problem, a scan that
    /// finished with a warning stores it, and a scan that failed stores the
    /// failure and leaves the last successful instant alone.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    /// - `request`: the scan to run; its `folder_id` is ignored.
    ///
    /// # Returns
    ///
    /// What the scan did.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`LibraryScanner::scan_one`].
    async fn scan_locked(
        &self,
        folder_id: Uuid,
        request: &ScanRequest,
    ) -> Result<ScanReport, LibraryError> {
        match self.scan_body(folder_id, request).await {
            Ok(report) => {
                let problem = report.problem();
                self.record_outcome(folder_id, Some(now_millis()), problem.as_deref())
                    .await;
                self.progress(folder_id, "done", &report, problem);
                Ok(report)
            }
            Err(error) => {
                let message = format!("Scan failed: {error}");
                self.record_outcome(folder_id, None, Some(&message)).await;
                self.progress_failed(folder_id, message);
                Err(error)
            }
        }
    }

    /// Records the outcome of a scan on the folder's row.
    ///
    /// The write is one short transaction and never fails the scan: the files
    /// were already walked and indexed, and a status column that cannot be
    /// written is a log line rather than a lost scan.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder that was scanned. A folder that no longer
    ///   exists updates no row, which is not an error.
    /// - `at`: when the scan finished, or `None` to leave the stored instant.
    /// - `error`: the problem the scan ended with, or `None` for a clean scan.
    async fn record_outcome(&self, folder_id: Uuid, at: Option<i64>, error: Option<&str>) {
        let mut tx = match self.writer.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::error!(%error, folder = %folder_id, "could not record the outcome of a scan");
                return;
            }
        };
        let stored = folders::set_library_folder_scan_outcome(&mut tx, folder_id, at, error).await;
        match stored {
            Ok(_) => {
                if let Err(error) = tx.commit().await {
                    tracing::error!(%error, folder = %folder_id, "could not record the outcome of a scan");
                }
            }
            Err(error) => {
                tracing::error!(%error, folder = %folder_id, "could not record the outcome of a scan");
            }
        }
    }

    /// Scans one folder, with its lock already held.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    /// - `request`: the scan to run; its `folder_id` is ignored.
    ///
    /// # Returns
    ///
    /// What the scan did.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`LibraryScanner::scan_one`].
    async fn scan_body(
        &self,
        folder_id: Uuid,
        request: &ScanRequest,
    ) -> Result<ScanReport, LibraryError> {
        let settings = self.settings.current();
        let separators = settings.get(keys::LIBRARY_SCAN_ARTIST_SEPARATORS)?;
        let separators = separators.as_slice();
        let concurrency = settings.get(keys::LIBRARY_SCAN_CONCURRENCY)?;
        // The setting is validated to 1..=64; clamping keeps a store built by
        // hand safe as well.
        let concurrency = concurrency.clamp(1, 64) as usize;
        let subtrees = subtrees(&request.paths);
        let mut read = self.readers.acquire().await?;
        let folder = folders::get_library_folder(&mut read, folder_id)
            .await?
            .ok_or(LibraryError::UnknownFolder { folder_id })?;
        // Overlapping folders are allowed, and a file belongs to the deepest
        // folder that contains it: this folder's walk skips the roots of the
        // folders inside it, and the rows the folders around it hold for the
        // files it walks are taken over below.
        let all_folders = folders::list_library_folders(&mut read).await?;
        // One read of the folder's rows replaces a lookup per walked file.
        let mut stored = media_files::list_folder_media_files(&mut read, folder.id).await?;
        let inherited = inherited_files(&mut read, &all_folders, &folder).await?;
        drop(read);

        let skip = folders::nested_roots(&all_folders, &folder);
        // The rows inside a nested folder's root are that folder's to take
        // over, so this folder's prune leaves them alone: pruning them would
        // delete the track the nested folder is about to adopt.
        let nested: Vec<String> = skip
            .iter()
            .filter_map(|path| path.strip_prefix(&folder.path).ok())
            .filter_map(|relative| relative.to_str().map(str::to_owned))
            .collect();
        let walk_root = folder.path.clone();
        let walk_subtrees = subtrees.clone();
        let outcome =
            tokio::task::spawn_blocking(move || walk(&walk_root, &walk_subtrees, &skip)).await??;
        let found = outcome.found;

        let mut report = ScanReport {
            folder_id,
            files_seen: found.len() as u64,
            unreadable: outcome.unreadable,
            unreadable_path: outcome.unreadable_path,
            ..ScanReport::default()
        };
        self.progress(folder.id, "indexing", &report, None);

        let seen: HashSet<String> = found.iter().map(|file| file.rel_path.clone()).collect();
        // An existing root with nothing in it is what an unmounted bind mount
        // looks like, so neither the rows this folder holds nor the rows an
        // enclosing folder holds here may go until a scan sees content again.
        let rows_at_risk = stored.len() + inherited.len();
        let prunable = !(outcome.root_empty && rows_at_risk > 0);
        if prunable {
            self.take_over(
                &folder,
                inherited,
                &mut stored,
                &seen,
                &subtrees,
                &mut report,
            )
            .await?;
        }

        // The files whose size or modification time changed, with the track
        // their stored row belongs to. A full scan probes every file.
        let mut changed: Vec<(FoundFile, Option<EntityId>)> = Vec::new();
        for file in found {
            let state = stored.get(&file.rel_path).copied();
            if !request.full
                && let Some(state) = state
                && state.size == file.size
                && state.mtime_ms == file.mtime_ms
            {
                report.files_skipped += 1;
                self.count_files(1);
                continue;
            }
            changed.push((file, state.map(|state| state.track_id)));
        }

        // Probing a file is mostly waiting on the disk and on `lofty`, so
        // several files are probed at once; the writes stay batched.
        let mut pending: Vec<PendingFile> = Vec::with_capacity(WRITE_BATCH);
        let mut probes = futures_util::stream::iter(changed)
            .map(|(file, track_id)| async move {
                let probe = hearthsound_media::probe(&file.absolute).await;
                (file, track_id, probe)
            })
            .buffer_unordered(concurrency);
        while let Some((file, track_id, probe)) = probes.next().await {
            self.count_files(1);
            let probe = match probe {
                Ok(probe) => probe,
                Err(error) => {
                    tracing::warn!(
                        path = %file.absolute.display(),
                        %error,
                        "skipped a file that could not be read as audio"
                    );
                    report.files_failed += 1;
                    continue;
                }
            };
            pending.push(PendingFile {
                found: file,
                probe,
                track_id,
            });
            if pending.len() >= WRITE_BATCH {
                self.flush(&folder, separators, &mut pending, &mut report)
                    .await?;
                self.progress(folder.id, "indexing", &report, None);
            }
        }
        self.flush(&folder, separators, &mut pending, &mut report)
            .await?;

        self.progress(folder.id, "pruning", &report, None);
        if prunable {
            self.prune(&folder, &seen, &subtrees, &nested, &mut report)
                .await?;
        } else {
            // The rows stay until a scan sees content again.
            report.prune_refused = true;
            tracing::warn!(
                folder = %folder.id,
                path = %folder.path.display(),
                files = rows_at_risk,
                "refused to prune a folder whose root is empty; the disk may be unmounted"
            );
        }
        Ok(report)
    }

    /// Writes a batch of probed files in one transaction.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
    /// - `separators`: the separators that split a single-valued artist tag.
    /// - `pending`: the batch; drained as it is written.
    /// - `report`: the counters to update.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a statement fails; the transaction is rolled
    /// back, so the batch can be retried.
    async fn flush(
        &self,
        folder: &LibraryFolder,
        separators: &[String],
        pending: &mut Vec<PendingFile>,
        report: &mut ScanReport,
    ) -> Result<(), LibraryError> {
        if pending.is_empty() {
            return Ok(());
        }
        // The batch upserts albums and tracks, each of which reads before it
        // writes, so the transaction starts as a write; see
        // [`READ_THEN_WRITE`].
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let mut added: Vec<EntityId> = Vec::new();
        for file in pending.drain(..) {
            let tags = IndexedTags::from_probe(&file.probe, &file.found.rel_path, separators);
            let album_id = match &tags.album {
                Some(album) => {
                    Some(albums::upsert_album(&mut tx, album, &tags.album_credits).await?)
                }
                None => None,
            };
            let mut track_input = tags.track;
            track_input.album_id = album_id;
            let refreshed = match file.track_id {
                Some(id) => {
                    tracks::update_track(&mut tx, id, &track_input, &tags.credits, &tags.genres)
                        .await?
                }
                None => false,
            };
            let track_id = match file.track_id {
                Some(id) if refreshed => id,
                _ => {
                    let id =
                        tracks::insert_track(&mut tx, &track_input, &tags.credits, &tags.genres)
                            .await?;
                    added.push(id);
                    id
                }
            };
            tracks::set_track_availability(&mut tx, track_id, Availability::Local, None).await?;
            media_files::upsert_folder_media_file(
                &mut tx,
                track_id,
                &scanned_file(folder.id, &file),
            )
            .await?;
            if folder.owner_user_id.is_some() {
                retention::add_pin_reason(&mut tx, track_id, PinReason::Folder).await?;
            }
            report.files_indexed += 1;
        }
        tx.commit().await?;
        for track_id in added {
            self.publish_track_change(track_id, LibraryChange::Added);
        }
        Ok(())
    }

    /// Takes over the rows an enclosing folder holds under this folder's root.
    ///
    /// A file belongs to the deepest folder that contains it, so the rows an
    /// enclosing folder holds for the files under this root are this folder's:
    /// re-pointing them, rather than indexing the same file again, keeps the
    /// track and with it the likes, saves, ratings, playlist entries and plays
    /// the track carries.
    ///
    /// Three cases are handled together:
    ///
    /// - a file this folder has no row for is adopted, so its row keeps its id
    ///   and its track, and an owned folder pins the adopted track as it pins
    ///   the files it indexes;
    /// - a row whose file is gone is removed, exactly as the prune removes a
    ///   row this folder holds for a vanished file;
    /// - a second row for the same file - what a database indexed before this
    ///   rule looks like - is removed, keeping the enclosing folder's track,
    ///   which is the older of the two and the one a like or a playlist entry
    ///   was made against while the file had no deeper owner.
    ///
    /// The rows are written in batches of [`DELETE_BATCH`], one short
    /// transaction each.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
    /// - `inherited`: the rows the enclosing folders hold under this root, the
    ///   deepest folder's first.
    /// - `stored`: this folder's rows, updated with the rows taken over, so the
    ///   rest of the scan treats an adopted file as already indexed.
    /// - `seen`: the relative paths the walk found.
    /// - `subtrees`: the directories the walk was restricted to; empty means
    ///   the whole folder. A row outside them is left alone, exactly as the
    ///   prune leaves a row outside them alone.
    /// - `report`: the counters to update.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a statement fails; the batch is rolled
    /// back, so it can be retried.
    async fn take_over(
        &self,
        folder: &LibraryFolder,
        inherited: Vec<InheritedFile>,
        stored: &mut HashMap<String, media_files::StoredFileState>,
        seen: &HashSet<String>,
        subtrees: &[String],
        report: &mut ScanReport,
    ) -> Result<(), LibraryError> {
        if inherited.is_empty() {
            return Ok(());
        }
        // The row of the deepest enclosing folder keeps the file: the rows a
        // folder further out holds for it are the duplicates.
        let mut claimed: HashSet<&str> = HashSet::with_capacity(inherited.len());
        for chunk in inherited.chunks(DELETE_BATCH) {
            // The batch reads before it writes; see [`READ_THEN_WRITE`].
            let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
            let mut removed: Vec<EntityId> = Vec::new();
            for file in chunk {
                if !in_subtrees(&file.rel_path, subtrees) {
                    continue;
                }
                if !seen.contains(&file.rel_path) || !claimed.insert(file.rel_path.as_str()) {
                    self.drop_media_file(&mut tx, file.state, report, &mut removed)
                        .await?;
                    continue;
                }
                // The row this folder already holds for the file is the
                // duplicate: the enclosing folder's row is the older one.
                if let Some(duplicate) = stored.remove(&file.rel_path) {
                    self.drop_media_file(&mut tx, duplicate, report, &mut removed)
                        .await?;
                }
                media_files::repoint_folder_media_file(
                    &mut tx,
                    file.state.id,
                    folder.id,
                    &file.rel_path,
                )
                .await?;
                // An owned folder's content is permanent in its owner's
                // library, and an adopted file is not written by the flush that
                // pins the files it indexes.
                if folder.owner_user_id.is_some() {
                    retention::add_pin_reason(&mut tx, file.state.track_id, PinReason::Folder)
                        .await?;
                }
                stored.insert(file.rel_path.clone(), file.state);
                tracing::debug!(
                    folder = %folder.id,
                    path = %file.rel_path,
                    track = %file.state.track_id,
                    "took over a row an enclosing folder indexed"
                );
            }
            tx.commit().await?;
            for track_id in removed {
                self.publish_track_change(track_id, LibraryChange::Removed);
            }
        }
        Ok(())
    }

    /// Removes one media file row and the track it leaves with no files.
    ///
    /// The row is re-read first, because a folder removal may have taken it
    /// while the scan was walking. A track left with no files is deleted when
    /// it was created by scanning and demoted to `remote` when it came from the
    /// catalog.
    ///
    /// # Parameters
    ///
    /// - `tx`: the transaction to write on.
    /// - `state`: the row as the scan read it.
    /// - `report`: the counters to update.
    /// - `removed`: the tracks deleted, for the caller's events.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a statement fails.
    async fn drop_media_file(
        &self,
        tx: &mut SqliteConnection,
        state: media_files::StoredFileState,
        report: &mut ScanReport,
        removed: &mut Vec<EntityId>,
    ) -> Result<(), LibraryError> {
        if media_files::get_media_file(&mut *tx, state.id)
            .await?
            .is_none()
        {
            return Ok(());
        }
        media_files::delete_media_file(&mut *tx, state.id).await?;
        report.files_removed += 1;
        let remaining = media_files::list_media_files_for_track(&mut *tx, state.track_id).await?;
        if !remaining.is_empty() {
            return Ok(());
        }
        match tracks::get_track(&mut *tx, state.track_id).await? {
            Some(track) if track.origin == Origin::Local => {
                tracks::delete_track(&mut *tx, state.track_id).await?;
                report.tracks_removed += 1;
                removed.push(state.track_id);
            }
            Some(_) => {
                tracks::set_track_availability(
                    &mut *tx,
                    state.track_id,
                    Availability::Remote,
                    None,
                )
                .await?;
            }
            None => {}
        }
        Ok(())
    }

    /// Removes the rows of files that are gone from the folder.
    ///
    /// Only the rows inside the scanned subtrees are considered, so a scan of
    /// one directory never removes a row outside it, and only the rows this
    /// folder owns are considered: a row under the root of a folder inside it
    /// is that folder's to take over or remove, and pruning it here would
    /// delete the track the nested folder adopts.
    ///
    /// The rows are read once and then removed in batches of [`DELETE_BATCH`],
    /// one short transaction each: a prune of a large folder must not hold the
    /// single writer connection while a like or a save from another request
    /// waits for it. Each batch re-reads the row it removes, because a folder
    /// removal may have taken it in the meantime.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
    /// - `seen`: the relative paths the walk found.
    /// - `subtrees`: the directories the walk was restricted to; empty means
    ///   the whole folder.
    /// - `nested`: the nested library folders' paths, relative to the folder
    ///   root, whose rows this folder leaves alone.
    /// - `report`: the counters to update.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a statement fails.
    async fn prune(
        &self,
        folder: &LibraryFolder,
        seen: &HashSet<String>,
        subtrees: &[String],
        nested: &[String],
        report: &mut ScanReport,
    ) -> Result<(), LibraryError> {
        let mut read = self.readers.acquire().await?;
        let existing = media_files::list_folder_media_files(&mut read, folder.id).await?;
        drop(read);
        let missing: Vec<media_files::StoredFileState> = existing
            .iter()
            .filter(|(rel_path, _)| {
                !seen.contains(*rel_path)
                    && in_subtrees(rel_path, subtrees)
                    && !inside_nested_root(rel_path, nested)
            })
            .map(|(_, state)| *state)
            .collect();

        for chunk in missing.chunks(DELETE_BATCH) {
            // The batch reads before it writes; see [`READ_THEN_WRITE`].
            let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
            let mut removed: Vec<EntityId> = Vec::new();
            for state in chunk {
                self.drop_media_file(&mut tx, *state, report, &mut removed)
                    .await?;
            }
            tx.commit().await?;
            for track_id in removed {
                self.publish_track_change(track_id, LibraryChange::Removed);
            }
        }
        Ok(())
    }

    /// Publishes a `scan.progress` event.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder being scanned.
    /// - `phase`: the phase name; `done` is the terminal phase of a scan that
    ///   finished, with or without a problem.
    /// - `report`: the counters so far.
    /// - `error`: the problem the scan ended with, when it ended with one.
    fn progress(&self, folder_id: Uuid, phase: &str, report: &ScanReport, error: Option<String>) {
        self.events.publish_for(
            Audience::Admins,
            ServerEvent::ScanProgress(ScanProgress {
                folder_id: folder_id.to_string(),
                phase: phase.to_owned(),
                files_seen: report.files_seen,
                files_done: report.files_indexed + report.files_skipped,
                error,
            }),
        );
    }

    /// Publishes the terminal `scan.progress` event of a scan that failed.
    ///
    /// The phase is `failed`, never `done`, so a client can tell a scan that
    /// did not finish from one that finished with a warning.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder whose scan failed.
    /// - `error`: what the failure was.
    fn progress_failed(&self, folder_id: Uuid, error: String) {
        self.progress(folder_id, "failed", &ScanReport::default(), Some(error));
    }

    /// Publishes a `library.changed` event for a track.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track that changed.
    /// - `change`: whether it was added or removed.
    fn publish_track_change(&self, track_id: EntityId, change: LibraryChange) {
        self.events
            .publish(ServerEvent::LibraryChanged(LibraryChanged {
                entity_kind: EntityKind::Track,
                entity_id: track_id.to_string(),
                change,
            }));
    }
}

impl IndexedTags {
    /// Builds the rows a probe implies.
    ///
    /// The title falls back to the file stem when the tags carry none. The
    /// album is only created when the tags name one, and its primary credit is
    /// the album artist when the tags carry one, otherwise the track's first
    /// artist. Track credits are the tagged artists in order, the first one as
    /// `main` and the rest as `featured`.
    ///
    /// A single-valued artist or album artist tag is split on the configured
    /// separators first, so one value that lists several artists becomes
    /// several credits; a multi-valued tag is used as it stands.
    ///
    /// # Parameters
    ///
    /// - `probe`: the probe result.
    /// - `rel_path`: the path relative to the folder root, used for the title
    ///   fallback.
    /// - `separators`: the separators that split a single-valued artist tag.
    ///
    /// # Returns
    ///
    /// The inputs to write.
    pub(crate) fn from_probe(
        probe: &ProbeResult,
        rel_path: &str,
        separators: &[String],
    ) -> IndexedTags {
        let tags = &probe.tags;
        let stem = Path::new(rel_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(rel_path);
        let title = match tags.title.as_deref() {
            Some(title) if !title.trim().is_empty() => title.to_owned(),
            _ => stem.to_owned(),
        };
        let artists = split_credit(&tags.artists, separators);
        let mut album_artists = match &tags.album_artist {
            Some(name) => split_artists(name, separators),
            None => Vec::new(),
        };
        // An album is a compilation when the file says so or when its album
        // artist is `Various Artists`, which is how a tagger spells the same
        // thing. A compilation the file gives no album artist for is credited
        // to `Various Artists`, so its tracks do not land under one of the
        // performers.
        let compilation = tags.compilation == Some(true)
            || album_artists
                .iter()
                .any(|name| name.eq_ignore_ascii_case(VARIOUS_ARTISTS));
        if album_artists.is_empty() {
            album_artists = if compilation {
                vec![VARIOUS_ARTISTS.to_owned()]
            } else {
                artists.iter().take(1).cloned().collect()
            };
        }
        let display_artist = join_credit(&artists);

        let mut credits: Vec<ArtistCredit> = artists
            .iter()
            .enumerate()
            .map(|(position, name)| {
                credit(
                    name,
                    if position == 0 { "main" } else { "featured" },
                    artist_mbid(&tags.mb_artist_ids, artists.len(), position),
                )
            })
            .collect();
        if credits.is_empty() {
            credits = album_credits_for(&album_artists, &tags.mb_album_artist_ids);
        }

        let genres: Vec<String> = tags
            .genres
            .iter()
            .map(|genre| genre.trim().to_owned())
            .filter(|genre| !genre.is_empty())
            .collect();

        let album = tags
            .album
            .as_deref()
            .filter(|album| !album.trim().is_empty())
            .map(|album_title| {
                let mut album = AlbumInput::new(album_title, Origin::Local);
                album.display_artist = join_credit(&album_artists);
                album.year = tags.year().map(i64::from);
                album.release_date = tags.date.clone();
                album.mb_release_id = tags.mb_release_id.clone();
                album.mb_release_group_id = tags.mb_release_group_id.clone();
                album.album_type = compilation.then(|| COMPILATION_ALBUM_TYPE.to_owned());
                album
            });
        let album_credits = match album {
            Some(_) => album_credits_for(&album_artists, &tags.mb_album_artist_ids),
            None => Vec::new(),
        };

        let mut track = TrackInput::new(title, Origin::Local, Availability::Local);
        track.display_artist = display_artist;
        track.disc_number = tags.disc_number.map(i64::from);
        track.track_number = tags.track_number.map(i64::from);
        track.duration_ms = Some(probe.duration_ms as i64);
        track.isrc = tags.isrc.clone();
        track.mb_recording_id = tags.mb_recording_id.clone();

        IndexedTags {
            album,
            album_credits,
            track,
            credits,
            genres,
        }
    }
}

/// Splits one artist list into credits.
///
/// A tag that carries one artist is a single value that may list several
/// artists, so it is split on the configured separators. A tag that already
/// carries several values is used as it stands: its authors separated them
/// themselves.
///
/// # Parameters
///
/// - `artists`: the artists the tags carried.
/// - `separators`: the separators that split a single-valued artist tag.
///
/// # Returns
///
/// The artists to credit, in order.
fn split_credit(artists: &[String], separators: &[String]) -> Vec<String> {
    match artists {
        [single] => split_artists(single, separators),
        several => several.to_vec(),
    }
}

/// Builds the credits of an album artist list.
///
/// # Parameters
///
/// - `artists`: the album artists, in order.
/// - `ids`: the MusicBrainz album artist ids the tags carried.
///
/// # Returns
///
/// One credit per artist, the first as `main` and the rest as `featured`.
fn album_credits_for(artists: &[String], ids: &[String]) -> Vec<ArtistCredit> {
    artists
        .iter()
        .enumerate()
        .map(|(position, name)| {
            credit(
                name,
                if position == 0 { "main" } else { "featured" },
                artist_mbid(ids, artists.len(), position),
            )
        })
        .collect()
}

/// Joins an artist list the way a display name spells it.
///
/// # Parameters
///
/// - `artists`: the credited artists, in order.
///
/// # Returns
///
/// The names joined with `, `, or `None` when there are none.
fn join_credit(artists: &[String]) -> Option<String> {
    if artists.is_empty() {
        None
    } else {
        Some(artists.join(", "))
    }
}

/// Builds an artist credit, carrying the MusicBrainz id when the file gave one.
///
/// # Parameters
///
/// - `name`: the credited artist's name.
/// - `role`: the credit role.
/// - `mbid`: the MusicBrainz artist id, when the file identifies the credit.
///
/// # Returns
///
/// The credit to write.
fn credit(name: &str, role: &str, mbid: Option<String>) -> ArtistCredit {
    let mut artist = ArtistInput::new(name);
    if let Some(mbid) = mbid {
        artist = artist.mb_artist_id(mbid);
    }
    ArtistCredit {
        artist,
        role: role.to_owned(),
    }
}

/// Picks the MusicBrainz id that belongs to one credit position.
///
/// A file that carries one id per credited artist maps them by position. A file
/// that carries a single id while naming several artists only identifies the
/// first one, so the later credits stay unknown. Any other count is ambiguous
/// and identifies nobody.
///
/// # Parameters
///
/// - `ids`: the ids the file carried.
/// - `artists`: how many artists the file credits.
/// - `position`: the credit's zero-based position.
///
/// # Returns
///
/// The id, or `None` when the file does not identify this credit.
fn artist_mbid(ids: &[String], artists: usize, position: usize) -> Option<String> {
    if ids.len() == artists {
        return ids.get(position).cloned();
    }
    if ids.len() == 1 && position == 0 {
        return ids.first().cloned();
    }
    None
}

/// Builds the media file row for a probed file.
///
/// # Parameters
///
/// - `folder_id`: the folder the file lives in.
/// - `file`: the pending file.
///
/// # Returns
///
/// The row to write.
fn scanned_file(folder_id: Uuid, file: &PendingFile) -> ScannedMediaFile {
    let probe = &file.probe;
    ScannedMediaFile {
        folder_id,
        rel_path: file.found.rel_path.clone(),
        size: file.found.size,
        mtime_ms: file.found.mtime_ms,
        format: Some(probe.container.as_str().to_owned()),
        codec: Some(probe.codec.as_str().to_owned()),
        bitrate_kbps: probe.bitrate_kbps.map(i64::from),
        sample_rate: probe.sample_rate.map(i64::from),
        bit_depth: probe.bit_depth.map(i64::from),
        channels: probe.channels.map(i64::from),
        duration_ms: Some(probe.duration_ms as i64),
        lossless: Some(probe.lossless),
        replaygain: ReplayGain::from_tags(&probe.tags),
        has_embedded_art: Some(probe.has_embedded_art),
    }
}

/// What one walk found, and what it could not read.
#[derive(Debug, Default)]
struct WalkOutcome {
    /// The audio files the walk found.
    found: Vec<FoundFile>,
    /// How many entries the walk could not read and skipped.
    unreadable: u64,
    /// The first entry the walk could not read.
    unreadable_path: Option<PathBuf>,
    /// Whether the whole folder was walked and its root held no entry at all.
    root_empty: bool,
}

impl WalkOutcome {
    /// Records that an entry could not be read.
    ///
    /// # Parameters
    ///
    /// - `path`: the entry that was skipped.
    fn note_unreadable(&mut self, path: &Path) {
        self.unreadable += 1;
        if self.unreadable_path.is_none() {
            self.unreadable_path = Some(path.to_path_buf());
        }
    }
}

/// Collects the rows the enclosing folders hold under a folder's root.
///
/// A file belongs to the deepest folder that contains it, so every row an
/// enclosing folder holds for a file under this root belongs to this folder's
/// scan to keep: it re-points the row instead of indexing the same file again.
/// Only the paths below the folder's root are read, so a scan of a small folder
/// inside a big one costs what the small folder holds, not what the big one
/// does.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `known`: every library folder.
/// - `folder`: the folder whose scan is about to run.
///
/// # Returns
///
/// One entry per row, the rows of the deepest enclosing folder first, so the
/// first row for a path is the one that keeps the track. A row whose path is
/// not expressible as a UTF-8 path relative to the folder is left out: no
/// walked file can carry such a path either.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when a query fails.
async fn inherited_files(
    conn: &mut SqliteConnection,
    known: &[LibraryFolder],
    folder: &LibraryFolder,
) -> Result<Vec<InheritedFile>, LibraryError> {
    let mut inherited: Vec<InheritedFile> = Vec::new();
    for enclosing in folders::enclosing_folders(known, folder) {
        let Ok(inside) = folder.path.strip_prefix(&enclosing.path) else {
            continue;
        };
        let Some(prefix) = inside.to_str() else {
            continue;
        };
        let rows = media_files::list_folder_media_files_under(conn, enclosing.id, prefix).await?;
        for (rel_path, state) in rows {
            let Some(under) = rel_path
                .strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix('/'))
            else {
                continue;
            };
            inherited.push(InheritedFile {
                rel_path: under.to_owned(),
                state,
            });
        }
    }
    Ok(inherited)
}

/// Walks a folder, or the listed subtrees of it, and returns its audio files.
///
/// The walk follows symbolic links, including ones whose target is outside the
/// folder: the administrator chose the folder, so a link into it is part of it.
/// A directory the walk already visited is skipped, so a link cycle cannot loop,
/// and the subdirectories that are library folders of their own are skipped, so
/// each file is indexed by the deepest folder that contains it.
///
/// An entry the walk cannot read - a directory it has no permission for, an
/// entry that vanished between listing and stating, a link that does not
/// resolve, a name that is not valid UTF-8 - is logged, counted, and skipped, so
/// one bad directory never costs the whole folder its scan. Hidden files and
/// directories (names starting with a dot) are skipped. Directories are visited
/// in the order the filesystem lists them; the result is sorted by relative path
/// so a scan is reproducible, and a file reachable through two subtrees is
/// returned once.
///
/// # Parameters
///
/// - `root`: the folder root, which must be a directory.
/// - `subtrees`: the directories to walk, relative to the root and already
///   normalized; empty walks the whole folder. A subtree that does not exist or
///   is not a directory contributes nothing.
/// - `skip`: the absolute canonical paths of the nested library folders of
///   other folders, which the walk does not descend into, so each file is
///   indexed by the deepest folder that contains it.
///
/// # Returns
///
/// Every recognised audio file under the walked directories with its path
/// relative to the root, and what the walk could not read.
///
/// # Errors
///
/// Returns [`LibraryError::Scan`] when the folder root itself cannot be read as
/// a directory, which is what a deleted folder or a path replaced by a file
/// looks like. Nothing inside the folder fails the walk.
fn walk(root: &Path, subtrees: &[String], skip: &[PathBuf]) -> Result<WalkOutcome, LibraryError> {
    let mut outcome = WalkOutcome::default();
    // Reading the root is both the existence check and the emptiness check: an
    // existing empty root reads fine with no entry, which is what an unmounted
    // bind mount looks like, while a missing root or a path that is a file
    // fails here.
    let mut root_entries = std::fs::read_dir(root).map_err(|source| LibraryError::Scan {
        path: root.to_path_buf(),
        source,
    })?;
    outcome.root_empty = subtrees.is_empty() && root_entries.next().is_none();
    drop(root_entries);

    let mut visited: HashSet<PathBuf> = HashSet::new();
    if subtrees.is_empty() {
        walk_into(root, root, skip, &mut visited, &mut outcome);
    } else {
        for subtree in subtrees {
            let start = root.join(subtree);
            if !start.is_dir() {
                continue;
            }
            let canonical = std::fs::canonicalize(&start).unwrap_or_else(|_| start.clone());
            if skip
                .iter()
                .any(|nested| start == *nested || canonical == *nested)
            {
                continue;
            }
            walk_into(&start, root, skip, &mut visited, &mut outcome);
        }
    }
    outcome
        .found
        .sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    outcome
        .found
        .dedup_by(|left, right| left.rel_path == right.rel_path);
    Ok(outcome)
}

/// Walks one directory tree and appends its audio files.
///
/// # Parameters
///
/// - `start`: the directory to walk.
/// - `root`: the folder root every reported path is relative to.
/// - `skip`: the absolute paths of nested library folders.
/// - `visited`: the canonical directories already walked, which is what stops a
///   link cycle.
/// - `outcome`: the files and counters, appended to.
fn walk_into(
    start: &Path,
    root: &Path,
    skip: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
    outcome: &mut WalkOutcome,
) {
    match std::fs::canonicalize(start) {
        Ok(canonical) => {
            visited.insert(canonical);
        }
        Err(error) => {
            tracing::warn!(
                path = %start.display(),
                %error,
                "skipped a directory the scan cannot read"
            );
            outcome.note_unreadable(start);
            return;
        }
    }
    let mut directories = vec![start.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    path = %directory.display(),
                    %error,
                    "skipped a directory the scan cannot read"
                );
                outcome.note_unreadable(&directory);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(
                        path = %directory.display(),
                        %error,
                        "skipped an entry the scan cannot read"
                    );
                    outcome.note_unreadable(&directory);
                    continue;
                }
            };
            if is_hidden(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            // Following the link decides what the entry is: a link to a
            // directory is walked, a link to a file is indexed, and a link that
            // does not resolve is counted and skipped.
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        "skipped an entry the scan cannot stat"
                    );
                    outcome.note_unreadable(&path);
                    continue;
                }
            };
            if metadata.is_dir() {
                if skip.contains(&path) {
                    continue;
                }
                let canonical = match std::fs::canonicalize(&path) {
                    Ok(canonical) => canonical,
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "skipped a directory the scan cannot read"
                        );
                        outcome.note_unreadable(&path);
                        continue;
                    }
                };
                // A nested folder that is itself a link is skipped by the
                // canonical path as well.
                if skip.contains(&canonical) {
                    continue;
                }
                if !visited.insert(canonical) {
                    tracing::debug!(
                        path = %path.display(),
                        "skipped a directory the walk already visited; a link points back into the tree"
                    );
                    continue;
                }
                directories.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if !is_audio_extension(extension) {
                continue;
            }
            // Both sides are built from the root, so a failure here is
            // unreachable; skipping keeps a stray absolute path out of the
            // table, because a stored `rel_path` is always relative.
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let Some(rel_path) = relative.to_str() else {
                tracing::warn!(
                    path = %path.display(),
                    "skipped a file whose name is not valid UTF-8"
                );
                outcome.note_unreadable(&path);
                continue;
            };
            outcome.found.push(FoundFile {
                rel_path: rel_path.to_owned(),
                absolute: path,
                size: metadata.len() as i64,
                mtime_ms: modified_ms(&metadata),
            });
        }
    }
}

/// Normalizes the subtree paths of a request.
///
/// A path is `/`-separated and relative to the folder root. Empty parts and `.`
/// are dropped, a `..` removes the part before it, and a `..` that would leave
/// the folder discards the whole path, so a request can never name a directory
/// outside the folder. A path that normalizes to nothing is dropped.
///
/// # Parameters
///
/// - `paths`: the paths the request carried.
///
/// # Returns
///
/// The normalized subtrees, in the order they were given and without
/// duplicates.
fn subtrees(paths: &[String]) -> Vec<String> {
    let mut subtrees: Vec<String> = Vec::with_capacity(paths.len());
    for path in paths {
        let Some(subtree) = normalize_subtree(path) else {
            continue;
        };
        if !subtrees.contains(&subtree) {
            subtrees.push(subtree);
        }
    }
    subtrees
}

/// Normalizes one subtree path.
///
/// # Parameters
///
/// - `path`: the path from a scan request.
///
/// # Returns
///
/// The normalized path, or `None` when it names the folder root itself or would
/// leave the folder.
fn normalize_subtree(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Reports whether a stored path is inside one of the scanned subtrees.
///
/// # Parameters
///
/// - `rel_path`: the stored path, relative to the folder root.
/// - `subtrees`: the normalized subtrees; empty means the whole folder.
///
/// # Returns
///
/// `true` when the path is inside a subtree or the whole folder is scanned.
fn in_subtrees(rel_path: &str, subtrees: &[String]) -> bool {
    subtrees.is_empty()
        || subtrees
            .iter()
            .any(|subtree| match rel_path.strip_prefix(subtree) {
                Some(rest) => rest.starts_with('/'),
                None => false,
            })
}

/// Reports whether a stored path is inside a directory that is a library folder
/// of its own.
///
/// # Parameters
///
/// - `rel_path`: the stored path, relative to the folder root.
/// - `nested`: the nested library folders' paths, relative to the folder root.
///
/// # Returns
///
/// `true` when the path is at or under one of them.
fn inside_nested_root(rel_path: &str, nested: &[String]) -> bool {
    nested.iter().any(|root| {
        rel_path
            .strip_prefix(root.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// Reports whether an extension names an audio file the scanner recognises.
///
/// # Parameters
///
/// - `extension`: the file extension without the leading dot.
///
/// # Returns
///
/// `true` when the extension is one of [`AUDIO_EXTENSIONS`], compared
/// case-insensitively.
pub(crate) fn is_audio_extension(extension: &str) -> bool {
    AUDIO_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
}

/// Reports whether a directory entry is hidden.
///
/// Hidden entries are skipped by the walk. This covers the `._<name>`
/// AppleDouble metadata files macOS writes next to every file on a foreign
/// filesystem, which carry an audio extension but hold no audio, as well as
/// hidden directories such as `.Trashes` or `.AppleDouble`.
///
/// # Parameters
///
/// - `name`: the entry's file name.
///
/// # Returns
///
/// `true` when the name starts with a dot.
fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

/// Reads a file's modification time in unix milliseconds.
///
/// # Parameters
///
/// - `metadata`: the file metadata.
///
/// # Returns
///
/// The modification time, or `0` when the filesystem does not report one.
fn modified_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Enqueues a scan without this process's scan counters.
///
/// The command line adds a folder in a process of its own, so it has no scanner
/// to count against. The server's workers claim the job - or, when no server is
/// running, the next server does - and the running server's scan status counts it
/// from the moment it is claimed.
///
/// # Parameters
///
/// - `queue`: the queue to enqueue on.
/// - `request`: the folders, subtrees, and depth to scan.
///
/// # Returns
///
/// The enqueued job.
///
/// # Errors
///
/// Returns [`LibraryError::Queue`] when the request cannot be serialized or the
/// job cannot be written.
pub async fn enqueue_scan(queue: &JobQueue, request: &ScanRequest) -> Result<Job, LibraryError> {
    let job = NewJob::new(SCAN_JOB_KIND).payload(request)?;
    Ok(queue.enqueue(job).await?)
}

/// Runs a `library.scan` job.
///
/// The payload is a [`ScanRequest`]; a job with no payload scans every folder.
#[derive(Clone)]
pub struct ScanJobHandler {
    /// The scanner the job runs.
    scanner: Arc<LibraryScanner>,
}

impl ScanJobHandler {
    /// Builds the handler.
    ///
    /// # Parameters
    ///
    /// - `scanner`: the scanner to run.
    pub fn new(scanner: Arc<LibraryScanner>) -> ScanJobHandler {
        ScanJobHandler { scanner }
    }
}

#[async_trait]
impl JobHandler for ScanJobHandler {
    /// Runs one scan.
    ///
    /// A scan of a folder that no longer exists is nothing to retry - the row
    /// was removed between the request and the claim, and the queue cannot drop
    /// a job by its payload - so it ends quietly. A malformed payload is an
    /// error, and it still releases the scanner's queued counter, because a
    /// status that reads `scanning` until the next restart is worse than the
    /// failed job.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the payload is malformed or the scan fails.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let request: ScanRequest = match job.payload_json() {
            Ok(payload) => payload.unwrap_or_default(),
            Err(error) => {
                self.scanner.request_started();
                return Err(JobError::new(error.to_string()));
            }
        };
        // The scan counts as running before its request stops counting as
        // queued, so a status read in between never sees an idle scanner.
        let _running = self.scanner.begin_scan();
        self.scanner.request_started();
        match self.scanner.scan(&request).await {
            Ok(_) => Ok(()),
            Err(LibraryError::UnknownFolder { folder_id }) => {
                tracing::info!(
                    folder = %folder_id,
                    "dropped a queued scan of a folder that no longer exists"
                );
                Ok(())
            }
            Err(error) => Err(JobError::new(error.to_string())),
        }
    }
}
