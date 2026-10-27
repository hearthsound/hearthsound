//! Uploads: a user's own files, ingested into the permanent library.
//!
//! An uploaded file belongs to the uploader. It is indexed exactly like a file
//! a scan finds - its tags are read, never rewritten, and become its track,
//! album and artist rows - but instead of staying in a scanned folder it is
//! moved into `<storage>/library` under `library.naming_template`. The track
//! gets a `user_library` row with origin `upload` for the uploader and the
//! `user-library` pin, so the file is permanent and shows in the uploader's
//! personal library, while every other user may stream it but finds it only
//! by searching or browsing the server.
//!
//! The caller writes the upload into a staging file first (the native API
//! streams it to `<storage>/uploads-staging/<upload-id>/`) and hands that
//! path to [`UploadIngest::ingest`]; the staging file is consumed on success
//! and left for the caller to remove on failure.
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
//! #     user: uuid::Uuid,
//! # ) -> Result<(), hearthsound_library::LibraryError> {
//! use hearthsound_library::upload::UploadIngest;
//!
//! let ingest = UploadIngest::new(db, roots, settings, events);
//! let stored = ingest
//!     .ingest(user, std::path::Path::new("/tmp/staged.flac"), "Song.flac")
//!     .await?;
//! println!("stored as {}", stored.rel_path);
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{EventBus, LibraryChange, LibraryChanged, ServerEvent};
use hearthsound_core::{Availability, EntityId, EntityKind, PinReason, StorageClass};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::index_queue;
use crate::maintenance::MaintenanceSignal;
use crate::model::{ManagedMediaFile, ReplayGain, StorageRoots};
use crate::naming::{NamingContext, NamingTemplate, NamingTemplateError};
use crate::scan::{IndexedTags, is_audio_extension};
use crate::{READ_THEN_WRITE, albums, media_files, now_millis, placement, retention, tracks};

/// What an ingested upload became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestedUpload {
    /// The track the file was indexed as.
    pub track_id: EntityId,
    /// The `media_files` row of the stored file.
    pub media_file_id: Uuid,
    /// The file's path relative to `<storage>/library`.
    pub rel_path: String,
}

/// Ingests uploaded files into the uploader's library.
///
/// Cloning is cheap: every clone shares the pools, the settings and the bus.
#[derive(Clone)]
pub struct UploadIngest {
    /// The writer pool every row goes through.
    writer: SqlitePool,
    /// The managed roots; an upload lands under the library root.
    roots: StorageRoots,
    /// The settings service, for the naming template and artist separators.
    settings: Settings,
    /// The bus `library.changed` is published on.
    events: Arc<EventBus>,
    /// Woken after an upload, because a stored file changes the storage usage.
    signal: MaintenanceSignal,
}

impl std::fmt::Debug for UploadIngest {
    /// Reports the type without the pools, the settings, or the bus.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UploadIngest")
    }
}

impl UploadIngest {
    /// Builds the ingest.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `roots`: the managed storage roots.
    /// - `settings`: the settings service.
    /// - `events`: the bus `library.changed` is published on.
    ///
    /// # Returns
    ///
    /// The ingest.
    pub fn new(
        db: &Database,
        roots: StorageRoots,
        settings: Settings,
        events: Arc<EventBus>,
    ) -> UploadIngest {
        UploadIngest {
            writer: db.writer().clone(),
            roots,
            settings,
            events,
            signal: MaintenanceSignal::new(),
        }
    }

    /// Wakes the given storage maintenance signal after every upload.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal the maintenance worker waits on.
    ///
    /// # Returns
    ///
    /// The ingest, notifying that signal.
    pub fn with_signal(mut self, signal: MaintenanceSignal) -> UploadIngest {
        self.signal = signal;
        self
    }

    /// Indexes and stores one uploaded file as the uploader's own.
    ///
    /// The file must carry an audio extension the scanner indexes and must
    /// probe as audio. Its tags become the track, album and artist rows the
    /// same way a scanned file's do, each upload its own track. The file moves
    /// from `staged` into `<storage>/library` at the path the naming template
    /// renders (a name something else holds moves to the next ` (N)` suffix),
    /// and in one transaction the rows, the uploader's `user_library` entry
    /// with origin `upload`, and the `user-library` pin are written. A failed
    /// transaction moves the file back to `staged`.
    ///
    /// # Parameters
    ///
    /// - `user`: the uploader.
    /// - `staged`: the staging file holding the upload.
    /// - `original_name`: the file name the uploader gave; its extension
    ///   decides whether the file is audio, and its stem names a track whose
    ///   tags carry no title.
    ///
    /// # Returns
    ///
    /// The track and stored file the upload became.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::UnsupportedUpload`] when the file is not audio
    /// the library indexes, [`LibraryError::Naming`] or
    /// [`LibraryError::Setting`] when a setting cannot be used,
    /// [`LibraryError::NoFreeName`] when every name is taken,
    /// [`LibraryError::Io`] when the file cannot be moved, and
    /// [`LibraryError::Database`] when a row cannot be written.
    pub async fn ingest(
        &self,
        user: Uuid,
        staged: &Path,
        original_name: &str,
    ) -> Result<IngestedUpload, LibraryError> {
        let extension = Path::new(original_name)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|extension| is_audio_extension(extension))
            .ok_or_else(|| LibraryError::UnsupportedUpload {
                name: original_name.to_owned(),
                reason: "the file name has no audio extension".to_owned(),
            })?;
        let probe = hearthsound_media::probe(staged).await.map_err(|error| {
            LibraryError::UnsupportedUpload {
                name: original_name.to_owned(),
                reason: error.to_string(),
            }
        })?;
        let size = i64::try_from(
            tokio::fs::metadata(staged)
                .await
                .map_err(|source| LibraryError::Io {
                    path: staged.to_path_buf(),
                    source,
                })?
                .len(),
        )
        .unwrap_or(i64::MAX);

        let snapshot = self.settings.current();
        let separators = snapshot.get(keys::LIBRARY_SCAN_ARTIST_SEPARATORS)?;
        let template = NamingTemplate::from_settings(&snapshot)?;
        drop(snapshot);
        let indexed = IndexedTags::from_probe(&probe, original_name, &separators);

        let album_artist = indexed
            .album
            .as_ref()
            .and_then(|album| album.display_artist.clone())
            .or_else(|| {
                indexed
                    .album_credits
                    .first()
                    .map(|credit| credit.artist.name.clone())
            })
            .or_else(|| indexed.track.display_artist.clone())
            .or_else(|| {
                indexed
                    .credits
                    .first()
                    .map(|credit| credit.artist.name.clone())
            });
        let relative = template
            .render(&NamingContext {
                album_artist: album_artist.as_deref(),
                album: indexed.album.as_ref().map(|album| album.title.as_str()),
                year: indexed.album.as_ref().and_then(|album| album.year),
                disc_number: indexed
                    .track
                    .disc_number
                    .and_then(|disc| u32::try_from(disc).ok()),
                disc_total: probe.tags.disc_total,
                track_number: indexed
                    .track
                    .track_number
                    .and_then(|number| u32::try_from(number).ok()),
                title: Some(indexed.track.title.as_str()),
                extension: Some(extension.as_str()),
            })
            .map_err(NamingTemplateError::from)?;
        let base = self.roots.library.join(&relative);
        let file = ManagedMediaFile {
            track_id: EntityId::new(hearthsound_core::IdPrefix::Track),
            storage: StorageClass::Library,
            rel_path: String::new(),
            size,
            mtime_ms: now_millis(),
            format: Some(probe.container.as_str().to_owned()),
            codec: Some(probe.codec.as_str().to_owned()),
            bitrate_kbps: probe.bitrate_kbps.map(i64::from),
            sample_rate: probe.sample_rate.map(i64::from),
            bit_depth: probe.bit_depth.map(i64::from),
            channels: probe.channels.map(i64::from),
            duration_ms: i64::try_from(probe.duration_ms).ok(),
            lossless: Some(probe.lossless),
            quality_tier: None,
            replaygain: ReplayGain::from_tags(&probe.tags).and_then(|gain| gain.to_json()),
            has_embedded_art: Some(probe.has_embedded_art),
            uploaded_by: Some(user),
            acquisition_id: None,
        };

        let mut taken: HashSet<PathBuf> = HashSet::new();
        for _ in 0..placement::MAX_COLLISIONS {
            let destination = placement::free_destination(&base, &taken).await?;
            placement::move_file(staged, &destination).await?;
            let rel_path =
                placement::relative_path(&self.roots, StorageClass::Library, &destination);
            match self.record(user, &indexed, &file, &rel_path).await {
                Ok(stored) => {
                    self.events
                        .publish(ServerEvent::LibraryChanged(LibraryChanged {
                            entity_kind: EntityKind::Track,
                            entity_id: stored.track_id.to_string(),
                            change: LibraryChange::Added,
                        }));
                    self.signal.notify();
                    tracing::info!(
                        %user,
                        track = %stored.track_id,
                        rel_path = stored.rel_path.as_str(),
                        "stored an upload"
                    );
                    return Ok(stored);
                }
                Err(error) => {
                    placement::move_file(&destination, staged).await?;
                    if is_unique_violation(&error) {
                        // Another writer claimed the name between the check and
                        // the row; the next suffix is tried.
                        taken.insert(destination);
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(LibraryError::NoFreeName { path: base })
    }

    /// Writes the rows of one stored upload in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Database`] when a row cannot be written.
    async fn record(
        &self,
        user: Uuid,
        indexed: &IndexedTags,
        file: &ManagedMediaFile,
        rel_path: &str,
    ) -> Result<IngestedUpload, LibraryError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        let album_id = match &indexed.album {
            Some(album) => {
                Some(albums::upsert_album(&mut tx, album, &indexed.album_credits).await?)
            }
            None => None,
        };
        let mut track = indexed.track.clone();
        track.album_id = album_id;
        track.availability = Availability::Local;
        let track_id =
            tracks::insert_track(&mut tx, &track, &indexed.credits, &indexed.genres).await?;
        let mut stored = file.clone();
        stored.track_id = track_id;
        stored.rel_path = rel_path.to_owned();
        let media_file_id = media_files::insert_managed_media_file(&mut tx, &stored).await?;
        let now = now_millis();
        let track_key = track_id.uuid();
        sqlx::query!(
            "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
             VALUES (?, 'track', ?, 'upload', ?) \
             ON CONFLICT DO NOTHING",
            user.as_bytes().as_slice(),
            track_key.as_bytes().as_slice(),
            now,
        )
        .execute(&mut *tx)
        .await?;
        retention::add_pin_reason(&mut tx, track_id, PinReason::UserLibrary).await?;
        index_queue::enqueue_library(&mut tx, EntityKind::Track, track_key).await?;
        tx.commit().await?;
        Ok(IngestedUpload {
            track_id,
            media_file_id,
            rel_path: rel_path.to_owned(),
        })
    }
}

/// `true` when an error is a unique-constraint violation of the database.
fn is_unique_violation(error: &LibraryError) -> bool {
    matches!(
        error,
        LibraryError::Database(sqlx::Error::Database(database)) if database.is_unique_violation()
    )
}
