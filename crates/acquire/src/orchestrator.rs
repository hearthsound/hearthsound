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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use hearthsound_catalog::CatalogService;
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::{
    AcquisitionUpdated, Audience, EventBus, ServerEvent, TrackAvailability,
};
use hearthsound_core::providers::AcquisitionContext;
use hearthsound_core::{
    AcquisitionState, Availability, EntityId, FailureReason, Granularity, IdPrefix, Priority,
    QualityTier, new_uuid_v7,
};
use hearthsound_db::Database;
use hearthsound_library::{LibraryRepository, StorageRoots, media_files, tracks};
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::{ProviderRegistry, RegistryInstance};
use sqlx::SqliteConnection;
use tokio::sync::{Notify, watch};
use uuid::Uuid;

use crate::announce::AnnounceGate;
use crate::download::DownloadOutcome;
use crate::error::AcquireError;
use crate::repo::{
    self, AcquisitionRepository, AcquisitionRow, AcquisitionSummary, LedgerQuery, ListFilter,
    NewAcquisition,
};
use crate::search::SearchOutcome;

/// How often the scheduler looks for work it was not woken for.
///
/// A wake-up is sent when an acquisition is admitted and when a worker slot is
/// released, which covers everything this process does. The tick covers the
/// rest: a setting that raises `acquisition.max_concurrent`, and rows another
/// process - the CLI - wrote.
const SCHEDULER_TICK: Duration = Duration::from_secs(1);

/// How long a resumed acquisition waits for a worker slot before looking again.
///
/// A server that resumes more acquisitions than there are slots has to wait for
/// the ones already running, and this is the longest it sleeps between two
/// attempts.
const RESUME_SLOT_WAIT: Duration = Duration::from_millis(100);

/// The window a user's acquisition quota is counted over.
///
/// It is a rolling day: the acquisitions the user started in the 24 hours
/// before a request are what the request is judged against.
const QUOTA_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

/// A request for a track that is not on disk.
///
/// The request carries everything admission decides on: what to acquire, how
/// urgent it is, who asked, whether that caller may acquire at all, whether the
/// caller is within its daily quota, and whether a cooldown should be ignored.
/// The quota is read from `user_quotas` for `requested_by`, next to the
/// permission check, because both are decided before any search starts.
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
    /// The request was refused before anything happened.
    ///
    /// Nothing was searched, admitted or written: a denied request leaves no
    /// trace in the ledger, which is also what the quota count relies on.
    Denied {
        /// The track the request named.
        track_id: EntityId,
        /// Why the request was refused: the requester lacks the `can_acquire`
        /// permission, or it has spent its daily acquisition quota.
        reason: FailureReason,
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
    /// The user the acquisition was requested for, or `None` when the server
    /// requested it.
    ///
    /// The event stream routes the acquisition's updates by this id.
    pub requested_by: Option<Uuid>,
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

/// The storage paths the acquisition pipeline writes under.
///
/// The composition root builds this from the bootstrap path configuration:
/// `staging_root` is `paths.staging`, and `roots` holds `paths.library` and
/// `paths.cache`.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
///
/// use hearthsound_acquire::orchestrator::AcquisitionStorage;
/// use hearthsound_library::StorageRoots;
///
/// let storage = AcquisitionStorage::new(
///     PathBuf::from("/srv/storage/staging"),
///     StorageRoots::new(
///         PathBuf::from("/srv/storage/library"),
///         PathBuf::from("/srv/storage/cache"),
///     ),
/// );
/// assert_eq!(storage.staging_root, PathBuf::from("/srv/storage/staging"));
/// ```
#[derive(Debug, Clone)]
pub struct AcquisitionStorage {
    /// The root per-acquisition staging directories are created under.
    pub staging_root: PathBuf,
    /// The managed storage roots imported files land under.
    pub roots: StorageRoots,
}

impl AcquisitionStorage {
    /// Builds the storage paths.
    ///
    /// # Parameters
    ///
    /// - `staging_root`: the staging root, `paths.staging`.
    /// - `roots`: the managed storage roots, `paths.library` and `paths.cache`.
    pub fn new(staging_root: PathBuf, roots: StorageRoots) -> AcquisitionStorage {
        AcquisitionStorage {
            staging_root,
            roots,
        }
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
/// use hearthsound_acquire::orchestrator::{AcquisitionOrchestrator, AcquisitionStorage};
/// use hearthsound_catalog::CatalogService;
/// use hearthsound_config::settings::Settings;
/// use hearthsound_core::events::EventBus;
/// use hearthsound_db::Database;
/// use hearthsound_library::StorageRoots;
/// use hearthsound_media::FfmpegRunner;
/// use hearthsound_plugin_host::ProviderRegistry;
///
/// # async fn example(
/// #     database: Arc<Database>,
/// #     settings: Settings,
/// #     registry: Arc<ProviderRegistry>,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// let catalog = Arc::new(CatalogService::new(
///     Arc::clone(&registry),
///     Arc::clone(&database),
///     settings.clone(),
///     PathBuf::from("/srv/hearthsound/artwork"),
/// ));
/// let orchestrator = Arc::new(AcquisitionOrchestrator::new(
///     database,
///     settings,
///     registry,
///     catalog,
///     Arc::new(FfmpegRunner::new(None, 8)),
///     Arc::new(EventBus::new()),
///     AcquisitionStorage::new(
///         PathBuf::from("/srv/hearthsound/storage/staging"),
///         StorageRoots::new(
///             PathBuf::from("/srv/hearthsound/storage/library"),
///             PathBuf::from("/srv/hearthsound/storage/cache"),
///         ),
///     ),
///));
/// orchestrator.start();
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct AcquisitionOrchestrator {
    /// The shared state every clone holds.
    inner: Arc<Inner>,
}

/// The hardlink function the import stage runs on the blocking pool.
///
/// # Parameters
///
/// - the absolute source path.
/// - the absolute destination path.
///
/// # Returns
///
/// The operating system's result, which the import classifies as a placed name,
/// a taken name or a failure that falls back to copying.
pub(crate) type Hardlink = dyn Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync + 'static;

/// The state one orchestrator shares with its clones.
struct Inner {
    /// The database holding the ledger.
    database: Arc<Database>,
    /// The settings service, read on every decision.
    settings: Settings,
    /// The provider instances the acquisition layer walks.
    registry: Arc<ProviderRegistry>,
    /// The catalog service identity resolution goes through.
    catalog: Arc<CatalogService>,
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
    /// The managed storage roots imported files land under.
    storage_roots: StorageRoots,
    /// The function the import stage links a source file into managed storage
    /// with.
    ///
    /// It is a field rather than a direct call so a test can make a link fail
    /// and see the copy fallback; production always uses
    /// [`std::fs::hard_link`].
    hardlink: RwLock<Arc<Hardlink>>,
    /// The import destinations this process has reserved but not stored yet.
    ///
    /// The filesystem alone cannot keep two imports apart: neither has written
    /// its file when both look, so both see the same name free. A path stays
    /// reserved from the moment an import picks it until that import's
    /// transaction has committed or the import has given up, which is exactly
    /// the window in which no file is there to be seen.
    reserved_destinations: Mutex<HashSet<PathBuf>>,
    /// One channel per acquisition anyone is watching.
    waiters: Mutex<HashMap<Uuid, watch::Sender<AcquisitionProgress>>>,
    /// The gate that decides which progress snapshots reach the bus, with the
    /// latest snapshot of each live acquisition.
    announce_gate: Mutex<AnnounceGate>,
    /// The acquisitions this process still has side effects for, and the signal
    /// that wakes a worker when another process ends one of them.
    ///
    /// A row is added when the acquisition is admitted, or when a starting
    /// process resumes it, and removed when its task ends and its staging
    /// directory is gone. That makes the map exactly the set of acquisitions
    /// whose cancellation this process has to notice:
    /// [`AcquisitionOrchestrator::reconcile`] walks it after another connection
    /// writes the ledger.
    in_flight: Mutex<HashMap<Uuid, Arc<Notify>>>,
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

/// An import destination one import holds until it commits or gives up.
///
/// The guard releases the path when it is dropped, so every way out of an
/// import - the transaction committing, an error, a panic - frees the name for
/// the next import.
pub(crate) struct DestinationReservation<'a> {
    /// The orchestrator whose reservation set holds the path.
    orchestrator: &'a AcquisitionOrchestrator,
    /// The absolute destination path that was reserved.
    destination: PathBuf,
}

impl Drop for DestinationReservation<'_> {
    /// Releases the reservation.
    fn drop(&mut self) {
        lock(&self.orchestrator.inner.reserved_destinations).remove(&self.destination);
    }
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
    /// - `catalog`: the catalog service; the `resolving` stage resolves the
    ///   wanted track's recording identity through it.
    /// - `ffmpeg`: the media runner the verification stage drives.
    /// - `events`: the bus every state change and availability change is
    ///   published on.
    /// - `storage`: the storage paths the pipeline writes under: the staging
    ///   root per-acquisition directories are created in, and the managed
    ///   library and cache roots the import stage puts files under.
    pub fn new(
        database: Arc<Database>,
        settings: Settings,
        registry: Arc<ProviderRegistry>,
        catalog: Arc<CatalogService>,
        ffmpeg: Arc<FfmpegRunner>,
        events: Arc<EventBus>,
        storage: AcquisitionStorage,
    ) -> AcquisitionOrchestrator {
        let library = LibraryRepository::new(&database);
        let repository = AcquisitionRepository::new(&database);
        AcquisitionOrchestrator {
            inner: Arc::new(Inner {
                database,
                settings,
                registry,
                catalog,
                ffmpeg,
                events,
                library,
                repository,
                staging_root: storage.staging_root,
                storage_roots: storage.roots,
                hardlink: RwLock::new(Arc::new(|source: &Path, destination: &Path| {
                    std::fs::hard_link(source, destination)
                })),
                reserved_destinations: Mutex::new(HashSet::new()),
                waiters: Mutex::new(HashMap::new()),
                announce_gate: Mutex::new(AnnounceGate::default()),
                in_flight: Mutex::new(HashMap::new()),
                running: AtomicUsize::new(0),
                search_budgets: crate::budget::InstanceBudgets::new(),
                download_budgets: crate::budget::DownloadBudgets::new(),
                started: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    /// Replaces the hardlink function the import stage runs.
    ///
    /// The stage links a source from a `link`-mode import root into managed
    /// storage, and a link the filesystem refuses falls back to a tagged copy.
    /// Production never replaces the function; a test replaces it with one that
    /// fails to exercise that fallback.
    ///
    /// # Parameters
    ///
    /// - `hardlink`: the function to call on the blocking pool, given the
    ///   absolute source and destination paths.
    ///
    /// # Returns
    ///
    /// The orchestrator, so a caller can chain the call while it builds one.
    pub fn with_hardlink<F>(&self, hardlink: F) -> &AcquisitionOrchestrator
    where
        F: Fn(&Path, &Path) -> std::io::Result<()> + Send + Sync + 'static,
    {
        *write_lock(&self.inner.hardlink) = Arc::new(hardlink);
        self
    }

    /// Links a source file to a destination on the blocking pool.
    ///
    /// # Parameters
    ///
    /// - `source`: the absolute path of the file to link.
    /// - `destination`: the absolute path the new name is created at.
    ///
    /// # Returns
    ///
    /// The operating system's result, so the caller can tell a taken name from
    /// a filesystem that cannot link.
    ///
    /// # Errors
    ///
    /// Returns the error the hardlink function reported, and an error when the
    /// blocking task itself could not be joined.
    pub(crate) async fn hardlink(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        let hardlink = Arc::clone(&read_lock(&self.inner.hardlink));
        let source = source.to_path_buf();
        let destination = destination.to_path_buf();
        tokio::task::spawn_blocking(move || hardlink(&source, &destination))
            .await
            .map_err(std::io::Error::other)?
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

    /// Returns the catalog service the identity stage resolves through.
    pub fn catalog(&self) -> &Arc<CatalogService> {
        &self.inner.catalog
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

    /// Returns the managed storage roots the import stage writes under.
    pub fn storage_roots(&self) -> &StorageRoots {
        &self.inner.storage_roots
    }

    /// Reserves an import destination for the calling import.
    ///
    /// The check and the insertion happen under one lock, so two imports that
    /// render the same relative path cannot both be handed it: the loser gets
    /// `None` and walks on to the next collision suffix. The caller holds the
    /// returned guard until its transaction has committed or its import has
    /// failed.
    ///
    /// # Parameters
    ///
    /// - `destination`: the absolute destination path to reserve.
    ///
    /// # Returns
    ///
    /// `Some(reservation)` when the path was free, and `None` when another
    /// import in this process already holds it.
    pub(crate) fn try_reserve_destination(
        &self,
        destination: &Path,
    ) -> Option<DestinationReservation<'_>> {
        if !lock(&self.inner.reserved_destinations).insert(destination.to_path_buf()) {
            return None;
        }
        Some(DestinationReservation {
            orchestrator: self,
            destination: destination.to_path_buf(),
        })
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
    /// A request is refused before anything is written when the caller may not
    /// acquire, or when it has spent the daily quota its `user_quotas` row sets.
    /// Joining an acquisition that is already running is decided first and
    /// costs nothing: a caller that is already watching a job is never turned
    /// away by a quota it may have spent since.
    ///
    /// # Parameters
    ///
    /// - `request`: what to acquire and on whose behalf.
    ///
    /// # Returns
    ///
    /// The outcome: the track is already available, an existing acquisition was
    /// joined, the track is cooling down, the request was denied, or a new
    /// acquisition was admitted. The last one has created the acquisition's
    /// staging directory, marked the track `acquiring`, and published both the
    /// `acquisition.updated` and the `track.availability` event.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownTrack`] when the id has no `tracks` row,
    /// and the database, settings, quota or filesystem error that stopped
    /// admission.
    pub async fn request(&self, request: AcquireRequest) -> Result<AcquireOutcome, AcquireError> {
        if !request.can_acquire {
            tracing::debug!(
                track = %request.track_id,
                user = ?request.requested_by,
                "refused an acquire request without the permission"
            );
            return Ok(AcquireOutcome::Denied {
                track_id: request.track_id,
                reason: FailureReason::PermissionDenied,
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

        let id = new_uuid_v7();
        let mut tx = self.inner.database.writer().begin().await?;
        if let Some(active) = repo::active_for_track(&mut tx, track.id).await? {
            let priority = active.priority.min(request.priority);
            let raised = if priority != active.priority {
                repo::set_priority(&mut tx, active.id, priority).await?
            } else {
                false
            };
            tx.commit().await?;
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
                active.requested_by,
            );
            self.stop_signal(active.id);
            return Ok(AcquireOutcome::Joined(handle));
        }

        if let Some(reason) = self.quota_denial(&mut tx, &request).await? {
            tracing::debug!(
                track = %track.id,
                user = ?request.requested_by,
                reason = reason.as_str(),
                "refused an acquire request over the requester's quota"
            );
            return Ok(AcquireOutcome::Denied {
                track_id: track.id,
                reason,
            });
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

        // The directory belongs to an acquisition that is admitted here, so it
        // is created only once every earlier refusal is behind us.
        let staging = self.staging_dir(id);
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|source| AcquireError::Io {
                path: staging.clone(),
                source,
            })?;

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
            requested_by: request.requested_by,
        };
        let handle = self.attach(
            id,
            Some(track.id),
            request.priority,
            AcquisitionState::Queued,
            None,
            request.requested_by,
        );
        self.stop_signal(id);
        self.announce(&progress);
        self.publish_availability(track.id, Availability::Acquiring, None);
        self.notify_work();
        Ok(AcquireOutcome::Started(handle))
    }

    /// Reports whether a request exceeds the requester's daily quota.
    ///
    /// The window is the rolling [`QUOTA_WINDOW_MS`] before now. It counts the
    /// acquisitions the user started in that window and the bytes of the files
    /// those acquisitions produced, because a request that attaches to an
    /// acquisition someone else started writes no row of its own and so costs
    /// the caller nothing.
    ///
    /// A request with no user behind it - the server's own work - a user with no
    /// `user_quotas` row, and a user whose quotas are unlimited are never
    /// refused. Administrators are not exempt; they simply have no row unless
    /// one is written for them.
    ///
    /// # Parameters
    ///
    /// - `conn`: the write transaction admission holds, so the count and the
    ///   admission that follows it cannot race another request from the same
    ///   user.
    /// - `request`: the request to judge.
    ///
    /// # Returns
    ///
    /// [`FailureReason::QuotaExceeded`] when the request is over quota, and
    /// `None` when it may proceed.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Quota`] when the stored quota cannot be read and
    /// [`AcquireError::Database`] when the count fails.
    async fn quota_denial(
        &self,
        conn: &mut SqliteConnection,
        request: &AcquireRequest,
    ) -> Result<Option<FailureReason>, AcquireError> {
        let Some(user) = request.requested_by else {
            return Ok(None);
        };
        let quota = hearthsound_auth::quotas::read(conn, user).await?;
        if quota.is_unlimited() {
            return Ok(None);
        }

        let since = crate::now_millis() - QUOTA_WINDOW_MS;
        let (count, bytes) = repo::admitted_since(conn, user, since).await?;
        let bytes = u64::try_from(bytes).unwrap_or(0);
        let over_count = quota
            .acquisitions_per_day
            .is_some_and(|limit| count >= i64::from(limit));
        let over_bytes = quota
            .acquisition_bytes_per_day
            .is_some_and(|limit| bytes >= limit);
        if over_count || over_bytes {
            return Ok(Some(FailureReason::QuotaExceeded));
        }
        Ok(None)
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
    /// is idempotent: a second call does nothing. Before the scheduler runs, the
    /// acquisitions a previous process left unfinished are resumed from the
    /// ledger, so a server that stopped mid-download or mid-import finishes what
    /// it started instead of leaving those rows stranded. The scheduler runs
    /// until the server stops, so the task it spawns is deliberately not joined.
    pub fn start(self: &Arc<Self>) {
        if self.inner.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = this.resume().await {
                tracing::warn!(%error, "resuming the acquisitions of a previous process failed");
            }
            this.schedule().await;
        });
    }

    /// Resumes the acquisitions a previous process left unfinished.
    ///
    /// The ledger is the pipeline's only memory, so a row that is not terminal
    /// names the stage it stopped in: a `queued` row is claimed by the scheduler
    /// like any other, a `resolving` or `searching` row runs the search again, a
    /// `downloading` row continues with the instance, candidate and token it
    /// recorded, and a `verifying` or `importing` row redoes the completion
    /// checks and the import from the files it resolved. Every resumed row is
    /// put under a worker that waits for a free slot, so a restart cannot exceed
    /// `acquisition.max_concurrent` any more than a fresh start can.
    ///
    /// # Errors
    ///
    /// Returns the database error of the read that loads the rows.
    async fn resume(&self) -> Result<(), AcquireError> {
        let rows = self.inner.repository.active().await?;
        for row in rows {
            self.stop_signal(row.id);
            if row.state == AcquisitionState::Queued {
                // The scheduler claims it, in priority order, like any other
                // queued acquisition.
                continue;
            }
            tracing::info!(
                acquisition = %row.id,
                state = row.state.as_str(),
                "resuming an acquisition"
            );
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(error) = this.acquire_slot().await {
                    tracing::warn!(
                        acquisition = %row.id,
                        %error,
                        "a resumed acquisition could not take a worker slot"
                    );
                    return;
                }
                this.advance(row.id).await;
                this.release_slot();
            });
        }
        self.notify_work();
        Ok(())
    }

    /// Waits for a free worker slot.
    ///
    /// # Returns
    ///
    /// `Ok(())` once a slot is reserved, which the caller has to release.
    ///
    /// # Errors
    ///
    /// Returns the settings error when `acquisition.max_concurrent` cannot be
    /// read; no slot is taken then.
    async fn acquire_slot(&self) -> Result<(), AcquireError> {
        loop {
            if self.try_reserve_slot()? {
                return Ok(());
            }
            tokio::select! {
                () = self.inner.notify.notified() => {}
                () = tokio::time::sleep(RESUME_SLOT_WAIT) => {}
            }
        }
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
    ///
    /// Every sweep that claims nothing must leave the [`Notify`] empty: a
    /// wake-up left behind by an empty sweep makes [`Notify::notified`] return
    /// at once, and the loop then sweeps again without ever sleeping, which is
    /// how an idle server ends up spinning at a full core. Only a sweep that
    /// claimed an acquisition, and the release of a slot whose pipeline task
    /// finished, notify the loop.
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
    /// A slot this sweep reserved and then found no work for is freed without
    /// a wake-up: an empty sweep changes nothing, so waking the scheduler for it
    /// would only make the next sweep empty too. A slot is released with a
    /// wake-up only by the task that used it.
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
                self.free_slot();
                return Ok(());
            };
            if !self.inner.repository.claim(row.id).await? {
                self.free_slot();
                continue;
            }
            tracing::debug!(acquisition = %row.id, priority = row.priority.as_str(), "claimed an acquisition");
            self.stop_signal(row.id);
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
    /// history. Identity resolution is best-effort - a `play` acquisition does
    /// not wait for it - which leaves a claimed acquisition in `searching`; the
    /// search stage then selects the candidates its download stage will try.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to advance.
    async fn advance(&self, id: Uuid) {
        if let Err(error) = self.run_pipeline(id).await {
            tracing::warn!(acquisition = %id, %error, "advancing an acquisition failed");
        }
        self.finish_task(id).await;
    }

    /// Cleans up after one pipeline task ended.
    ///
    /// A task that fails, completes or cancels its acquisition has already
    /// undone admission's side effects, so this only forgets the row. A task
    /// that stops because another process cancelled the acquisition has written
    /// nothing, so the cancellation's cleanup happens here, which is what makes
    /// an external cancel remove the staging directory and free the track even
    /// when the worker noticed it late.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition whose task ended.
    async fn finish_task(&self, id: Uuid) {
        match self.inner.repository.get(id).await {
            Ok(Some(row)) if row.state == AcquisitionState::Cancelled => {
                if let Err(error) = self.finish_cancelled(&row, None).await {
                    tracing::warn!(
                        acquisition = %id,
                        %error,
                        "cleaning up a cancelled acquisition failed"
                    );
                }
            }
            Ok(Some(row)) if row.state.is_terminal() => self.forget(id),
            // The row is still active, which means the pipeline stopped on an
            // error the stages could not handle. It stays tracked so a
            // cancellation can still reach it, and a later start resumes it.
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    acquisition = %id,
                    %error,
                    "reading an acquisition after its task ended failed"
                );
            }
        }
    }

    /// Runs the stages an acquisition is ready for.
    ///
    /// The state the row is in decides where the pipeline enters: `resolving`
    /// runs identity resolution and moves on to the search, `downloading`
    /// continues the attempt the ledger recorded, and `verifying` or `importing`
    /// redoes verification and the import from the files that attempt resolved.
    /// Those last two only happen after a restart, because a process that drives
    /// an acquisition through the stages itself never leaves one there.
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
            self.resolve_identity(&row).await;
            self.inner
                .repository
                .transition(
                    id,
                    AcquisitionState::Searching,
                    Some("identity resolution is best-effort"),
                )
                .await?;
            row.state = AcquisitionState::Searching;
            self.announce(&self.progress_of_row(&row, row.priority));
        }
        if row.state == AcquisitionState::Downloading {
            match self.resume_download(&row).await? {
                DownloadOutcome::Ready | DownloadOutcome::Stopped | DownloadOutcome::Cancelled => {
                    return Ok(());
                }
                DownloadOutcome::RestartSearch => row.state = AcquisitionState::Searching,
            }
        } else if matches!(
            row.state,
            AcquisitionState::Verifying | AcquisitionState::Importing
        ) {
            return self.resume_import(&row).await;
        }
        loop {
            if row.state == AcquisitionState::Searching {
                match self.search(&row).await? {
                    SearchOutcome::Failed(_reason) => return Ok(()),
                    // The acquisition was cancelled while the search ran. The
                    // row already says so; the task ends and the cleanup
                    // happens in `finish_task`.
                    SearchOutcome::Cancelled => return Ok(()),
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
                            DownloadOutcome::Ready
                            | DownloadOutcome::Stopped
                            | DownloadOutcome::Cancelled => return Ok(()),
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
    /// This is the one terminal path every failure reason takes, whether it came
    /// out of the search, the download or the verification stage: the row is
    /// marked `failed` with the reason and the backend's own detail, the track
    /// becomes `unavailable` until the cooldown elapses, the backend call the
    /// acquisition still has in flight is cancelled, the staging directory is
    /// removed so no partial download is left behind, and the waiters and the
    /// bus are told. The cooldown is
    /// `acquisition.unavailable_cooldown_hours`, which stops a track nobody can
    /// find from making every client retry hit the backends again.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to fail.
    /// - `reason`: why it failed, one of the fixed [`FailureReason`] literals.
    /// - `detail`: the backend's own message, when the failure has one.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the acquisition was failed, and also when it had already
    /// ended, because a cancellation racing the failure must not turn into an
    /// error.
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
        if row.state.is_terminal() {
            tracing::debug!(
                acquisition = %id,
                state = row.state.as_str(),
                "an acquisition that already ended was not failed again"
            );
            self.forget(id);
            return Ok(());
        }
        let cooldown_hours = self
            .inner
            .settings
            .current()
            .get(keys::ACQUISITION_UNAVAILABLE_COOLDOWN_HOURS)?;
        let unavailable_until = crate::now_millis() + cooldown_hours.max(0) * 3_600_000;

        // The backend is stopped before the row is written, so a failed
        // acquisition cannot keep a download alive in the background.
        self.cancel_backend(&row).await;

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
            self.publish_availability(track_id, Availability::Unavailable, None);
        }
        self.release_staging(id).await;
        self.forget(id);
        tracing::info!(
            acquisition = %id,
            track = ?row.track_id,
            reason = reason.as_str(),
            unavailable_until,
            "failed an acquisition"
        );
        Ok(())
    }

    /// Cancels an acquisition.
    ///
    /// Cancellation ends the acquisition without retrying it: the state becomes
    /// `cancelled`, the backend call it holds is cancelled, its staging
    /// directory is removed, its worker slot is given back when the pipeline
    /// task ends, the track goes back to catalog-only availability unless it
    /// already has a file, and the waiters and the bus are told. Nothing is
    /// written to the track's cooldown, because a cancellation is not a failure
    /// the retry policy should remember.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition to cancel.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the acquisition is cancelled, and also when it had already
    /// ended, so cancelling twice is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when no acquisition has that
    /// id, and the database or filesystem error that stopped the cleanup.
    pub async fn cancel(&self, acquisition_id: Uuid) -> Result<(), AcquireError> {
        let Some(row) = self.inner.repository.get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        if row.state.is_terminal() {
            tracing::debug!(
                acquisition = %acquisition_id,
                state = row.state.as_str(),
                "an acquisition that already ended was not cancelled again"
            );
            self.forget(acquisition_id);
            return Ok(());
        }
        self.finish_cancelled(&row, Some("cancelled")).await
    }

    /// Starts a new attempt for a failed or cancelled acquisition.
    ///
    /// The attempt is a forced request for the same track at the same priority
    /// and with the original reason, so a track still cooling down after its
    /// failure is tried again. The request then goes through every admission
    /// check a new request does: the requester's permission, quota and the
    /// track's availability.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition to retry.
    /// - `requested_by`: the user asking for the retry, or `None` for the server.
    /// - `can_acquire`: whether that requester may acquire.
    ///
    /// # Returns
    ///
    /// The outcome of the new request; a retry that joins an acquisition
    /// already running for the track reports that acquisition.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::UnknownAcquisition`] when no row has the id,
    /// [`AcquireError::NotRetryable`] when the row is not failed or cancelled
    /// or names no track, and the database error of a failed read.
    ///
    /// # Panics
    ///
    /// Never panics.
    pub async fn retry(
        &self,
        acquisition_id: Uuid,
        requested_by: Option<Uuid>,
        can_acquire: bool,
    ) -> Result<AcquireOutcome, AcquireError> {
        let Some(row) = self.inner.repository.get(acquisition_id).await? else {
            return Err(AcquireError::UnknownAcquisition(acquisition_id));
        };
        let retryable = matches!(
            row.state,
            AcquisitionState::Failed | AcquisitionState::Cancelled
        );
        let Some(track_id) = row.track_id.filter(|_| retryable) else {
            return Err(AcquireError::NotRetryable {
                id: acquisition_id,
                state: row.state,
            });
        };
        let request = AcquireRequest::new(track_id, row.priority, row.reason)
            .with_requested_by(requested_by)
            .with_can_acquire(can_acquire)
            .forced();
        self.request(request).await
    }

    /// Brings this process in line with the ledger another process wrote.
    ///
    /// The CLI writes the database directly instead of talking to the server,
    /// so a cancellation it records has to be noticed here: every acquisition
    /// this process still holds side effects for is looked up in the ledger and,
    /// when the row is now `cancelled`, its worker is woken and its side effects
    /// are undone. A row another connection ended some other way is dropped from
    /// the tracked set, because there is nothing left to do for it.
    ///
    /// The composition root calls this after `PRAGMA data_version` changes,
    /// which is how the server notices a write it did not make.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the tracked acquisitions agree with the ledger.
    ///
    /// # Errors
    ///
    /// Returns the database error of a read that failed; a row this process
    /// cannot read stops the sweep, because it may be a cancellation that must
    /// be undone.
    pub async fn reconcile(&self) -> Result<(), AcquireError> {
        let tracked: Vec<Uuid> = lock(&self.inner.in_flight).keys().copied().collect();
        for id in tracked {
            let Some(row) = self.inner.repository.get(id).await? else {
                self.forget(id);
                continue;
            };
            if row.state == AcquisitionState::Cancelled {
                self.finish_cancelled(&row, None).await?;
            } else if row.state.is_terminal() {
                self.forget(id);
            }
        }
        Ok(())
    }

    /// Lists acquisitions for the administrative view.
    ///
    /// # Parameters
    ///
    /// - `filter`: which rows to return and how many.
    ///
    /// # Returns
    ///
    /// The matching rows, newest first, each with the title and artist of its
    /// track.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn list(&self, filter: &ListFilter) -> Result<Vec<AcquisitionSummary>, AcquireError> {
        self.inner.repository.list(filter).await
    }

    /// Lists one page of the acquisition ledger.
    ///
    /// This is the listing a client-facing view reads: whole rows, taken from
    /// one window of the ledger, optionally restricted to one requester and
    /// paged.
    ///
    /// # Parameters
    ///
    /// - `query`: which rows to return, and how many.
    ///
    /// # Returns
    ///
    /// The matching rows, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`AcquireError::Database`] when the read fails.
    pub async fn list_window(
        &self,
        query: &LedgerQuery,
    ) -> Result<Vec<AcquisitionRow>, AcquireError> {
        self.inner.repository.list_window(query).await
    }

    /// Ends an acquisition as cancelled and undoes what admission did for it.
    ///
    /// This is the one place a cancellation is applied, whether it came from
    /// [`AcquisitionOrchestrator::cancel`] or from a row another process wrote.
    /// Every step is idempotent, so the caller that noticed the cancellation and
    /// the worker task that was polling the acquisition may both run it: the
    /// ledger row is only written when it is not terminal yet, the track is only
    /// returned to `remote` while it is still `acquiring` and has no file, and a
    /// staging directory that is already gone is not an error.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition's ledger row.
    /// - `message`: why it was cancelled, when the caller knows.
    ///
    /// # Errors
    ///
    /// Returns the database or filesystem error that stopped the cleanup.
    async fn finish_cancelled(
        &self,
        row: &AcquisitionRow,
        message: Option<&str>,
    ) -> Result<(), AcquireError> {
        let id = row.id;
        // Stop the backend first: a worker may still be polling the token, and a
        // backend that is still transferring would otherwise keep writing into a
        // staging directory this call is about to remove.
        self.cancel_backend(row).await;
        self.stop_signal(id).notify_one();

        let mut restored = false;
        let mut tx = self.inner.database.writer().begin().await?;
        if !row.state.is_terminal() {
            repo::cancel(&mut tx, id, message).await?;
        }
        if let Some(track_id) = row.track_id {
            restored = self.restore_track(&mut tx, track_id).await?;
        }
        tx.commit().await?;

        let mut progress = self.progress_of_row(row, row.priority);
        progress.state = AcquisitionState::Cancelled;
        progress.failure_reason = Some(FailureReason::Cancelled);
        self.announce(&progress);
        if restored && let Some(track_id) = row.track_id {
            self.publish_availability(track_id, Availability::Remote, None);
        }
        self.release_staging(id).await;
        self.forget(id);
        tracing::info!(
            acquisition = %id,
            track = ?row.track_id,
            "cancelled an acquisition"
        );
        Ok(())
    }

    /// Returns a cancelled acquisition's track to catalog-only availability.
    ///
    /// A track an acquisition marked `acquiring` goes back to `remote`, which is
    /// what lets it be acquired again after the caller's mind changed. A track
    /// that already has a file keeps the availability that file gives it: the
    /// cancelled acquisition did not produce one, but another one may have.
    ///
    /// # Parameters
    ///
    /// - `conn`: the transaction or connection to write on.
    /// - `track_id`: the track to restore.
    ///
    /// # Returns
    ///
    /// `true` when the availability was changed, so the caller knows whether to
    /// publish an event.
    ///
    /// # Errors
    ///
    /// Returns the library error of a failed read or write.
    async fn restore_track(
        &self,
        conn: &mut SqliteConnection,
        track_id: EntityId,
    ) -> Result<bool, AcquireError> {
        let Some(track) = tracks::get_track(&mut *conn, track_id).await? else {
            return Ok(false);
        };
        if track.availability != Availability::Acquiring {
            return Ok(false);
        }
        if media_files::preferred_media_file(&mut *conn, track_id)
            .await?
            .is_some()
        {
            return Ok(false);
        }
        tracks::set_track_availability(&mut *conn, track_id, Availability::Remote, None).await?;
        Ok(true)
    }

    /// Cancels the backend call an acquisition has in flight, when it has one.
    ///
    /// The ledger only stores a download token, so a token in the row names a
    /// download to cancel; a search is cancelled by the stage that started it.
    /// A backend that no longer knows the token answers with an error, which is
    /// ignored: what matters is that the backend stops working for this
    /// acquisition.
    ///
    /// # Parameters
    ///
    /// - `row`: the acquisition whose backend call to cancel.
    async fn cancel_backend(&self, row: &AcquisitionRow) {
        let Some(token) = row.plugin_token.as_deref() else {
            return;
        };
        let Some(instance) = row.instance_id.and_then(|id| self.instance_by_id(id)) else {
            return;
        };
        let Some(provider) = instance.acquisition() else {
            return;
        };
        let context = AcquisitionContext::new(self.staging_dir(row.id));
        provider.cancel_download(&context, token).await;
        tracing::debug!(
            acquisition = %row.id,
            instance = instance.slug().as_str(),
            "cancelled the acquisition's backend download"
        );
    }

    /// Removes an acquisition's staging directory.
    ///
    /// The staging directory is the only place a download and an import write
    /// before a file reaches managed storage, so removing it is what makes an
    /// acquisition that ends without a file leave none behind. A directory that
    /// is already gone is not an error.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition whose directory to remove.
    async fn remove_staging(&self, id: Uuid) {
        let staging = self.staging_dir(id);
        if let Err(error) = tokio::fs::remove_dir_all(&staging).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %staging.display(),
                %error,
                "an acquisition's staging directory could not be removed"
            );
        }
    }

    /// Stops an acquisition's tool processes and removes its staging directory.
    ///
    /// Every acquisition-layer instance is asked to release the directory,
    /// because a process is addressed by the handle of the instance that started
    /// it, and a search can fall through several instances before one download
    /// starts. Asking all of them also covers the instance a ledger row does not
    /// name. The directory removal is the same one
    /// [`AcquisitionOrchestrator::remove_staging`] performs.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition that reached a terminal state.
    pub(crate) async fn release_staging(&self, id: Uuid) {
        let staging = self.staging_dir(id);
        for instance in self
            .inner
            .registry
            .layers(hearthsound_core::Layer::Acquisition)
        {
            if let Some(provider) = instance.acquisition() {
                provider.release_staging(&staging).await;
            }
        }
        self.remove_staging(id).await;
    }

    /// Returns the acquisition-layer instance with `id`.
    ///
    /// A ledger row names its instance by id and the registry is keyed by slug,
    /// so the layer is walked. Only the acquisition layer can serve an
    /// acquisition, which keeps the walk to the handful of instances that
    /// actually download.
    ///
    /// # Parameters
    ///
    /// - `id`: the instance id a ledger row names.
    ///
    /// # Returns
    ///
    /// The instance, or `None` when the registry no longer serves it.
    pub(crate) fn instance_by_id(&self, id: Uuid) -> Option<Arc<RegistryInstance>> {
        self.inner
            .registry
            .layers(hearthsound_core::Layer::Acquisition)
            .into_iter()
            .find(|instance| instance.id() == id)
    }

    /// Returns the signal that wakes the worker of an acquisition.
    ///
    /// The entry is created on first use and removed by
    /// [`AcquisitionOrchestrator::forget`], so the map holds exactly the
    /// acquisitions this process has side effects for.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition whose signal is wanted.
    ///
    /// # Returns
    ///
    /// The signal, created empty when this is its first use.
    pub(crate) fn stop_signal(&self, id: Uuid) -> Arc<Notify> {
        let mut in_flight = lock(&self.inner.in_flight);
        Arc::clone(
            in_flight
                .entry(id)
                .or_insert_with(|| Arc::new(Notify::new())),
        )
    }

    /// Forgets an acquisition this process no longer has side effects for.
    ///
    /// # Parameters
    ///
    /// - `id`: the acquisition to forget.
    fn forget(&self, id: Uuid) {
        lock(&self.inner.in_flight).remove(&id);
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
    /// - `requested_by`: the user the acquisition was requested for, or `None`
    ///   when the server requested it.
    fn attach(
        &self,
        id: Uuid,
        track_id: Option<EntityId>,
        priority: Priority,
        state: AcquisitionState,
        failure_reason: Option<FailureReason>,
        requested_by: Option<Uuid>,
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
            requested_by,
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

    /// Announces a snapshot: waiters always see it, the bus sees it when the
    /// announce gate admits it.
    ///
    /// A state, priority, instance, failure or file change is published at
    /// once. Any other change is published at most once per
    /// `ANNOUNCE_INTERVAL_MS` per acquisition; the held snapshot is what the
    /// next heartbeat publishes.
    ///
    /// # Parameters
    ///
    /// - `progress`: the snapshot to announce.
    pub(crate) fn announce(&self, progress: &AcquisitionProgress) {
        {
            let waiters = lock(&self.inner.waiters);
            if let Some(sender) = waiters.get(&progress.acquisition_id) {
                sender.send_replace(progress.clone());
            }
        }
        if lock(&self.inner.announce_gate).admit(progress, Instant::now()) {
            self.publish_progress(progress);
        }
    }

    /// Publishes the latest held snapshot of an acquisition, when it has one.
    ///
    /// The download loop calls this on its two-second ticker so a transfer
    /// that makes no progress between polls still produces an event. The
    /// waiters are not touched: they already hold the latest snapshot.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose held snapshot is published.
    pub(crate) fn heartbeat(&self, acquisition_id: Uuid) {
        let latest = lock(&self.inner.announce_gate).heartbeat(acquisition_id, Instant::now());
        if let Some(progress) = latest {
            self.publish_progress(&progress);
        }
    }

    /// Publishes one snapshot on the bus, to the audience its requester implies.
    ///
    /// # Parameters
    ///
    /// - `progress`: the snapshot to publish.
    fn publish_progress(&self, progress: &AcquisitionProgress) {
        let audience = match progress.requested_by {
            Some(user) => Audience::User(user),
            None => Audience::Admins,
        };
        self.inner.events.publish_for(
            audience,
            ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
                acquisition_id: EntityId::from_uuid(IdPrefix::Acquisition, progress.acquisition_id)
                    .to_string(),
                target_id: progress.target_id.clone(),
                state: progress.state,
                priority: progress.priority,
                percent: progress.percent,
                eta_secs: progress.eta_secs,
                instance: progress.instance.clone(),
                failure_reason: progress.failure_reason,
            }),
        );
    }

    /// Publishes a track's availability change.
    ///
    /// # Parameters
    ///
    /// - `track_id`: the track that changed.
    /// - `availability`: its new availability.
    /// - `quality`: the tier of the file the track is now served from, when a
    ///   file produced the change.
    pub(crate) fn publish_availability(
        &self,
        track_id: EntityId,
        availability: Availability,
        quality: Option<QualityTier>,
    ) {
        self.inner
            .events
            .publish(ServerEvent::TrackAvailability(TrackAvailability {
                track_id: track_id.to_string(),
                availability,
                quality,
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
            requested_by: row.requested_by,
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
    /// The wake-up is for a finished pipeline task: the scheduler has to look
    /// for the next queued acquisition now that a slot is free again.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when no slot was taken, which is a bookkeeping
    /// bug in this module.
    fn release_slot(&self) {
        self.free_slot();
        self.notify_work();
    }

    /// Returns a worker slot without waking the scheduler.
    ///
    /// This is the release of a sweep that reserved a slot and then found
    /// nothing to run. Nothing changed for anyone waiting, and a stored permit
    /// would make [`Notify::notified`] return immediately, so the scheduler
    /// would sweep in a loop instead of sleeping.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when no slot was taken, which is a bookkeeping
    /// bug in this module.
    fn free_slot(&self) {
        let previous = self.inner.running.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "a worker slot was released twice");
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

/// Reads a read-write lock, ignoring poisoning.
///
/// # Panics
///
/// Does not panic; the underlying lock's poisoning is ignored.
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Writes a read-write lock, ignoring poisoning.
///
/// # Panics
///
/// Does not panic; the underlying lock's poisoning is ignored.
fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use hearthsound_catalog::CatalogService;
    use hearthsound_config::SettingSources;
    use hearthsound_config::SettingsRegistry;
    use hearthsound_config::secret::SecretKey;
    use hearthsound_db::MIGRATOR;
    use hearthsound_plugin_host::{PluginHost, PluginRepository, ProviderRegistry};

    /// Builds an orchestrator over an empty ledger in `dir`.
    ///
    /// The plugin tree is empty, so no instance is configured and nothing the
    /// scheduler claims could run; these tests only exercise the sweep and the
    /// wake-ups it leaves behind.
    async fn idle_orchestrator(dir: &std::path::Path) -> Arc<AcquisitionOrchestrator> {
        let database = Arc::new(Database::open(&dir.join("hearthsound.db")).await.unwrap());
        database
            .migrate(&MIGRATOR, &dir.join("backups"))
            .await
            .unwrap();
        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
        )
        .await
        .unwrap();
        let host = Arc::new(
            PluginHost::new(
                dir.join("plugins"),
                settings.current(),
                Arc::clone(&database),
            )
            .unwrap(),
        );
        let secrets = SecretKey::load(&dir.join("secret.key"), &BTreeMap::new())
            .unwrap()
            .secret_box();
        let registry = Arc::new(ProviderRegistry::new(
            host,
            PluginRepository::new(&database),
            secrets,
        ));
        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            dir.join("artwork"),
        ));
        Arc::new(AcquisitionOrchestrator::new(
            database,
            settings,
            registry,
            catalog,
            Arc::new(FfmpegRunner::new(None, 8)),
            Arc::new(EventBus::new()),
            AcquisitionStorage::new(
                dir.join("storage/staging"),
                StorageRoots::new(dir.join("storage/library"), dir.join("storage/cache")),
            ),
        ))
    }

    /// How long a wake-up may take to arrive before a test calls it missing.
    const WAKE_TIMEOUT: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn an_empty_sweep_leaves_the_scheduler_asleep() {
        let dir = tempfile::tempdir().unwrap();
        let orchestrator = idle_orchestrator(dir.path()).await;

        orchestrator.dispatch().await.unwrap();
        assert_eq!(orchestrator.running(), 0);

        // A permit left behind here would make every `notified()` return at
        // once, so the scheduler would sweep without ever sleeping.
        assert!(
            tokio::time::timeout(WAKE_TIMEOUT, orchestrator.inner.notify.notified())
                .await
                .is_err(),
            "an empty sweep must not leave a wake-up behind"
        );
    }

    #[tokio::test]
    async fn releasing_the_slot_of_a_finished_acquisition_wakes_the_scheduler() {
        let dir = tempfile::tempdir().unwrap();
        let orchestrator = idle_orchestrator(dir.path()).await;

        assert!(orchestrator.try_reserve_slot().unwrap());
        orchestrator.release_slot();

        tokio::time::timeout(WAKE_TIMEOUT, orchestrator.inner.notify.notified())
            .await
            .expect("releasing a used slot wakes the scheduler for the next queued acquisition");
    }
}
