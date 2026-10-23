//! Fingerprint verification and source quarantine for acquired files.
//!
//! The checks in [`crate::import`] prove that a file decodes, lasts about as
//! long as the wanted track and lives up to the tier its candidate claimed. A
//! file can pass all of them and still be a different recording - a live take, a
//! cover, the wrong track from a folder - so when
//! `acquisition.verify.fingerprint` is on, the host computes the file's
//! Chromaprint fingerprint and asks the first enabled instance of the
//! verification layer whether it is the wanted recording.
//!
//! A mismatch quarantines the file's source in `source_blocklist` for
//! `acquisition.verify.quarantine_days`, which keeps later searches from
//! selecting it again. A `match` accepts the file; an `unknown` verdict and a
//! provider error also accept it, because the server must not refuse an
//! acquisition over a service that has no opinion or is down. Every such
//! outcome is written to the acquisition's log, so an administrator can see
//! what the check decided.
//!
//! The stage runs before the import for every priority but `play`, which a user
//! is waiting on: that file is imported and streamed first, checked afterwards,
//! and a mismatch removes it again and starts a background acquisition.

use std::path::Path;

use hearthsound_config::keys;
use hearthsound_core::providers::{ExpectedTrack, ExternalId, Fingerprint, Verdict};
use hearthsound_core::{Layer, Priority};
use hearthsound_library::{Track, placement};
use hearthsound_media::fingerprint;
use hearthsound_plugin_host::RegistryInstance;
use uuid::Uuid;

use crate::blocklist;
use crate::download::ResolvedFile;
use crate::error::AcquireError;
use crate::orchestrator::{AcquireRequest, AcquisitionOrchestrator};
use crate::repo::AcquisitionRow;

/// The source and file a quarantine entry names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceFile<'a> {
    /// The backend's key for the source.
    pub(crate) source_key: &'a str,
    /// The candidate file's name.
    pub(crate) file_key: &'a str,
}

impl AcquisitionOrchestrator {
    /// Returns the first enabled instance of the verification layer.
    ///
    /// # Returns
    ///
    /// The instance's verification provider, or `None` when no enabled instance
    /// exports the verification interface.
    pub(crate) fn verification_provider(
        &self,
    ) -> Option<std::sync::Arc<dyn hearthsound_core::providers::VerificationProvider>> {
        self.registry()
            .layers(Layer::Verification)
            .into_iter()
            .find_map(|instance| instance.verification().cloned())
    }

    /// Returns whether fingerprint verification is enabled.
    ///
    /// # Errors
    ///
    /// Returns the settings error when the flag cannot be read.
    pub(crate) fn fingerprint_verification_enabled(&self) -> Result<bool, AcquireError> {
        Ok(self
            .settings()
            .current()
            .get(keys::ACQUISITION_VERIFY_FINGERPRINT)?)
    }

    /// Builds the recording a fingerprint is checked against from a track.
    ///
    /// # Parameters
    ///
    /// - `track`: the library track the file must be.
    ///
    /// # Returns
    ///
    /// The title, the artists in credit order, the album, the duration and the
    /// identifiers the library knows for the recording.
    ///
    /// # Errors
    ///
    /// Returns the library error of the artist or album read.
    pub(crate) async fn expected_track(
        &self,
        track: &Track,
    ) -> Result<ExpectedTrack, AcquireError> {
        let credits = self.library().list_track_artists(track.id).await?;
        let album = match track.album_id {
            Some(album_id) => self.library().get_album(album_id).await?,
            None => None,
        };
        let mut ids = Vec::new();
        if let Some(value) = &track.mb_recording_id {
            ids.push(ExternalId {
                namespace: "mb-recording".to_owned(),
                value: value.clone(),
            });
        }
        if let Some(value) = &track.isrc {
            ids.push(ExternalId {
                namespace: "isrc".to_owned(),
                value: value.clone(),
            });
        }
        if let Some(value) = album.as_ref().and_then(|album| album.mb_release_id.clone()) {
            ids.push(ExternalId {
                namespace: "mb-release".to_owned(),
                value,
            });
        }
        for credit in &credits {
            if let Some(value) = &credit.mb_artist_id {
                ids.push(ExternalId {
                    namespace: "mb-artist".to_owned(),
                    value: value.clone(),
                });
            }
        }
        Ok(ExpectedTrack {
            title: track.title.clone(),
            artists: credits.into_iter().map(|credit| credit.name).collect(),
            album: album.map(|album| album.title),
            duration_ms: track
                .duration_ms
                .and_then(|duration| u32::try_from(duration).ok()),
            ids,
        })
    }

    /// Fingerprints one file and checks it against the wanted track.
    ///
    /// The check only runs when fingerprint verification is enabled and the
    /// verification layer has an enabled instance. A file the service cannot
    /// fingerprint, a service that cannot be reached and an `unknown` verdict
    /// all let the file through; each outcome is written to the acquisition's
    /// log. A `mismatch` quarantines the source and is returned to the caller.
    ///
    /// # Parameters
    ///
    /// - `instance`: the acquisition instance that served the file.
    /// - `row`: the acquisition's ledger row, whose candidate names the source.
    /// - `track`: the track the file must be.
    /// - `file`: the downloaded file to check.
    /// - `source`: the backend source key and candidate file name a quarantine
    ///   entry stores.
    ///
    /// # Returns
    ///
    /// `Some(detail)` when the service judged the file a different recording,
    /// and `None` when the file may pass.
    ///
    /// # Errors
    ///
    /// Returns the library or settings error that stopped the check.
    pub(crate) async fn check_fingerprint(
        &self,
        instance: &RegistryInstance,
        row: &AcquisitionRow,
        track: &Track,
        file: &ResolvedFile,
        source: SourceFile<'_>,
    ) -> Result<Option<String>, AcquireError> {
        let acquisition_id = row.id;
        if !self.fingerprint_verification_enabled()? {
            return Ok(None);
        }
        let Some(provider) = self.verification_provider() else {
            return Ok(None);
        };
        let expected = self.expected_track(track).await?;
        let print = match fingerprint(&file.absolute_path, self.ffmpeg()).await {
            Ok(print) => print,
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition_id,
                    path = %file.absolute_path.display(),
                    %error,
                    "a downloaded file could not be fingerprinted"
                );
                self.log_message(
                    acquisition_id,
                    row.state,
                    &format!(
                        "could not fingerprint `{}`; the file passes: {error}",
                        file.path
                    ),
                )
                .await?;
                return Ok(None);
            }
        };
        let result = match provider
            .verify(
                &expected,
                &fingerprint_of_core(&print.value, print.duration_secs),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition_id,
                    instance = instance.slug().as_str(),
                    %error,
                    "the verification provider could not check a downloaded file"
                );
                self.log_message(
                    acquisition_id,
                    row.state,
                    &format!(
                        "the verification service could not check `{}`; the file passes: {error}",
                        file.path
                    ),
                )
                .await?;
                return Ok(None);
            }
        };
        match result.verdict {
            Verdict::Match => Ok(None),
            Verdict::Unknown => {
                self.log_message(
                    acquisition_id,
                    row.state,
                    &format!(
                        "the verification service could not identify `{}`; the file passes",
                        file.path
                    ),
                )
                .await?;
                Ok(None)
            }
            Verdict::Mismatch => {
                let detail = result
                    .detail
                    .unwrap_or_else(|| format!("the service scored the file {:.2}", result.score));
                self.quarantine(
                    acquisition_id,
                    instance,
                    row,
                    source.source_key,
                    source.file_key,
                    &detail,
                )
                .await?;
                Ok(Some(detail))
            }
        }
    }

    /// Checks a file this acquisition imported at `play` priority.
    ///
    /// The file has already been streamed, so the check runs in the background
    /// of the acquisition: a mismatch quarantines the source, removes the
    /// managed file and its row, publishes the track's new availability and
    /// starts a background acquisition from another source. Every failure is
    /// logged rather than returned, because the acquisition has completed and
    /// the stream must not depend on the check.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition that imported the file.
    /// - `instance`: the acquisition instance that served it.
    /// - `row`: the acquisition's ledger row.
    /// - `track`: the track the file was imported for.
    /// - `imported`: the managed file the import wrote.
    /// - `source`: the backend source key and candidate file name a quarantine
    ///   entry stores.
    pub(crate) async fn check_play_import(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        row: &AcquisitionRow,
        track: &Track,
        imported: &crate::import::ImportedFile,
        source: SourceFile<'_>,
    ) {
        if row.priority != Priority::Play {
            return;
        }
        let checked = self
            .check_imported_fingerprint(
                acquisition_id,
                instance,
                row,
                track,
                &imported.destination,
                source,
            )
            .await;
        let detail = match checked {
            Ok(Some(detail)) => detail,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition_id,
                    track = %track.id,
                    %error,
                    "a play import could not be fingerprint checked"
                );
                return;
            }
        };
        if let Err(error) = self
            .remove_imported_file(
                acquisition_id,
                track,
                &imported.destination,
                imported.media_file_id,
                &detail,
            )
            .await
        {
            tracing::warn!(
                acquisition = %acquisition_id,
                track = %track.id,
                %error,
                "a mismatching play import could not be removed"
            );
            return;
        }
        if let Err(error) = self
            .request(AcquireRequest::new(
                track.id,
                Priority::Background,
                format!("fingerprint mismatch: {detail}"),
            ))
            .await
        {
            tracing::warn!(
                acquisition = %acquisition_id,
                track = %track.id,
                %error,
                "a track whose play import mismatched could not be acquired again"
            );
        }
    }

    /// Runs the fingerprint check of a managed `play` import.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition that imported the file.
    /// - `instance`: the acquisition instance that served it.
    /// - `row`: the acquisition's ledger row.
    /// - `track`: the track the file was imported for.
    /// - `managed`: the managed path the import wrote.
    /// - `source`: the backend source key and candidate file name a quarantine
    ///   entry stores.
    ///
    /// # Returns
    ///
    /// `Some(detail)` when the file is a different recording.
    ///
    /// # Errors
    ///
    /// Returns the library or settings error that stopped the check.
    async fn check_imported_fingerprint(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        row: &AcquisitionRow,
        track: &Track,
        managed: &Path,
        source: SourceFile<'_>,
    ) -> Result<Option<String>, AcquireError> {
        if !self.fingerprint_verification_enabled()? {
            return Ok(None);
        }
        let Some(provider) = self.verification_provider() else {
            return Ok(None);
        };
        let expected = self.expected_track(track).await?;
        let print = match fingerprint(managed, self.ffmpeg()).await {
            Ok(print) => print,
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition_id,
                    track = %track.id,
                    %error,
                    "an imported play file could not be fingerprinted"
                );
                self.log_message(
                    acquisition_id,
                    row.state,
                    &format!("could not fingerprint the imported file; it stays: {error}"),
                )
                .await?;
                return Ok(None);
            }
        };
        let result = match provider
            .verify(
                &expected,
                &fingerprint_of_core(&print.value, print.duration_secs),
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition_id,
                    instance = instance.slug().as_str(),
                    %error,
                    "the verification provider could not check an imported play file"
                );
                self.log_message(
                    acquisition_id,
                    row.state,
                    &format!("the verification service could not check the imported file; it stays: {error}"),
                )
                .await?;
                return Ok(None);
            }
        };
        match result.verdict {
            Verdict::Match => Ok(None),
            Verdict::Unknown => {
                self.log_message(
                    acquisition_id,
                    row.state,
                    "the verification service could not identify the imported file; it stays",
                )
                .await?;
                Ok(None)
            }
            Verdict::Mismatch => {
                let detail = result
                    .detail
                    .unwrap_or_else(|| format!("the service scored the file {:.2}", result.score));
                self.quarantine(
                    acquisition_id,
                    instance,
                    row,
                    source.source_key,
                    source.file_key,
                    &detail,
                )
                .await?;
                Ok(Some(detail))
            }
        }
    }

    /// Writes a mismatching file's source to the blocklist.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose source is quarantined.
    /// - `instance`: the instance that offered the file.
    /// - `row`: the acquisition's ledger row, for the log's state.
    /// - `source_key`: the backend's key for the source.
    /// - `file_key`: the candidate file's name.
    /// - `detail`: what the verification service said, for the reason.
    ///
    /// # Errors
    ///
    /// Returns the settings or database error that stopped the write. The
    /// caller still treats the file as rejected, because the verdict is the
    /// service's, not the table's.
    async fn quarantine(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        row: &AcquisitionRow,
        source_key: &str,
        file_key: &str,
        detail: &str,
    ) -> Result<(), AcquireError> {
        let days = self
            .settings()
            .current()
            .get(keys::ACQUISITION_VERIFY_QUARANTINE_DAYS)?;
        let reason = format!("fingerprint mismatch: {detail}");
        let now = crate::now_millis();
        let mut tx = self.database().writer().begin().await?;
        let expires_at = blocklist::block(
            &mut tx,
            instance.id(),
            source_key,
            file_key,
            &reason,
            now,
            days,
        )
        .await?;
        tx.commit().await?;
        tracing::warn!(
            acquisition = %acquisition_id,
            instance = instance.slug().as_str(),
            source = source_key,
            file = file_key,
            expires_at,
            detail,
            "quarantined a source that handed back the wrong recording"
        );
        self.log_message(
            acquisition_id,
            row.state,
            &format!("quarantined `{file_key}` from the source `{source_key}`: {reason}"),
        )
        .await?;
        Ok(())
    }

    /// Removes a managed file a `play` acquisition imported and re-settles the
    /// track.
    ///
    /// The row and the track's availability change in one transaction, through
    /// the same removal path eviction uses; the file itself is removed after it
    /// commits and the directories it emptied are pruned, and the new
    /// availability is published.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition that imported the file.
    /// - `track`: the track the file was imported for.
    /// - `managed`: the managed path the import wrote.
    /// - `media_file_id`: the `media_files` row the import wrote.
    /// - `detail`: what the verification service said, for the log.
    ///
    /// # Errors
    ///
    /// Returns the library, database or filesystem error that stopped the
    /// removal.
    async fn remove_imported_file(
        &self,
        acquisition_id: Uuid,
        track: &Track,
        managed: &Path,
        media_file_id: Uuid,
        detail: &str,
    ) -> Result<(), AcquireError> {
        let mut tx = self.database().writer().begin().await?;
        let removed =
            hearthsound_library::maintenance::remove_managed_file(&mut tx, media_file_id).await?;
        tx.commit().await?;
        let _ = tokio::fs::remove_file(managed).await;
        let roots = self.storage_roots();
        let root = if managed.starts_with(&roots.cache) {
            roots.cache.as_path()
        } else {
            roots.library.as_path()
        };
        let _ = placement::prune_empty_parents(managed, root).await;
        if let Some((track_id, availability)) = removed
            && let Some(availability) = availability
        {
            self.publish_availability(track_id, availability, None);
        }
        self.log_message(
            acquisition_id,
            hearthsound_core::AcquisitionState::Completed,
            &format!(
                "removed the imported file for track {} after a fingerprint mismatch: {detail}",
                track.id
            ),
        )
        .await?;
        Ok(())
    }
}

/// Converts a media fingerprint into the provider-facing type.
///
/// # Parameters
///
/// - `value`: the compressed fingerprint.
/// - `duration_secs`: the file's duration in whole seconds.
fn fingerprint_of_core(value: &str, duration_secs: u32) -> Fingerprint {
    Fingerprint {
        value: value.to_owned(),
        duration_secs,
    }
}
