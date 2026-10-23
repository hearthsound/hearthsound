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
//! candidate. The design also quarantines the offending source in
//! `source_blocklist`; that table does not exist yet, so a rejected source is
//! only skipped for this acquisition - the next search may select it again.
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
//! The destination name is reserved before the file is written and held until
//! the transaction has committed, because two imports that render the same
//! relative path would otherwise both see it free and both choose it; a name a
//! stored row already holds is left to that row and the next collision suffix is
//! tried. An import that fails nonetheless ends the acquisition through the
//! normal failure path, so nothing is left in `importing`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use hearthsound_catalog::CoverArtRequest;
use hearthsound_config::keys;
use hearthsound_core::events::{LibraryChange, LibraryChanged, ServerEvent};
use hearthsound_core::providers::{CandidateFile, CatalogAlbum};
use hearthsound_core::{
    AcquisitionState, Availability, CatalogId, ClientId, EntityId, EntityKind, FailureReason,
    ImportMode, PinReason, QualityTier, StorageClass,
};
use hearthsound_library::naming::{NamingContext, NamingTemplate};
use hearthsound_library::{
    LibraryError, ManagedMediaFile, Track, media_files, placement, retention, tracks,
};
use hearthsound_media::{
    Artwork, Codec, ProbeResult, probe, verify_decodes, write_sidecar_cover, write_tags,
};
use hearthsound_plugin_host::RegistryInstance;
use hearthsound_plugin_host::manifest::ImageCachePolicy;
use uuid::Uuid;

use crate::download::{ResolvedFile, STAGING_ROOT, StoredResolvedFile, stored_candidate};
use crate::error::AcquireError;
use crate::orchestrator::{AcquisitionOrchestrator, DestinationReservation};
use crate::profile;
use crate::repo::{self, AcquisitionRow};

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

/// Why a candidate was rejected.
struct Rejection {
    /// The failure reason the acquisition uses when nothing is left.
    reason: FailureReason,
    /// What the check saw.
    detail: String,
}

/// What an album row's issuing catalog instance contributes to an import.
///
/// A materialized catalog track can be the only stored row of its album, so the
/// album row says nothing about how many tracks or discs the release has; the
/// album its issuing instance lists is then the source. An album nothing issued
/// is local, and its stored rows are all of it.
enum IssuedAlbum {
    /// The album the issuing catalog instance lists: the artist it credits the
    /// album with, when it credits one, and the totals its tracklist implies for
    /// the imported track.
    Catalog {
        /// The first artist the album credits.
        album_artist: Option<String>,
        /// The disc and track totals the tracklist implies.
        totals: AlbumTotals,
    },
    /// No catalog instance issued the album, so its stored rows are all of it.
    Local,
    /// A catalog instance issued the album but its album could not be read.
    Unavailable,
}

impl IssuedAlbum {
    /// Returns the artist the issuing catalog instance credits the album with.
    ///
    /// # Returns
    ///
    /// The credit's display name, or `None` for anything but a catalog album
    /// that credits an artist. A provider whose track-embedded album carries no
    /// artist - Deezer's, for example - leaves the caller to fall back to the
    /// track's own artist, which is the only album artist such an import has.
    fn album_artist(&self) -> Option<&str> {
        match self {
            IssuedAlbum::Catalog { album_artist, .. } => album_artist.as_deref(),
            IssuedAlbum::Local | IssuedAlbum::Unavailable => None,
        }
    }
}

/// The sources an import picks the album artist from, in order of reliability.
///
/// The stored album row comes first because it is what the rest of the library
/// shows. The issuing catalog instance's own album comes next, because the
/// album reference a lone materialized track carries may omit the artist -
/// Deezer's does - while its full album lists one. The track's own artist is the
/// last resort: an album nobody credited still has to produce a file whose path
/// and `albumartist` tag name an artist.
struct AlbumArtistSources<'a> {
    /// The album row's display artist.
    stored: Option<&'a str>,
    /// The first artist the album stored a credit for.
    stored_credit: Option<&'a str>,
    /// The first artist the issuing catalog instance credits the album with.
    catalog_credit: Option<&'a str>,
    /// The track's display artist.
    track: Option<&'a str>,
    /// The first artist the track is credited to.
    track_credit: Option<&'a str>,
}

impl AlbumArtistSources<'_> {
    /// Returns the album artist the imported file is named and tagged with.
    ///
    /// # Returns
    ///
    /// The first source that names an artist, or `None` when none of them does.
    fn pick(&self) -> Option<String> {
        self.stored
            .or(self.stored_credit)
            .or(self.catalog_credit)
            .or(self.track)
            .or(self.track_credit)
            .map(str::to_owned)
    }
}

/// The disc and track totals an imported file is tagged and rendered with.
///
/// A total the import cannot derive is `None`, which writes no tag for it and
/// renders no disc prefix rather than claiming a number nothing supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AlbumTotals {
    /// How many discs the release has, when it is known to have more than one.
    disc_total: Option<u32>,
    /// How many tracks the imported track's disc holds, when it is known.
    track_total: Option<u32>,
}

/// Counts the totals a catalog album's tracklist implies.
///
/// A multi-disc release counts only the tracks it holds on the imported track's
/// own disc, which is what a `TRACKTOTAL` tag on such a release means, and a
/// multi-disc release whose imported track names no disc gets no track total,
/// because the disc the count would describe is unknown.
///
/// # Parameters
///
/// - `album`: the album the issuing instance lists.
/// - `disc_number`: the imported track's disc number.
///
/// # Returns
///
/// The release's disc count and the imported disc's track count.
fn totals_of_catalog_album(album: &CatalogAlbum, disc_number: Option<i64>) -> AlbumTotals {
    let disc_total = album
        .tracks
        .iter()
        .filter_map(|track| track.disc_number)
        .max()
        .filter(|disc| *disc > 1)
        .map(u32::from);
    let track_total = match disc_total {
        Some(_) => disc_number
            .and_then(|disc| u16::try_from(disc).ok())
            .and_then(|disc| {
                let count = album
                    .tracks
                    .iter()
                    .filter(|track| track.disc_number == Some(disc))
                    .count();
                u32::try_from(count).ok()
            }),
        None => u32::try_from(album.tracks.len()).ok(),
    }
    .filter(|count| *count > 0);
    AlbumTotals {
        disc_total,
        track_total,
    }
}

/// Counts the totals a local album's stored rows imply.
///
/// # Parameters
///
/// - `rows`: the album's stored track rows.
///
/// # Returns
///
/// The highest disc number the rows carry, when they carry more than one, and
/// how many rows the album has.
fn totals_of_stored_rows(rows: &[Track]) -> AlbumTotals {
    let disc_total = rows
        .iter()
        .filter_map(|track| track.disc_number)
        .max()
        .and_then(|disc| u32::try_from(disc).ok())
        .filter(|disc| *disc > 1);
    let track_total = u32::try_from(rows.len()).ok().filter(|count| *count > 0);
    AlbumTotals {
        disc_total,
        track_total,
    }
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
                self.log_rejection(acquisition_id, &rejection).await;
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
                self.log_rejection(id, &rejection).await;
                self.fail_acquisition(id, rejection.reason, Some(&rejection.detail))
                    .await
            }
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
    /// - `rejection`: what the check saw.
    async fn log_rejection(&self, acquisition_id: Uuid, rejection: &Rejection) {
        let message = format!(
            "rejected a candidate as {}: {}",
            rejection.reason.as_str(),
            rejection.detail
        );
        let Ok(mut tx) = self.database().writer().begin().await else {
            return;
        };
        if crate::repo::write_log(
            &mut tx,
            acquisition_id,
            AcquisitionState::Downloading,
            Some(&message),
        )
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
            verified.push(VerifiedFile {
                file: file.clone(),
                probe,
            });
        }
        Ok(verified)
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
            Ok(()) => Ok(ImportOutcome::Imported),
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
    ) -> Result<(), AcquireError> {
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

        let track = self
            .library()
            .get_track(track_id)
            .await?
            .ok_or(AcquireError::UnknownTrack(track_id))?;
        let album = match track.album_id {
            Some(album_id) => self.library().get_album(album_id).await?,
            None => None,
        };
        let album_tracks = match track.album_id {
            Some(album_id) => self.library().list_tracks_for_album(album_id).await?,
            None => Vec::new(),
        };
        let issued = match track.album_id {
            Some(album_id) => self.issued_album(album_id, track.disc_number).await,
            None => IssuedAlbum::Local,
        };
        let artists = self.library().list_track_artists(track_id).await?;
        let album_artists = match track.album_id {
            Some(album_id) => self.library().list_album_artists(album_id).await?,
            None => Vec::new(),
        };
        let genres = self.library().list_track_genres(track_id).await?;
        let pinned = self.library().track_is_pinned(track_id).await?;

        let snapshot = self.settings().current();
        let template = NamingTemplate::from_settings(&snapshot)?;
        let embed_cover = snapshot.get(keys::LIBRARY_TAGS_EMBED_COVER)?;
        let cover_filename = snapshot.get(keys::LIBRARY_COVER_FILENAME)?;

        let extension = extension_of(&verified.file, &verified.probe);
        let album_artist = AlbumArtistSources {
            stored: album
                .as_ref()
                .and_then(|album| album.display_artist.as_deref()),
            stored_credit: album_artists.first().map(|artist| artist.name.as_str()),
            catalog_credit: issued.album_artist(),
            track: track.display_artist.as_deref(),
            track_credit: artists.first().map(|artist| artist.name.as_str()),
        }
        .pick();
        let album_title = album.as_ref().map(|album| album.title.clone());
        let year = album.as_ref().and_then(|album| album.year);
        let AlbumTotals {
            disc_total,
            track_total,
        } = match issued {
            IssuedAlbum::Catalog { totals, .. } => totals,
            IssuedAlbum::Local => totals_of_stored_rows(&album_tracks),
            IssuedAlbum::Unavailable => AlbumTotals::default(),
        };
        let context = NamingContext {
            album_artist: album_artist.as_deref(),
            album: album_title.as_deref(),
            year,
            disc_number: track.disc_number.and_then(|disc| u32::try_from(disc).ok()),
            disc_total,
            track_number: track
                .track_number
                .and_then(|number| u32::try_from(number).ok()),
            title: Some(track.title.as_str()),
            extension: Some(extension.as_str()),
        };
        let relative = template.render(&context)?;
        let storage = if pinned {
            StorageClass::Library
        } else {
            StorageClass::Cache
        };
        let base = retention::import_destination(pinned, self.storage_roots(), &relative);

        // Work on a copy inside the acquisition's staging directory, so the
        // backend's own file is never written to and a crash cannot leave a
        // half-tagged file at the destination path.
        let staged_dir = self.staging_dir(acquisition_id).join(IMPORT_STAGING);
        create_dir_all(&staged_dir).await?;

        let cover = self.cover_art(track_id, track.album_id).await;
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
            genres: genres.iter().map(|genre| genre.name.clone()).collect(),
            isrc: track.isrc.clone(),
            mb_recording_id: track.mb_recording_id.clone(),
            mb_release_id: album.as_ref().and_then(|album| album.mb_release_id.clone()),
            mb_release_group_id: album
                .as_ref()
                .and_then(|album| album.mb_release_group_id.clone()),
            mb_artist_ids: artists
                .iter()
                .filter_map(|artist| artist.mb_artist_id.clone())
                .collect(),
            mb_album_artist_ids: album_artists
                .iter()
                .filter_map(|artist| artist.mb_artist_id.clone())
                .collect(),
            acquired_via: Some(instance.slug().clone()),
            ..Default::default()
        };

        // The names this import has already lost: one another import in this
        // process reserved, one a file already holds, and one a stored row
        // holds while its file is gone. Every lost name moves the import to the
        // next collision suffix, which is what keeps the retry from picking the
        // same name again.
        let mut taken: HashSet<PathBuf> = HashSet::new();
        for _ in 0..placement::MAX_COLLISIONS {
            let (destination, reservation) = self.reserve_destination(&base, &mut taken).await?;
            let rel_path = placement::relative_path(self.storage_roots(), storage, &destination);
            let staged = staged_dir.join(
                destination
                    .file_name()
                    .unwrap_or_else(|| relative.as_os_str()),
            );
            copy_file(&verified.file.absolute_path, &staged).await?;
            write_tags(
                &staged,
                &tags,
                if embed_cover { cover.as_ref() } else { None },
            )
            .await?;
            match install(&staged, &destination).await? {
                Installed::Placed => {}
                Installed::Taken => {
                    taken.insert(destination);
                    continue;
                }
            }

            if let Some(artwork) = &cover
                && let Some(parent) = destination.parent()
            {
                write_sidecar_cover(parent, &cover_filename, artwork).await?;
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
            let availability = if pinned {
                Availability::Local
            } else {
                Availability::Cached
            };

            let mut tx = self.database().writer().begin().await?;
            let inserted = media_files::insert_managed_media_file(
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
            .await;
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
            // A track with no retention row would be invisible to the eviction
            // query, which joins `track_retention`; the row is created by the first
            // pin write, so an unpinned track gets an empty one here.
            if !pinned {
                retention::add_pin_reason(&mut tx, track_id, PinReason::Admin).await?;
                retention::remove_pin_reason(&mut tx, track_id, PinReason::Admin).await?;
            }
            tracks::set_track_availability(&mut tx, track_id, availability, None).await?;
            repo::complete(
                &mut tx,
                acquisition_id,
                media_file_id,
                Some("the file was imported into managed storage"),
            )
            .await?;
            tx.commit().await?;
            drop(reservation);

            let mut progress = self.progress_of_row(row, row.priority);
            progress.state = AcquisitionState::Completed;
            progress.instance = Some(instance.slug().as_str().to_owned());
            progress.percent = None;
            progress.eta_secs = None;
            progress.file = Some(destination.clone());
            self.announce(&progress);
            self.publish_availability(track_id, availability, tier);
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
                storage = storage.as_str(),
                rel_path = rel_path.as_str(),
                source_root = verified.file.root.as_str(),
                source_path = verified.file.path.as_str(),
                source_bytes = verified.file.size,
                "imported the acquired file"
            );

            post_import(instance, &verified.file).await;
            self.release_staging(acquisition_id).await;
            return Ok(());
        }

        Err(AcquireError::ImportPath { path: base })
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

    /// What the issuing catalog instance adds to a stored album row's import.
    ///
    /// A materialized catalog track can be the only stored row of its album, so
    /// the album row says nothing about how many tracks or discs the release
    /// has; the instance's own album is the reliable source. The album is
    /// found through the `p:<slug>` alias the materialization wrote, and read
    /// through the catalog cache, so a listing an album page just fetched costs
    /// no second provider call.
    ///
    /// # Parameters
    ///
    /// - `album_id`: the stored album row to resolve.
    /// - `disc_number`: the imported track's disc number, which selects the disc
    ///   a multi-disc album's track total counts.
    ///
    /// # Returns
    ///
    /// [`IssuedAlbum::Catalog`] with the instance's album artist and the totals
    /// its tracklist implies for an album a catalog instance issued,
    /// [`IssuedAlbum::Local`] for one nothing issued, and
    /// [`IssuedAlbum::Unavailable`] when the alias exists but its album could
    /// not be read. A failure is logged at debug and never propagates: an import
    /// that cannot reach its catalog instance still imports the file it
    /// verified.
    async fn issued_album(&self, album_id: EntityId, disc_number: Option<i64>) -> IssuedAlbum {
        let alias = match self.catalog().provider_aliases(album_id).await {
            Ok(aliases) => match aliases.into_iter().next() {
                Some(alias) => alias,
                None => return IssuedAlbum::Local,
            },
            Err(error) => {
                tracing::debug!(
                    album = %album_id,
                    %error,
                    "the provider alias of an imported track's album could not be read"
                );
                return IssuedAlbum::Unavailable;
            }
        };
        let id = match CatalogId::new(alias.slug, EntityKind::Album, alias.provider_id) {
            Ok(id) => id,
            Err(error) => {
                tracing::debug!(
                    album = %album_id,
                    %error,
                    "the provider alias of an imported track's album does not name an album"
                );
                return IssuedAlbum::Unavailable;
            }
        };
        match self.catalog().album(&id).await {
            Ok(result) => IssuedAlbum::Catalog {
                album_artist: result
                    .album
                    .artists
                    .first()
                    .map(|credit| credit.name.clone()),
                totals: totals_of_catalog_album(&result.album, disc_number),
            },
            Err(error) => {
                tracing::debug!(
                    album = %album_id,
                    instance = id.slug().as_str(),
                    %error,
                    "the catalog album of an imported track could not be read"
                );
                IssuedAlbum::Unavailable
            }
        }
    }

    /// Fetches the cover art an imported track should carry.
    ///
    /// The cover is only persisted when the catalog instance that issued the
    /// track's catalog entity allows its images to be stored: an instance whose
    /// manifest declares `catalog.imageCachePolicy = "no-store"` (Deezer, for
    /// example) gets no sidecar file and no embedded picture, because storing
    /// its bytes on disk is outside what its manifest permits.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track being imported.
    /// - `album_id`: the track's album, when it has one; the cover is looked up
    ///   for the album when it does.
    ///
    /// # Returns
    ///
    /// The cover, or `None` when no catalog instance issued the track, the
    /// issuing instance forbids storing its images, or it has no image.
    async fn cover_art(&self, track_id: EntityId, album_id: Option<EntityId>) -> Option<Artwork> {
        let aliases = self.catalog().provider_aliases(track_id).await.ok()?;
        let alias = aliases.first()?;
        let instance = self.registry().instance(alias.slug.as_str())?;
        if instance.image_cache_policy() != ImageCachePolicy::Cache {
            return None;
        }
        let request = CoverArtRequest {
            id: ClientId::Entity(album_id.unwrap_or(track_id)),
            size: None,
        };
        let cover = self.catalog().cover_art(&request).await.ok()?;
        Some(Artwork::new(cover.mime_type, cover.bytes))
    }
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

/// What installing a tagged staged file at its destination did.
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
/// links gets a durable copy instead, and a name that turns out to be taken is
/// reported so the caller can reserve the next collision suffix rather than
/// overwrite the file that holds it.
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
        Err(_) => {}
    }
    if placement::path_exists(destination).await? {
        return Ok(Installed::Taken);
    }
    copy_durable(staged, destination).await?;
    let _ = tokio::fs::remove_file(staged).await;
    Ok(Installed::Placed)
}

/// Copies a file durably to a destination on another filesystem.
///
/// The bytes are written to a temporary file beside the destination, flushed to
/// disk, and renamed into place, so a crash never exposes a partial file at the
/// destination path.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when a step fails.
async fn copy_durable(source: &Path, destination: &Path) -> Result<(), AcquireError> {
    let temporary = match destination.parent() {
        Some(parent) => parent.join(format!(".{}.tmp", Uuid::now_v7())),
        None => destination.with_extension("tmp"),
    };
    copy_file(source, &temporary).await?;
    let file = tokio::fs::File::open(&temporary)
        .await
        .map_err(|source| AcquireError::Io {
            path: temporary.clone(),
            source,
        })?;
    file.sync_all().await.map_err(|source| AcquireError::Io {
        path: temporary.clone(),
        source,
    })?;
    drop(file);
    tokio::fs::rename(&temporary, destination)
        .await
        .map_err(|source| AcquireError::Io {
            path: destination.to_path_buf(),
            source,
        })
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

#[cfg(test)]
mod tests {
    use hearthsound_core::IdPrefix;
    use hearthsound_core::providers::CatalogTrack;
    use hearthsound_library::Origin;
    use uuid::Uuid;

    use super::*;

    /// Builds a catalog album holding `discs[i]` tracks on disc `i + 1`.
    ///
    /// # Parameters
    ///
    /// - `discs`: the track count of each disc, disc 1 first.
    fn album(discs: &[u16]) -> CatalogAlbum {
        let mut tracks = Vec::new();
        for (index, count) in discs.iter().enumerate() {
            let disc = u16::try_from(index + 1).unwrap();
            for number in 1..=*count {
                tracks.push(CatalogTrack {
                    id: format!("track-{disc}-{number}"),
                    title: format!("Song {disc}-{number}"),
                    artists: Vec::new(),
                    album: None,
                    duration_ms: None,
                    disc_number: Some(disc),
                    track_number: Some(number),
                    isrc: None,
                    explicit: None,
                    ids: Vec::new(),
                });
            }
        }
        CatalogAlbum {
            id: "album-1".to_owned(),
            title: "Album".to_owned(),
            artists: Vec::new(),
            release_date: None,
            album_type: None,
            upc: None,
            genres: Vec::new(),
            images: Vec::new(),
            tracks,
            ids: Vec::new(),
        }
    }

    /// Builds a stored track row on the given disc.
    ///
    /// # Parameters
    ///
    /// - `disc_number`: the row's disc number.
    fn row(disc_number: Option<i64>) -> Track {
        Track {
            id: EntityId::from_uuid(IdPrefix::Track, Uuid::nil()),
            album_id: None,
            title: "Song".to_owned(),
            sort_title: "Song".to_owned(),
            display_artist: None,
            disc_number,
            track_number: None,
            duration_ms: None,
            isrc: None,
            mb_recording_id: None,
            explicit: None,
            origin: Origin::Catalog,
            availability: Availability::Remote,
            unavailable_until: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn the_album_artist_comes_from_the_first_source_that_names_one() {
        let all = AlbumArtistSources {
            stored: Some("Stored"),
            stored_credit: Some("Stored Credit"),
            catalog_credit: Some("Catalog Credit"),
            track: Some("Track Artist"),
            track_credit: Some("Track Credit"),
        };
        assert_eq!(all.pick().as_deref(), Some("Stored"));

        let without_stored = AlbumArtistSources {
            stored: None,
            ..all
        };
        assert_eq!(without_stored.pick().as_deref(), Some("Stored Credit"));

        let catalog_only = AlbumArtistSources {
            stored_credit: None,
            ..without_stored
        };
        assert_eq!(catalog_only.pick().as_deref(), Some("Catalog Credit"));

        let track_only = AlbumArtistSources {
            catalog_credit: None,
            ..catalog_only
        };
        assert_eq!(track_only.pick().as_deref(), Some("Track Artist"));

        let credit_only = AlbumArtistSources {
            track: None,
            ..track_only
        };
        assert_eq!(credit_only.pick().as_deref(), Some("Track Credit"));

        let nothing = AlbumArtistSources {
            track_credit: None,
            ..credit_only
        };
        assert_eq!(nothing.pick(), None);
    }

    #[test]
    fn a_single_disc_album_counts_all_of_its_tracks() {
        assert_eq!(
            totals_of_catalog_album(&album(&[5]), Some(1)),
            AlbumTotals {
                disc_total: None,
                track_total: Some(5),
            }
        );
    }

    #[test]
    fn a_multi_disc_album_counts_the_tracks_of_the_imported_disc() {
        let album = album(&[4, 3]);
        assert_eq!(
            totals_of_catalog_album(&album, Some(2)),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(3),
            }
        );
        assert_eq!(
            totals_of_catalog_album(&album, Some(1)),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(4),
            }
        );
    }

    #[test]
    fn a_multi_disc_album_without_a_disc_number_writes_no_track_total() {
        assert_eq!(
            totals_of_catalog_album(&album(&[4, 3]), None),
            AlbumTotals {
                disc_total: Some(2),
                track_total: None,
            }
        );
    }

    #[test]
    fn an_album_without_tracks_writes_no_total() {
        assert_eq!(
            totals_of_catalog_album(&album(&[]), Some(1)),
            AlbumTotals::default()
        );
        assert_eq!(totals_of_stored_rows(&[]), AlbumTotals::default());
    }

    #[test]
    fn stored_rows_count_toward_a_local_album() {
        assert_eq!(
            totals_of_stored_rows(&[row(Some(1)), row(Some(1)), row(Some(2))]),
            AlbumTotals {
                disc_total: Some(2),
                track_total: Some(3),
            }
        );
        assert_eq!(
            totals_of_stored_rows(&[row(Some(1)), row(None)]),
            AlbumTotals {
                disc_total: None,
                track_total: Some(2),
            }
        );
    }
}
