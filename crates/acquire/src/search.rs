//! Searching the acquisition instances within their slot and rate budgets.
//!
//! The search stage turns a `searching` acquisition into the ordered list of
//! candidates its download stage should try. It walks the acquisition layer in
//! the order the administrator assigned, skips an instance that does not
//! advertise the `track` granularity, waits for that instance's search slot and,
//! for bulk work, for its hourly token, and then polls the search until it
//! completes, the plugin call deadline passes, or an interactive request already
//! has a candidate it can use.
//!
//! Candidates are scored and ordered here, in the core: a plugin reports raw
//! candidates and never ranks them. When nothing can be tried the acquisition
//! fails through
//! [`AcquisitionOrchestrator::fail_acquisition`](crate::orchestrator::AcquisitionOrchestrator::fail_acquisition),
//! which puts the track into cooldown, so a track nobody can find does not make
//! every client retry hit the backends again.

use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_core::providers::{
    AcquisitionContext, AcquisitionProvider, AcquisitionQuery, Candidate, ExternalId, SearchState,
    WantedTrack,
};
use hearthsound_core::{
    AcquisitionState, FailureReason, Granularity, Layer, ProviderError, QualityTier,
};
use hearthsound_library::Track;
use hearthsound_plugin_host::RegistryInstance;
use tokio::time::Instant;

use crate::error::AcquireError;
use crate::orchestrator::AcquisitionOrchestrator;
use crate::profile::QualityProfile;
use crate::repo::{self, AcquisitionRow};
use crate::score::MatchTarget;
use crate::select::{
    AttemptPlan, CandidateSlot, RequestClass, SelectionRequest, select_candidates,
};

/// How long the search stage waits between two polls of a running search.
const SEARCH_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// What one search produced.
pub(crate) enum SearchOutcome {
    /// Candidates to try, best first, and the instances that were searched in
    /// layer order, so an attempt's `instance_index` addresses
    /// [`SearchOutcome::Planned::instances`].
    Planned {
        /// The instances that were searched, in layer order.
        instances: Vec<Arc<RegistryInstance>>,
        /// The candidates to try, best first.
        plan: AttemptPlan,
    },
    /// Nothing could be tried and the acquisition was failed.
    Failed(FailureReason),
}

impl AcquisitionOrchestrator {
    /// Runs the search stage of one acquisition.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, in `searching`.
    ///
    /// # Returns
    ///
    /// The plan to download, or the reason the acquisition was failed. A
    /// failure is recorded before it is returned: the row is `failed`, the
    /// track is `unavailable` until the cooldown elapses, and both events have
    /// been published.
    ///
    /// # Errors
    ///
    /// Returns the library, settings or database error that stopped the search.
    /// A plugin that cannot be reached is not an error: it is remembered as a
    /// failed instance and reported as `backend-unavailable` when no candidate
    /// comes out of the whole layer.
    pub(crate) async fn search(&self, row: &AcquisitionRow) -> Result<SearchOutcome, AcquireError> {
        let track_id = row
            .track_id
            .ok_or(AcquireError::UnknownAcquisition(row.id))?;
        let track = self
            .library()
            .get_track(track_id)
            .await?
            .ok_or(AcquireError::UnknownTrack(track_id))?;
        let artists = self
            .library()
            .list_track_artists(track_id)
            .await?
            .into_iter()
            .map(|artist| artist.name)
            .collect::<Vec<_>>();
        let album = match track.album_id {
            Some(album_id) => self.library().get_album(album_id).await?,
            None => None,
        };
        let album_artist = match album.as_ref() {
            Some(album) => match album.display_artist.clone() {
                Some(name) => Some(name),
                // A release without a display artist is credited to its album
                // artists; the first one is the one the search names.
                None => match track.album_id {
                    Some(album_id) => self
                        .library()
                        .list_album_artists(album_id)
                        .await?
                        .into_iter()
                        .next()
                        .map(|artist| artist.name),
                    None => None,
                },
            },
            None => None,
        };
        let wanted = wanted_track(&track, &artists);

        let snapshot = self.settings().current();
        let profile = QualityProfile::builtin(&snapshot.get(keys::ACQUISITION_QUALITY_PROFILE)?)
            .unwrap_or_default();
        let selection = SelectionRequest {
            class: RequestClass::from_priority(row.priority),
            target: MatchTarget::from_track(
                &wanted,
                album.as_ref().map(|album| album.title.as_str()),
            ),
            profile: profile.clone(),
            min_match_score: snapshot.get(keys::ACQUISITION_MIN_MATCH_SCORE)?,
            interactive_eta_target: Duration::from_secs(
                snapshot
                    .get(keys::ACQUISITION_INTERACTIVE_ETA_TARGET_SECS)?
                    .max(0) as u64,
            ),
            max_candidates: snapshot.get(keys::ACQUISITION_MAX_CANDIDATES)?.max(0) as usize,
        };
        let query = AcquisitionQuery {
            granularity: Granularity::Track,
            album: album.as_ref().map(|album| album.title.clone()),
            album_artist,
            year: album
                .as_ref()
                .and_then(|album| album.year)
                .and_then(|year| u16::try_from(year).ok()),
            tracks: vec![wanted],
            preferred_formats: preferred_formats(&profile),
        };

        let mut run = LayerSearch {
            orchestrator: self,
            context: AcquisitionContext::new(self.staging_dir(row.id)),
            query,
            selection,
            instances: Vec::new(),
            slots: Vec::new(),
            backend_failed: false,
        };
        for instance in self.registry().layers(Layer::Acquisition) {
            run.run_instance(&instance).await;
            // A user is waiting: a usable candidate from one instance is
            // enough, and the rest of the layer is not worth the wait. Bulk
            // work searches every instance to maximize the choice of quality.
            if run.selection.class.is_interactive() && run.has_usable_candidate() {
                break;
            }
        }

        let plan = run.plan();
        if plan.attempts.is_empty() {
            let reason = if run.backend_failed {
                FailureReason::BackendUnavailable
            } else {
                plan.failure.unwrap_or(FailureReason::NoCandidates)
            };
            self.fail_acquisition(
                row.id,
                reason,
                Some("no candidate came out of the acquisition layer"),
            )
            .await?;
            return Ok(SearchOutcome::Failed(reason));
        }

        self.log_search_result(row.id, &plan).await?;
        tracing::info!(
            acquisition = %row.id,
            track = %track_id,
            candidates = plan.attempts.len(),
            "the search selected candidates"
        );
        Ok(SearchOutcome::Planned {
            instances: run.instances,
            plan,
        })
    }

    /// Appends the search result to the acquisition's log.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition the search belongs to.
    /// - `plan`: the candidates that were selected.
    ///
    /// # Errors
    ///
    /// Returns the database error when the log entry cannot be written.
    async fn log_search_result(
        &self,
        id: uuid::Uuid,
        plan: &AttemptPlan,
    ) -> Result<(), AcquireError> {
        let best = plan
            .attempts
            .first()
            .map(|attempt| attempt.candidate.path.as_str())
            .unwrap_or_default();
        let message = format!(
            "the search selected {} candidate(s); the best is `{best}`",
            plan.attempts.len()
        );
        let mut tx = self.database().writer().begin().await?;
        repo::write_log(
            &mut tx,
            id,
            AcquisitionState::Searching,
            Some(message.as_str()),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// One run over the acquisition layer.
///
/// The run holds everything the instances share: the query, the selection
/// thresholds, the job context, the instances that were searched, and the
/// candidates accumulated so far.
struct LayerSearch<'a> {
    /// The orchestrator, for the instance budgets.
    orchestrator: &'a AcquisitionOrchestrator,
    /// The job context, which names the staging root.
    context: AcquisitionContext,
    /// The search text and the wanted track.
    query: AcquisitionQuery,
    /// The thresholds and the profile the candidates are judged by.
    selection: SelectionRequest,
    /// The instances that were searched, in layer order.
    instances: Vec<Arc<RegistryInstance>>,
    /// The candidates accumulated so far.
    slots: Vec<CandidateSlot>,
    /// Whether an instance could not be searched at all.
    backend_failed: bool,
}

impl LayerSearch<'_> {
    /// Returns the plan the accumulated candidates produce.
    fn plan(&self) -> AttemptPlan {
        select_candidates(&self.selection, self.slots.clone())
    }

    /// Returns whether a usable candidate has been found.
    fn has_usable_candidate(&self) -> bool {
        !self.plan().attempts.is_empty()
    }

    /// Searches one instance of the layer, if it can search a track.
    ///
    /// An instance that does not advertise the `track` granularity is skipped.
    /// Bulk work waits for the instance's hourly token before it takes a search
    /// slot, so a throttled search can never block an interactive one that is
    /// waiting for the same slot.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance to search.
    async fn run_instance(&mut self, instance: &Arc<RegistryInstance>) {
        let Some(provider) = instance.acquisition().cloned() else {
            return;
        };
        match provider.get_capabilities(&self.context).await {
            Ok(capabilities) => {
                if !capabilities.granularities.contains(&Granularity::Track) {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        "skipped an instance that cannot search a track"
                    );
                    return;
                }
            }
            Err(error) => {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    %error,
                    "an acquisition instance could not report its capabilities"
                );
                self.backend_failed = true;
                return;
            }
        }

        let budget = self.orchestrator.search_budgets().search_budget(instance);
        if !self.selection.class.is_interactive() {
            budget.hourly.until_ready().await;
        }
        let Ok(_search_slot) = Arc::clone(&budget.searches).acquire_owned().await else {
            // The semaphore is never closed; this arm exists so a closed budget
            // cannot panic the pipeline.
            self.backend_failed = true;
            return;
        };

        let index = self.instances.len();
        self.instances.push(Arc::clone(instance));
        match self.poll(provider.as_ref(), instance, index).await {
            Ok(()) => {}
            Err(failure) => {
                if failure {
                    self.backend_failed = true;
                }
            }
        }
    }

    /// Starts and polls one instance's search.
    ///
    /// # Parameters
    ///
    /// - `provider`: the instance's acquisition provider.
    /// - `instance`: the instance, for its manifest and its slug in logs.
    /// - `index`: the position of the instance in the searched list.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the instance answered, whether or not it found anything.
    /// `Err(true)` when the instance could not be searched at all, and
    /// `Err(false)` when the search was abandoned after it had started, which is
    /// a normal end: the deadline passed, or the token was lost.
    async fn poll(
        &mut self,
        provider: &dyn AcquisitionProvider,
        instance: &RegistryInstance,
        index: usize,
    ) -> Result<(), bool> {
        let deadline = Instant::now()
            + Duration::from_millis(u64::from(instance.manifest().resources.call_timeout_ms));
        let token = match provider.start_search(&self.context, &self.query).await {
            Ok(token) => token,
            Err(error) => {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    %error,
                    "an acquisition instance could not start a search"
                );
                return Err(true);
            }
        };

        loop {
            match provider.poll_search(&self.context, &token).await {
                Ok(SearchState::Running(candidates)) => {
                    self.accumulate(index, candidates);
                }
                Ok(SearchState::Complete(candidates)) => {
                    self.accumulate(index, candidates);
                    return Ok(());
                }
                Err(ProviderError::NotFound) => {
                    tracing::warn!(
                        instance = instance.slug().as_str(),
                        "the backend lost the search token"
                    );
                    return Err(false);
                }
                Err(error) => {
                    tracing::warn!(
                        instance = instance.slug().as_str(),
                        %error,
                        "polling a search failed"
                    );
                    provider.cancel_search(&self.context, &token).await;
                    return Err(true);
                }
            }

            // A user is waiting: as soon as the candidates collected so far
            // contain one this request would download, the rest of this
            // instance's search is not worth the wait.
            if self.selection.class.is_interactive() && self.has_usable_candidate() {
                provider.cancel_search(&self.context, &token).await;
                return Ok(());
            }

            if Instant::now() + SEARCH_POLL_INTERVAL >= deadline {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    "the search deadline passed"
                );
                provider.cancel_search(&self.context, &token).await;
                return Ok(());
            }
            tokio::time::sleep(SEARCH_POLL_INTERVAL).await;
        }
    }

    /// Appends one poll's candidates to the accumulator.
    ///
    /// # Parameters
    ///
    /// - `index`: the position of the instance that reported the candidates.
    /// - `candidates`: the candidates the poll returned.
    fn accumulate(&mut self, index: usize, candidates: Vec<Candidate>) {
        for candidate in candidates {
            let candidate_index = self
                .slots
                .iter()
                .filter(|slot| slot.instance_index == index)
                .count();
            self.slots
                .push(CandidateSlot::new(index, candidate_index, candidate));
        }
    }
}

/// The wanted track the query and the scoring target are built from.
///
/// # Parameters
///
/// - `track`: the library track being acquired.
/// - `artists`: the credited artist names, in credit order.
fn wanted_track(track: &Track, artists: &[String]) -> WantedTrack {
    WantedTrack {
        title: track.title.clone(),
        artists: artists.to_vec(),
        track_number: track
            .track_number
            .and_then(|value| u16::try_from(value).ok()),
        disc_number: track
            .disc_number
            .and_then(|value| u16::try_from(value).ok()),
        duration_ms: track
            .duration_ms
            .and_then(|value| u32::try_from(value).ok()),
        isrc: track.isrc.clone(),
        ids: track
            .mb_recording_id
            .clone()
            .map(|value| ExternalId {
                namespace: "mb-recording".to_owned(),
                value,
            })
            .into_iter()
            .collect(),
    }
}

/// The file extensions a quality profile prefers, best first.
///
/// The profile orders tiers, not formats, so each tier contributes the
/// extensions its formats are stored as, in the order the profile prefers the
/// tiers and with the first occurrence winning. A search passes these to the
/// provider, which may use them to widen or narrow its query.
///
/// # Parameters
///
/// - `profile`: the server-wide quality profile.
///
/// # Returns
///
/// The extension names, without a leading dot, best first.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::profile::QualityProfile;
/// use hearthsound_acquire::search::preferred_formats;
///
/// let formats = preferred_formats(&QualityProfile::lossless_preferred());
/// assert_eq!(formats.first().map(String::as_str), Some("flac"));
/// assert!(formats.contains(&"mp3".to_owned()));
/// ```
pub fn preferred_formats(profile: &QualityProfile) -> Vec<String> {
    let mut formats: Vec<String> = Vec::new();
    for tier in &profile.order {
        for format in formats_for(*tier) {
            if !formats.iter().any(|known| known == format) {
                formats.push((*format).to_owned());
            }
        }
    }
    formats
}

/// The canonical extensions of one quality tier.
///
/// # Parameters
///
/// - `tier`: the tier whose formats are wanted.
fn formats_for(tier: QualityTier) -> &'static [&'static str] {
    match tier {
        QualityTier::Lossless | QualityTier::LosslessHires => {
            &["flac", "alac", "wav", "aiff", "wv"]
        }
        QualityTier::LossyHigh | QualityTier::LossyMid | QualityTier::LossyLow => {
            &["mp3", "m4a", "opus", "ogg"]
        }
    }
}
