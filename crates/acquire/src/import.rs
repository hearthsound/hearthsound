//! Verifying a downloaded candidate and importing the file it produced.
//!
//! Once the download stage has a candidate whose files stopped growing, it hands
//! them here. Each file is checked in the order the design fixes: it has to
//! decode, its duration has to be within five seconds of the track's known
//! catalog duration, and a candidate that claimed a lossless tier has to have
//! produced a file that actually decodes losslessly. A file that fails any check
//! rejects the whole candidate, and the download stage moves on to the next one,
//! exactly as it does after a failed download; when no candidate is left the
//! acquisition fails with `verification-failed`.
//!
//! A rejected candidate's staged copies are removed, so a backend that keeps
//! listing its staging directory cannot hand the same file back as a fresh
//! candidate. The offending file's source is also written to `source_blocklist`
//! when the verification layer's fingerprint check judged it a different
//! recording, so it is skipped by every later search until its quarantine
//! lapses.
//!
//! Verification runs while the acquisition is still `downloading`, which is what
//! lets a failure take the retry edge back to the next candidate. A candidate
//! that passes moves the acquisition to `verifying` and is then imported: the
//! file is copied into the acquisition's staging directory, tagged there, and
//! installed atomically under the managed storage root the track's retention
//! chooses, before one short transaction records the media file, the track's
//! availability and the acquisition's completion. A failed download or a failed
//! import therefore never leaves a partially named file in managed storage.
//!
//! A verified source that came from an import root in `link` mode takes the
//! other path: it is hardlinked straight to its managed destination, with no
//! copy and no tags, so the source keeps its bytes, inode and modification time
//! and the server stores no second copy. A filesystem that refuses the link - a
//! destination on another device, one without hard links or a permission the
//! server lacks - falls back to the copy-and-tag path above. The source and its
//! directory are never modified or removed in either case.
//!
//! The destination name is reserved before the file is written and held until
//! the transaction has committed, because two imports that render the same
//! relative path would otherwise both see it free and both choose it; a name a
//! stored row already holds is left to that row and the next collision suffix is
//! tried. An import that fails nonetheless ends the acquisition through the
//! normal failure path, so nothing is left in `importing`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hearthsound_config::keys;
use hearthsound_core::events::{LibraryChange, LibraryChanged, ServerEvent};
use hearthsound_core::providers::{
    Candidate, CandidateFile, ExternalId, ISRC, MB_ARTIST, MB_RECORDING, MB_RELEASE,
    MB_RELEASE_GROUP,
};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, EntityKind, FailureReason, ImportMode, PinReason,
    Priority, QualityTier, StorageClass,
};
use hearthsound_library::naming::NamingTemplate;
use hearthsound_library::{
    LibraryError, ManagedMediaFile, ScannedMediaFile, Track, media_files, placement, retention,
    tracks,
};
use hearthsound_media::{
    Artwork, Codec, ProbeResult, probe, verify_decodes, write_sidecar_cover, write_tags,
};
use hearthsound_plugin_host::RegistryInstance;
use uuid::Uuid;

use crate::download::{ResolvedFile, STAGING_ROOT, StoredResolvedFile, stored_candidate};
use crate::error::AcquireError;
use crate::orchestrator::{AcquisitionOrchestrator, DestinationReservation};
use crate::profile;
use crate::repo::{self, AcquisitionRow};
use crate::save_folder::{
    AlbumTotals, FolderDestination, NamingInputs, SaveTarget, issued_album, naming_facts,
};
use crate::select::AlbumFileMapping;
use crate::verify::SourceFile;

/// The window within which a downloaded file's duration must match the wanted
/// track's catalog duration, in milliseconds.
const DURATION_TOLERANCE_MS: i64 = 5_000;

/// The acquisition's staging subdirectory the tagged copy is written in.
const IMPORT_STAGING: &str = "import";

/// What handing a completed candidate to this stage produced.
pub(crate) enum HandoffOutcome {
    /// The candidate passed verification and its file was imported, so the
    /// acquisition is `completed`.
    Completed,
    /// The candidate passed verification but its file could not be imported,
    /// so the acquisition was failed and the download stage is over.
    ImportFailed,
    /// The candidate was rejected; the acquisition tries the next one, and
    /// fails with this reason when none is left.
    Rejected {
        /// Why the candidate was rejected, one of the fixed literals.
        reason: FailureReason,
        /// What the check saw, for the ledger's `error_detail`.
        detail: Option<String>,
    },
}

/// What importing one verified file did.
enum ImportOutcome {
    /// The file reached managed storage and the acquisition is `completed`.
    Imported,
    /// The import failed; the acquisition is `failed` and the stage is over.
    Failed,
}

/// A downloaded file that passed verification, with what the probe found.
pub(crate) struct VerifiedFile {
    /// The file the download resolved, with its root and absolute path.
    pub(crate) file: ResolvedFile,
    /// The probe of the file, which the import's rows are built from.
    pub(crate) probe: ProbeResult,
}

/// The managed file one import wrote.
pub(crate) struct ImportedFile {
    /// The absolute path the file landed at.
    pub(crate) destination: PathBuf,
    /// The path relative to the storage root, as the row stores it.
    pub(crate) rel_path: String,
    /// The storage root the file landed under.
    pub(crate) storage: StorageClass,
    /// The availability the imported file gives its track.
    pub(crate) availability: Availability,
    /// The quality tier computed from the probe.
    pub(crate) tier: Option<QualityTier>,
    /// The `media_files` row the import wrote.
    pub(crate) media_file_id: Uuid,
}

/// What importing one album download's files produced.
pub(crate) struct AlbumImportSummary {
    /// How many of the album's tracks were imported.
    pub(crate) imported: usize,
    /// The detail of the last per-file failure, for the acquisition's log.
    pub(crate) detail: Option<String>,
}

/// Everything placing one tagged file needs.
///
/// The file is tagged in the acquisition's staging directory first, so the
/// placement itself is only the choice of a free name, the install and the row;
/// this carries what those three steps read.
struct Placement<'a> {
    /// The acquisition the file belongs to.
    acquisition_id: Uuid,
    /// The instance that served the download, for its import roots.
    instance: &'a RegistryInstance,
    /// The track the file is for.
    track_id: EntityId,
    /// The verified file, whose probe the row is built from.
    verified: &'a VerifiedFile,
    /// The path the naming template rendered, relative to the root.
    relative: &'a Path,
    /// Whether the track is pinned, which decides library against cache.
    pinned: bool,
    /// The acquisition's staging directory.
    staged_dir: &'a Path,
    /// The tags the staged copy is written with.
    tags: &'a hearthsound_media::TrackTags,
    /// The cover the staged copy is written with, when there is one.
    cover: Option<&'a Artwork>,
    /// Whether the cover is embedded into the file's tags.
    embed_cover: bool,
    /// The sidecar cover's file name.
    cover_filename: &'a str,
}

/// Why a candidate was rejected.
struct Rejection {
    /// The failure reason the acquisition uses when nothing is left.
    reason: FailureReason,
    /// What the check saw.
    detail: String,
}

impl AcquisitionOrchestrator {
    /// Verifies a completed candidate's files and imports the first one.
    ///
    /// The acquisition is still `downloading` while the files are checked, so a
    /// rejection simply returns to the download stage's candidate loop. A
    /// candidate that passes is recorded in `verifying` and then imported, and
    /// the completion is published when the import's transaction commits.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose files are ready.
    /// - `instance`: the instance that served the download.
    /// - `tier`: the quality tier the candidate claimed, which is the lossless
    ///   claim the check verifies.
    /// - `files`: the resolved files, in the order the backend reported them.
    ///
    /// # Returns
    ///
    /// [`HandoffOutcome::Completed`] when the file was imported,
    /// [`HandoffOutcome::ImportFailed`] when the file could not be imported and
    /// the acquisition was failed, and [`HandoffOutcome::Rejected`] with the
    /// reason otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when the acquisition has no
    /// row, and the library, settings, filesystem or ledger error that stopped
    /// the stage.
    pub(crate) async fn hand_off_for_verification(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        tier: QualityTier,
        files: &[ResolvedFile],
    ) -> Result<HandoffOutcome, AcquireError> {
        let Some(row) = self.repository().get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        let catalog_duration_ms = match row.track_id {
            Some(track_id) => self
                .library()
                .get_track(track_id)
                .await?
                .and_then(|track| track.duration_ms),
            None => None,
        };

        let verified = match self.verify_files(tier, catalog_duration_ms, files).await {
            Ok(verified) => verified,
            Err(rejection) => {
                cleanup_rejected(files).await;
                self.log_rejection(acquisition_id, AcquisitionState::Downloading, &rejection)
                    .await;
                tracing::warn!(
                    acquisition = %acquisition_id,
                    instance = instance.slug().as_str(),
                    reason = rejection.reason.as_str(),
                    detail = rejection.detail.as_str(),
                    "rejected a downloaded candidate"
                );
                return Ok(HandoffOutcome::Rejected {
                    reason: rejection.reason,
                    detail: Some(rejection.detail),
                });
            }
        };

        if row.priority != Priority::Play
            && let Some(rejection) = self
                .fingerprint_rejection(instance, &row, &verified)
                .await?
        {
            cleanup_rejected(files).await;
            self.log_rejection(acquisition_id, AcquisitionState::Downloading, &rejection)
                .await;
            tracing::warn!(
                acquisition = %acquisition_id,
                instance = instance.slug().as_str(),
                reason = rejection.reason.as_str(),
                detail = rejection.detail.as_str(),
                "quarantined a downloaded candidate"
            );
            return Ok(HandoffOutcome::Rejected {
                reason: rejection.reason,
                detail: Some(rejection.detail),
            });
        }

        self.repository()
            .transition(
                acquisition_id,
                AcquisitionState::Verifying,
                Some("the candidate passed verification"),
            )
            .await?;
        let mut progress = self.progress_of_row(&row, row.priority);
        progress.state = AcquisitionState::Verifying;
        progress.instance = Some(instance.slug().as_str().to_owned());
        progress.percent = None;
        progress.eta_secs = None;
        self.announce(&progress);

        match self
            .import_candidate(acquisition_id, instance, &verified, &row)
            .await?
        {
            ImportOutcome::Imported => Ok(HandoffOutcome::Completed),
            ImportOutcome::Failed => Ok(HandoffOutcome::ImportFailed),
        }
    }

    /// Verifies and imports the files of a download a previous process finished.
    ///
    /// A process that stops between a download completing and its file reaching
    /// managed storage leaves the acquisition in `verifying` or `importing` with
    /// the resolved files in the ledger. This runs the same stages again from
    /// those paths - the completion rule, then verification, then the import -
    /// which is why the download stage stores them: nothing is downloaded twice,
    /// and the acquisition finishes where it stopped. A row whose files cannot be
    /// recovered is failed, because the state machine has no edge from
    /// `verifying` or `importing` back to a search.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, in `verifying` or `importing`.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the acquisition was completed or failed.
    ///
    /// # Errors
    ///
    /// Returns the ledger, settings, library or filesystem error that stopped the
    /// stage.
    pub(crate) async fn resume_import(&self, row: &AcquisitionRow) -> Result<(), AcquireError> {
        let id = row.id;
        let stored = stored_candidate(row.candidate.as_deref());
        let instance = row.instance_id.and_then(|id| self.instance_by_id(id));
        let (Some(stored), Some(instance)) = (stored, instance) else {
            return self
                .fail_acquisition(
                    id,
                    FailureReason::VerificationFailed,
                    Some("the ledger no longer names the files to import"),
                )
                .await;
        };
        let Some(resolved) = stored.resolved.as_ref().filter(|files| !files.is_empty()) else {
            return self
                .fail_acquisition(
                    id,
                    FailureReason::VerificationFailed,
                    Some("the ledger does not hold the resolved files to import"),
                )
                .await;
        };
        let files: Vec<ResolvedFile> = resolved
            .iter()
            .map(StoredResolvedFile::to_resolved)
            .collect();
        let Some(tier) = stored
            .to_candidate()
            .files
            .first()
            .and_then(profile::quality_tier)
        else {
            return self
                .fail_acquisition(
                    id,
                    FailureReason::VerificationFailed,
                    Some("the imported candidate has no usable quality tier"),
                )
                .await;
        };

        // The completion rule runs again, because the files were written by a
        // process that may have died while they were still growing.
        let mut settled = Vec::with_capacity(files.len());
        for file in &files {
            let (size, _stable) = self.wait_for_stable_size(&file.absolute_path).await?;
            settled.push(ResolvedFile {
                size,
                ..file.clone()
            });
        }

        let catalog_duration_ms = match row.track_id {
            Some(track_id) => self
                .library()
                .get_track(track_id)
                .await?
                .and_then(|track| track.duration_ms),
            None => None,
        };
        match self.verify_files(tier, catalog_duration_ms, &settled).await {
            Ok(verified) => {
                if row.priority != Priority::Play
                    && let Some(rejection) = self
                        .fingerprint_rejection(&instance, row, &verified)
                        .await?
                {
                    cleanup_rejected(&settled).await;
                    self.log_rejection(id, AcquisitionState::Verifying, &rejection)
                        .await;
                    return self
                        .fail_acquisition(id, rejection.reason, Some(&rejection.detail))
                        .await;
                }
                tracing::info!(
                    acquisition = %id,
                    instance = instance.slug().as_str(),
                    "resuming the import of a verified download"
                );
                // An import that fails has already failed the acquisition, so
                // either outcome leaves nothing more to do here.
                self.import_candidate(id, &instance, &verified, row)
                    .await
                    .map(|_| ())
            }
            Err(rejection) => {
                cleanup_rejected(&settled).await;
                self.log_rejection(id, AcquisitionState::Verifying, &rejection)
                    .await;
                self.fail_acquisition(id, rejection.reason, Some(&rejection.detail))
                    .await
            }
        }
    }

    /// Imports every verified file of an album download against its own track.
    ///
    /// Each file is checked against the duration and tier of the track its
    /// mapping names and, when it passes, imported for that track. A file that
    /// fails verification or cannot be imported is logged and skipped, so one
    /// bad file does not lose the tracks that did import; the download stage
    /// turns the summary into completion or failure and hands the remaining
    /// tracks to the per-track fan-out.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the album acquisition being imported.
    /// - `instance`: the instance that served the download.
    /// - `row`: the album acquisition's ledger row, whose state decides whether
    ///   the `importing` transition still has to be written.
    /// - `tracks`: the album's tracks, in playing order.
    /// - `mappings`: the candidate's mapping, in track order.
    /// - `files`: the resolved files aligned with the mapping; a slot is empty
    ///   when the backend reported no file for that track.
    ///
    /// # Returns
    ///
    /// How many tracks were imported and the detail of the last per-file
    /// failure, when one failed.
    ///
    /// # Errors
    ///
    /// Returns the ledger, settings or filesystem error that stopped the
    /// transition or a library read.
    pub(crate) async fn import_album_files(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        candidate: &Candidate,
        tracks: &[Track],
        mappings: &[AlbumFileMapping],
        verified: &[Option<VerifiedFile>],
    ) -> Result<AlbumImportSummary, AcquireError> {
        let Some(current) = self.repository().get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        if current.state == AcquisitionState::Downloading {
            self.repository()
                .transition(
                    acquisition_id,
                    AcquisitionState::Verifying,
                    Some("the album folder passed the download stage"),
                )
                .await?;
        }
        if current.state != AcquisitionState::Importing {
            self.repository()
                .transition(
                    acquisition_id,
                    AcquisitionState::Importing,
                    Some("importing the verified album files"),
                )
                .await?;
        }
        let mut summary = AlbumImportSummary {
            imported: 0,
            detail: None,
        };
        let target = self.save_target(&current).await?;
        for (mapping, verified) in mappings.iter().zip(verified) {
            let Some(verified) = verified else {
                continue;
            };
            let Some(track) = tracks.get(mapping.track_index) else {
                continue;
            };
            match self
                .import_file_for_track(acquisition_id, instance, track.id, verified, &target)
                .await
            {
                Ok(imported) => {
                    summary.imported += 1;
                    self.publish_availability(track.id, imported.availability, imported.tier);
                    self.events()
                        .publish(ServerEvent::LibraryChanged(LibraryChanged {
                            entity_kind: EntityKind::Track,
                            entity_id: track.id.to_string(),
                            change: LibraryChange::Added,
                        }));
                    post_import(instance, &verified.file).await;
                    let file_key = candidate
                        .files
                        .get(mapping.file_index as usize)
                        .map(|offered| offered.name.clone())
                        .unwrap_or_else(|| verified.file.path.clone());
                    self.check_play_import(
                        acquisition_id,
                        instance,
                        &current,
                        track,
                        &imported,
                        SourceFile {
                            source_key: &candidate.source.key,
                            file_key: &file_key,
                        },
                    )
                    .await;
                }
                Err(error) => {
                    tracing::warn!(
                        acquisition = %acquisition_id,
                        track = %track.id,
                        path = %verified.file.absolute_path.display(),
                        %error,
                        "an album file could not be imported"
                    );
                    summary.detail = Some(format!(
                        "the import of `{}` failed: {error}",
                        verified.file.path
                    ));
                }
            }
        }
        Ok(summary)
    }

    /// Verifies every file of an album download against the track it maps to.
    ///
    /// A file that fails is logged and removed from the staging directory; the
    /// tracks that did verify keep their files, because one bad file does not
    /// make a whole folder useless. When fingerprint verification is enabled and
    /// the priority is not `play`, each file that passed the local checks is
    /// also checked against its own track, and a file the service judges a
    /// different recording is rejected and its source quarantined.
    ///
    /// # Parameters
    ///
    /// - `row`: the album acquisition's ledger row.
    /// - `instance`: the instance that served the download.
    /// - `tracks`: the album's tracks, in playing order.
    /// - `mappings`: the candidate's mapping, in track order.
    /// - `files`: the resolved files aligned with the mapping.
    ///
    /// # Returns
    ///
    /// One verified file per mapping that passed, `None` for a slot without a
    /// file and for a file that was rejected, plus the detail of the last
    /// rejection.
    pub(crate) async fn verify_album_files(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        candidate: &Candidate,
        tracks: &[Track],
        mappings: &[AlbumFileMapping],
        files: &[Option<ResolvedFile>],
    ) -> (Vec<Option<VerifiedFile>>, Option<String>) {
        let acquisition_id = row.id;
        let mut verified = Vec::with_capacity(mappings.len());
        let mut detail = None;
        for (mapping, file) in mappings.iter().zip(files) {
            let Some(file) = file else {
                verified.push(None);
                continue;
            };
            let Some(track) = tracks.get(mapping.track_index) else {
                verified.push(None);
                continue;
            };
            let checked = match self.verify_one(file, track.duration_ms, mapping.tier).await {
                Ok(one) => one,
                Err(rejection) => {
                    self.reject_album_file(acquisition_id, track, file, &rejection)
                        .await;
                    detail = Some(rejection.detail);
                    verified.push(None);
                    continue;
                }
            };
            if row.priority != Priority::Play {
                let file_key = candidate
                    .files
                    .get(mapping.file_index as usize)
                    .map(|offered| offered.name.as_str())
                    .unwrap_or(file.path.as_str());
                let mismatch = match self
                    .check_fingerprint(
                        instance,
                        row,
                        track,
                        file,
                        SourceFile {
                            source_key: &candidate.source.key,
                            file_key,
                        },
                    )
                    .await
                {
                    Ok(mismatch) => mismatch,
                    Err(error) => {
                        tracing::warn!(
                            acquisition = %acquisition_id,
                            track = %track.id,
                            %error,
                            "an album file could not be fingerprint checked"
                        );
                        None
                    }
                };
                if let Some(detail_text) = mismatch {
                    let rejection = Rejection {
                        reason: FailureReason::VerificationFailed,
                        detail: format!("fingerprint mismatch: {detail_text}"),
                    };
                    self.reject_album_file(acquisition_id, track, file, &rejection)
                        .await;
                    detail = Some(rejection.detail);
                    verified.push(None);
                    continue;
                }
            }
            verified.push(Some(checked));
        }
        (verified, detail)
    }

    /// Logs a rejected album file and removes its staged copy.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the album acquisition being verified.
    /// - `track`: the track the file was mapped to.
    /// - `file`: the file that was rejected.
    /// - `rejection`: why it was rejected.
    async fn reject_album_file(
        &self,
        acquisition_id: Uuid,
        track: &Track,
        file: &ResolvedFile,
        rejection: &Rejection,
    ) {
        tracing::warn!(
            acquisition = %acquisition_id,
            track = %track.id,
            reason = rejection.reason.as_str(),
            detail = rejection.detail.as_str(),
            "rejected an album file"
        );
        self.log_rejection(acquisition_id, AcquisitionState::Downloading, rejection)
            .await;
        if file.root == STAGING_ROOT {
            let _ = tokio::fs::remove_file(&file.absolute_path).await;
        }
    }

    /// Verifies and imports the files of an album download a previous process
    /// finished.
    ///
    /// The ledger holds the resolved files and the track each belongs to, so
    /// the import runs where the previous process stopped. A row whose files or
    /// mapping cannot be recovered is failed and fanned out, because the album
    /// pipeline has no way back to a search from `verifying` or `importing`.
    ///
    /// # Parameters
    ///
    /// - `row`: the album acquisition's ledger row, in `verifying` or
    ///   `importing`.
    ///
    /// # Errors
    ///
    /// Returns the ledger, settings, library or filesystem error that stopped
    /// the import.
    pub(crate) async fn resume_album_import(
        &self,
        row: &AcquisitionRow,
    ) -> Result<(), AcquireError> {
        let id = row.id;
        let Some(album_id) = row.album_id else {
            return self
                .fail_album_acquisition(
                    row,
                    FailureReason::VerificationFailed,
                    Some("the ledger does not name the album to import"),
                )
                .await;
        };
        let stored = stored_candidate(row.candidate.as_deref());
        let instance = row.instance_id.and_then(|id| self.instance_by_id(id));
        let (Some(stored), Some(instance)) = (stored, instance) else {
            return self
                .fail_album_acquisition(
                    row,
                    FailureReason::VerificationFailed,
                    Some("the ledger no longer names the files to import"),
                )
                .await;
        };
        let (Some(resolved), Some(album)) = (stored.resolved.as_ref(), stored.album.as_ref())
        else {
            return self
                .fail_album_acquisition(
                    row,
                    FailureReason::VerificationFailed,
                    Some("the ledger does not hold the resolved album files"),
                )
                .await;
        };
        if resolved.is_empty() || album.is_empty() {
            return self
                .fail_album_acquisition(
                    row,
                    FailureReason::VerificationFailed,
                    Some("the ledger holds no resolved album file"),
                )
                .await;
        }
        let candidate = stored.to_candidate();
        let mut mappings = Vec::with_capacity(album.len());
        let mut files = Vec::with_capacity(album.len());
        for (mapping, file) in album.iter().zip(resolved) {
            let tier = candidate
                .files
                .get(mapping.file_index as usize)
                .and_then(profile::quality_tier)
                .unwrap_or(QualityTier::LossyLow);
            mappings.push(AlbumFileMapping {
                file_index: mapping.file_index,
                track_index: mapping.track_index as usize,
                score: 0.0,
                tier,
            });
            let (size, _) = self
                .wait_for_stable_size(&file.to_resolved().absolute_path)
                .await?;
            files.push(Some(ResolvedFile {
                size,
                ..file.to_resolved()
            }));
        }
        let tracks = self.library().list_tracks_for_album(album_id).await?;
        tracing::info!(
            acquisition = %id,
            instance = instance.slug().as_str(),
            "resuming the import of a verified album download"
        );
        let (verified, detail) = self
            .verify_album_files(row, &instance, &candidate, &tracks, &mappings, &files)
            .await;
        if verified.iter().all(Option::is_none) {
            return self
                .fail_album_acquisition(row, FailureReason::VerificationFailed, detail.as_deref())
                .await;
        }
        let summary = self
            .import_album_files(id, &instance, &candidate, &tracks, &mappings, &verified)
            .await?;
        if summary.imported > 0 {
            self.complete_album_acquisition(row, summary.imported).await
        } else {
            self.fail_album_acquisition(
                row,
                FailureReason::VerificationFailed,
                summary.detail.as_deref(),
            )
            .await
        }
    }

    /// Appends a rejected candidate to the acquisition's log.
    ///
    /// The rejection does not move the acquisition's state, but the admin
    /// timeline should still show why a candidate was dropped, so the entry
    /// carries the state the acquisition is in and the check's own message.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose log to append to.
    /// - `state`: the state the acquisition is in.
    /// - `rejection`: what the check saw.
    async fn log_rejection(
        &self,
        acquisition_id: Uuid,
        state: AcquisitionState,
        rejection: &Rejection,
    ) {
        let message = format!(
            "rejected a candidate as {}: {}",
            rejection.reason.as_str(),
            rejection.detail
        );
        let Ok(mut tx) = self.database().writer().begin().await else {
            return;
        };
        if crate::repo::write_log(&mut tx, acquisition_id, state, Some(&message))
            .await
            .is_err()
            || tx.commit().await.is_err()
        {
            tracing::debug!(acquisition = %acquisition_id, "a rejection could not be logged");
        }
    }

    /// Runs the verification checks on every file of one candidate.
    ///
    /// # Parameters
    ///
    /// - `claimed_tier`: the quality tier the candidate claimed; a lossless tier
    ///   is a claim the file has to live up to.
    /// - `catalog_duration_ms`: the wanted track's catalog duration, when it is
    ///   known; the check is skipped when it is not.
    /// - `files`: the resolved files to check.
    ///
    /// # Returns
    ///
    /// The verified files with their probes, or the reason the candidate was
    /// rejected.
    async fn verify_files(
        &self,
        claimed_tier: QualityTier,
        catalog_duration_ms: Option<i64>,
        files: &[ResolvedFile],
    ) -> Result<Vec<VerifiedFile>, Rejection> {
        let mut verified = Vec::with_capacity(files.len());
        for file in files {
            verified.push(
                self.verify_one(file, catalog_duration_ms, claimed_tier)
                    .await?,
            );
        }
        Ok(verified)
    }

    /// Fingerprints every verified file of a track candidate and asks the
    /// verification layer whether it is the wanted recording.
    ///
    /// A file the service judges a different recording is quarantined by the
    /// check itself; the first mismatch becomes the rejection of the whole
    /// candidate, exactly like a failed decode or duration check.
    ///
    /// # Parameters
    ///
    /// - `instance`: the acquisition instance that served the candidate.
    /// - `row`: the acquisition's ledger row, whose candidate names the source.
    /// - `verified`: the files that passed the decode, duration and tier checks.
    ///
    /// # Returns
    ///
    /// The rejection when a file mismatched, and `None` when every checked file
    /// passed or the check was skipped.
    ///
    /// # Errors
    ///
    /// Returns the library, settings or database error that stopped the check.
    async fn fingerprint_rejection(
        &self,
        instance: &RegistryInstance,
        row: &AcquisitionRow,
        verified: &[VerifiedFile],
    ) -> Result<Option<Rejection>, AcquireError> {
        let Some(track_id) = row.track_id else {
            return Ok(None);
        };
        let Some(track) = self.library().get_track(track_id).await? else {
            return Ok(None);
        };
        let Some(candidate) =
            stored_candidate(row.candidate.as_deref()).map(|stored| stored.to_candidate())
        else {
            return Ok(None);
        };
        for file in verified {
            let file_key = candidate_file_key(&candidate, &file.file);
            if let Some(detail) = self
                .check_fingerprint(
                    instance,
                    row,
                    &track,
                    &file.file,
                    SourceFile {
                        source_key: &candidate.source.key,
                        file_key: &file_key,
                    },
                )
                .await?
            {
                return Ok(Some(Rejection {
                    reason: FailureReason::VerificationFailed,
                    detail: format!("fingerprint mismatch: {detail}"),
                }));
            }
        }
        Ok(None)
    }

    /// Runs the verification checks on one file.
    ///
    /// # Parameters
    ///
    /// - `file`: the resolved file to check.
    /// - `catalog_duration_ms`: the wanted track's catalog duration, when it is
    ///   known; the duration check is skipped when it is not.
    /// - `claimed_tier`: the tier the candidate claimed for the file; a lossless
    ///   tier is a claim the file has to live up to.
    ///
    /// # Returns
    ///
    /// The verified file, or the reason it was rejected.
    async fn verify_one(
        &self,
        file: &ResolvedFile,
        catalog_duration_ms: Option<i64>,
        claimed_tier: QualityTier,
    ) -> Result<VerifiedFile, Rejection> {
        let path = &file.absolute_path;
        if let Err(error) = verify_decodes(path, self.ffmpeg()).await {
            return Err(Rejection {
                reason: FailureReason::VerificationFailed,
                detail: format!("`{}` does not decode: {error}", path.display()),
            });
        }
        let probe = match probe(path).await {
            Ok(probe) => probe,
            Err(error) => {
                return Err(Rejection {
                    reason: FailureReason::VerificationFailed,
                    detail: format!("`{}` cannot be probed: {error}", path.display()),
                });
            }
        };
        if let Some(catalog) = catalog_duration_ms {
            let difference = (probe.duration_ms as i64 - catalog).abs();
            if difference > DURATION_TOLERANCE_MS {
                return Err(Rejection {
                    reason: FailureReason::VerificationFailed,
                    detail: format!(
                        "`{}` lasts {} ms, which is more than {DURATION_TOLERANCE_MS} ms from the track's {catalog} ms",
                        path.display(),
                        probe.duration_ms
                    ),
                });
            }
        }
        if matches!(
            claimed_tier,
            QualityTier::Lossless | QualityTier::LosslessHires
        ) && !probe.lossless
        {
            return Err(Rejection {
                reason: FailureReason::VerificationFailed,
                detail: format!(
                    "`{}` was claimed as {claimed_tier} but decodes as a lossy codec",
                    path.display()
                ),
            });
        }
        Ok(VerifiedFile {
            file: file.clone(),
            probe,
        })
    }

    /// Imports one verified file into managed storage.
    ///
    /// The file is copied into the acquisition's staging directory, tagged
    /// there, and installed under the root retention chooses. Only then does one
    /// short transaction write the `media_files` row, ensure the track's
    /// retention row, set the track's availability and complete the acquisition;
    /// the events and the final path are published after it commits.
    ///
    /// An import that fails leaves no way to finish the acquisition, so it is
    /// failed here instead of being left in `importing`: the storage layer
    /// refused the file this candidate verified as, which is the one thing the
    /// download stage cannot answer by trying the next candidate. The reason is
    /// `all-candidates-failed`, the literal that means the acquisition ended
    /// without a file, and the detail carries what the import failed on.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition being imported.
    /// - `instance`: the instance that served the download, for its slug and its
    ///   import roots.
    /// - `verified`: the verified files; the first one is imported.
    /// - `row`: the acquisition's ledger row, whose state decides whether the
    ///   `importing` transition still has to be written: a row that a resume
    ///   found in `importing` is already there.
    ///
    /// # Returns
    ///
    /// [`ImportOutcome::Imported`] when the file reached managed storage, and
    /// [`ImportOutcome::Failed`] when the import failed and the acquisition was
    /// failed instead.
    ///
    /// # Errors
    ///
    /// Returns the error that stopped the import when the acquisition could not
    /// be failed either, and the ledger error that stopped the failure.
    async fn import_candidate(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        verified: &[VerifiedFile],
        row: &AcquisitionRow,
    ) -> Result<ImportOutcome, AcquireError> {
        match self
            .import_verified_file(acquisition_id, instance, verified, row)
            .await
        {
            Ok(imported) => {
                if let (Some(track_id), Some(verified)) = (row.track_id, verified.first()) {
                    match self.library().get_track(track_id).await {
                        Ok(Some(track)) => {
                            if let Some(candidate) = stored_candidate(row.candidate.as_deref())
                                .map(|stored| stored.to_candidate())
                            {
                                let file_key = candidate_file_key(&candidate, &verified.file);
                                self.check_play_import(
                                    acquisition_id,
                                    instance,
                                    row,
                                    &track,
                                    &imported,
                                    SourceFile {
                                        source_key: &candidate.source.key,
                                        file_key: &file_key,
                                    },
                                )
                                .await;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(
                                acquisition = %acquisition_id,
                                track = %track_id,
                                %error,
                                "the imported track could not be read for its fingerprint check"
                            );
                        }
                    }
                }
                Ok(ImportOutcome::Imported)
            }
            Err(error) => {
                let detail = format!("the import failed: {error}");
                tracing::warn!(
                    acquisition = %acquisition_id,
                    %error,
                    "the import of a verified file failed"
                );
                self.fail_acquisition(
                    acquisition_id,
                    FailureReason::AllCandidatesFailed,
                    Some(&detail),
                )
                .await?;
                Ok(ImportOutcome::Failed)
            }
        }
    }

    /// Imports the first verified file, reserving the name it lands under.
    ///
    /// The destination the naming template renders is reserved before anything
    /// is written and the reservation is held until the transaction has
    /// committed, so two imports that render the same relative path cannot both
    /// pick it. A destination a stored row already holds - the one conflict the
    /// reservation cannot see, because that row's file is gone from the
    /// filesystem - is left to that row and the next collision suffix is tried.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition being imported.
    /// - `instance`: the instance that served the download, for its slug and its
    ///   import roots.
    /// - `verified`: the verified files; the first one is imported.
    /// - `row`: the acquisition's ledger row, whose state decides whether the
    ///   `importing` transition still has to be written.
    ///
    /// # Returns
    ///
    /// The managed file the import wrote.
    ///
    /// # Errors
    ///
    /// Returns the library, settings, filesystem or ledger error that stopped
    /// the import, and [`AcquireError::ImportPath`] when every name under the
    /// destination's directory is taken.
    async fn import_verified_file(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        verified: &[VerifiedFile],
        row: &AcquisitionRow,
    ) -> Result<ImportedFile, AcquireError> {
        let track_id = row
            .track_id
            .ok_or(AcquireError::UnknownAcquisition(acquisition_id))?;
        let verified = verified
            .first()
            .ok_or(AcquireError::UnknownAcquisition(acquisition_id))?;

        if row.state != AcquisitionState::Importing {
            self.repository()
                .transition(
                    acquisition_id,
                    AcquisitionState::Importing,
                    Some("importing the verified file"),
                )
                .await?;
        }

        let target = self.save_target(row).await?;
        let imported = self
            .import_file_for_track(acquisition_id, instance, track_id, verified, &target)
            .await?;

        let mut tx = self.database().writer().begin().await?;
        repo::complete(
            &mut tx,
            acquisition_id,
            imported.media_file_id,
            Some("the file was imported into managed storage"),
        )
        .await?;
        // An imported track is a library track, so its metadata is indexed like
        // a scanned or uploaded one.
        hearthsound_library::index_queue::enqueue_library(
            &mut tx,
            EntityKind::Track,
            track_id.uuid(),
        )
        .await?;
        tx.commit().await?;

        let mut progress = self.progress_of_row(row, row.priority);
        progress.state = AcquisitionState::Completed;
        progress.instance = Some(instance.slug().as_str().to_owned());
        progress.percent = None;
        progress.eta_secs = None;
        progress.file = Some(imported.destination.clone());
        self.announce(&progress);
        self.publish_availability(track_id, imported.availability, imported.tier);
        self.events()
            .publish(ServerEvent::LibraryChanged(LibraryChanged {
                entity_kind: EntityKind::Track,
                entity_id: track_id.to_string(),
                change: LibraryChange::Added,
            }));
        tracing::info!(
            acquisition = %acquisition_id,
            track = %track_id,
            instance = instance.slug().as_str(),
            storage = imported.storage.as_str(),
            rel_path = imported.rel_path.as_str(),
            source_root = verified.file.root.as_str(),
            source_path = verified.file.path.as_str(),
            source_bytes = verified.file.size,
            "imported the acquired file"
        );
        post_import(instance, &verified.file).await;
        self.release_staging(acquisition_id).await;
        Ok(imported)
    }

    /// Imports one verified file for one track into the file's save target.
    ///
    /// The file is copied into the acquisition's staging directory and tagged
    /// there, or hardlinked from a `link`-mode import root, before it is
    /// installed under the root retention chooses; a save whose account has a
    /// scanned folder as its save location is installed into that folder instead,
    /// as a `folder` media file with no managed copy. The transaction writes the
    /// `media_files` row, ensures the track's retention row and sets the track's
    /// availability; the availability and library-change events are published
    /// after it commits. No acquisition state is written, so an album
    /// acquisition can import several files before it completes.
    ///
    /// A folder that cannot be written - it was removed, unmounted or mounted
    /// read-only - falls back to the managed library once, publishes the
    /// save-folder notice and releases the folder's lock, because the tagged copy
    /// is still in staging and the retry is a rename rather than a re-download.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition the file is imported for.
    /// - `instance`: the instance that served the download, for its slug and its
    ///   import roots.
    /// - `track_id`: the track the file is for.
    /// - `verified`: the verified file, whose probe the rows are built from.
    /// - `target`: where the file goes: the saver's scanned folder or the
    ///   managed library.
    ///
    /// # Returns
    ///
    /// The managed file the import wrote.
    ///
    /// # Errors
    ///
    /// Returns the library, settings, filesystem or ledger error that stopped
    /// the import, and [`AcquireError::ImportPath`] when every name under the
    /// destination's directory is taken.
    async fn import_file_for_track(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        track_id: EntityId,
        verified: &VerifiedFile,
        target: &SaveTarget,
    ) -> Result<ImportedFile, AcquireError> {
        let track = self
            .library()
            .get_track(track_id)
            .await?
            .ok_or(AcquireError::UnknownTrack(track_id))?;
        let album = match track.album_id {
            Some(album_id) => self.library().get_album(album_id).await?,
            None => None,
        };
        // The album's page is walked before the file is tagged, so the totals and
        // the tags describe the whole release rather than the single row a
        // remote track left behind.
        let totals = match track.album_id {
            Some(album_id) => {
                issued_album(self.catalog(), self.library(), album_id, track.disc_number).await
            }
            None => AlbumTotals::default(),
        };
        let artists = self.library().list_track_artists(track_id).await?;
        let album_artists = match track.album_id {
            Some(album_id) => self.library().list_album_artists(album_id).await?,
            None => Vec::new(),
        };
        let genres = self.genres_of(track_id, track.album_id).await;
        let pinned = self.library().track_is_pinned(track_id).await?;

        let snapshot = self.settings().current();
        let template = NamingTemplate::from_settings(&snapshot)?;
        let embed_cover = snapshot.get(keys::LIBRARY_TAGS_EMBED_COVER)?;
        let cover_filename = snapshot.get(keys::LIBRARY_COVER_FILENAME)?;

        let extension = extension_of(&verified.file, &verified.probe);
        let facts = naming_facts(
            &track,
            NamingInputs {
                album: album.as_ref(),
                totals,
                artists: &artists,
                album_artists: &album_artists,
            },
        );
        let album_artist = facts.album_artist.clone();
        let album_title = facts.album.clone();
        let year = facts.year;
        let disc_total = facts.disc_total;
        let track_total = facts.track_total;
        let relative = facts.render(&template, &extension)?;

        // Work on a copy inside the acquisition's staging directory, so the
        // backend's own file is never written to and a crash cannot leave a
        // half-tagged file at the destination path.
        let staged_dir = self.staging_dir(acquisition_id).join(IMPORT_STAGING);
        create_dir_all(&staged_dir).await?;

        let cover = self.cover_art(track_id, track.album_id).await;
        let track_ids = self.stored_ids(EntityKind::Track, track_id).await?;
        let album_ids = match track.album_id {
            Some(album_id) => self.stored_ids(EntityKind::Album, album_id).await?,
            None => Vec::new(),
        };
        let mut mb_artist_ids = Vec::new();
        for artist in &artists {
            mb_artist_ids.extend(
                self.stored_ids(EntityKind::Artist, artist.id)
                    .await?
                    .into_iter()
                    .filter(|id| id.namespace == MB_ARTIST),
            );
        }
        let mut mb_album_artist_ids = Vec::new();
        for artist in &album_artists {
            mb_album_artist_ids.extend(
                self.stored_ids(EntityKind::Artist, artist.id)
                    .await?
                    .into_iter()
                    .filter(|id| id.namespace == MB_ARTIST),
            );
        }
        let tags = hearthsound_media::TrackTags {
            title: Some(track.title.clone()),
            artists: artists.iter().map(|artist| artist.name.clone()).collect(),
            album: album_title.clone(),
            album_artist: album_artist.clone(),
            track_number: track
                .track_number
                .and_then(|number| u32::try_from(number).ok()),
            track_total,
            disc_number: track.disc_number.and_then(|disc| u32::try_from(disc).ok()),
            disc_total,
            date: album
                .as_ref()
                .and_then(|album| album.release_date.clone())
                .or_else(|| year.map(|year| year.to_string())),
            genres,
            isrc: id_value(&track_ids, ISRC),
            mb_recording_id: id_value(&track_ids, MB_RECORDING),
            mb_release_id: id_value(&album_ids, MB_RELEASE),
            mb_release_group_id: id_value(&album_ids, MB_RELEASE_GROUP),
            mb_artist_ids: mb_artist_ids.into_iter().map(|id| id.value).collect(),
            mb_album_artist_ids: mb_album_artist_ids.into_iter().map(|id| id.value).collect(),
            acquired_via: Some(instance.slug().clone()),
            ..Default::default()
        };

        // A save whose account chose a scanned folder writes there instead, and
        // the folder's lock is held across the file write and the row insert so a
        // scan of the folder cannot index the file as a second track.
        let reserved = self.save_folders().reserve(target).await?;
        let placement = Placement {
            acquisition_id,
            instance,
            track_id,
            verified,
            relative: &relative,
            pinned,
            staged_dir: &staged_dir,
            tags: &tags,
            cover: cover.as_ref(),
            embed_cover,
            cover_filename: &cover_filename,
        };
        let Some(folder) = reserved else {
            return self.place_file(&placement, None).await;
        };
        match self.place_file(&placement, Some(&folder)).await {
            Ok(imported) => Ok(imported),
            Err(error) if is_folder_unavailable(&error) => {
                let name = folder.folder.name.as_str();
                tracing::warn!(
                    acquisition = %acquisition_id,
                    folder = %folder.folder.path.display(),
                    %error,
                    "the save folder could not be written; keeping the file in the managed library"
                );
                if let Some(user) = folder.folder.owner_user_id {
                    self.save_folders().notice(user, Some(name));
                }
                // The tagged copy is still in staging, so the retry is a rename
                // and not a second download.
                drop(folder);
                self.place_file(&placement, None).await
            }
            Err(error) => Err(error),
        }
    }

    /// Places one tagged file at its destination and records the row.
    ///
    /// # Parameters
    ///
    /// - `placement`: what the import writes.
    /// - `folder`: the save folder to write into, or `None` for the managed
    ///   library.
    ///
    /// # Returns
    ///
    /// The file the import wrote.
    ///
    /// # Errors
    ///
    /// Returns the library, settings or filesystem error that stopped the
    /// import, and [`AcquireError::ImportPath`] when every name under the
    /// destination's directory is taken.
    async fn place_file(
        &self,
        placement: &Placement<'_>,
        folder: Option<&FolderDestination>,
    ) -> Result<ImportedFile, AcquireError> {
        let Placement {
            acquisition_id,
            instance,
            track_id,
            verified,
            relative,
            pinned,
            staged_dir,
            tags,
            cover,
            embed_cover,
            cover_filename,
        } = *placement;
        let (base, storage, availability, link_mode) = match folder {
            Some(destination) => (
                destination.folder.path.join(relative),
                StorageClass::Folder,
                Availability::Local,
                // A folder file must be its own inode: a hardlink would share it
                // with the backend's file, which the backend may rewrite.
                false,
            ),
            None => {
                let storage = if pinned {
                    StorageClass::Library
                } else {
                    StorageClass::Cache
                };
                (
                    retention::import_destination(pinned, self.storage_roots(), relative),
                    storage,
                    if pinned {
                        Availability::Local
                    } else {
                        Availability::Cached
                    },
                    import_root(instance, &verified.file)
                        .is_some_and(|root| root.mode == ImportMode::Link),
                )
            }
        };

        // The names this import has already lost: one another import in this
        // process reserved, one a file already holds, and one a stored row
        // holds while its file is gone. Every lost name moves the import to the
        // next collision suffix, which is what keeps the retry from picking the
        // same name again.
        let mut taken: HashSet<PathBuf> = HashSet::new();
        let folder_root = folder.map(|destination| destination.folder.path.as_path());
        for _ in 0..placement::MAX_COLLISIONS {
            let (destination, reservation) = match folder_root {
                // A folder's names are held by the folder lock the caller holds,
                // so a scan and another save cannot pick the same one.
                Some(_) => (
                    placement::free_destination(&base, &taken)
                        .await
                        .map_err(AcquireError::Library)?,
                    None,
                ),
                None => {
                    let (destination, reservation) =
                        self.reserve_destination(&base, &mut taken).await?;
                    (destination, Some(reservation))
                }
            };
            let rel_path = match folder_root {
                Some(root) => folder_relative(root, &destination),
                None => placement::relative_path(self.storage_roots(), storage, &destination),
            };
            let mut linked = false;
            if link_mode {
                match self
                    .link_into_place(&verified.file.absolute_path, &destination)
                    .await?
                {
                    Linked::Placed => linked = true,
                    Linked::Taken => {
                        taken.insert(destination);
                        continue;
                    }
                    Linked::Unsupported => {}
                }
            }
            if !linked {
                let staged =
                    staged_dir.join(destination.file_name().unwrap_or(relative.as_os_str()));
                copy_file(&verified.file.absolute_path, &staged).await?;
                write_tags(&staged, tags, if embed_cover { cover } else { None }).await?;
                match install(&staged, &destination).await? {
                    Installed::Placed => {}
                    Installed::Taken => {
                        taken.insert(destination);
                        continue;
                    }
                }
            }

            if let Some(artwork) = cover
                && let Some(parent) = destination.parent()
            {
                write_sidecar_cover(parent, cover_filename, artwork).await?;
            }

            let metadata =
                tokio::fs::metadata(&destination)
                    .await
                    .map_err(|source| AcquireError::Io {
                        path: destination.clone(),
                        source,
                    })?;
            let size = metadata.len();
            let mtime_ms = mtime_millis(&metadata);
            let tier = quality_tier_of(&destination, &verified.probe);

            let mut tx = self.database().writer().begin().await?;
            let inserted = match folder {
                Some(destination_folder) => {
                    media_files::upsert_folder_media_file(
                        &mut tx,
                        track_id,
                        &ScannedMediaFile {
                            folder_id: destination_folder.folder.id,
                            rel_path: rel_path.clone(),
                            size: i64::try_from(size).unwrap_or(i64::MAX),
                            mtime_ms,
                            format: Some(verified.probe.container.as_str().to_owned()),
                            codec: Some(verified.probe.codec.as_str().to_owned()),
                            bitrate_kbps: verified.probe.bitrate_kbps.map(i64::from),
                            sample_rate: verified.probe.sample_rate.map(i64::from),
                            bit_depth: verified.probe.bit_depth.map(i64::from),
                            channels: verified.probe.channels.map(i64::from),
                            duration_ms: i64::try_from(verified.probe.duration_ms).ok(),
                            lossless: Some(verified.probe.lossless),
                            replaygain: None,
                            has_embedded_art: Some(verified.probe.has_embedded_art),
                        },
                    )
                    .await
                }
                None => {
                    media_files::insert_managed_media_file(
                        &mut tx,
                        &ManagedMediaFile {
                            track_id,
                            storage,
                            rel_path: rel_path.clone(),
                            size: i64::try_from(size).unwrap_or(i64::MAX),
                            mtime_ms,
                            format: Some(verified.probe.container.as_str().to_owned()),
                            codec: Some(verified.probe.codec.as_str().to_owned()),
                            bitrate_kbps: verified.probe.bitrate_kbps.map(i64::from),
                            sample_rate: verified.probe.sample_rate.map(i64::from),
                            bit_depth: verified.probe.bit_depth.map(i64::from),
                            channels: verified.probe.channels.map(i64::from),
                            duration_ms: i64::try_from(verified.probe.duration_ms).ok(),
                            lossless: Some(verified.probe.lossless),
                            quality_tier: tier,
                            replaygain: None,
                            has_embedded_art: Some(verified.probe.has_embedded_art),
                            uploaded_by: None,
                            acquisition_id: Some(acquisition_id),
                        },
                    )
                    .await
                }
            };
            let media_file_id = match inserted {
                Ok(media_file_id) => media_file_id,
                Err(error) if is_stored_path_conflict(&error) => {
                    // A stored row already names this path while no file is there
                    // to be seen, so the name belongs to that row: this import
                    // takes its own file away again and tries the next suffix.
                    let _ = tx.rollback().await;
                    let _ = tokio::fs::remove_file(&destination).await;
                    taken.insert(destination);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            match folder {
                // A folder file is permanent whatever the pins say, and the pin
                // is what a later scan of an owned folder writes for it too.
                Some(_) => {
                    retention::add_pin_reason(&mut tx, track_id, PinReason::Folder).await?;
                }
                // A track with no retention row would be invisible to the eviction
                // query, which joins `track_retention`; the row is created by the
                // first pin write, so an unpinned track gets an empty one here.
                None if !pinned => {
                    retention::add_pin_reason(&mut tx, track_id, PinReason::Admin).await?;
                    retention::remove_pin_reason(&mut tx, track_id, PinReason::Admin).await?;
                }
                None => {}
            }
            tracks::set_track_availability(&mut tx, track_id, availability, None).await?;
            tx.commit().await?;
            drop(reservation);

            return Ok(ImportedFile {
                destination,
                rel_path,
                storage,
                availability,
                tier,
                media_file_id,
            });
        }

        Err(AcquireError::ImportPath { path: base })
    }

    /// Resolves where one acquisition's file goes.
    ///
    /// Only a save and a permanent playlist addition are the user saying they
    /// mean to keep the track, and only those write into the requester's save
    /// folder; everything else - a play, a prefetch, an administrative retry -
    /// stays in managed storage.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, for its reason and its requester.
    ///
    /// # Returns
    ///
    /// The save location, resolved once for the whole acquisition.
    ///
    /// # Errors
    ///
    /// Returns the settings or folder read error that stopped the resolution.
    async fn save_target(&self, row: &AcquisitionRow) -> Result<SaveTarget, AcquireError> {
        if row.reason != crate::adopt::SAVE_REASON && row.reason != crate::adopt::PLAYLIST_REASON {
            return Ok(SaveTarget::Managed);
        }
        self.save_folders().target(row.requested_by).await
    }

    /// Reserves the next free destination for a rendered relative path.
    ///
    /// The rendered name is offered first, then its ` (N)` collision suffixes,
    /// and a name is taken only when no file holds it and no other import in
    /// this process has reserved it. The reservation is what makes the choice
    /// atomic: an import that loses a name to another import walks on to the
    /// next one instead of writing to the same path.
    ///
    /// # Parameters
    ///
    /// - `base`: the absolute destination path the naming template rendered.
    /// - `taken`: the names this import has already lost, which are skipped even
    ///   when nothing holds them.
    ///
    /// # Returns
    ///
    /// The reserved destination and the guard that releases it.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::ImportPath`] when every name is taken, and the
    /// filesystem error when a path cannot be stat-ed.
    async fn reserve_destination(
        &self,
        base: &Path,
        taken: &mut HashSet<PathBuf>,
    ) -> Result<(PathBuf, DestinationReservation<'_>), AcquireError> {
        loop {
            let candidate = match placement::free_destination(base, taken).await {
                Ok(candidate) => candidate,
                Err(LibraryError::NoFreeName { path }) => {
                    return Err(AcquireError::ImportPath { path });
                }
                Err(error) => return Err(error.into()),
            };
            match self.try_reserve_destination(&candidate) {
                Some(reservation) => return Ok((candidate, reservation)),
                None => {
                    taken.insert(candidate);
                }
            }
        }
    }

    /// Fetches the cover art an imported track should carry.
    ///
    /// The bytes come from the server's own image store, which is the only place
    /// an image is kept: the artwork layer stored what it fetched during
    /// indexing, and a save never depends on a provider being reachable.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track being imported.
    /// - `album_id`: the track's album, when it has one; the cover is the album's
    ///   when it does, and the track's own otherwise.
    ///
    /// # Returns
    ///
    /// The cover, or `None` when the entity has no image.
    async fn cover_art(&self, track_id: EntityId, album_id: Option<EntityId>) -> Option<Artwork> {
        let (kind, id) = match album_id {
            Some(album) => (EntityKind::Album, album.uuid()),
            None => (EntityKind::Track, track_id.uuid()),
        };
        let served = self.catalog().images().serve(kind, id, None).await.ok()??;
        Some(Artwork::new(served.content_type, served.bytes))
    }

    /// Returns the genre names an imported file is tagged with.
    ///
    /// The album's genres are the release's, which is what a `GENRE` tag means,
    /// and they are the ones the catalog stored for the album during indexing.
    /// A track without an album, or an album no provider named genres for, falls
    /// back to the track's own stored genres.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track being imported.
    /// - `album_id`: the track's album, when it has one.
    ///
    /// # Returns
    ///
    /// The genre names, ordered by name.
    async fn genres_of(&self, track_id: EntityId, album_id: Option<EntityId>) -> Vec<String> {
        if let Some(album_id) = album_id {
            match self.album_genres(album_id).await {
                Ok(genres) if !genres.is_empty() => return genres,
                Ok(_) => {}
                Err(error) => tracing::debug!(
                    album = %album_id,
                    %error,
                    "an album's genres could not be read"
                ),
            }
        }
        match self.library().list_track_genres(track_id).await {
            Ok(genres) => genres.into_iter().map(|genre| genre.name).collect(),
            Err(error) => {
                tracing::debug!(track = %track_id, %error, "a track's genres could not be read");
                Vec::new()
            }
        }
    }

    /// Reads an album's stored genre names.
    ///
    /// # Parameters
    ///
    /// - `album_id`: the album to read.
    ///
    /// # Returns
    ///
    /// The genre names, ordered by name.
    ///
    /// # Errors
    ///
    /// Returns [`sqlx::Error`] when the read fails.
    async fn album_genres(&self, album_id: EntityId) -> Result<Vec<String>, sqlx::Error> {
        let mut connection = self.database().readers().acquire().await?;
        sqlx::query_scalar::<_, String>(
            "SELECT g.name FROM album_genres ag JOIN genres g ON g.id = ag.genre_id \
             WHERE ag.album_id = ? ORDER BY g.name",
        )
        .bind(album_id.uuid().as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await
    }
}

/// Returns the value of one namespace from a row's stored identifiers.
///
/// # Parameters
///
/// - `ids`: the identifiers stored for the row.
/// - `namespace`: the namespace to look for.
///
/// # Returns
///
/// The first value in that namespace, when one is stored.
fn id_value(ids: &[ExternalId], namespace: &str) -> Option<String> {
    ids.iter()
        .find(|id| id.namespace == namespace)
        .map(|id| id.value.clone())
}

/// Returns the key a candidate file is blocklisted under.
///
/// The candidate's own name is what a later search compares, so a resolved
/// output path is matched back to the candidate file it came from by its file
/// name; an unmatched path falls back to itself.
///
/// # Parameters
///
/// - `candidate`: the stored candidate, whose files name what was offered.
/// - `file`: the file the download produced.
///
/// # Returns
///
/// The candidate file's name, or the reported path when no file matches.
fn candidate_file_key(candidate: &Candidate, file: &ResolvedFile) -> String {
    let name = Path::new(&file.path)
        .file_name()
        .and_then(|name| name.to_str());
    candidate
        .files
        .iter()
        .find(|offered| {
            offered.name == file.path
                || Some(offered.name.as_str()) == name
                || Path::new(&offered.name)
                    .file_name()
                    .and_then(|name| name.to_str())
                    == name
        })
        .map(|offered| offered.name.clone())
        .unwrap_or_else(|| file.path.clone())
}

/// Removes the staged copies of a rejected candidate.
///
/// Only files that were staged for this acquisition are removed; a file a
/// backend wrote into one of its own import roots is left alone, because it
/// belongs to the backend, not to the server.
///
/// # Parameters
///
/// - `files`: the resolved files of the rejected candidate.
async fn cleanup_rejected(files: &[ResolvedFile]) {
    for file in files {
        if file.root == STAGING_ROOT
            && let Err(error) = tokio::fs::remove_file(&file.absolute_path).await
        {
            tracing::debug!(
                path = %file.absolute_path.display(),
                %error,
                "a rejected staged file could not be removed"
            );
        }
    }
}

/// Returns the import root a resolved file came from.
///
/// # Parameters
///
/// - `instance`: the instance that served the download.
/// - `file`: the resolved file.
///
/// # Returns
///
/// The root, or `None` when the file was staged rather than written into one of
/// the instance's import roots.
fn import_root<'a>(
    instance: &'a RegistryInstance,
    file: &ResolvedFile,
) -> Option<&'a hearthsound_plugin_host::ImportRoot> {
    if file.root == STAGING_ROOT {
        return None;
    }
    instance.host_config().import_root(&file.root)
}

/// Removes a `move` import root's copy of an imported file and its emptied
/// parent directories.
///
/// The cleanup runs after the import's transaction has committed, so a failure
/// here leaves a still-present backend file rather than a lost one. Directories
/// are removed only while they are empty and never past the import root.
///
/// # Parameters
///
/// - `instance`: the instance whose import root the file came from.
/// - `file`: the file the download resolved.
async fn post_import(instance: &RegistryInstance, file: &ResolvedFile) {
    let Some(root) = import_root(instance, file) else {
        return;
    };
    if root.read_only || root.mode != ImportMode::Move {
        return;
    }
    match tokio::fs::remove_file(&file.absolute_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                path = %file.absolute_path.display(),
                %error,
                "an imported file could not be removed from its import root"
            );
            return;
        }
    }
    let mut current = file.absolute_path.parent();
    while let Some(dir) = current {
        if dir == root.path || !dir.starts_with(&root.path) {
            break;
        }
        if tokio::fs::remove_dir(dir).await.is_err() {
            break;
        }
        current = dir.parent();
    }
}

/// Returns the file extension an imported file is named with.
///
/// # Parameters
///
/// - `file`: the resolved file.
/// - `probe`: its probe, the fallback when the reported path has no extension.
///
/// # Returns
///
/// The lowercased extension without the leading dot, or the probe's container.
fn extension_of(file: &ResolvedFile, probe: &ProbeResult) -> String {
    file.absolute_path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .filter(|extension| !extension.is_empty())
        .unwrap_or_else(|| probe.container.as_str().to_owned())
}

/// Returns the quality tier of an imported file.
///
/// The tier is computed from the probe's own attributes, so a backend that
/// mislabeled a file cannot give it a better tier than it deserves. Opus is the
/// one codec the container does not name, so it is passed through explicitly.
///
/// # Parameters
///
/// - `destination`: the file's final path, which names it.
/// - `probe`: the file's probe.
///
/// # Returns
///
/// The tier, or `None` when the attributes do not decide one.
fn quality_tier_of(destination: &Path, probe: &ProbeResult) -> Option<QualityTier> {
    let format = match probe.codec {
        Codec::Opus => "opus".to_owned(),
        _ => probe.container.as_str().to_owned(),
    };
    let candidate = CandidateFile {
        name: destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned(),
        size: None,
        format: Some(format),
        bitrate_kbps: probe.bitrate_kbps,
        sample_rate: probe.sample_rate,
        bit_depth: probe.bit_depth,
        duration_ms: u32::try_from(probe.duration_ms).ok(),
        track_index: None,
    };
    profile::quality_tier(&candidate)
}

/// Returns a path's part relative to a scanned folder's root.
///
/// # Parameters
///
/// - `root`: the folder's absolute path.
/// - `path`: the absolute path inside the folder.
///
/// # Returns
///
/// The path relative to the root, as the `media_files` row stores it, or the
/// whole path when it is not under the root.
fn folder_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Returns whether an import failed because the save folder cannot be written.
///
/// A folder that was removed, unmounted or mounted read-only, and one whose file
/// handle went stale, all answer one of these kinds; the import then keeps the
/// file in the managed library instead of failing the acquisition. Any other
/// error is a real failure.
///
/// # Parameters
///
/// - `error`: the error the placement returned.
///
/// # Returns
///
/// `true` when the file could be placed in the managed library instead.
fn is_folder_unavailable(error: &AcquireError) -> bool {
    let AcquireError::Io { source, .. } = error else {
        return false;
    };
    matches!(
        source.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::StaleNetworkFileHandle
    )
}

/// Returns whether a media file insert lost its path to a stored row.
///
/// SQLite reports the violated index in the message rather than in a field, so
/// the message is what identifies the conflict as one of the `media_files` path
/// indexes; a unique violation from anywhere else in the statement is a real
/// error and is not retried.
///
/// # Parameters
///
/// - `error`: the error the media file insert returned.
///
/// # Returns
///
/// `true` when the error is a unique-index conflict on a `media_files` path.
fn is_stored_path_conflict(error: &LibraryError) -> bool {
    let LibraryError::Database(sqlx::Error::Database(database)) = error else {
        return false;
    };
    database.is_unique_violation() && database.message().contains("media_files")
}

/// What linking a verified source into managed storage did.
enum Linked {
    /// The destination names the source's inode.
    Placed,
    /// A file already held the destination, which was left untouched.
    Taken,
    /// The filesystem refused the link, so the caller copies and tags instead.
    Unsupported,
}

impl AcquisitionOrchestrator {
    /// Links a verified source from a `link`-mode import root into managed
    /// storage.
    ///
    /// The link is created on the blocking pool, because linking across a busy
    /// filesystem can take as long as a stat. A destination that already exists
    /// is reported rather than replaced, which is what lets the caller move on
    /// to the next collision suffix.
    ///
    /// # Parameters
    ///
    /// Nothing in the server writes into a managed file after the import, so a
    /// `link`-mode file shares its inode with the import root's file without the
    /// source ever being modified: the tags are written to the staged copy in
    /// the acquisition's staging directory, and the cover art is written beside
    /// the file, never into it. A future retag or cover-embed feature has to
    /// break the link first.
    ///
    /// # Parameters
    ///
    /// - `source`: the absolute path of the file the candidate verified.
    /// - `destination`: the absolute managed path to link it to.
    ///
    /// # Returns
    ///
    /// [`Linked::Placed`] when the destination now names the source,
    /// [`Linked::Taken`] when a file holds the name, and
    /// [`Linked::Unsupported`] when the filesystem refused the link - a
    /// cross-device destination, a filesystem without hard links or a
    /// permission the server does not have.
    ///
    /// # Errors
    ///
    /// Returns the filesystem error when the destination's directory cannot be
    /// created.
    async fn link_into_place(
        &self,
        source: &Path,
        destination: &Path,
    ) -> Result<Linked, AcquireError> {
        if let Some(parent) = destination.parent() {
            create_dir_all(parent).await?;
        }
        match self.hardlink(source, destination).await {
            Ok(()) => Ok(Linked::Placed),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(Linked::Taken),
            Err(error) => {
                tracing::debug!(
                    source = %source.display(),
                    destination = %destination.display(),
                    %error,
                    "a source could not be linked into managed storage; copying it instead"
                );
                Ok(Linked::Unsupported)
            }
        }
    }
}

/// What installing a tagged staged file at its destination did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Installed {
    /// The file is at the destination.
    Placed,
    /// The destination already held a file, which was left untouched.
    Taken,
}

/// Installs a tagged staged file at its destination without replacing a file.
///
/// The staged file is hardlinked into place - the one install primitive that is
/// both atomic and refuses a name that is taken - and the staged copy is removed
/// afterwards, which is what a move would have done. A filesystem without hard
/// links gets a durable copy instead, and the copy claims its name the same way a
/// link does: it is linked from the temporary file, so a name that appeared
/// while the bytes were being written is reported as taken instead of being
/// replaced. Every way out removes the temporary file.
///
/// # Parameters
///
/// - `staged`: the tagged file in the acquisition's staging directory.
/// - `destination`: the final managed path.
///
/// # Returns
///
/// [`Installed::Placed`] when the file is at the destination, and
/// [`Installed::Taken`] when a file already held the name.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when a step fails for another reason.
async fn install(staged: &Path, destination: &Path) -> Result<Installed, AcquireError> {
    if let Some(parent) = destination.parent() {
        create_dir_all(parent).await?;
    }
    match tokio::fs::hard_link(staged, destination).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(staged).await;
            return Ok(Installed::Placed);
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(Installed::Taken);
        }
        // The two paths are on different filesystems, or the filesystem has no
        // hard links: copy instead, claiming the name on the way in.
        Err(_) => {}
    }
    let installed = copy_durable(staged, destination).await?;
    if installed == Installed::Placed {
        let _ = tokio::fs::remove_file(staged).await;
    }
    Ok(installed)
}

/// Copies a file durably to a destination on another filesystem.
///
/// The bytes are written to a temporary file beside the destination, flushed to
/// disk, and then linked into place. Linking is what claims the name: a
/// destination that appeared after the import chose it makes the link fail with
/// `AlreadyExists`, and the temporary file is removed instead of the other
/// file being replaced. Only a filesystem that refuses the link for another
/// reason falls back to a rename, which is the one path that can still replace
/// a file that appeared in between.
///
/// # Parameters
///
/// - `source`: the file to copy.
/// - `destination`: the final managed path.
///
/// # Returns
///
/// [`Installed::Placed`] when the file is at the destination and
/// [`Installed::Taken`] when a file holds the name; no temporary file is left
/// behind either way.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when a step fails; the temporary file is removed
/// first.
async fn copy_durable(source: &Path, destination: &Path) -> Result<Installed, AcquireError> {
    let temporary = match destination.parent() {
        Some(parent) => parent.join(format!(".{}.tmp", Uuid::now_v7())),
        None => destination.with_extension("tmp"),
    };
    if let Err(error) = copy_file(source, &temporary).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    let synced = async {
        let file = tokio::fs::File::open(&temporary).await?;
        file.sync_all().await
    }
    .await;
    if let Err(source) = synced {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(AcquireError::Io {
            path: temporary.clone(),
            source,
        });
    }
    match tokio::fs::hard_link(&temporary, destination).await {
        Ok(()) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(Installed::Placed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = tokio::fs::remove_file(&temporary).await;
            Ok(Installed::Taken)
        }
        Err(_) => {
            let renamed = tokio::fs::rename(&temporary, destination).await;
            if renamed.is_err() {
                let _ = tokio::fs::remove_file(&temporary).await;
            }
            renamed.map_err(|source| AcquireError::Io {
                path: destination.to_path_buf(),
                source,
            })?;
            Ok(Installed::Placed)
        }
    }
}

/// Copies a file, mapping the error onto the failing path.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when the copy fails.
async fn copy_file(source: &Path, destination: &Path) -> Result<(), AcquireError> {
    tokio::fs::copy(source, destination)
        .await
        .map_err(|source| AcquireError::Io {
            path: destination.to_path_buf(),
            source,
        })?;
    Ok(())
}

/// Creates a directory tree.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when the directory cannot be created.
async fn create_dir_all(path: &Path) -> Result<(), AcquireError> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|source| AcquireError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// Returns a file's modification time in unix milliseconds.
///
/// # Parameters
///
/// - `metadata`: the file's metadata.
///
/// # Returns
///
/// The modification time, or zero when the platform cannot report it.
fn mtime_millis(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}
