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
//! every client retry hit the backends again. A search that is still running
//! when the acquisition is cancelled is cancelled on the backend instead and
//! ends the stage without a failure.

use std::collections::HashSet;
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
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::blocklist::BlockedFile;
use crate::error::AcquireError;
use crate::orchestrator::AcquisitionOrchestrator;
use crate::profile::QualityProfile;
use crate::repo::{self, AcquisitionRow};
use crate::score::MatchTarget;
use crate::select::{
    AlbumAttemptPlan, AlbumSelectionRequest, AttemptPlan, CandidateSlot, RequestClass,
    SelectionRequest, select_album_candidates, select_candidates,
};

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
    /// Album candidates to try, best first, with the album's tracks in playing
    /// order so a mapping's `track_index` addresses them.
    AlbumPlanned {
        /// The instances that were searched, in layer order.
        instances: Vec<Arc<RegistryInstance>>,
        /// The candidates to try, best first.
        plan: AlbumAttemptPlan,
        /// The album's tracks, in playing order.
        tracks: Vec<Track>,
    },
    /// Nothing could be tried and the acquisition was failed.
    Failed(FailureReason),
    /// The acquisition was cancelled while the search ran, so the backend
    /// searches were abandoned and the ledger row already says `cancelled`.
    Cancelled,
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
        self.run_search(row, true).await
    }

    /// Runs the track search stage and reports an empty result without failing.
    ///
    /// This is what a caller that has already decided what an empty search means
    /// uses, for example the second search after every candidate failed
    /// fingerprint verification.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, in `searching`.
    ///
    /// # Returns
    ///
    /// The same outcomes as [`AcquisitionOrchestrator::search`], except that an
    /// empty plan is only reported as [`SearchOutcome::Failed`].
    ///
    /// # Errors
    ///
    /// Returns the library, settings or database error that stopped the search.
    pub(crate) async fn search_without_failing(
        &self,
        row: &AcquisitionRow,
    ) -> Result<SearchOutcome, AcquireError> {
        self.run_search(row, false).await
    }

    /// Runs one track search and decides whether an empty plan fails the job.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row, in `searching`.
    /// - `fail_when_empty`: whether an empty plan marks the acquisition failed.
    ///
    /// # Returns
    ///
    /// The plan, the reason nothing came out of the layer, or the cancellation.
    ///
    /// # Errors
    ///
    /// Returns the library, settings or database error that stopped the search.
    async fn run_search(
        &self,
        row: &AcquisitionRow,
        fail_when_empty: bool,
    ) -> Result<SearchOutcome, AcquireError> {
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
        let selection = Selection::Track(SelectionRequest {
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
        });
        let poll_interval = Duration::from_millis(
            snapshot
                .get(keys::ACQUISITION_SEARCH_POLL_INTERVAL_MS)?
                .max(0) as u64,
        );
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
            granularity: Granularity::Track,
            selection,
            album_tracks: Vec::new(),
            poll_interval,
            instances: Vec::new(),
            slots: Vec::new(),
            backend_failed: false,
            cancelled: false,
            stop: self.stop_signal(row.id),
        };
        for instance in self.registry().layers(Layer::Acquisition) {
            run.run_instance(&instance).await;
            if run.cancelled {
                break;
            }
            // A user is waiting: a usable candidate from one instance is
            // enough, and the rest of the layer is not worth the wait. Bulk
            // work searches every instance to maximize the choice of quality.
            if run.interactive() && run.has_usable_candidate() {
                break;
            }
        }

        if run.cancelled {
            tracing::info!(acquisition = %row.id, "abandoned the search of a cancelled acquisition");
            return Ok(SearchOutcome::Cancelled);
        }

        let plan = run.plan();
        if plan.is_empty() {
            let reason = if run.backend_failed {
                FailureReason::BackendUnavailable
            } else {
                plan.failure().unwrap_or(FailureReason::NoCandidates)
            };
            if fail_when_empty {
                self.fail_acquisition(
                    row.id,
                    reason,
                    Some("no candidate came out of the acquisition layer"),
                )
                .await?;
            } else {
                tracing::info!(
                    acquisition = %row.id,
                    reason = reason.as_str(),
                    "a search found no candidate"
                );
            }
            return Ok(SearchOutcome::Failed(reason));
        }

        self.log_search_result(row.id, &plan).await?;
        tracing::info!(
            acquisition = %row.id,
            track = %track_id,
            candidates = plan.len(),
            "the search selected candidates"
        );
        let SearchPlan::Track(plan) = plan else {
            // A track run only ever builds a track plan.
            return Err(AcquireError::UnknownAcquisition(row.id));
        };
        Ok(SearchOutcome::Planned {
            instances: run.instances,
            plan,
        })
    }

    /// Runs the search stage of an album-granular acquisition.
    ///
    /// The query asks the instances for the whole album: every album track is a
    /// wanted track, and each wanted track also carries the album-level ids the
    /// server knows - the MusicBrainz release group and release and the UPC - so
    /// a provider that looks albums up by release group can.
    ///
    /// # Parameters
    ///
    /// - `row`: the album acquisition's ledger row, in `searching`.
    ///
    /// # Returns
    ///
    /// The album plan to download, the reason nothing could be tried, or the
    /// cancellation. Unlike a track search, an empty plan does not fail the
    /// acquisition here: the caller falls back to one acquisition per track.
    ///
    /// # Errors
    ///
    /// Returns the library, settings or database error that stopped the search.
    pub(crate) async fn search_album(
        &self,
        row: &AcquisitionRow,
    ) -> Result<SearchOutcome, AcquireError> {
        let album_id = row
            .album_id
            .ok_or(AcquireError::UnknownAcquisition(row.id))?;
        let album = self.library().get_album(album_id).await?;
        let tracks = self.library().list_tracks_for_album(album_id).await?;
        if tracks.is_empty() {
            return Err(AcquireError::UnknownAlbum(album_id));
        }
        let album_artist = match album.as_ref() {
            Some(album) => match album.display_artist.clone() {
                Some(name) => Some(name),
                None => self
                    .library()
                    .list_album_artists(album_id)
                    .await?
                    .into_iter()
                    .next()
                    .map(|artist| artist.name),
            },
            None => None,
        };
        let mut wanted = Vec::with_capacity(tracks.len());
        for track in &tracks {
            let artists = self
                .library()
                .list_track_artists(track.id)
                .await?
                .into_iter()
                .map(|artist| artist.name)
                .collect::<Vec<_>>();
            let mut wanted_track = wanted_track(track, &artists);
            if let Some(album) = &album {
                wanted_track.ids.extend(album_external_ids(album));
            }
            wanted.push(wanted_track);
        }

        let snapshot = self.settings().current();
        let profile = QualityProfile::builtin(&snapshot.get(keys::ACQUISITION_QUALITY_PROFILE)?)
            .unwrap_or_default();
        let mut request = AlbumSelectionRequest::new(
            RequestClass::from_priority(row.priority),
            profile.clone(),
            album.as_ref().map(|album| album.title.clone()),
        );
        request.min_match_score = snapshot.get(keys::ACQUISITION_MIN_MATCH_SCORE)?;
        request.interactive_eta_target = Duration::from_secs(
            snapshot
                .get(keys::ACQUISITION_INTERACTIVE_ETA_TARGET_SECS)?
                .max(0) as u64,
        );
        request.max_candidates = snapshot.get(keys::ACQUISITION_MAX_CANDIDATES)?.max(0) as usize;
        let poll_interval = Duration::from_millis(
            snapshot
                .get(keys::ACQUISITION_SEARCH_POLL_INTERVAL_MS)?
                .max(0) as u64,
        );
        let query = AcquisitionQuery {
            granularity: Granularity::Album,
            album: album.as_ref().map(|album| album.title.clone()),
            album_artist,
            year: album
                .as_ref()
                .and_then(|album| album.year)
                .and_then(|year| u16::try_from(year).ok()),
            tracks: wanted,
            preferred_formats: preferred_formats(&profile),
        };

        let mut run = LayerSearch {
            orchestrator: self,
            context: AcquisitionContext::new(self.staging_dir(row.id)),
            query,
            granularity: Granularity::Album,
            selection: Selection::Album(Box::new(request)),
            album_tracks: tracks,
            poll_interval,
            instances: Vec::new(),
            slots: Vec::new(),
            backend_failed: false,
            cancelled: false,
            stop: self.stop_signal(row.id),
        };
        for instance in self.registry().layers(Layer::Acquisition) {
            run.run_instance(&instance).await;
            if run.cancelled {
                break;
            }
            if run.interactive() && run.has_usable_candidate() {
                break;
            }
        }

        if run.cancelled {
            tracing::info!(acquisition = %row.id, "abandoned the album search of a cancelled acquisition");
            return Ok(SearchOutcome::Cancelled);
        }

        let plan = run.plan();
        if plan.is_empty() {
            let reason = if run.backend_failed {
                FailureReason::BackendUnavailable
            } else {
                plan.failure().unwrap_or(FailureReason::NoCandidates)
            };
            return Ok(SearchOutcome::Failed(reason));
        }

        self.log_search_result(row.id, &plan).await?;
        tracing::info!(
            acquisition = %row.id,
            album = %album_id,
            candidates = plan.len(),
            "the album search selected candidates"
        );
        let SearchPlan::Album(plan) = plan else {
            // An album run only ever builds an album plan.
            return Err(AcquireError::UnknownAlbum(album_id));
        };
        Ok(SearchOutcome::AlbumPlanned {
            instances: run.instances,
            plan,
            tracks: run.album_tracks,
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
        plan: &SearchPlan,
    ) -> Result<(), AcquireError> {
        let best = match plan {
            SearchPlan::Track(plan) => plan
                .attempts
                .first()
                .map(|attempt| attempt.candidate.path.as_str()),
            SearchPlan::Album(plan) => plan
                .attempts
                .first()
                .map(|attempt| attempt.candidate.path.as_str()),
        }
        .unwrap_or_default();
        let message = format!(
            "the search selected {} candidate(s); the best is `{best}`",
            plan.len()
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
/// candidates accumulated so far. A track run and an album run differ only in
/// the granularity they ask for and in the rules that turn the candidates into
/// a plan.
struct LayerSearch<'a> {
    /// The orchestrator, for the instance budgets.
    orchestrator: &'a AcquisitionOrchestrator,
    /// The job context, which names the staging root.
    context: AcquisitionContext,
    /// The search text and the wanted tracks.
    query: AcquisitionQuery,
    /// The granularity the instances have to advertise.
    granularity: Granularity,
    /// The thresholds and the profile the candidates are judged by.
    selection: Selection,
    /// The album's tracks, in playing order; empty for a track run.
    album_tracks: Vec<Track>,
    /// How long to wait between two polls of a running search
    /// (`acquisition.search_poll_interval_ms`).
    poll_interval: Duration,
    /// The instances that were searched, in layer order.
    instances: Vec<Arc<RegistryInstance>>,
    /// The candidates accumulated so far.
    slots: Vec<CandidateSlot>,
    /// Whether an instance could not be searched at all.
    backend_failed: bool,
    /// Whether the acquisition was cancelled, which ends the run.
    cancelled: bool,
    /// Wakes the run when the acquisition is cancelled.
    stop: Arc<Notify>,
}

/// Which selection rules a layer search applies.
enum Selection {
    /// One wanted track.
    Track(SelectionRequest),
    /// A whole album.
    Album(Box<AlbumSelectionRequest>),
}

/// The plan a layer search produced.
enum SearchPlan {
    /// The track candidates to try.
    Track(AttemptPlan),
    /// The album candidates to try.
    Album(AlbumAttemptPlan),
}

impl SearchPlan {
    /// Returns whether the plan has nothing to try.
    fn is_empty(&self) -> bool {
        match self {
            SearchPlan::Track(plan) => plan.attempts.is_empty(),
            SearchPlan::Album(plan) => plan.attempts.is_empty(),
        }
    }

    /// Returns why the plan has nothing to try.
    fn failure(&self) -> Option<FailureReason> {
        match self {
            SearchPlan::Track(plan) => plan.failure,
            SearchPlan::Album(plan) => plan.failure,
        }
    }

    /// Returns the number of candidates the plan holds.
    fn len(&self) -> usize {
        match self {
            SearchPlan::Track(plan) => plan.attempts.len(),
            SearchPlan::Album(plan) => plan.attempts.len(),
        }
    }
}

impl LayerSearch<'_> {
    /// Returns the plan the accumulated candidates produce.
    fn plan(&self) -> SearchPlan {
        match &self.selection {
            Selection::Track(request) => {
                SearchPlan::Track(select_candidates(request, self.slots.clone()))
            }
            Selection::Album(request) => SearchPlan::Album(select_album_candidates(
                request,
                &self.query.tracks,
                self.slots.clone(),
            )),
        }
    }

    /// Returns whether the run serves an interactive request.
    fn interactive(&self) -> bool {
        match &self.selection {
            Selection::Track(request) => request.class.is_interactive(),
            Selection::Album(request) => request.class.is_interactive(),
        }
    }

    /// Returns whether a usable candidate has been found.
    fn has_usable_candidate(&self) -> bool {
        !self.plan().is_empty()
    }

    /// Searches one instance of the layer, if it can search the granularity.
    ///
    /// An instance that does not advertise the run's granularity is skipped.
    /// Bulk work waits for the instance's hourly token before it takes a search
    /// slot, so a throttled search can never block an interactive one that is
    /// waiting for the same slot.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance to search.
    async fn run_instance(&mut self, instance: &Arc<RegistryInstance>) {
        if self.cancelled {
            return;
        }
        let Some(provider) = instance.acquisition().cloned() else {
            return;
        };
        match provider.get_capabilities(&self.context).await {
            Ok(capabilities) => {
                if !capabilities.granularities.contains(&self.granularity) {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        granularity = self.granularity.as_str(),
                        "skipped an instance that cannot search this granularity"
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
        if !self.interactive() {
            budget.hourly.until_ready().await;
        }
        let Ok(_search_slot) = Arc::clone(&budget.searches).acquire_owned().await else {
            // The semaphore is never closed; this arm exists so a closed budget
            // cannot panic the pipeline.
            self.backend_failed = true;
            return;
        };

        let index = self.instances.len();
        let blocked = self.orchestrator.blocked_files(instance.id()).await;
        self.instances.push(Arc::clone(instance));
        match self
            .poll(provider.as_ref(), instance, index, &blocked)
            .await
        {
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
    /// - `blocked`: the source keys and file names the instance may not offer
    ///   again.
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
        blocked: &HashSet<BlockedFile>,
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

        let stop = Arc::clone(&self.stop);
        loop {
            match provider.poll_search(&self.context, &token).await {
                Ok(SearchState::Running(candidates)) => {
                    self.accumulate(index, candidates, blocked);
                }
                Ok(SearchState::Complete(candidates)) => {
                    self.accumulate(index, candidates, blocked);
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
            if self.interactive() && self.has_usable_candidate() {
                provider.cancel_search(&self.context, &token).await;
                return Ok(());
            }

            if Instant::now() + self.poll_interval >= deadline {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    "the search deadline passed"
                );
                provider.cancel_search(&self.context, &token).await;
                return Ok(());
            }
            tokio::select! {
                () = tokio::time::sleep(self.poll_interval) => {}
                () = stop.notified() => {
                    // The acquisition was cancelled: the search is no longer
                    // wanted, and the backend should hear about it.
                    provider.cancel_search(&self.context, &token).await;
                    self.cancelled = true;
                    return Ok(());
                }
            }
        }
    }

    /// Appends one poll's candidates to the accumulator.
    ///
    /// A candidate file whose source and file name are quarantined is dropped
    /// before the candidate is scored, and a candidate whose files are all
    /// dropped is not accumulated at all: the backend offered nothing this
    /// server may download.
    ///
    /// # Parameters
    ///
    /// - `index`: the position of the instance that reported the candidates.
    /// - `candidates`: the candidates the poll returned.
    /// - `blocked`: the source keys and file names that are quarantined.
    fn accumulate(
        &mut self,
        index: usize,
        candidates: Vec<Candidate>,
        blocked: &HashSet<BlockedFile>,
    ) {
        for mut candidate in candidates {
            if !blocked.is_empty() {
                let source_key = candidate.source.key.clone();
                let offered = candidate.files.len();
                candidate
                    .files
                    .retain(|file| !blocked.contains(&(source_key.clone(), file.name.clone())));
                if offered > 0 && candidate.files.is_empty() {
                    tracing::info!(
                        source = candidate.source.key.as_str(),
                        candidate = candidate.path.as_str(),
                        "dropped a candidate whose every file is quarantined"
                    );
                    continue;
                }
            }
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

/// The album-level ids an album search repeats on every wanted track.
///
/// A track's own ids name the recording; an album folder is looked up by its
/// release, so the release group, the release and the UPC are carried on each
/// wanted track where the library knows them.
///
/// # Parameters
///
/// - `album`: the album being searched for.
///
/// # Returns
///
/// The ids, in a fixed order, with the ones the album does not know left out.
fn album_external_ids(album: &hearthsound_library::Album) -> Vec<ExternalId> {
    let mut ids = Vec::new();
    for (namespace, value) in [
        ("mb-release-group", album.mb_release_group_id.as_deref()),
        ("mb-release", album.mb_release_id.as_deref()),
        ("upc", album.upc.as_deref()),
    ] {
        if let Some(value) = value {
            ids.push(ExternalId {
                namespace: namespace.to_owned(),
                value: value.to_owned(),
            });
        }
    }
    ids
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

#[cfg(test)]
mod tests {
    use hearthsound_core::EntityId;
    use hearthsound_core::IdPrefix;
    use hearthsound_library::{Album, Origin};

    use super::*;

    /// Builds an album with the ids a test cares about.
    fn album(
        mb_release_group_id: Option<&str>,
        mb_release_id: Option<&str>,
        upc: Option<&str>,
    ) -> Album {
        Album {
            id: EntityId::new(IdPrefix::Album),
            title: "Album".to_owned(),
            sort_title: "Album".to_owned(),
            display_artist: Some("Hello Artist".to_owned()),
            year: Some(1993),
            release_date: None,
            album_type: None,
            mb_release_id: mb_release_id.map(str::to_owned),
            mb_release_group_id: mb_release_group_id.map(str::to_owned),
            upc: upc.map(str::to_owned),
            origin: Origin::Catalog,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn an_album_search_carries_the_album_level_ids() {
        let ids = album_external_ids(&album(Some("group-1"), Some("release-1"), Some("upc-1")));
        assert_eq!(
            ids,
            [
                ExternalId {
                    namespace: "mb-release-group".to_owned(),
                    value: "group-1".to_owned(),
                },
                ExternalId {
                    namespace: "mb-release".to_owned(),
                    value: "release-1".to_owned(),
                },
                ExternalId {
                    namespace: "upc".to_owned(),
                    value: "upc-1".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn an_album_without_ids_contributes_none() {
        assert!(album_external_ids(&album(None, None, None)).is_empty());
    }
}
