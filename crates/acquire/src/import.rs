//! Verifying a downloaded candidate before it is imported.
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
//! lets a failure take the retry edge back to the next candidate. Only a
//! candidate that passes moves the acquisition to `verifying`.

use uuid::Uuid;

use hearthsound_core::{AcquisitionState, FailureReason, QualityTier};
use hearthsound_media::{probe, verify_decodes};
use hearthsound_plugin_host::RegistryInstance;

use crate::download::{ResolvedFile, STAGING_ROOT};
use crate::error::AcquireError;
use crate::orchestrator::AcquisitionOrchestrator;
use crate::select::ScoredCandidate;

/// The window within which a downloaded file's duration must match the wanted
/// track's catalog duration, in milliseconds.
const DURATION_TOLERANCE_MS: i64 = 5_000;

/// What handing a completed candidate to this stage produced.
pub(crate) enum HandoffOutcome {
    /// The candidate passed verification and the acquisition moved on.
    Verified,
    /// The candidate was rejected; the acquisition tries the next one, and
    /// fails with this reason when none is left.
    Rejected {
        /// Why the candidate was rejected, one of the fixed literals.
        reason: FailureReason,
        /// What the check saw, for the ledger's `error_detail`.
        detail: Option<String>,
    },
}

/// A downloaded file that passed verification, with what the probe found.
pub(crate) struct VerifiedFile {
    /// The file the download resolved, with its root and absolute path.
    pub(crate) file: ResolvedFile,
    /// The probe of the file, which the import stage's rows are built from.
    pub(crate) probe: hearthsound_media::ProbeResult,
}

/// Why a candidate was rejected.
struct Rejection {
    /// The failure reason the acquisition uses when nothing is left.
    reason: FailureReason,
    /// What the check saw.
    detail: String,
}

impl AcquisitionOrchestrator {
    /// Verifies a completed candidate's files and moves the acquisition on when
    /// they pass.
    ///
    /// The acquisition is still `downloading` while the files are checked, so a
    /// rejection simply returns to the download stage's candidate loop. A
    /// candidate that passes is recorded in `verifying` and the first verified
    /// path is published on the progress channel.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose files are ready.
    /// - `instance`: the instance that served the download.
    /// - `candidate`: the candidate that was downloaded, whose tier is the
    ///   lossless claim the check verifies.
    /// - `files`: the resolved files, in the order the backend reported them.
    ///
    /// # Returns
    ///
    /// [`HandoffOutcome::Verified`] when every file passed, and
    /// [`HandoffOutcome::Rejected`] with the reason otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when the acquisition has no
    /// row, and the library or ledger error that stopped the stage.
    pub(crate) async fn hand_off_for_verification(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        candidate: &ScoredCandidate,
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

        let verified = match self
            .verify_files(candidate.tier, catalog_duration_ms, files)
            .await
        {
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
        progress.file = verified
            .first()
            .map(|verified| verified.file.absolute_path.clone());
        self.announce(&progress);
        let first = verified.first();
        tracing::info!(
            acquisition = %acquisition_id,
            instance = instance.slug().as_str(),
            files = verified.len(),
            bytes = files.iter().map(|file| file.size).sum::<u64>(),
            codec = first.map(|verified| verified.probe.codec.as_str()).unwrap_or_default(),
            lossless = first.map(|verified| verified.probe.lossless).unwrap_or_default(),
            duration_ms = first.map(|verified| verified.probe.duration_ms).unwrap_or_default(),
            path = first.map(|verified| verified.file.path.as_str()).unwrap_or_default(),
            "the candidate passed verification"
        );
        Ok(HandoffOutcome::Verified)
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
        let transaction = self.database().writer().begin().await;
        let Ok(mut tx) = transaction else {
            return;
        };
        if let Err(error) = crate::repo::write_log(
            &mut tx,
            acquisition_id,
            AcquisitionState::Downloading,
            Some(&message),
        )
        .await
        {
            tracing::debug!(acquisition = %acquisition_id, %error, "a rejection could not be logged");
            return;
        }
        if let Err(error) = tx.commit().await {
            tracing::debug!(acquisition = %acquisition_id, %error, "a rejection could not be logged");
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
    ///
    /// # Errors
    ///
    /// Does not return the caller's error type; the rejection is the value.
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
