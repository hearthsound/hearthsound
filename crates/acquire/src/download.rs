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
//! candidate, and the acquisition fails only when every candidate is spent.
//!
//! A backend that reports `complete` may not have flushed the file yet, so the
//! completion rule waits for each reported file's size to hold across two checks
//! a second apart, with a 30 second ceiling. Only then does the acquisition move
//! to `verifying` and the resolved files go to
//! [`AcquisitionOrchestrator::hand_off_for_verification`], which is where the
//! verification and import stages are implemented.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_core::providers::{
    AcquisitionContext, AcquisitionProvider, Candidate, DownloadState, OutputFile, Progress,
};
use hearthsound_core::{AcquisitionState, FailureReason, ProviderError};
use hearthsound_plugin_host::RegistryInstance;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::time::Instant;
use uuid::Uuid;

use crate::error::AcquireError;
use crate::orchestrator::AcquisitionOrchestrator;
use crate::repo::AcquisitionRow;
use crate::select::{AttemptPlan, RequestClass, ScoredCandidate};

/// How long an interactive download waits between two polls.
const INTERACTIVE_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a bulk download waits between two polls.
const BACKGROUND_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// How long the completion rule waits between two size checks.
const COMPLETION_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// How long the completion rule waits for a file to stop growing.
const COMPLETION_MAX_WAIT: Duration = Duration::from_secs(30);

/// The root name that addresses an acquisition's own staging directory.
pub(crate) const STAGING_ROOT: &str = "staging";

/// What the download stage did with an acquisition.
pub(crate) enum DownloadOutcome {
    /// The files are resolved and the acquisition is in `verifying`.
    Ready,
    /// The backend lost the download token, so the acquisition has to be
    /// searched again from the beginning.
    RestartSearch,
    /// The acquisition was failed, or is otherwise finished.
    Stopped,
}

/// One file a completed download produced, resolved to an absolute path.
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
/// candidate, which is not itself serializable.
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
                .map(|file| StoredCandidateFile {
                    name: file.name.clone(),
                    size: file.size,
                    format: file.format.clone(),
                    bitrate_kbps: file.bitrate_kbps,
                    sample_rate: file.sample_rate,
                    bit_depth: file.bit_depth,
                    duration_ms: file.duration_ms,
                })
                .collect(),
            source: StoredSource {
                key: candidate.source.key.clone(),
                speed_bps: candidate.source.speed_bps,
                queue_length: candidate.source.queue_length,
                free_slot: candidate.source.free_slot,
            },
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
    /// [`DownloadOutcome::Ready`] when the files are resolved and the
    /// acquisition is `verifying`, [`DownloadOutcome::RestartSearch`] when the
    /// backend lost the token and the acquisition has to be searched again, and
    /// [`DownloadOutcome::Stopped`] when the acquisition was failed.
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
        let snapshot = self.settings().current();
        let class = RequestClass::from_priority(row.priority);
        let poll_interval = if class.is_interactive() {
            INTERACTIVE_POLL_INTERVAL
        } else {
            BACKGROUND_POLL_INTERVAL
        };
        let stall_timeout =
            Duration::from_secs(snapshot.get(keys::ACQUISITION_STALL_TIMEOUT_SECS)?.max(0) as u64);
        let queue_timeout = Duration::from_secs(
            if class.is_interactive() {
                snapshot.get(keys::ACQUISITION_QUEUE_TIMEOUT_SECS_INTERACTIVE)?
            } else {
                snapshot.get(keys::ACQUISITION_QUEUE_TIMEOUT_SECS_BACKGROUND)?
            }
            .max(0) as u64,
        );
        let max_candidates = snapshot.get(keys::ACQUISITION_MAX_CANDIDATES)?.max(0) as usize;
        let limits = AttemptLimits {
            poll_interval,
            stall_timeout,
            queue_timeout,
        };

        let mut attempts = row.attempts;
        let mut failure: Option<(FailureReason, Option<String>)> = None;
        for attempt in plan.attempts.iter().take(max_candidates) {
            let Some(instance) = instances.get(attempt.instance_index) else {
                continue;
            };
            let Some(provider) = instance.acquisition().cloned() else {
                continue;
            };
            let context = AcquisitionContext::new(self.staging_dir(row.id));
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

            match outcome {
                CandidateAttempt::Downloaded { token, files } => {
                    match self
                        .stabilize_download(instance, &context, provider.as_ref(), &token, &files)
                        .await?
                    {
                        Stability::Stable(resolved) | Stability::StillComplete(resolved) => {
                            self.repository()
                                .transition(
                                    row.id,
                                    AcquisitionState::Verifying,
                                    Some("the download completed with stable files"),
                                )
                                .await?;
                            self.hand_off_for_verification(row.id, instance, &resolved)
                                .await?;
                            return Ok(DownloadOutcome::Ready);
                        }
                        Stability::Failed { reason, detail } => {
                            failure = Some((reason, detail));
                        }
                    }
                }
                CandidateAttempt::Failed { reason, detail } => {
                    failure = Some((reason, detail));
                }
                CandidateAttempt::LostToken => {
                    self.repository()
                        .transition(
                            row.id,
                            AcquisitionState::Searching,
                            Some("the backend lost the download token; searching again"),
                        )
                        .await?;
                    self.repository().set_progress(row.id, None).await?;
                    return Ok(DownloadOutcome::RestartSearch);
                }
                CandidateAttempt::Preempted => unreachable!("the preemption loop retries"),
            }
        }

        let (reason, detail) = failure.unwrap_or((FailureReason::AllCandidatesFailed, None));
        self.fail_acquisition(row.id, reason, detail.as_deref())
            .await?;
        Ok(DownloadOutcome::Stopped)
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
        let slots = self.download_budgets().slots(instance);
        let available = if must_wait {
            None
        } else {
            Arc::clone(&slots).try_acquire_owned().ok()
        };
        let permit = match available {
            Some(permit) => permit,
            None => {
                if RequestClass::from_priority(row.priority).is_interactive()
                    && let Some(victim) =
                        self.download_budgets().preempt(instance.id(), row.priority)
                {
                    tracing::info!(
                        acquisition = %victim,
                        instance = instance.slug().as_str(),
                        requester = %row.id,
                        "preempted a bulk download for an interactive request"
                    );
                }
                match Arc::clone(&slots).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => {
                        return Ok(CandidateAttempt::Failed {
                            reason: FailureReason::BackendUnavailable,
                            detail: Some("the download budget is closed".to_owned()),
                        });
                    }
                }
            }
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

        let mut last_bytes = 0_u64;
        let mut last_progress = Instant::now();
        let mut queued_since: Option<Instant> = None;
        loop {
            tokio::select! {
                () = tokio::time::sleep(limits.poll_interval) => {}
                () = preempted.notified() => {
                    provider.cancel_download(&context, &token).await;
                    return Ok(CandidateAttempt::Preempted);
                }
            }

            let state = match provider.poll_download(&context, &token).await {
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
                    provider.cancel_download(&context, &token).await;
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
                        provider.cancel_download(&context, &token).await;
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
                        provider.cancel_download(&context, &token).await;
                        return Ok(CandidateAttempt::Failed {
                            reason: FailureReason::Timeout,
                            detail: Some("the transfer stalled".to_owned()),
                        });
                    }
                }
                DownloadState::Complete(files) => {
                    return Ok(CandidateAttempt::Downloaded { token, files });
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
    /// includes a file the backend reported but never wrote.
    async fn wait_for_stable_size(&self, path: &Path) -> Result<(u64, bool), AcquireError> {
        let mut previous = file_size(path).await?;
        let deadline = Instant::now() + COMPLETION_MAX_WAIT;
        loop {
            tokio::time::sleep(COMPLETION_CHECK_INTERVAL).await;
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

    /// Hands a completed download's files to the verification and import stages.
    ///
    /// This is the entry point of the rest of the pipeline: the verification
    /// stage probes the files, checks their duration against the wanted track
    /// and optionally fingerprints them, and the import stage writes tags, names
    /// the files by the library template, moves them into managed storage and
    /// records the media file. It is handed files whose sizes are already
    /// stable, each with the absolute path it resolves to.
    ///
    /// Until those stages are implemented here, this function publishes the
    /// resolved file on the acquisition's progress - which is what a client
    /// waiting for audio reads - and leaves the acquisition in `verifying`,
    /// where the ledger records that the download completed.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose files are ready.
    /// - `instance`: the instance that served the download.
    /// - `files`: the resolved files, in the order the backend reported them.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when the acquisition has no
    /// row, and the ledger error when the progress cannot be published.
    pub(crate) async fn hand_off_for_verification(
        &self,
        acquisition_id: Uuid,
        instance: &RegistryInstance,
        files: &[ResolvedFile],
    ) -> Result<(), AcquireError> {
        let Some(row) = self.repository().get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        let mut progress = self.progress_of_row(&row, row.priority);
        progress.state = AcquisitionState::Verifying;
        progress.instance = Some(instance.slug().as_str().to_owned());
        progress.percent = None;
        progress.eta_secs = None;
        progress.file = files.first().map(|file| file.absolute_path.clone());
        self.announce(&progress);
        let first = files.first();
        tracing::info!(
            acquisition = %acquisition_id,
            instance = instance.slug().as_str(),
            files = files.len(),
            bytes = files.iter().map(|file| file.size).sum::<u64>(),
            root = first.map(|file| file.root.as_str()).unwrap_or_default(),
            path = first.map(|file| file.path.as_str()).unwrap_or_default(),
            absolute = first
                .map(|file| file.absolute_path.display().to_string())
                .unwrap_or_default(),
            "the download completed and the files are resolved"
        );
        Ok(())
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
