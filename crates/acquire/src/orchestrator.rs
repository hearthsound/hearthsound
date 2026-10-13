//! The acquisition orchestrator: request intake, the queue, and the worker
//! slots that drive an acquisition through its stages.
//!
//! [`AcquisitionOrchestrator`] is the single entry point into the acquisition
//! pipeline. A caller that wants a track that is not on disk hands it an
//! [`AcquireRequest`]; the orchestrator decides between the five admission
//! outcomes of [`AcquireOutcome`] and, when it admits a new job, makes it the
//! one active acquisition of that track.
//!
//! The orchestrator owns three pieces of state:
//!
//! - the ledger in `acquisitions` and `acquisition_log`, through
//!   [`crate::repo`];
//! - the waiter channels, one `tokio::sync::watch` per acquisition anyone is
//!   watching, which is how the stream service learns that a file is ready or
//!   that the acquisition failed;
//! - the worker slots. At most `acquisition.max_concurrent` acquisitions are
//!   worked on at once. The queue is the `queued` rows of the ledger ordered by
//!   priority, so a `play` request that arrives behind a background run is
//!   taken first; a slot is held from the moment a worker claims an acquisition
//!   until its pipeline task ends.
//!
//! Everything the pipeline reports leaves through two places: the watchdog
//! channel, which carries the live [`AcquisitionProgress`] of an acquisition,
//! and the event bus, which carries the same snapshot as a
//! `ServerEvent::AcquisitionUpdated`, so a client that is not watching a
//! specific acquisition still sees the state of the server.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{AcquisitionUpdated, EventBus, ServerEvent, TrackAvailability};
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, FailureReason, Granularity, IdPrefix, Priority,
    new_uuid_v7,
};
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::ProviderRegistry;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::download::DownloadOutcome;
use crate::error::AcquireError;
use crate::repo::{self, AcquisitionRepository, AcquisitionRow, NewAcquisition};
use crate::search::SearchOutcome;

/// How often the scheduler looks for work it was not woken for.
///
/// A wake-up is sent when an acquisition is admitted and when a worker slot is
/// released, which covers everything this process does. The tick covers the
/// rest: a setting that raises `acquisition.max_concurrent`, and rows another
/// process - the CLI - wrote.
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

/// A request for a track that is not on disk.
///
/// The request carries everything admission decides on: what to acquire, how
/// urgent it is, who asked, whether that caller may acquire at all, and whether
/// a cooldown should be ignored. Quotas are not part of admission yet; when they
/// arrive they belong here, next to `can_acquire`, because both are decided
/// before any search starts.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::orchestrator::AcquireRequest;
/// use hearthsound_core::{EntityId, IdPrefix, Priority};
///
/// let track = EntityId::new(IdPrefix::Track);
/// let request = AcquireRequest::new(track, Priority::Play, "user request");
/// assert_eq!(request.priority, Priority::Play);
/// assert!(request.can_acquire);
/// assert!(!request.force);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquireRequest {
    /// The materialized track to acquire.
    pub track_id: EntityId,
    /// How urgent the acquisition is.
    pub priority: Priority,
    /// Why the acquisition was requested, for the ledger and the admin view.
    pub reason: String,
    /// The user whose action created the request, when there was one.
    pub requested_by: Option<Uuid>,
    /// Whether the requester may acquire at all.
    ///
    /// A caller acting for a user passes that user's `can_acquire` permission; a
    /// caller acting for the server itself - the prefetch scheduler, a
    /// scheduled pre-acquisition - passes `true`. A denied request writes
    /// nothing, so a user without the permission cannot fill the ledger.
    pub can_acquire: bool,
    /// Whether the track's cooldown is ignored.
    ///
    /// A forced request starts a fresh attempt even when the track is
    /// `unavailable` with time left on its cooldown, which is what the admin
    /// retry does.
    pub force: bool,
}

impl AcquireRequest {
    /// Builds a request a server-side caller makes.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the materialized track to acquire.
    /// - `priority`: how urgent the acquisition is.
    /// - `reason`: why it was requested.
    ///
    /// # Returns
    ///
    /// The request, with the `can_acquire` permission granted and the cooldown
    /// honored. Use [`AcquireRequest::with_can_acquire`] for a request made on
    /// behalf of a user and [`AcquireRequest::forced`] for an admin retry.
    pub fn new(
        track_id: EntityId,
        priority: Priority,
        reason: impl Into<String>,
    ) -> AcquireRequest {
        AcquireRequest {
            track_id,
            priority,
            reason: reason.into(),
            requested_by: None,
            can_acquire: true,
            force: false,
        }
    }

    /// Sets the user whose action created the request.
    ///
    /// # Parameters
    ///
    /// - `requested_by`: the user's id, or `None` for a server-side request.
    pub fn with_requested_by(mut self, requested_by: Option<Uuid>) -> AcquireRequest {
        self.requested_by = requested_by;
        self
    }

    /// Sets whether the requester may acquire.
    ///
    /// # Parameters
    ///
    /// - `can_acquire`: the requester's `can_acquire` permission.
    pub fn with_can_acquire(mut self, can_acquire: bool) -> AcquireRequest {
        self.can_acquire = can_acquire;
        self
    }

    /// Ignores the track's cooldown.
    pub fn forced(mut self) -> AcquireRequest {
        self.force = true;
        self
    }

    /// Ignores the track's cooldown when `force` is set.
    ///
    /// # Parameters
    ///
    /// - `force`: whether the cooldown is ignored.
    pub fn forced_if(mut self, force: bool) -> AcquireRequest {
        self.force = force;
        self
    }
}

/// A request for a whole album.
///
/// Album requests fan out into one track request per album track, all carrying
/// the album request's priority and reason. A single album-granular job, which
/// the design allows for an instance that advertises the `album` granularity,
/// is not created yet: the fan-out is what every acquisition instance can
/// serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumAcquireRequest {
    /// The materialized album to acquire.
    pub album_id: EntityId,
    /// How urgent every track request is.
    pub priority: Priority,
    /// Why the album was requested.
    pub reason: String,
    /// The user whose action created the request, when there was one.
    pub requested_by: Option<Uuid>,
    /// Whether the requester may acquire at all.
    pub can_acquire: bool,
    /// Whether every track's cooldown is ignored.
    pub force: bool,
}

/// One album track and what admitting it produced.
#[derive(Debug, Clone)]
pub struct AlbumTrackOutcome {
    /// The track the outcome belongs to.
    pub track_id: EntityId,
    /// What admission decided for that track.
    pub outcome: AcquireOutcome,
}

/// What admission decided for one request.
#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    /// The track already has a file: it is `local` or `cached`, so the stream
    /// service can serve it and no acquisition is needed.
    AlreadyAvailable {
        /// The track that is already available.
        track_id: EntityId,
    },
    /// An active acquisition of the same track was joined.
    ///
    /// The existing job was promoted to the more urgent of the two priorities
    /// when the new request was more urgent; both callers watch the same
    /// acquisition.
    Joined(AcquisitionHandle),
    /// The track failed recently and its cooldown has not elapsed.
    ///
    /// Nothing was searched, admitted or written. A forced request ignores the
    /// cooldown and never produces this outcome.
    CoolingDown {
        /// The track that is cooling down.
        track_id: EntityId,
        /// The instant before which the track is not retried, in unix
        /// milliseconds.
        unavailable_until: i64,
    },
    /// The requester may not acquire.
    ///
    /// Nothing was written: a denied request leaves no trace in the ledger.
    Denied {
        /// The track the request named.
        track_id: EntityId,
    },
    /// A new acquisition was admitted.
    Started(AcquisitionHandle),
}

impl AcquireOutcome {
    /// Returns the acquisition the caller can watch.
    ///
    /// # Returns
    ///
    /// The handle for a started or joined acquisition, `None` for every other
    /// outcome.
    pub fn handle(&self) -> Option<&AcquisitionHandle> {
        match self {
            AcquireOutcome::Started(handle) | AcquireOutcome::Joined(handle) => Some(handle),
            _ => None,
        }
    }

    /// Returns the id of the acquisition the outcome refers to.
    ///
    /// # Returns
    ///
    /// The acquisition id for a started or joined acquisition, `None` for every
    /// other outcome.
    pub fn acquisition_id(&self) -> Option<Uuid> {
        self.handle().map(|handle| handle.id)
    }
}

/// A handle on an acquisition the caller can watch.
///
/// The handle's [`watch::Receiver`] carries the acquisition's live progress. It
/// already holds the current snapshot, so a caller that only wants to know the
/// state does not have to wait for the next change, and a caller that is
/// waiting for audio awaits changes until
/// [`AcquisitionProgress::is_terminal`] is true.
#[derive(Debug, Clone)]
pub struct AcquisitionHandle {
    /// The acquisition's id.
    pub id: Uuid,
    /// The track the acquisition is for.
    pub track_id: Option<EntityId>,
    /// The acquisition's priority, after a join may have raised it.
    pub priority: Priority,
    /// The acquisition's progress.
    pub progress: watch::Receiver<AcquisitionProgress>,
}

impl AcquisitionHandle {
    /// Returns the current progress snapshot.
    pub fn snapshot(&self) -> AcquisitionProgress {
        self.progress.borrow().clone()
    }

    /// Waits until the acquisition ends.
    ///
    /// # Returns
    ///
    /// The terminal snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when the channel is closed
    /// before the acquisition ends, which happens when the server stops while
    /// the acquisition is still in flight.
    pub async fn wait(mut self) -> Result<AcquisitionProgress, AcquireError> {
        loop {
            let snapshot = self.progress.borrow_and_update().clone();
            if snapshot.is_terminal() {
                return Ok(snapshot);
            }
            self.progress
                .changed()
                .await
                .map_err(|_| AcquireError::UnknownAcquisition(self.id))?;
        }
    }
}

/// One acquisition's progress, as a waiter sees it.
///
/// The snapshot is what [`AcquisitionHandle`] hands out and what the SSE event
/// stream renders, so the two never disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquisitionProgress {
    /// The acquisition's id.
    pub acquisition_id: Uuid,
    /// The client id of the track or album the acquisition targets.
    pub target_id: String,
    /// The acquisition's state.
    pub state: AcquisitionState,
    /// The acquisition's priority.
    pub priority: Priority,
    /// How far the download has progressed, as a whole percentage, when the
    /// backend reports one.
    pub percent: Option<u8>,
    /// The estimated number of seconds left, when it can be estimated.
    pub eta_secs: Option<u32>,
    /// The instance serving the acquisition, when one is.
    pub instance: Option<String>,
    /// Why the acquisition failed, when it has failed.
    pub failure_reason: Option<FailureReason>,
    /// The absolute path of a file the acquisition has produced, once its
    /// download completed and the files were resolved.
    ///
    /// This is the file the download wrote, not the managed file the import
    /// writes; the import stage replaces it with the library path.
    pub file: Option<PathBuf>,
}

impl AcquisitionProgress {
    /// Returns whether the acquisition has ended.
    ///
    /// # Returns
    ///
    /// `true` for `completed`, `failed` and `cancelled`.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Returns whether the acquisition produced a file.
    ///
    /// # Returns
    ///
    /// `true` when the state is `completed`.
    pub fn succeeded(&self) -> bool {
        self.state == AcquisitionState::Completed
    }

    /// Returns whether the acquisition ended without a file.
    ///
    /// # Returns
    ///
    /// `true` when the state is `failed` or `cancelled`.
    pub fn failed(&self) -> bool {
        matches!(
            self.state,
            AcquisitionState::Failed | AcquisitionState::Cancelled
        )
    }
}

/// The acquisition orchestrator.
///
/// One orchestrator is built by the composition root and shared through an
/// `Arc`. Cloning is cheap: every clone shares the same ledger, provider
/// registry, settings and worker slots.
///
/// # Examples
///
/// ```no_run
/// use std::path::PathBuf;
/// use std::sync::Arc;
///
/// use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
/// use hearthsound_config::settings::Settings;
/// use hearthsound_core::events::EventBus;
/// use hearthsound_db::Database;
/// use hearthsound_media::FfmpegRunner;
/// use hearthsound_plugin_host::ProviderRegistry;
///
/// # async fn example(
/// #     database: Arc<Database>,
/// #     settings: Settings,
/// #     registry: Arc<ProviderRegistry>,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// let orchestrator = Arc::new(AcquisitionOrchestrator::new(
///     database,
///     settings,
///     registry,
///     Arc::new(FfmpegRunner::new(None, 8)),
///     Arc::new(EventBus::new()),
///     PathBuf::from("/srv/hearthsound/storage/staging"),
/// ));
/// orchestrator.start();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct AcquisitionOrchestrator {
    /// The shared state every clone holds.
    inner: Arc<Inner>,
}

/// The state one orchestrator shares with its clones.
struct Inner {
    /// The database holding the ledger.
    database: Arc<Database>,
    /// The settings service, read on every decision.
    settings: Settings,
    /// The provider instances the acquisition layer walks.
    registry: Arc<ProviderRegistry>,
    /// The media runner the verification stage uses.
    ffmpeg: Arc<FfmpegRunner>,
    /// The bus every state change is published on.
    events: Arc<EventBus>,
    /// The library tables the wanted track is read from.
    library: LibraryRepository,
    /// The ledger.
    repository: AcquisitionRepository,
    /// The root of `<storage>/staging/`.
    staging_root: PathBuf,
    /// One channel per acquisition anyone is watching.
    waiters: Mutex<HashMap<Uuid, watch::Sender<AcquisitionProgress>>>,
    /// How many worker slots are in use.
    running: AtomicUsize,
    /// The per-instance search budgets the search stage respects.
    search_budgets: crate::budget::InstanceBudgets,
    /// The per-instance download slots and the in-flight registry.
    download_budgets: crate::budget::DownloadBudgets,
    /// Whether the scheduler task has been started.
    started: AtomicBool,
    /// Wakes the scheduler when there is work.
    notify: Notify,
}

impl AcquisitionOrchestrator {
    /// Builds the orchestrator.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database; the orchestrator owns the
    ///   `acquisitions` and `acquisition_log` tables in it.
    /// - `settings`: the settings service; every decision reads its current
    ///   snapshot, so a change takes effect without a restart.
    /// - `registry`: the provider instances of the acquisition layer.
    /// - `ffmpeg`: the media runner the verification stage drives.
    /// - `events`: the bus every state change and availability change is
    ///   published on.
    /// - `staging_root`: the directory per-acquisition staging directories are
    ///   created under, which is `paths.staging` - `<storage>/staging`.
    pub fn new(
        database: Arc<Database>,
        settings: Settings,
        registry: Arc<ProviderRegistry>,
        ffmpeg: Arc<FfmpegRunner>,
        events: Arc<EventBus>,
        staging_root: PathBuf,
    ) -> AcquisitionOrchestrator {
        let library = LibraryRepository::new(&database);
        let repository = AcquisitionRepository::new(&database);
        AcquisitionOrchestrator {
            inner: Arc::new(Inner {
                database,
                settings,
                registry,
                ffmpeg,
                events,
                library,
                repository,
                staging_root,
                waiters: Mutex::new(HashMap::new()),
                running: AtomicUsize::new(0),
                search_budgets: crate::budget::InstanceBudgets::new(),
                download_budgets: crate::budget::DownloadBudgets::new(),
                started: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Returns the open database.
    pub fn database(&self) -> &Database {
        &self.inner.database
    }

    /// Returns the settings service.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Returns the provider registry the acquisition layer is read from.
    pub fn registry(&self) -> &ProviderRegistry {
        &self.inner.registry
    }

    /// Returns the media runner the verification stage drives.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.inner.ffmpeg
    }

    /// Returns the event bus every state change is published on.
    pub fn events(&self) -> &EventBus {
        &self.inner.events
    }

    /// Returns the library repository the wanted track is read from.
    pub fn library(&self) -> &LibraryRepository {
        &self.inner.library
    }

    /// Returns the acquisition ledger.
    pub fn repository(&self) -> &AcquisitionRepository {
        &self.inner.repository
    }

    /// Returns the per-instance search budgets.
    pub(crate) fn search_budgets(&self) -> &crate::budget::InstanceBudgets {
        &self.inner.search_budgets
    }

    /// Returns the per-instance download slots and the in-flight registry.
    pub(crate) fn download_budgets(&self) -> &crate::budget::DownloadBudgets {
        &self.inner.download_budgets
    }

    /// Returns the root per-acquisition staging directories are created under.
    pub fn staging_root(&self) -> &Path {
        &self.inner.staging_root
    }

    /// Returns the staging directory of one acquisition.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose directory to build.
    ///
    /// # Returns
    ///
    /// `<staging_root>/<acquisition-id>/`.
    pub fn staging_dir(&self, acquisition_id: Uuid) -> PathBuf {
        self.inner.staging_root.join(acquisition_id.to_string())
    }

    /// Admits or joins an acquisition for one track.
    ///
    /// Admission is decided in one write transaction, so two requests for the
    /// same track that arrive together cannot both create a job: the second sees
    /// the first's row and joins it.
    ///
    /// # Parameters
    ///
    /// - `request`: what to acquire and on whose behalf.
    ///
    /// # Returns
    ///
    /// The outcome: the track is already available, an existing acquisition was
    /// joined, the track is cooling down, the caller may not acquire, or a new
    /// acquisition was admitted. The last one has created the acquisition's
    /// staging directory, marked the track `acquiring`, and published both the
    /// `acquisition.updated` and the `track.availability` event.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownTrack`] when the id has no `tracks` row,
    /// and the database, settings or filesystem error that stopped admission.
    pub async fn request(&self, request: AcquireRequest) -> Result<AcquireOutcome, AcquireError> {
        if !request.can_acquire {
            tracing::debug!(
                track = %request.track_id,
                user = ?request.requested_by,
                "refused an acquire request without the permission"
            );
            return Ok(AcquireOutcome::Denied {
                track_id: request.track_id,
            });
        }

        let track = self
            .inner
            .library
            .get_track(request.track_id)
            .await?
            .ok_or(AcquireError::UnknownTrack(request.track_id))?;

        if matches!(
            track.availability,
            Availability::Local | Availability::Cached
        ) {
            return Ok(AcquireOutcome::AlreadyAvailable { track_id: track.id });
        }

        if !request.force && track.availability == Availability::Unavailable {
            let until = track.unavailable_until.unwrap_or(0);
            if until > crate::now_millis() {
                tracing::debug!(
                    track = %track.id,
                    unavailable_until = until,
                    "refused an acquire request while the track cools down"
                );
                return Ok(AcquireOutcome::CoolingDown {
                    track_id: track.id,
                    unavailable_until: until,
                });
            }
        }

        let id = new_uuid_v7();
        let staging = self.staging_dir(id);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|source| AcquireError::Io {
                path: staging.clone(),
                source,
            })?;

        let mut tx = self.inner.database.writer().begin().await?;
        if let Some(active) = repo::active_for_track(&mut tx, track.id).await? {
            let priority = active.priority.min(request.priority);
            let raised = if priority != active.priority {
                repo::set_priority(&mut tx, active.id, priority).await?
            } else {
                false
            };
            tx.commit().await?;
            // The directory belongs to an acquisition that was not created, so
            // it is removed rather than left behind empty.
            let _ = tokio::fs::remove_dir_all(&staging).await;
            if raised {
                tracing::debug!(
                    acquisition = %active.id,
                    track = %track.id,
                    priority = priority.as_str(),
                    "raised an acquisition's priority"
                );
                self.announce(&self.progress_of_row(&active, priority));
            }
            let handle = self.attach(
                active.id,
                Some(track.id),
                priority,
                active.state,
                active.failure_reason,
            );
            return Ok(AcquireOutcome::Joined(handle));
        }

        let new = NewAcquisition {
            id,
            track_id: Some(track.id),
            album_id: None,
            granularity: Granularity::Track,
            priority: request.priority,
            reason: request.reason.clone(),
            requested_by: request.requested_by,
        };
        repo::insert(&mut tx, &new).await?;
        hearthsound_library::tracks::set_track_availability(
            &mut tx,
            track.id,
            Availability::Acquiring,
            None,
        )
        .await?;
        tx.commit().await?;

        tracing::info!(
            acquisition = %id,
            track = %track.id,
            priority = request.priority.as_str(),
            reason = request.reason.as_str(),
            user = ?request.requested_by,
            "admitted an acquisition"
        );

        let progress = AcquisitionProgress {
            acquisition_id: id,
            target_id: track.id.to_string(),
            state: AcquisitionState::Queued,
            priority: request.priority,
            percent: None,
            eta_secs: None,
            instance: None,
            failure_reason: None,
            file: None,
        };
        let handle = self.attach(
            id,
            Some(track.id),
            request.priority,
            AcquisitionState::Queued,
            None,
        );
        self.announce(&progress);
        self.publish_availability(track.id, Availability::Acquiring);
        self.notify_work();
        Ok(AcquireOutcome::Started(handle))
    }

    /// Admits or joins one acquisition per track of an album.
    ///
    /// # Parameters
    ///
    /// - `request`: the album and the priority, reason, user and permissions
    ///   every track request inherits.
    ///
    /// # Returns
    ///
    /// One outcome per album track, in playing order.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAlbum`] when the album has no tracks, and
    /// the error that stopped admitting a track otherwise. Outcomes already
    /// produced are kept: the album's tracks before the failure have their own
    /// acquisitions.
    pub async fn request_album(
        &self,
        request: AlbumAcquireRequest,
    ) -> Result<Vec<AlbumTrackOutcome>, AcquireError> {
        let tracks = self
            .inner
            .library
            .list_tracks_for_album(request.album_id)
            .await?;
        if tracks.is_empty() {
            return Err(AcquireError::UnknownAlbum(request.album_id));
        }
        let mut outcomes = Vec::with_capacity(tracks.len());
        for track in tracks {
            let outcome = self
                .request(
                    AcquireRequest::new(track.id, request.priority, request.reason.clone())
                        .with_requested_by(request.requested_by)
                        .with_can_acquire(request.can_acquire)
                        .forced_if(request.force),
                )
                .await?;
            outcomes.push(AlbumTrackOutcome {
                track_id: track.id,
                outcome,
            });
        }
        Ok(outcomes)
    }

    /// Subscribes to an acquisition's progress.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition to watch.
    ///
    /// # Returns
    ///
    /// A receiver holding the current snapshot, or `None` when the acquisition
    /// is unknown or has no live channel: an acquisition that ended before this
    /// call, or one this process did not admit, is read from the ledger with
    /// [`AcquisitionOrchestrator::progress_of`] instead.
    pub fn subscribe(&self, acquisition_id: Uuid) -> Option<watch::Receiver<AcquisitionProgress>> {
        let waiters = lock(&self.inner.waiters);
        waiters.get(&acquisition_id).map(watch::Sender::subscribe)
    }

    /// Reads an acquisition's progress from the ledger.
    ///
    /// This is the view a client that is not subscribed gets, and the view the
    /// live channel starts from. It carries the state, the priority, the
    /// instance the ledger names, the failure reason, and the percentage and ETA
    /// of the last progress a download reported. Only the file a completed
    /// download resolved is exclusive to the live channel, because the ledger
    /// does not store paths.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition to read.
    ///
    /// # Returns
    ///
    /// The snapshot, or `None` when no acquisition has that id.
    ///
    /// # Errors
    ///
    /// Returns the database error when the row cannot be read.
    pub async fn progress_of(
        &self,
        acquisition_id: Uuid,
    ) -> Result<Option<AcquisitionProgress>, AcquireError> {
        let Some(row) = self.inner.repository.get(acquisition_id).await? else {
            return Ok(None);
        };
        Ok(Some(self.progress_of_row(&row, row.priority)))
    }

    /// Starts the scheduler that claims queued acquisitions.
    ///
    /// The composition root calls this once after building the orchestrator. It
    /// is idempotent: a second call does nothing. The scheduler runs until the
    /// server stops, so the task it spawns is deliberately not joined.
    pub fn start(self: &Arc<Self>) {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move { this.schedule().await });
    }

    /// Returns how many worker slots are in use.
    ///
    /// # Returns
    ///
    /// The number of acquisitions whose pipeline task is running.
    pub fn running(&self) -> usize {
        self.inner.running.load(Ordering::Acquire)
    }

    /// Runs the scheduler loop.
    async fn schedule(self: Arc<Self>) {
        loop {
            if let Err(error) = self.dispatch().await {
                tracing::warn!(%error, "the acquisition scheduler could not look for work");
            }
            tokio::select! {
                () = self.inner.notify.notified() => {}
                () = tokio::time::sleep(SCHEDULER_TICK) => {}
            }
        }
    }

    /// Claims and starts as many queued acquisitions as there are free slots.
    ///
    /// # Errors
    ///
    /// Returns the error of the setting read or ledger query that stopped the
    /// sweep. A slot reserved for an acquisition that could not be claimed is
    /// released again.
    async fn dispatch(&self) -> Result<(), AcquireError> {
        loop {
            if !self.try_reserve_slot()? {
                return Ok(());
            }
            let Some(row) = self.inner.repository.next_queued().await? else {
                self.release_slot();
                return Ok(());
            };
            if !self.inner.repository.claim(row.id).await? {
                self.release_slot();
                continue;
            }
            tracing::debug!(acquisition = %row.id, priority = row.priority.as_str(), "claimed an acquisition");
            self.notify_work();
            let orchestrator = self.clone();
            tokio::spawn(async move {
                orchestrator.advance(row.id).await;
                orchestrator.release_slot();
            });
        }
    }

    /// Advances one claimed acquisition.
    ///
    /// The pipeline runs stage by stage and the row records how far it got, so
    /// this method is driven by the acquisition's state rather than by its
    /// history. Identity resolution is a pass-through until the metadata layer
    /// is wired in, which leaves a claimed acquisition in `searching`; the
    /// search stage then selects the candidates its download stage will try.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to advance.
    async fn advance(&self, id: Uuid) {
        if let Err(error) = self.run_pipeline(id).await {
            tracing::warn!(acquisition = %id, %error, "advancing an acquisition failed");
        }
    }

    /// Runs the stages an acquisition is ready for.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to advance.
    ///
    /// # Errors
    ///
    /// Returns the library, settings, plugin or database error that stopped a
    /// stage. A stage that decides the acquisition cannot succeed records the
    /// failure itself and returns `Ok`.
    async fn run_pipeline(&self, id: Uuid) -> Result<(), AcquireError> {
        let Some(mut row) = self.inner.repository.get(id).await? else {
            return Err(AcquireError::UnknownAcquisition(id));
        };
        if row.state == AcquisitionState::Resolving {
            self.inner
                .repository
                .transition(
                    id,
                    AcquisitionState::Searching,
                    Some("identity resolution passed through"),
                )
                .await?;
            row.state = AcquisitionState::Searching;
            self.announce(&self.progress_of_row(&row, row.priority));
        }
        loop {
            if row.state == AcquisitionState::Searching {
                match self.search(&row).await? {
                    SearchOutcome::Failed(_reason) => return Ok(()),
                    // The search selected the candidates to try and logged
                    // them. The best one names the instance and the ETA a client
                    // sees while the download starts.
                    SearchOutcome::Planned { instances, plan } => {
                        let mut progress = self.progress_of_row(&row, row.priority);
                        if let Some(best) = plan.attempts.first() {
                            progress.instance = instances
                                .get(best.instance_index)
                                .map(|instance| instance.slug().as_str().to_owned());
                            progress.eta_secs = best
                                .eta
                                .map(|eta| eta.as_secs().min(u64::from(u32::MAX)) as u32);
                        }
                        self.announce(&progress);

                        match self.download(row.id, &instances, &plan).await? {
                            DownloadOutcome::Ready | DownloadOutcome::Stopped => return Ok(()),
                            DownloadOutcome::RestartSearch => {
                                // The backend lost the token. The download stage
                                // moved the acquisition back to `searching`, so
                                // the pipeline starts the search again.
                                row.state = AcquisitionState::Searching;
                                continue;
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
    }

    /// Fails an acquisition and puts its track into cooldown.
    ///
    /// The failure, the track's new availability and the cooldown end are
    /// written in one transaction, so a client can never see a failed
    /// acquisition whose track is still `acquiring`, and both events are
    /// published afterwards. The cooldown is
    /// `acquisition.unavailable_cooldown_hours`, which stops a track nobody has
    /// can find from making every client retry hit the backends again.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to fail.
    /// - `reason`: why it failed, one of the fixed [`FailureReason`] literals.
    /// - `detail`: the backend's own message, when the failure has one.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when the row is missing, the
    /// settings error when the cooldown cannot be read, and
    /// [`AcquireError::IllegalTransition`] when the current state has no edge to
    /// `failed`.
    pub(crate) async fn fail_acquisition(
        &self,
        id: Uuid,
        reason: FailureReason,
        detail: Option<&str>,
    ) -> Result<(), AcquireError> {
        let Some(row) = self.inner.repository.get(id).await? else {
            return Err(AcquireError::UnknownAcquisition(id));
        };
        let cooldown_hours = self
            .inner
            .settings
            .current()
            .get(keys::ACQUISITION_UNAVAILABLE_COOLDOWN_HOURS)?;
        let unavailable_until = crate::now_millis() + cooldown_hours.max(0) * 3_600_000;

        let mut tx = self.inner.database.writer().begin().await?;
        repo::fail(&mut tx, id, reason, detail).await?;
        if let Some(track_id) = row.track_id {
            hearthsound_library::tracks::set_track_availability(
                &mut tx,
                track_id,
                Availability::Unavailable,
                Some(unavailable_until),
            )
            .await?;
        }
        tx.commit().await?;

        let mut progress = self.progress_of_row(&row, row.priority);
        progress.state = AcquisitionState::Failed;
        progress.failure_reason = Some(reason);
        self.announce(&progress);
        if let Some(track_id) = row.track_id {
            self.publish_availability(track_id, Availability::Unavailable);
        }
        tracing::info!(
            acquisition = %id,
            track = ?row.track_id,
            reason = reason.as_str(),
            unavailable_until,
            "failed an acquisition"
        );
        Ok(())
    }

    /// Attaches a waiter to an acquisition and returns its handle.
    ///
    /// The channel is created on first use with the snapshot the ledger holds,
    /// so a caller that joins an acquisition admitted earlier - or in another
    /// process - still sees a current value. Terminal channels whose receivers
    /// are all gone are dropped when the next acquisition is admitted.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to watch.
    /// - `track_id`: the acquisition's track, when it has one.
    /// - `priority`: the acquisition's current priority.
    /// - `state`: the acquisition's current state.
    /// - `failure_reason`: the acquisition's failure reason, when it failed.
    fn attach(
        &self,
        id: Uuid,
        track_id: Option<EntityId>,
        priority: Priority,
        state: AcquisitionState,
        failure_reason: Option<FailureReason>,
    ) -> AcquisitionHandle {
        let progress = AcquisitionProgress {
            acquisition_id: id,
            target_id: track_id.map(|track| track.to_string()).unwrap_or_default(),
            state,
            priority,
            percent: None,
            eta_secs: None,
            instance: None,
            failure_reason,
            file: None,
        };
        let mut waiters = lock(&self.inner.waiters);
        waiters.retain(|_, sender| sender.receiver_count() > 0 || !sender.borrow().is_terminal());
        let sender = waiters
            .entry(id)
            .or_insert_with(|| watch::channel(progress).0);
        let receiver = sender.subscribe();
        drop(waiters);
        AcquisitionHandle {
            id,
            track_id,
            priority,
            progress: receiver,
        }
    }

    /// Publishes a snapshot to the acquisition's waiters and to the bus.
    ///
    /// # Parameters
    ///
    /// - `progress`: the snapshot to publish.
    pub(crate) fn announce(&self, progress: &AcquisitionProgress) {
        {
            let waiters = lock(&self.inner.waiters);
            if let Some(sender) = waiters.get(&progress.acquisition_id) {
                sender.send_replace(progress.clone());
            }
        }
        self.inner
            .events
            .publish(ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
                acquisition_id: EntityId::from_uuid(IdPrefix::Acquisition, progress.acquisition_id)
                    .to_string(),
                target_id: progress.target_id.clone(),
                state: progress.state,
                priority: progress.priority,
                percent: progress.percent,
                eta_secs: progress.eta_secs,
                instance: progress.instance.clone(),
                failure_reason: progress.failure_reason,
            }));
    }

    /// Publishes a track's availability change.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track that changed.
    /// - `availability`: its new availability.
    pub(crate) fn publish_availability(&self, track_id: EntityId, availability: Availability) {
        self.inner
            .events
            .publish(ServerEvent::TrackAvailability(TrackAvailability {
                track_id: track_id.to_string(),
                availability,
                quality: None,
            }));
    }

    /// Builds a snapshot from a ledger row.
    ///
    /// # Parameters
    ///
    /// - `row`: the row to read.
    /// - `priority`: the priority to report, which is the row's after a join
    ///   may have raised it.
    pub(crate) fn progress_of_row(
        &self,
        row: &AcquisitionRow,
        priority: Priority,
    ) -> AcquisitionProgress {
        let stored = crate::download::stored_progress(row.progress.as_deref());
        AcquisitionProgress {
            acquisition_id: row.id,
            target_id: row
                .track_id
                .map(|track| track.to_string())
                .or_else(|| row.album_id.map(|album| album.to_string()))
                .unwrap_or_default(),
            state: row.state,
            priority,
            percent: stored
                .as_ref()
                .and_then(crate::download::StoredProgress::percent),
            eta_secs: stored
                .as_ref()
                .and_then(crate::download::StoredProgress::eta_secs),
            instance: row.instance_id.and_then(|id| self.instance_slug(id)),
            failure_reason: row.failure_reason,
            file: None,
        }
    }

    /// Returns the slug of an instance the acquisition layer serves.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the instance id a ledger row names.
    ///
    /// # Returns
    ///
    /// The slug, or `None` when the registry no longer serves that instance.
    fn instance_slug(&self, instance_id: Uuid) -> Option<String> {
        self.inner
            .registry
            .layers(hearthsound_core::Layer::Acquisition)
            .into_iter()
            .find(|instance| instance.id() == instance_id)
            .map(|instance| instance.slug().as_str().to_owned())
    }

    /// Returns how many acquisitions may run at once.
    ///
    /// # Returns
    ///
    /// `acquisition.max_concurrent`, never below one.
    ///
    /// # Errors
    ///
    /// Returns the settings error when the key cannot be read.
    fn max_concurrent(&self) -> Result<usize, AcquireError> {
        let snapshot = self.inner.settings.current();
        let value = snapshot.get(keys::ACQUISITION_MAX_CONCURRENT)?;
        Ok(value.max(1) as usize)
    }

    /// Takes a worker slot when one is free.
    ///
    /// # Returns
    ///
    /// `true` when a slot was taken; the caller has to release it.
    ///
    /// # Errors
    ///
    /// Returns the settings error when the bound cannot be read; no slot is
    /// taken then.
    fn try_reserve_slot(&self) -> Result<bool, AcquireError> {
        let max = self.max_concurrent()?;
        Ok(self
            .inner
            .running
            .try_update(Ordering::AcqRel, Ordering::Acquire, |running| {
                (running < max).then_some(running + 1)
            })
            .is_ok())
    }

    /// Releases a worker slot and wakes the scheduler.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when no slot was taken, which is a bookkeeping
    /// bug in this module.
    fn release_slot(&self) {
        let previous = self.inner.running.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "a worker slot was released twice");
        self.notify_work();
    }

    /// Wakes the scheduler.
    fn notify_work(&self) {
        self.inner.notify.notify_one();
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// The state behind the locked mutex is a map of channel senders and a counter,
/// which stays consistent even if a task panicked while holding the lock, so a
/// poisoned lock is not a reason to stop the server.
///
/// # Panics
///
/// Does not panic; the underlying lock's poisoning is ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
