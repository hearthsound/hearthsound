//! Monitoring a download to completion, and handing the files on.
//!
//! Once the search has selected candidates, this stage downloads them one after
//! another until a candidate produces a complete, stable file. It holds the
//! instance's download slot for the whole attempt, persists the attempt in the
//! ledger (`instance_id`, `candidate`, `plugin_token`, `progress`, `attempts`)
//! so a restart can tell what was in flight, and publishes
//! `acquisition.updated` with the percentage and the ETA as the bytes advance.
//!
//! Two limits end an attempt early: a candidate that makes no progress for
//! `acquisition.stall_timeout_secs` counts as stalled, and one that sits queued
//! for longer than `acquisition.queue_timeout_secs.interactive` or
//! `.background` is abandoned. Both cancel the download and move to the next
//! candidate, and the acquisition fails only when every candidate is spent. A
//! third wake-up ends the attempt differently: the acquisition being cancelled,
//! here or in another process, cancels the download and abandons the attempt
//! without trying anything else.
//!
//! A backend that reports `complete` may not have flushed the file yet, so the
//! completion rule waits for each reported file's size to hold across two checks
//! a second apart, with a 30 second ceiling. Only then are the resolved files
//! handed to [`AcquisitionOrchestrator::hand_off_for_verification`], which
//! verifies them and, when they pass, moves the acquisition on; a candidate that
//! fails verification is a failed candidate like any other, and the loop tries
//! the next one.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_core::providers::{
    AcquisitionContext, AcquisitionProvider, Candidate, CandidateFile, DownloadState, OutputFile,
    Progress, SourceInfo,
};
use hearthsound_core::{AcquisitionState, FailureReason, ProviderError, QualityTier};
use hearthsound_plugin_host::RegistryInstance;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, OwnedSemaphorePermit};
use tokio::time::Instant;
use uuid::Uuid;

use crate::error::AcquireError;
use crate::import::HandoffOutcome;
use crate::orchestrator::AcquisitionOrchestrator;
use crate::repo::AcquisitionRow;
use crate::select::{AttemptPlan, RequestClass, ScoredCandidate};

/// How long the completion rule waits for a file to stop growing.
const COMPLETION_MAX_WAIT: Duration = Duration::from_secs(30);

/// The root name that addresses an acquisition's own staging directory.
pub(crate) const STAGING_ROOT: &str = "staging";

/// What the download stage did with an acquisition.
pub(crate) enum DownloadOutcome {
    /// A candidate's files were resolved and passed verification, so the
    /// acquisition has moved on past the download stage.
    Ready,
    /// The backend lost the download token, so the acquisition has to be
    /// searched again from the beginning.
    RestartSearch,
    /// The acquisition was failed, or is otherwise finished.
    Stopped,
    /// The acquisition was cancelled, here or in another process, so its
    /// download was cancelled too and the ledger row already says `cancelled`.
    Cancelled,
}

/// One file a completed download produced, resolved to an absolute path.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedFile {
    /// The host root the plugin named, `staging` or an import root id.
    pub(crate) root: String,
    /// The path relative to that root.
    pub(crate) path: String,
    /// The absolute path the two resolve to.
    pub(crate) absolute_path: PathBuf,
    /// The file's size in bytes, as the last check saw it.
    pub(crate) size: u64,
}

/// What one candidate's download attempt produced.
enum CandidateAttempt {
    /// The download completed and reported these files.
    Downloaded {
        /// The plugin's token for the finished download.
        token: String,
        /// The files the backend reported.
        files: Vec<OutputFile>,
    },
    /// A `play` request took this attempt's download slot.
    Preempted,
    /// The acquisition was cancelled, so the attempt was abandoned.
    Cancelled,
    /// The attempt failed; the acquisition may try the next candidate.
    Failed {
        /// Why the attempt failed.
        reason: FailureReason,
        /// The backend's own message, when it gave one.
        detail: Option<String>,
    },
    /// The backend no longer knows the download token.
    LostToken,
}

/// What one candidate's attempt leaves to do.
pub(crate) enum AttemptVerdict {
    /// The download stage is over with this outcome.
    Done(DownloadOutcome),
    /// The candidate was rejected; the caller may try the next one, and fails
    /// the acquisition with the reason when none is left.
    Next(Option<(FailureReason, Option<String>)>),
}

/// How long one candidate's attempt may take and how often it is polled.
#[derive(Debug, Clone, Copy)]
struct AttemptLimits {
    /// How long to wait between two polls.
    poll_interval: Duration,
    /// How long the download may make no progress.
    stall_timeout: Duration,
    /// How long the download may sit queued.
    queue_timeout: Duration,
}

/// What the completion rule decided about a finished download.
enum Stability {
    /// Every file's size held across two checks.
    Stable(Vec<ResolvedFile>),
    /// The ceiling passed with a file still growing, but the backend still
    /// reports the download as complete.
    StillComplete(Vec<ResolvedFile>),
    /// The backend no longer reports the download as complete.
    Failed {
        /// Why the completion was rejected.
        reason: FailureReason,
        /// The backend's own message, when it gave one.
        detail: Option<String>,
    },
}

/// The candidate as it is stored in `acquisitions.candidate`.
///
/// The ledger keeps the candidate as JSON so a restart can tell which files a
/// persisted download token is fetching. The shape mirrors the provider's
/// candidate, which is not itself serializable; `resolved` is added once the
/// backend reports the download complete, and is what lets a process that starts
/// later verify and import the files without asking the backend again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredCandidate {
    /// The provider's token that addresses the candidate.
    pub(crate) token: String,
    /// The candidate's title, as the backend reported it.
    pub(crate) title: String,
    /// The backend's path for the candidate.
    pub(crate) path: String,
    /// The files the candidate offers.
    pub(crate) files: Vec<StoredCandidateFile>,
    /// What the backend knows about the source.
    pub(crate) source: StoredSource,
    /// The files the download produced, once its completion was accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resolved: Option<Vec<StoredResolvedFile>>,
}

/// One file of a [`StoredCandidate`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredCandidateFile {
    /// The file name as the backend reports it.
    pub(crate) name: String,
    /// The file size in bytes, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) size: Option<u64>,
    /// The container format, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) format: Option<String>,
    /// The bitrate in kbps, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bitrate_kbps: Option<u32>,
    /// The sample rate in Hz, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sample_rate: Option<u32>,
    /// The bit depth, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) bit_depth: Option<u8>,
    /// The duration in milliseconds, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u32>,
}

impl StoredCandidateFile {
    /// Stores one of a candidate's files.
    ///
    /// # Parameters
    ///
    /// - `file`: the file the provider reported.
    pub(crate) fn from_candidate_file(file: &CandidateFile) -> StoredCandidateFile {
        StoredCandidateFile {
            name: file.name.clone(),
            size: file.size,
            format: file.format.clone(),
            bitrate_kbps: file.bitrate_kbps,
            sample_rate: file.sample_rate,
            bit_depth: file.bit_depth,
            duration_ms: file.duration_ms,
        }
    }

    /// Returns the provider file this stored form was built from.
    ///
    /// The track index the provider may carry is not stored, so the returned file
    /// has none; nothing in the pipeline reads it after selection.
    pub(crate) fn to_candidate_file(&self) -> CandidateFile {
        CandidateFile {
            name: self.name.clone(),
            size: self.size,
            format: self.format.clone(),
            bitrate_kbps: self.bitrate_kbps,
            sample_rate: self.sample_rate,
            bit_depth: self.bit_depth,
            duration_ms: self.duration_ms,
            track_index: None,
        }
    }
}

/// The source of a [`StoredCandidate`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredSource {
    /// The backend's own identifier for the source.
    pub(crate) key: String,
    /// The source's upload speed in bytes per second, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) speed_bps: Option<u64>,
    /// The source's queue length, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) queue_length: Option<u32>,
    /// Whether the source has a free upload slot, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) free_slot: Option<bool>,
}

impl StoredCandidate {
    /// Converts a provider candidate into its stored form.
    ///
    /// # Parameters
    ///
    /// - `candidate`: the candidate the provider reported.
    pub(crate) fn from_candidate(candidate: &Candidate) -> StoredCandidate {
        StoredCandidate {
            token: candidate.token.clone(),
            title: candidate.title.clone(),
            path: candidate.path.clone(),
            files: candidate
                .files
                .iter()
                .map(StoredCandidateFile::from_candidate_file)
                .collect(),
            source: StoredSource {
                key: candidate.source.key.clone(),
                speed_bps: candidate.source.speed_bps,
                queue_length: candidate.source.queue_length,
                free_slot: candidate.source.free_slot,
            },
            resolved: None,
        }
    }

    /// Returns the provider candidate this stored form was built from.
    ///
    /// The two forms carry the same fields, because the provider's candidate is
    /// what the pipeline reasons about: a resume needs the candidate's tier,
    /// which is derived from its files.
    ///
    /// # Returns
    ///
    /// The candidate, without the track index of a file, which the stored form
    /// does not keep.
    pub(crate) fn to_candidate(&self) -> Candidate {
        Candidate {
            token: self.token.clone(),
            title: self.title.clone(),
            path: self.path.clone(),
            files: self
                .files
                .iter()
                .map(StoredCandidateFile::to_candidate_file)
                .collect(),
            source: SourceInfo {
                key: self.source.key.clone(),
                speed_bps: self.source.speed_bps,
                queue_length: self.source.queue_length,
                free_slot: self.source.free_slot,
            },
        }
    }
}

/// One file a completed download produced, as it is stored in
/// `acquisitions.candidate`.
///
/// The absolute path is stored rather than recomputed, because resolving it needs
/// the instance's import roots and a path the backend may have reported relative
/// to one of them; keeping what the download resolved is what makes a restart
/// able to import the same file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredResolvedFile {
    /// The host root the plugin named, `staging` or an import root id.
    pub(crate) root: String,
    /// The path relative to that root.
    pub(crate) path: String,
    /// The absolute path the two resolve to.
    pub(crate) absolute_path: String,
    /// The file's size in bytes, as the last check saw it.
    pub(crate) size: u64,
}

impl StoredResolvedFile {
    /// Stores a resolved file.
    ///
    /// # Parameters
    ///
    /// - `file`: the file the download resolved.
    pub(crate) fn from_resolved(file: &ResolvedFile) -> StoredResolvedFile {
        StoredResolvedFile {
            root: file.root.clone(),
            path: file.path.clone(),
            absolute_path: file.absolute_path.to_string_lossy().into_owned(),
            size: file.size,
        }
    }

    /// Returns the resolved file this stored form was built from.
    pub(crate) fn to_resolved(&self) -> ResolvedFile {
        ResolvedFile {
            root: self.root.clone(),
            path: self.path.clone(),
            absolute_path: PathBuf::from(&self.absolute_path),
            size: self.size,
        }
    }
}

/// The download progress as it is stored in `acquisitions.progress`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredProgress {
    /// The bytes written so far.
    pub(crate) bytes: u64,
    /// The total bytes expected, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) total_bytes: Option<u64>,
    /// The transfer speed in bytes per second, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) speed_bps: Option<u64>,
}

impl StoredProgress {
    /// Returns the progress of a download as a whole percentage.
    ///
    /// # Returns
    ///
    /// The percentage, or `None` when the total size is unknown or zero.
    pub(crate) fn percent(&self) -> Option<u8> {
        let total = self.total_bytes?;
        if total == 0 {
            return None;
        }
        Some(((self.bytes.saturating_mul(100) / total).min(100)) as u8)
    }

    /// Returns the estimated seconds left.
    ///
    /// # Returns
    ///
    /// The estimate, or `None` when the total size or the speed is unknown.
    pub(crate) fn eta_secs(&self) -> Option<u32> {
        let total = self.total_bytes?;
        let speed = self.speed_bps.filter(|speed| *speed > 0)?;
        let remaining = total.saturating_sub(self.bytes);
        Some((remaining / speed).min(u64::from(u32::MAX)) as u32)
    }
}

/// Parses a stored progress snapshot.
///
/// # Parameters
///
/// - `text`: the JSON text of `acquisitions.progress`, when it has one.
///
/// # Returns
///
/// The snapshot, or `None` when the column is empty or unreadable.
pub(crate) fn stored_progress(text: Option<&str>) -> Option<StoredProgress> {
    serde_json::from_str(text?).ok()
}

/// Parses the candidate a download is working on.
///
/// # Parameters
///
/// - `text`: the JSON text of `acquisitions.candidate`, when it has one.
///
/// # Returns
///
/// The candidate, or `None` when the column is empty or unreadable.
pub(crate) fn stored_candidate(text: Option<&str>) -> Option<StoredCandidate> {
    serde_json::from_str(text?).ok()
}

impl AcquisitionOrchestrator {
    /// Runs the download stage of one acquisition.
    ///
    /// The candidates are tried in the order the search selected them, which
    /// already interleaves the instances the way the request class requires, so
    /// "the next candidate" is also "the next instance" once an instance's
    /// candidates are spent.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition to download, whose row is read again
    ///   here so the attempt count and the state are the ones the ledger holds.
    /// - `instances`: the instances the search walked, in layer order.
    /// - `plan`: the candidates to try, best first.
    ///
    /// # Returns
    ///
    /// [`DownloadOutcome::Ready`] when the files are resolved and passed
    /// verification, [`DownloadOutcome::RestartSearch`] when the backend lost
    /// the token and the acquisition has to be searched again,
    /// [`DownloadOutcome::Stopped`] when the acquisition was failed, and
    /// [`DownloadOutcome::Cancelled`] when it was cancelled.
    ///
    /// # Errors
    ///
    /// Returns the settings, ledger or filesystem error that stopped the stage.
    pub(crate) async fn download(
        &self,
        acquisition_id: Uuid,
        instances: &[Arc<RegistryInstance>],
        plan: &AttemptPlan,
    ) -> Result<DownloadOutcome, AcquireError> {
        let Some(row) = self.repository().get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        let row = &row;
        let max_candidates = self
            .settings()
            .current()
            .get(keys::ACQUISITION_MAX_CANDIDATES)?
            .max(0) as usize;
        let limits = self.attempt_limits(row)?;

        let mut attempts = row.attempts;
        let mut failure: Option<(FailureReason, Option<String>)> = None;
        for attempt in plan.attempts.iter().take(max_candidates) {
            let Some(instance) = instances.get(attempt.instance_index) else {
                continue;
            };
            let Some(provider) = instance.acquisition().cloned() else {
                continue;
            };
            if attempts == row.attempts {
                self.repository()
                    .transition(
                        row.id,
                        AcquisitionState::Downloading,
                        Some(&format!("downloading `{}`", attempt.candidate.path)),
                    )
                    .await?;
            }
            attempts += 1;
            self.repository().set_attempts(row.id, attempts).await?;
            tracing::info!(
                acquisition = %row.id,
                instance = instance.slug().as_str(),
                candidate = attempt.candidate.path.as_str(),
                attempt = attempts,
                "starting a candidate download"
            );

            let mut must_wait = false;
            let outcome = loop {
                match self
                    .run_candidate(row, instance, provider.as_ref(), attempt, limits, must_wait)
                    .await?
                {
                    CandidateAttempt::Preempted => {
                        tracing::info!(
                            acquisition = %row.id,
                            instance = instance.slug().as_str(),
                            "a play request preempted this download"
                        );
                        self.log_message(
                            row.id,
                            AcquisitionState::Downloading,
                            "preempted by an interactive request",
                        )
                        .await?;
                        // The retry waits for a slot instead of taking one, so
                        // the request that preempted this attempt keeps the slot
                        // it asked for.
                        must_wait = true;
                        continue;
                    }
                    other => break other,
                }
            };

            match self
                .settle_attempt(row, instance, provider.as_ref(), attempt.tier, outcome)
                .await?
            {
                AttemptVerdict::Done(outcome) => return Ok(outcome),
                AttemptVerdict::Next(reason) => {
                    if reason.is_some() {
                        failure = reason;
                    }
                }
            }
        }

        let (reason, detail) = failure.unwrap_or((FailureReason::AllCandidatesFailed, None));
        self.fail_acquisition(row.id, reason, detail.as_deref())
            .await?;
        Ok(DownloadOutcome::Stopped)
    }

    /// Resumes a download that was already started.
    ///
    /// The ledger names the instance, the candidate and the token of the attempt
    /// that was in flight when the previous process stopped, so the download is
    /// picked up where it stopped: the same backend token is polled on the same
    /// instance, and the files it reports when it completes are verified and
    /// imported like any other candidate's. A row that cannot be resumed
    /// continues as the ledger allows instead of failing: a download that is
    /// missing its instance, candidate or token, or whose candidate has no
    /// usable tier, goes back to `searching`, and an instance the registry no
    /// longer serves fails the acquisition as `backend-unavailable`.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, in `downloading`.
    ///
    /// # Returns
    ///
    /// The same outcomes [`AcquisitionOrchestrator::download`] returns.
    ///
    /// # Errors
    ///
    /// Returns the settings, ledger or filesystem error that stopped the resume.
    pub(crate) async fn resume_download(
        &self,
        row: &AcquisitionRow,
    ) -> Result<DownloadOutcome, AcquireError> {
        let Some(instance_id) = row.instance_id else {
            return self
                .restart_search(row, "the ledger does not name the instance to resume from")
                .await;
        };
        let Some(stored) = stored_candidate(row.candidate.as_deref()) else {
            return self
                .restart_search(row, "the ledger does not hold the candidate to resume")
                .await;
        };
        let Some(token) = row.plugin_token.as_deref() else {
            return self
                .restart_search(row, "the ledger does not hold the token to resume")
                .await;
        };
        let Some(tier) = stored
            .to_candidate()
            .files
            .first()
            .and_then(crate::profile::quality_tier)
        else {
            return self
                .restart_search(row, "the resumed candidate has no usable quality tier")
                .await;
        };
        let Some(instance) = self.instance_by_id(instance_id) else {
            tracing::warn!(
                acquisition = %row.id,
                instance = %instance_id,
                "the instance that served a download is gone"
            );
            self.fail_acquisition(
                row.id,
                FailureReason::BackendUnavailable,
                Some("the instance that served the download is gone"),
            )
            .await?;
            return Ok(DownloadOutcome::Stopped);
        };
        let Some(provider) = instance.acquisition().cloned() else {
            tracing::warn!(
                acquisition = %row.id,
                instance = instance.slug().as_str(),
                "the instance can no longer download"
            );
            self.fail_acquisition(
                row.id,
                FailureReason::BackendUnavailable,
                Some("the instance that served the download can no longer download"),
            )
            .await?;
            return Ok(DownloadOutcome::Stopped);
        };
        let limits = self.attempt_limits(row)?;
        tracing::info!(
            acquisition = %row.id,
            instance = instance.slug().as_str(),
            "resuming a download"
        );

        let outcome = self
            .run_resumed_candidate(row, &instance, provider.as_ref(), token, limits)
            .await?;
        match self
            .settle_attempt(row, &instance, provider.as_ref(), tier, outcome)
            .await?
        {
            AttemptVerdict::Done(outcome) => Ok(outcome),
            // The plan the previous process selected is gone with it, so a
            // rejected candidate sends the acquisition back to a fresh search
            // instead of to a next candidate that is no longer known.
            AttemptVerdict::Next(_) => {
                self.restart_search(row, "the resumed candidate was rejected")
                    .await
            }
        }
    }

    /// Moves a downloading acquisition back to `searching`.
    ///
    /// The retry edge of the state machine is `downloading -> searching`, which
    /// is what a lost or unusable download uses: the row's instance, candidate,
    /// token and progress describe an attempt that no longer exists, so they are
    /// cleared and the pipeline searches again.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `message`: why the search is restarted, for the log.
    ///
    /// # Returns
    ///
    /// [`DownloadOutcome::RestartSearch`], so a caller can return it directly.
    ///
    /// # Errors
    ///
    /// Returns the ledger error when the transition or a write fails.
    async fn restart_search(
        &self,
        row: &AcquisitionRow,
        message: &str,
    ) -> Result<DownloadOutcome, AcquireError> {
        self.repository()
            .transition(row.id, AcquisitionState::Searching, Some(message))
            .await?;
        self.repository().set_progress(row.id, None).await?;
        self.repository()
            .set_download(row.id, None, None, None)
            .await?;
        tracing::info!(acquisition = %row.id, reason = message, "restarting a download as a search");
        Ok(DownloadOutcome::RestartSearch)
    }

    /// Acts on one finished candidate attempt.
    ///
    /// The download of the candidate is over, so the acquisition either moves
    /// past the download stage, has to search again, or may try the next
    /// candidate. The files a completed download resolved are written to the
    /// ledger before verification runs, which is what lets a process that stops
    /// in the middle of verification or import finish the job later.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance that served the attempt.
    /// - `provider`: the instance's acquisition provider.
    /// - `tier`: the quality tier the candidate claimed.
    /// - `outcome`: what the attempt produced.
    ///
    /// # Returns
    ///
    /// [`AttemptVerdict::Done`] when the download stage is over, and
    /// [`AttemptVerdict::Next`] when the caller should try the next candidate.
    ///
    /// # Errors
    ///
    /// Returns the ledger or filesystem error that stopped the stage.
    async fn settle_attempt(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        provider: &dyn AcquisitionProvider,
        tier: QualityTier,
        outcome: CandidateAttempt,
    ) -> Result<AttemptVerdict, AcquireError> {
        let context = AcquisitionContext::new(self.staging_dir(row.id));
        match outcome {
            CandidateAttempt::Downloaded { token, files } => {
                match self
                    .stabilize_download(instance, &context, provider, &token, &files)
                    .await?
                {
                    Stability::Stable(resolved) | Stability::StillComplete(resolved) => {
                        self.record_resolved(row.id, instance, &token, &resolved)
                            .await?;
                        match self
                            .hand_off_for_verification(row.id, instance, tier, &resolved)
                            .await?
                        {
                            HandoffOutcome::Completed => {
                                Ok(AttemptVerdict::Done(DownloadOutcome::Ready))
                            }
                            // The import failed the acquisition, so there is no
                            // candidate left to try and nothing more to do.
                            HandoffOutcome::ImportFailed => {
                                Ok(AttemptVerdict::Done(DownloadOutcome::Stopped))
                            }
                            HandoffOutcome::Rejected { reason, detail } => {
                                Ok(AttemptVerdict::Next(Some((reason, detail))))
                            }
                        }
                    }
                    Stability::Failed { reason, detail } => {
                        Ok(AttemptVerdict::Next(Some((reason, detail))))
                    }
                }
            }
            CandidateAttempt::Failed { reason, detail } => {
                Ok(AttemptVerdict::Next(Some((reason, detail))))
            }
            CandidateAttempt::Cancelled => {
                // The acquisition was cancelled while this candidate was
                // downloading; the row already says so.
                Ok(AttemptVerdict::Done(DownloadOutcome::Cancelled))
            }
            CandidateAttempt::LostToken => {
                let outcome = self
                    .restart_search(row, "the backend lost the download token; searching again")
                    .await?;
                Ok(AttemptVerdict::Done(outcome))
            }
            CandidateAttempt::Preempted => unreachable!("the preemption loop retries"),
        }
    }

    /// Reads the limits one download attempt obeys.
    ///
    /// The poll interval and the queue timeout follow the request's class: a user
    /// is waiting for a `play` or `next` acquisition, so it is polled often and
    /// abandoned sooner.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    ///
    /// # Returns
    ///
    /// The limits the attempt's stages use.
    ///
    /// # Errors
    ///
    /// Returns the settings error when a key cannot be read.
    fn attempt_limits(&self, row: &AcquisitionRow) -> Result<AttemptLimits, AcquireError> {
        let snapshot = self.settings().current();
        let interactive = RequestClass::from_priority(row.priority).is_interactive();
        Ok(AttemptLimits {
            poll_interval: Duration::from_millis(
                if interactive {
                    snapshot.get(keys::ACQUISITION_POLL_INTERVAL_MS_INTERACTIVE)?
                } else {
                    snapshot.get(keys::ACQUISITION_POLL_INTERVAL_MS_BACKGROUND)?
                }
                .max(0) as u64,
            ),
            stall_timeout: Duration::from_secs(
                snapshot.get(keys::ACQUISITION_STALL_TIMEOUT_SECS)?.max(0) as u64,
            ),
            queue_timeout: Duration::from_secs(
                if interactive {
                    snapshot.get(keys::ACQUISITION_QUEUE_TIMEOUT_SECS_INTERACTIVE)?
                } else {
                    snapshot.get(keys::ACQUISITION_QUEUE_TIMEOUT_SECS_BACKGROUND)?
                }
                .max(0) as u64,
            ),
        })
    }

    /// Downloads one candidate, holding an instance download slot for the whole
    /// attempt.
    ///
    /// The slot is taken without waiting when one is free. When none is, an
    /// interactive request preempts the instance's least urgent bulk download
    /// and then waits for the slot, which the preempted download releases as
    /// soon as it is woken; the waiters are served in order, so the interactive
    /// request takes the slot first.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance the candidate came from.
    /// - `provider`: the instance's acquisition provider.
    /// - `attempt`: the candidate to download.
    /// - `limits`: how long the attempt may take and how often it is polled.
    /// - `must_wait`: whether the slot has to be waited for, which is set for the
    ///   retry that follows a preemption.
    ///
    /// # Errors
    ///
    /// Returns the ledger or filesystem error that stopped the attempt.
    async fn run_candidate(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        provider: &dyn AcquisitionProvider,
        attempt: &ScoredCandidate,
        limits: AttemptLimits,
        must_wait: bool,
    ) -> Result<CandidateAttempt, AcquireError> {
        let Some(permit) = self.take_download_slot(row, instance, must_wait).await else {
            return Ok(CandidateAttempt::Failed {
                reason: FailureReason::BackendUnavailable,
                detail: Some("the download budget is closed".to_owned()),
            });
        };
        let preempted = self
            .download_budgets()
            .register(row.id, instance.id(), row.priority);
        let outcome = self
            .poll_candidate(row, instance, provider, attempt, limits, &preempted)
            .await;
        self.download_budgets().unregister(row.id);
        drop(permit);
        outcome
    }

    /// Polls a download that was already started, holding an instance slot.
    ///
    /// This is [`AcquisitionOrchestrator::run_candidate`] without the start: the
    /// attempt's download is already in flight at the backend, so it takes a
    /// download slot, registers for preemption like any other attempt, and polls
    /// the token the ledger holds.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance serving the download.
    /// - `provider`: the instance's acquisition provider.
    /// - `token`: the download token the previous process received.
    /// - `limits`: how long the attempt may take and how often it is polled.
    ///
    /// # Errors
    ///
    /// Returns the ledger or filesystem error that stopped the attempt.
    async fn run_resumed_candidate(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        provider: &dyn AcquisitionProvider,
        token: &str,
        limits: AttemptLimits,
    ) -> Result<CandidateAttempt, AcquireError> {
        let Some(permit) = self.take_download_slot(row, instance, true).await else {
            return Ok(CandidateAttempt::Failed {
                reason: FailureReason::BackendUnavailable,
                detail: Some("the download budget is closed".to_owned()),
            });
        };
        let preempted = self
            .download_budgets()
            .register(row.id, instance.id(), row.priority);
        let outcome = self
            .watch_download(row, instance, provider, token, limits, &preempted)
            .await;
        self.download_budgets().unregister(row.id);
        drop(permit);
        outcome
    }

    /// Takes one of an instance's download slots for an attempt.
    ///
    /// The slot is taken without waiting when one is free. When none is, an
    /// interactive request preempts the instance's least urgent bulk download and
    /// then waits for the slot, which the preempted download releases as soon as
    /// it is woken; the waiters are served in order, so the interactive request
    /// takes the slot first.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance the attempt runs on.
    /// - `must_wait`: whether the slot has to be waited for, which is set for the
    ///   retry that follows a preemption.
    ///
    /// # Returns
    ///
    /// The permit, which the caller must hold for the whole attempt, or `None`
    /// when the instance's budget is closed.
    async fn take_download_slot(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        must_wait: bool,
    ) -> Option<OwnedSemaphorePermit> {
        let slots = self.download_budgets().slots(instance);
        if !must_wait && let Ok(permit) = Arc::clone(&slots).try_acquire_owned() {
            return Some(permit);
        }
        if RequestClass::from_priority(row.priority).is_interactive()
            && let Some(victim) = self.download_budgets().preempt(instance.id(), row.priority)
        {
            tracing::info!(
                acquisition = %victim,
                instance = instance.slug().as_str(),
                requester = %row.id,
                "preempted a bulk download for an interactive request"
            );
        }
        Arc::clone(&slots).acquire_owned().await.ok()
    }

    /// Starts a candidate's download and polls it until it ends.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance serving the download.
    /// - `provider`: the instance's acquisition provider.
    /// - `attempt`: the candidate to download.
    /// - `limits`: how long the attempt may take and how often it is polled.
    /// - `preempted`: the notifier a `play` request wakes to take the slot.
    ///
    /// # Errors
    ///
    /// Returns the ledger or filesystem error that stopped the attempt.
    async fn poll_candidate(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        provider: &dyn AcquisitionProvider,
        attempt: &ScoredCandidate,
        limits: AttemptLimits,
        preempted: &Notify,
    ) -> Result<CandidateAttempt, AcquireError> {
        let context = AcquisitionContext::new(self.staging_dir(row.id));
        let files: Vec<u32> = (0..attempt.candidate.files.len() as u32).collect();
        let token = match provider
            .start_download(&context, &attempt.candidate.token, &files)
            .await
        {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    %error,
                    "a download could not be started"
                );
                return Ok(CandidateAttempt::Failed {
                    reason: FailureReason::BackendUnavailable,
                    detail: Some(error.to_string()),
                });
            }
        };
        self.record_download(row.id, instance, &attempt.candidate, &token)
            .await?;
        self.watch_download(row, instance, provider, &token, limits, preempted)
            .await
    }

    /// Polls a running download until it ends.
    ///
    /// The token is polled every `limits.poll_interval` until the backend
    /// reports the transfer complete, a failure, or a state the limits reject. A
    /// `play` request that needs the download's slot wakes the task through
    /// `preempted` and the attempt is abandoned so the slot is free at once; a
    /// cancellation - from [`AcquisitionOrchestrator::cancel`] or from
    /// [`AcquisitionOrchestrator::reconcile`] noticing another process's write -
    /// wakes it the same way and abandons the attempt as cancelled.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance serving the download.
    /// - `provider`: the instance's acquisition provider.
    /// - `token`: the download token the backend issued.
    /// - `limits`: how long the attempt may take and how often it is polled.
    /// - `preempted`: the notifier a `play` request wakes to take the slot.
    ///
    /// # Errors
    ///
    /// Returns the ledger error when a progress snapshot cannot be written.
    async fn watch_download(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        provider: &dyn AcquisitionProvider,
        token: &str,
        limits: AttemptLimits,
        preempted: &Notify,
    ) -> Result<CandidateAttempt, AcquireError> {
        let context = AcquisitionContext::new(self.staging_dir(row.id));
        let stop = self.stop_signal(row.id);
        let mut last_bytes = 0_u64;
        let mut last_progress = Instant::now();
        let mut queued_since: Option<Instant> = None;
        loop {
            tokio::select! {
                () = tokio::time::sleep(limits.poll_interval) => {}
                () = preempted.notified() => {
                    provider.cancel_download(&context, token).await;
                    return Ok(CandidateAttempt::Preempted);
                }
                () = stop.notified() => {
                    provider.cancel_download(&context, token).await;
                    tracing::info!(
                        acquisition = %row.id,
                        instance = instance.slug().as_str(),
                        "abandoned the download of a cancelled acquisition"
                    );
                    return Ok(CandidateAttempt::Cancelled);
                }
            }

            let state = match provider.poll_download(&context, token).await {
                Ok(state) => state,
                Err(ProviderError::NotFound) => {
                    tracing::warn!(
                        instance = instance.slug().as_str(),
                        acquisition = %row.id,
                        "the backend lost the download token"
                    );
                    return Ok(CandidateAttempt::LostToken);
                }
                Err(error) => {
                    tracing::warn!(
                        instance = instance.slug().as_str(),
                        %error,
                        "polling a download failed"
                    );
                    provider.cancel_download(&context, token).await;
                    return Ok(CandidateAttempt::Failed {
                        reason: FailureReason::BackendUnavailable,
                        detail: Some(error.to_string()),
                    });
                }
            };

            match state {
                DownloadState::Queued(position) => {
                    let since = *queued_since.get_or_insert_with(Instant::now);
                    self.announce_download(row, instance, Some(0), None);
                    if since.elapsed() >= limits.queue_timeout {
                        tracing::warn!(
                            acquisition = %row.id,
                            position = ?position,
                            "a download sat queued for too long"
                        );
                        provider.cancel_download(&context, token).await;
                        return Ok(CandidateAttempt::Failed {
                            reason: FailureReason::Timeout,
                            detail: Some("the transfer stayed queued".to_owned()),
                        });
                    }
                }
                DownloadState::Running(progress) => {
                    queued_since = None;
                    if progress.bytes > last_bytes {
                        last_bytes = progress.bytes;
                        last_progress = Instant::now();
                    }
                    self.record_progress(row.id, &progress).await?;
                    let stored = StoredProgress {
                        bytes: progress.bytes,
                        total_bytes: progress.total_bytes,
                        speed_bps: progress.speed_bps,
                    };
                    self.announce_download(row, instance, stored.percent(), stored.eta_secs());
                    if last_progress.elapsed() >= limits.stall_timeout {
                        tracing::warn!(
                            acquisition = %row.id,
                            bytes = progress.bytes,
                            "a download stopped making progress"
                        );
                        provider.cancel_download(&context, token).await;
                        return Ok(CandidateAttempt::Failed {
                            reason: FailureReason::Timeout,
                            detail: Some("the transfer stalled".to_owned()),
                        });
                    }
                }
                DownloadState::Complete(files) => {
                    return Ok(CandidateAttempt::Downloaded {
                        token: token.to_owned(),
                        files,
                    });
                }
                DownloadState::Failed(detail) => {
                    tracing::warn!(
                        acquisition = %row.id,
                        instance = instance.slug().as_str(),
                        detail = detail.as_str(),
                        "the backend reported a failed download"
                    );
                    return Ok(CandidateAttempt::Failed {
                        reason: FailureReason::AllCandidatesFailed,
                        detail: Some(detail),
                    });
                }
            }
        }
    }

    /// Waits for a completed download's files to stop growing.
    ///
    /// Each reported file is resolved to an absolute path and checked until its
    /// size holds across two checks a second apart. When the 30 second ceiling
    /// passes with a file still growing, the backend is polled once more: if it
    /// still reports the download as complete the files are handed on anyway,
    /// because verification is what rejects a truncated file.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that served the download, for its import
    ///   roots.
    /// - `context`: the job context.
    /// - `provider`: the instance's acquisition provider.
    /// - `token`: the finished download's token.
    /// - `files`: the files the backend reported.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownRoot`] or
    /// [`AcquireError::UnsafeOutputPath`] when a reported path cannot be
    /// resolved, and the filesystem error of a failed stat.
    async fn stabilize_download(
        &self,
        instance: &RegistryInstance,
        context: &AcquisitionContext,
        provider: &dyn AcquisitionProvider,
        token: &str,
        files: &[OutputFile],
    ) -> Result<Stability, AcquireError> {
        let mut resolved = Vec::with_capacity(files.len());
        let mut all_stable = true;
        for file in files {
            let absolute = resolve_output(instance, context, file)?;
            let (size, stable) = self.wait_for_stable_size(&absolute).await?;
            all_stable &= stable;
            resolved.push(ResolvedFile {
                root: file.root.clone(),
                path: file.path.clone(),
                absolute_path: absolute,
                size,
            });
        }
        if all_stable {
            return Ok(Stability::Stable(resolved));
        }
        match provider.poll_download(context, token).await {
            Ok(DownloadState::Complete(_)) => {
                tracing::warn!("a file kept growing past the completion window");
                Ok(Stability::StillComplete(resolved))
            }
            Ok(DownloadState::Failed(detail)) => Ok(Stability::Failed {
                reason: FailureReason::AllCandidatesFailed,
                detail: Some(detail),
            }),
            Ok(DownloadState::Queued(_) | DownloadState::Running(_)) => Ok(Stability::Failed {
                reason: FailureReason::Timeout,
                detail: Some("the transfer did not finish within the completion window".to_owned()),
            }),
            Err(error) => Ok(Stability::Failed {
                reason: FailureReason::BackendUnavailable,
                detail: Some(error.to_string()),
            }),
        }
    }

    /// Waits until a file's size stops changing.
    ///
    /// The size is checked every `acquisition.completion_check_interval_ms`. The
    /// import stage runs the same check after a resume, which is why it is
    /// shared.
    ///
    /// # Parameters
    ///
    /// - `path`: the absolute path of the file.
    ///
    /// # Returns
    ///
    /// The last size seen and whether it held across two checks.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Io`] when the file cannot be stat-ed, which
    /// includes a file the backend reported but never wrote, and the settings
    /// error when the check interval cannot be read.
    pub(crate) async fn wait_for_stable_size(
        &self,
        path: &Path,
    ) -> Result<(u64, bool), AcquireError> {
        let interval = Duration::from_millis(
            self.settings()
                .current()
                .get(keys::ACQUISITION_COMPLETION_CHECK_INTERVAL_MS)?
                .max(0) as u64,
        );
        let mut previous = file_size(path).await?;
        let deadline = Instant::now() + COMPLETION_MAX_WAIT;
        loop {
            tokio::time::sleep(interval).await;
            let current = file_size(path).await?;
            if current == previous {
                return Ok((current, true));
            }
            previous = current;
            if Instant::now() >= deadline {
                return Ok((current, false));
            }
        }
    }

    /// Appends a message to an acquisition's log without changing its state.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition the message belongs to.
    /// - `state`: the state the acquisition is in.
    /// - `message`: the message to append.
    ///
    /// # Errors
    ///
    /// Returns the database error when the entry cannot be written.
    async fn log_message(
        &self,
        id: Uuid,
        state: AcquisitionState,
        message: &str,
    ) -> Result<(), AcquireError> {
        let mut tx = self.database().writer().begin().await?;
        crate::repo::write_log(&mut tx, id, state, Some(message)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records the instance, candidate and token of a download that started.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition being downloaded.
    /// - `instance`: the instance serving the download.
    /// - `candidate`: the candidate being downloaded.
    /// - `token`: the plugin's token for the download.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Json`] when the candidate cannot be serialized
    /// and the ledger error when the row cannot be written.
    async fn record_download(
        &self,
        id: Uuid,
        instance: &RegistryInstance,
        candidate: &Candidate,
        token: &str,
    ) -> Result<(), AcquireError> {
        let stored = serde_json::to_string(&StoredCandidate::from_candidate(candidate))?;
        self.repository()
            .set_download(id, Some(instance.id()), Some(&stored), Some(token))
            .await?;
        Ok(())
    }

    /// Records a download's latest progress snapshot.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition being downloaded.
    /// - `progress`: the progress the backend reported.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Json`] when the progress cannot be serialized and
    /// the ledger error when the row cannot be written.
    async fn record_progress(&self, id: Uuid, progress: &Progress) -> Result<(), AcquireError> {
        let stored = serde_json::to_string(&StoredProgress {
            bytes: progress.bytes,
            total_bytes: progress.total_bytes,
            speed_bps: progress.speed_bps,
        })?;
        self.repository().set_progress(id, Some(&stored)).await?;
        Ok(())
    }

    /// Records the files a completed download resolved.
    ///
    /// The candidate column already says which candidate the token fetched; this
    /// adds the absolute paths the backend reported, so a process that starts
    /// after a crash can verify and import the same files without asking the
    /// backend again. The instance and the token are written with them, because
    /// they are what a resumed attempt polls.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition being downloaded.
    /// - `instance`: the instance that served the download.
    /// - `token`: the download token the backend issued.
    /// - `resolved`: the files the completion rule accepted.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Json`] when the candidate cannot be serialized and
    /// the ledger error when the row cannot be written.
    async fn record_resolved(
        &self,
        id: Uuid,
        instance: &RegistryInstance,
        token: &str,
        resolved: &[ResolvedFile],
    ) -> Result<(), AcquireError> {
        let Some(mut stored) = self
            .repository()
            .get(id)
            .await?
            .and_then(|row| stored_candidate(row.candidate.as_deref()))
        else {
            // A candidate the ledger cannot parse is one a resume will search
            // again; recording its files would not make it resumable.
            return Ok(());
        };
        stored.resolved = Some(
            resolved
                .iter()
                .map(StoredResolvedFile::from_resolved)
                .collect(),
        );
        let text = serde_json::to_string(&stored)?;
        self.repository()
            .set_download(id, Some(instance.id()), Some(&text), Some(token))
            .await?;
        Ok(())
    }

    /// Publishes a download progress snapshot.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `instance`: the instance serving the download.
    /// - `percent`: the progress as a whole percentage, when known.
    /// - `eta_secs`: the estimated seconds left, when known.
    fn announce_download(
        &self,
        row: &AcquisitionRow,
        instance: &RegistryInstance,
        percent: Option<u8>,
        eta_secs: Option<u32>,
    ) {
        let mut progress = self.progress_of_row(row, row.priority);
        progress.state = AcquisitionState::Downloading;
        progress.instance = Some(instance.slug().as_str().to_owned());
        progress.percent = percent;
        progress.eta_secs = eta_secs;
        self.announce(&progress);
    }
}

/// Returns the size of a file.
///
/// # Parameters
///
/// - `path`: the absolute path of the file.
///
/// # Errors
///
/// Returns [`AcquireError::Io`] when the file cannot be stat-ed.
async fn file_size(path: &Path) -> Result<u64, AcquireError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|source| AcquireError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(metadata.len())
}

/// Resolves one reported output file to an absolute path.
///
/// The root name the plugin used is either `staging`, which addresses the
/// acquisition's own staging directory, or the id of an import root the
/// administrator granted the instance.
///
/// # Parameters
///
/// - `instance`: the instance that served the download.
/// - `context`: the job context, which names the staging root.
/// - `file`: the file the backend reported.
///
/// # Errors
///
/// Returns [`AcquireError::UnknownRoot`] when the file names a root the instance
/// does not grant, and [`AcquireError::UnsafeOutputPath`] when the path leaves
/// that root.
fn resolve_output(
    instance: &RegistryInstance,
    context: &AcquisitionContext,
    file: &OutputFile,
) -> Result<PathBuf, AcquireError> {
    if file.root == STAGING_ROOT {
        return safe_join(context.staging_root(), file);
    }
    let Some(root) = instance.host_config().import_root(&file.root) else {
        return Err(AcquireError::UnknownRoot {
            instance: instance.slug().as_str().to_owned(),
            root: file.root.clone(),
        });
    };
    safe_join(&root.path, file)
}

/// Joins a reported relative path onto the root it belongs to.
///
/// The path a plugin reports is data, not a host import, so it is checked here:
/// an absolute path, a drive or root component, or any `..` component is
/// refused, which keeps a download from naming a file outside the root it was
/// granted.
///
/// # Parameters
///
/// - `root`: the absolute directory of the root.
/// - `file`: the file the backend reported.
///
/// # Errors
///
/// Returns [`AcquireError::UnsafeOutputPath`] when the path leaves the root.
fn safe_join(root: &Path, file: &OutputFile) -> Result<PathBuf, AcquireError> {
    let reported = Path::new(&file.path);
    let unsafe_path = reported.is_absolute()
        || reported.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
    if unsafe_path {
        return Err(AcquireError::UnsafeOutputPath {
            root: file.root.clone(),
            path: file.path.clone(),
        });
    }
    Ok(root.join(reported))
}
