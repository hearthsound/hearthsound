//! Scanning library folders and indexing their tags.
//!
//! A scan walks a folder, recognises the audio files in it, probes each one for
//! its format and tags, and writes the artists, albums, tracks, and media files
//! it finds. It never writes into the folder: a scanned folder is read-only
//! input, and only the database rows describing it change.
//!
//! The scan is incremental. A file whose size and modification time match the
//! stored media file is skipped without probing, a file whose size or
//! modification time changed has its probe columns and its track refreshed, and
//! a file that disappeared has its media file row removed. A track left with no
//! files at all is deleted when it was created by scanning and demoted to
//! `remote` when it came from the catalog.
//!
//! Probing never happens inside a write transaction. The walk is blocking work
//! and runs on a blocking thread; the probes are asynchronous and run outside
//! any transaction; the writes are grouped into short transactions so the
//! shared writer's window stays small.
//!
//! A folder with an owner pins its tracks with the `folder` reason in the same
//! transaction that writes them, which is what makes an owned folder's content
//! permanent in its owner's personal library.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use hearthsound_core::events::{
    EventBus, LibraryChange, LibraryChanged, ScanProgress, ServerEvent,
};
use hearthsound_core::{Availability, EntityId, EntityKind, PinReason};
use hearthsound_db::Database;
use hearthsound_jobs::{Job, JobError, JobHandler};
use hearthsound_media::ProbeResult;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{
    AlbumInput, ArtistCredit, ArtistInput, LibraryFolder, Origin, ScannedMediaFile, TrackInput,
};
use crate::{READ_THEN_WRITE, albums, folders, media_files, now_millis, retention, tracks};

/// The job kind a library scan is enqueued under.
pub const SCAN_JOB_KIND: &str = "library.scan";

/// How many files one write transaction covers.
///
/// Small enough that the writer's window stays short, large enough that a scan
/// of a big folder does not commit once per file.
const WRITE_BATCH: usize = 64;

/// The file extensions the scanner treats as audio.
const AUDIO_EXTENSIONS: [&str; 14] = [
    "aac", "aif", "aiff", "ape", "flac", "m4a", "mp3", "mp4", "mpc", "oga", "ogg", "opus", "wav",
    "wv",
];

/// The JSON payload of a `library.scan` job.
///
/// # Examples
///
/// ```
/// use hearthsound_library::scan::ScanRequest;
///
/// let request = ScanRequest::all();
/// assert_eq!(request.folder_id, None);
/// assert_eq!(serde_json::to_string(&request).unwrap(), "{\"folderId\":null}");
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanRequest {
    /// The folder to scan, or `None` to scan every folder.
    pub folder_id: Option<Uuid>,
}

impl ScanRequest {
    /// Builds a request that scans every folder.
    pub fn all() -> ScanRequest {
        ScanRequest { folder_id: None }
    }

    /// Builds a request that scans one folder.
    ///
    /// # Parameters
    ///
    /// - `folder_id`: the folder to scan.
    pub fn folder(folder_id: Uuid) -> ScanRequest {
        ScanRequest {
            folder_id: Some(folder_id),
        }
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
    /// How many media files were removed because the file is gone.
    pub files_removed: u64,
    /// How many locally created tracks were removed because they lost their last
    /// file.
    pub tracks_removed: u64,
}

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
struct IndexedTags {
    /// The album to upsert, when the tags name one.
    album: Option<AlbumInput>,
    /// The album's credits.
    album_credits: Vec<ArtistCredit>,
    /// The track to insert or update.
    track: TrackInput,
    /// The track's credits.
    credits: Vec<ArtistCredit>,
    /// The track's genres.
    genres: Vec<String>,
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
/// use hearthsound_library::scan::LibraryScanner;
///
/// # async fn example(db: &Database, folder_id: uuid::Uuid) -> Result<(), Box<dyn std::error::Error>> {
/// let scanner = LibraryScanner::new(db, Arc::new(EventBus::new()));
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
}

impl LibraryScanner {
    /// Builds a scanner over a database and an event bus.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `events`: the bus `scan.progress` and `library.changed` are published
    ///   on.
    ///
    /// # Returns
    ///
    /// The scanner.
    pub fn new(db: &Database, events: Arc<EventBus>) -> LibraryScanner {
        LibraryScanner {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            events,
        }
    }

    /// Scans every library folder.
    ///
    /// # Returns
    ///
    /// One report per folder, in the order the folders are listed.
    ///
    /// # Errors
    ///
    /// Returns the first folder's error. A scan is idempotent, so the caller can
    /// retry it after fixing what made the folder unreadable.
    pub async fn scan_all(&self) -> Result<Vec<ScanReport>, LibraryError> {
        let mut read = self.readers.acquire().await?;
        let folders = folders::list_library_folders(&mut read).await?;
        let mut reports = Vec::with_capacity(folders.len());
        for folder in folders {
            reports.push(self.scan_folder(folder.id).await?);
        }
        Ok(reports)
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
    /// be read, and [`LibraryError::Media`] when a probe fails.
    pub async fn scan_folder(&self, folder_id: Uuid) -> Result<ScanReport, LibraryError> {
        let mut read = self.readers.acquire().await?;
        let folder = folders::get_library_folder(&mut read, folder_id)
            .await?
            .ok_or(LibraryError::UnknownFolder { folder_id })?;

        let walk_root = folder.path.clone();
        let found = tokio::task::spawn_blocking(move || walk(&walk_root)).await??;

        let mut report = ScanReport {
            folder_id,
            files_seen: found.len() as u64,
            ..ScanReport::default()
        };
        self.progress(&folder, "indexing", &report, None);

        let mut seen: HashSet<String> = HashSet::with_capacity(found.len());
        let mut pending: Vec<PendingFile> = Vec::with_capacity(WRITE_BATCH);
        for file in found {
            seen.insert(file.rel_path.clone());
            let existing =
                media_files::find_folder_media_file(&mut read, folder.id, &file.rel_path).await?;
            if let Some(row) = &existing
                && row.size == file.size
                && row.mtime_ms == file.mtime_ms
            {
                report.files_skipped += 1;
                continue;
            }
            let probe = hearthsound_media::probe(&file.absolute).await?;
            pending.push(PendingFile {
                found: file,
                probe,
                track_id: existing.map(|row| row.track_id),
            });
            if pending.len() >= WRITE_BATCH {
                self.flush(&folder, &mut pending, &mut report).await?;
                self.progress(&folder, "indexing", &report, None);
            }
        }
        self.flush(&folder, &mut pending, &mut report).await?;

        self.progress(&folder, "pruning", &report, None);
        self.prune(&folder, &seen, &mut report).await?;

        let mut tx = self.writer.begin().await?;
        folders::set_library_folder_last_scan(&mut tx, folder.id, now_millis()).await?;
        tx.commit().await?;

        self.progress(&folder, "done", &report, None);
        Ok(report)
    }

    /// Writes a batch of probed files in one transaction.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
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
            let tags = IndexedTags::from_probe(&file.probe, &file.found.rel_path);
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

    /// Removes the rows of files that are gone from the folder.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
    /// - `seen`: the relative paths the walk found.
    /// - `report`: the counters to update.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a statement fails.
    async fn prune(
        &self,
        folder: &LibraryFolder,
        seen: &HashSet<String>,
        report: &mut ScanReport,
    ) -> Result<(), LibraryError> {
        // The prune lists the folder's files before it removes any, so the
        // transaction starts as a write; see [`READ_THEN_WRITE`].
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let existing = media_files::list_folder_media_files(&mut tx, folder.id).await?;
        let mut removed: Vec<EntityId> = Vec::new();
        for file in existing {
            if seen.contains(&file.rel_path) {
                continue;
            }
            media_files::delete_media_file(&mut tx, file.id).await?;
            report.files_removed += 1;
            let remaining = media_files::list_media_files_for_track(&mut tx, file.track_id).await?;
            if !remaining.is_empty() {
                continue;
            }
            match tracks::get_track(&mut tx, file.track_id).await? {
                Some(track) if track.origin == Origin::Local => {
                    tracks::delete_track(&mut tx, file.track_id).await?;
                    report.tracks_removed += 1;
                    removed.push(file.track_id);
                }
                Some(_) => {
                    tracks::set_track_availability(
                        &mut tx,
                        file.track_id,
                        Availability::Remote,
                        None,
                    )
                    .await?;
                }
                None => {}
            }
        }
        tx.commit().await?;
        for track_id in removed {
            self.publish_track_change(track_id, LibraryChange::Removed);
        }
        Ok(())
    }

    /// Publishes a `scan.progress` event.
    ///
    /// # Parameters
    ///
    /// - `folder`: the folder being scanned.
    /// - `phase`: the phase name.
    /// - `report`: the counters so far.
    /// - `error`: the error that stopped the scan, when it failed.
    fn progress(
        &self,
        folder: &LibraryFolder,
        phase: &str,
        report: &ScanReport,
        error: Option<String>,
    ) {
        self.events.publish(ServerEvent::ScanProgress(ScanProgress {
            folder_id: folder.id.to_string(),
            phase: phase.to_owned(),
            files_seen: report.files_seen,
            files_done: report.files_indexed + report.files_skipped,
            error,
        }));
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
    /// # Parameters
    ///
    /// - `probe`: the probe result.
    /// - `rel_path`: the path relative to the folder root, used for the title
    ///   fallback.
    ///
    /// # Returns
    ///
    /// The inputs to write.
    fn from_probe(probe: &ProbeResult, rel_path: &str) -> IndexedTags {
        let tags = &probe.tags;
        let stem = Path::new(rel_path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(rel_path);
        let title = match tags.title.as_deref() {
            Some(title) if !title.trim().is_empty() => title.to_owned(),
            _ => stem.to_owned(),
        };
        let album_artist = tags
            .album_artist
            .clone()
            .or_else(|| tags.artists.first().cloned());
        let display_artist = if tags.artists.is_empty() {
            None
        } else {
            Some(tags.artists.join(", "))
        };

        let mut credits: Vec<ArtistCredit> = tags
            .artists
            .iter()
            .enumerate()
            .map(|(position, name)| ArtistCredit {
                artist: ArtistInput::new(name),
                role: if position == 0 { "main" } else { "featured" }.to_owned(),
            })
            .collect();
        if credits.is_empty()
            && let Some(name) = &album_artist
        {
            credits.push(ArtistCredit::new(name, "main"));
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
                album.display_artist = album_artist.clone();
                album.year = tags.year().map(i64::from);
                album.release_date = tags.date.clone();
                album.mb_release_id = tags.mb_release_id.clone();
                album.mb_release_group_id = tags.mb_release_group_id.clone();
                album
            });
        let album_credits = match (&album, &album_artist) {
            (Some(_), Some(name)) => vec![ArtistCredit::new(name, "main")],
            (Some(_), None) => credits.clone(),
            (None, _) => Vec::new(),
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
        has_embedded_art: Some(probe.has_embedded_art),
    }
}

/// Walks a folder and returns its audio files.
///
/// The walk is depth-first and never follows a symbolic link, so it can never
/// leave the folder it started in. Directories are visited in the order the
/// filesystem lists them; the result is sorted by relative path so a scan is
/// reproducible.
///
/// # Parameters
///
/// - `root`: the folder root.
///
/// # Returns
///
/// Every recognised audio file under the root.
///
/// # Errors
///
/// Returns [`LibraryError::Scan`] when a directory or an entry cannot be read.
fn walk(root: &Path) -> Result<Vec<FoundFile>, LibraryError> {
    let mut found = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|source| LibraryError::Scan {
            path: directory.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| LibraryError::Scan {
                path: directory.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| LibraryError::Scan {
                path: entry.path(),
                source,
            })?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                directories.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
                continue;
            };
            if !is_audio_extension(extension) {
                continue;
            }
            let metadata = entry.metadata().map_err(|source| LibraryError::Scan {
                path: path.clone(),
                source,
            })?;
            let rel_path = path
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            found.push(FoundFile {
                rel_path,
                absolute: path,
                size: metadata.len() as i64,
                mtime_ms: modified_ms(&metadata),
            });
        }
    }
    found.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(found)
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
fn is_audio_extension(extension: &str) -> bool {
    AUDIO_EXTENSIONS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(extension))
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
    /// # Parameters
    ///
    /// - `job`: the claimed job.
    ///
    /// # Errors
    ///
    /// Returns [`JobError`] when the payload is malformed or the scan fails.
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let request: ScanRequest = job
            .payload_json()
            .map_err(|error| JobError::new(error.to_string()))?
            .unwrap_or_default();
        let result = match request.folder_id {
            Some(folder_id) => self.scanner.scan_folder(folder_id).await.map(|_| ()),
            None => self.scanner.scan_all().await.map(|_| ()),
        };
        result.map_err(|error| JobError::new(error.to_string()))
    }
}
