//! The services every `/rest` endpoint shares.
//!
//! [`SubsonicState`] is the router's state: the database later endpoints query,
//! the settings service a response reads its tunables from, the credential
//! resolver, and the proxy list the client address is derived with. It is a
//! cloneable handle over one shared set of services, and reads the current
//! settings snapshot once per call so a change takes effect without a restart.

use std::net::IpAddr;
use std::sync::Arc;

use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_acquire::prefetch::PrefetchScheduler;
use hearthsound_activity::ActivityService;
use hearthsound_auth::{
    Authenticator, Principal, ResolveError, Sessions, SubsonicCredentials, TrustedProxies,
    UserSettings,
};
use hearthsound_catalog::{CatalogService, DiscoveryService};
use hearthsound_config::SettingsStore;
use hearthsound_config::settings::Settings;
use hearthsound_db::Database;
use hearthsound_jobs::JobQueue;
use hearthsound_library::StorageRoots;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::ProviderRegistry;

use crate::response::SubsonicError;
use crate::stream::{PendingSource, StreamService};

/// The services every `/rest` endpoint shares.
///
/// Cloning is cheap: every clone shares the same services.
#[derive(Debug, Clone)]
pub struct SubsonicState {
    /// The services this state shares with its clones.
    inner: Arc<Inner>,
}

/// The services one [`SubsonicState`] holds.
struct Inner {
    /// The open database, for the queries later endpoints run.
    database: Arc<Database>,
    /// The web session table a cookie-authenticated call resolves through.
    sessions: Sessions,
    /// The settings service, which resolves the tunables of a response.
    settings: Settings,
    /// The per-user setting overrides browse scope resolution reads.
    user_settings: UserSettings,
    /// The catalog layer, which search and the catalog-id browse endpoints use.
    catalog: Arc<CatalogService>,
    /// The activity service, which `scrobble` records plays through.
    activity: Arc<ActivityService>,
    /// The stream service, which `/rest/stream` serves bytes through.
    stream: StreamService,
    /// The ffmpeg process runner the transcode path uses.
    ffmpeg: Arc<FfmpegRunner>,
    /// The credential resolver.
    authenticator: Arc<Authenticator>,
    /// The prefetch scheduler, when the composition root has one.
    prefetch: Option<Arc<PrefetchScheduler>>,
    /// The acquisition layer, when the composition root has one.
    acquisitions: Option<Arc<AcquisitionOrchestrator>>,
    /// The provider registry, when the composition root has one.
    registry: Option<Arc<ProviderRegistry>>,
    /// The proxies whose forwarding headers are trusted.
    trusted_proxies: Arc<TrustedProxies>,
    /// The library scanner, when the composition root has one.
    scanner: Option<Arc<LibraryScanner>>,
    /// The job queue, when the composition root has one.
    jobs: Option<Arc<JobQueue>>,
    /// The discovery layer, when the composition root has one.
    ///
    /// The similar-songs and artist-information endpoints ask it for
    /// suggestions. Without one they answer the same empty or local-only shapes
    /// a server with no discovery instance answers.
    discovery: Option<Arc<DiscoveryService>>,
}

impl std::fmt::Debug for Inner {
    /// Writes the services, naming the two that do not implement [`Debug`]
    /// themselves by whether the state holds one.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("database", &self.database)
            .field("settings", &self.settings)
            .field("user_settings", &self.user_settings)
            .field("catalog", &"CatalogService")
            .field("activity", &self.activity)
            .field("stream", &self.stream)
            .field("ffmpeg", &self.ffmpeg)
            .field("authenticator", &self.authenticator)
            .field("prefetch", &self.prefetch)
            .field(
                "acquisitions",
                &self
                    .acquisitions
                    .as_ref()
                    .map(|_| "AcquisitionOrchestrator"),
            )
            .field(
                "registry",
                &self.registry.as_ref().map(|_| "ProviderRegistry"),
            )
            .field("trusted_proxies", &self.trusted_proxies)
            .field("scanner", &self.scanner)
            .field("jobs", &self.jobs)
            .field(
                "discovery",
                &self.discovery.as_ref().map(|_| "DiscoveryService"),
            )
            .finish()
    }
}

impl SubsonicState {
    /// Builds the shared state.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `sessions`: the web session table, shared with the native API so a
    ///   session one surface issued authenticates on the other.
    /// - `settings`: the settings service; a call reads its current snapshot.
    /// - `authenticator`: the credential resolver, shared with the other
    ///   surfaces so one login rate limiter covers them all.
    /// - `catalog`: the catalog layer, shared with the composition root that
    ///   built it over the same provider registry.
    /// - `activity`: the activity service, shared with the stream service so
    ///   the "no song audio delivered" guard reads the same session registry.
    /// - `ffmpeg`: the ffmpeg process runner the stream service transcodes
    ///   through, shared with the composition root so one concurrency cap
    ///   covers every transcode the process runs.
    /// - `roots`: the managed storage roots a media file's path resolves
    ///   against.
    ///
    /// # Returns
    ///
    /// The state, which trusts no proxy until
    /// [`with_trusted_proxies`](SubsonicState::with_trusted_proxies) says
    /// otherwise and has no acquisition layer until
    /// [`with_pending_source`](SubsonicState::with_pending_source) adds one.
    // The constructor takes one argument per service the surface shares, which
    // is what keeps every one of them explicit at the call site.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: Arc<Database>,
        sessions: Sessions,
        settings: Settings,
        authenticator: Arc<Authenticator>,
        catalog: Arc<CatalogService>,
        activity: Arc<ActivityService>,
        ffmpeg: Arc<FfmpegRunner>,
        roots: StorageRoots,
    ) -> SubsonicState {
        let user_settings = UserSettings::new(&database, settings.registry().clone());
        let stream = StreamService::new(
            Arc::clone(&database),
            settings.clone(),
            roots,
            Arc::clone(&ffmpeg),
            activity.sessions().clone(),
            None,
        );
        SubsonicState {
            inner: Arc::new(Inner {
                database,
                sessions,
                settings,
                user_settings,
                catalog,
                activity,
                stream,
                ffmpeg,
                authenticator,
                prefetch: None,
                acquisitions: None,
                registry: None,
                trusted_proxies: Arc::new(TrustedProxies::default()),
                scanner: None,
                jobs: None,
                discovery: None,
            }),
        }
    }

    /// Sets the proxies whose `X-Forwarded-For` header is trusted.
    ///
    /// # Parameters
    ///
    /// - `trusted_proxies`: the addresses and CIDR blocks from the bootstrap
    ///   configuration's `server.trusted_proxies`. An empty list, the default,
    ///   trusts no forwarding header and uses the peer address.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    pub fn with_trusted_proxies(self, trusted_proxies: TrustedProxies) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: self.inner.acquisitions.clone(),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::new(trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Sets the acquisition layer pending streams are answered from.
    ///
    /// # Parameters
    ///
    /// - `pending`: the acquisition layer, which must be the same orchestrator
    ///   the composition root started, so a stream's request and the server's
    ///   worker slots are one pipeline.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    pub fn with_pending_source(self, pending: Arc<dyn PendingSource>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone().with_pending_source(pending),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: self.inner.acquisitions.clone(),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Sets the prefetch scheduler the listing endpoints feed.
    ///
    /// # Parameters
    ///
    /// - `prefetch`: the scheduler, which must be the same one the composition
    ///   root built over the running acquisition layer, so the look-ahead it
    ///   starts and the acquisitions the rest of the server watches are one
    ///   ledger.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    /// Without a scheduler, `getAlbum` records no listing and `stream` notes no
    /// streamed track, which is what a surface that serves only stored files
    /// wants.
    pub fn with_prefetch(self, prefetch: Arc<PrefetchScheduler>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: Some(prefetch),
                acquisitions: self.inner.acquisitions.clone(),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Sets the acquisition layer the vendor endpoints answer from.
    ///
    /// # Parameters
    ///
    /// - `acquisitions`: the orchestrator, which must be the same one the
    ///   composition root started, so an acquisition a client asks for over the
    ///   vendor extension is the one the server's workers run.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    /// Without an acquisition layer, `hsGetAvailability` reports the availability
    /// the library stores without the acquisition fields, and the endpoints that
    /// admit, list or cancel an acquisition answer a generic error.
    pub fn with_acquisitions(self, acquisitions: Arc<AcquisitionOrchestrator>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: Some(acquisitions),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Sets the provider registry the vendor endpoints read the layers from.
    ///
    /// # Parameters
    ///
    /// - `registry`: the registry, which must be the one the composition root
    ///   built over the plugin tables, so what a response reports about the
    ///   enabled layers is what the server is running.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    pub fn with_registry(self, registry: Arc<ProviderRegistry>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: self.inner.acquisitions.clone(),
                registry: Some(registry),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Sets the discovery layer the suggestion endpoints ask.
    ///
    /// # Parameters
    ///
    /// - `discovery`: the service built over the same provider registry and
    ///   catalog layer, so a suggestion it returns resolves through the catalog
    ///   the rest of the surface serves.
    ///
    /// # Returns
    ///
    /// The state, sharing everything else with the state it was called on.
    pub fn with_discovery(self, discovery: Arc<DiscoveryService>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: self.inner.acquisitions.clone(),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: self.inner.scanner.clone(),
                jobs: self.inner.jobs.clone(),
                discovery: Some(discovery),
            }),
        }
    }

    /// Returns the web session table.
    ///
    /// `/rest` resolves the `hs_session` cookie through it, so a web client
    /// signed in through `/api/v1/auth/login` is authenticated on both
    /// surfaces by the same row.
    pub fn sessions(&self) -> &Sessions {
        &self.inner.sessions
    }

    /// Returns the open database.
    pub fn database(&self) -> &Database {
        self.inner.database.as_ref()
    }

    /// Returns the settings service.
    ///
    /// A handler that needs a tunable reads the current snapshot with
    /// [`settings_snapshot`](SubsonicState::settings_snapshot), or subscribes
    /// with [`Settings::subscribe`] when it wants to follow changes.
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Returns the settings in effect right now.
    ///
    /// # Returns
    ///
    /// An immutable snapshot, shared with every caller that read it since the
    /// last change.
    pub fn settings_snapshot(&self) -> Arc<SettingsStore> {
        self.inner.settings.current()
    }

    /// Returns the per-user setting overrides.
    ///
    /// A browse request reads the user's `subsonic.default_scope` override
    /// through this repository, falling back to the server-wide value of that
    /// setting when the user has none.
    pub fn user_settings(&self) -> &UserSettings {
        &self.inner.user_settings
    }

    /// Returns the accounts table.
    ///
    /// # Returns
    ///
    /// The users repository, shared with the credential resolver.
    pub fn users(&self) -> &hearthsound_auth::Users {
        self.inner.authenticator.users()
    }

    /// Returns the credential resolver.
    ///
    /// # Returns
    ///
    /// The resolver every `/rest` call authenticates through.
    pub fn authenticator(&self) -> &Authenticator {
        &self.inner.authenticator
    }

    /// Adds the library scanner and the job queue.
    ///
    /// The `/rest` surface serves `getScanStatus` and `startScan` through them:
    /// the scanner reports what it is doing, and a scan a client starts is
    /// enqueued on the same queue the watcher and the command line use.
    ///
    /// # Parameters
    ///
    /// - `scanner`: the library scanner, shared with the composition root.
    /// - `jobs`: the job queue the scan is enqueued on.
    ///
    /// # Returns
    ///
    /// The state, with the scanner and the queue.
    pub fn with_scanner(self, scanner: Arc<LibraryScanner>, jobs: Arc<JobQueue>) -> SubsonicState {
        SubsonicState {
            inner: Arc::new(Inner {
                database: Arc::clone(&self.inner.database),
                sessions: self.inner.sessions.clone(),
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                acquisitions: self.inner.acquisitions.clone(),
                registry: self.inner.registry.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
                scanner: Some(scanner),
                jobs: Some(jobs),
                discovery: self.inner.discovery.clone(),
            }),
        }
    }

    /// Returns the library scanner, when the composition root added one.
    ///
    /// # Returns
    ///
    /// The scanner, or `None` in a process that does not scan.
    pub fn scanner(&self) -> Option<&Arc<LibraryScanner>> {
        self.inner.scanner.as_ref()
    }

    /// Returns the job queue, when the composition root added one.
    ///
    /// # Returns
    ///
    /// The queue, or `None` in a process that does not run jobs.
    pub fn jobs(&self) -> Option<&Arc<JobQueue>> {
        self.inner.jobs.as_ref()
    }

    /// Returns the proxies whose forwarding headers are trusted.
    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.inner.trusted_proxies
    }

    /// Returns the catalog layer.
    ///
    /// `search3` searches it, the browse endpoints compose their pages from its
    /// views, `getCoverArt` serves its images, and an id a client sends resolves
    /// through its redirects.
    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
    }

    /// Returns the discovery layer, when the composition root has one.
    ///
    /// `getSimilarSongs` asks it for suggestions. `getArtistInfo` and
    /// `getTopSongs` answer from the stored similar artists and top tracks, so a
    /// server with no discovery instance still shows what the indexer stored.
    pub fn discovery(&self) -> Option<&DiscoveryService> {
        self.inner.discovery.as_deref()
    }

    /// Returns the activity service.
    ///
    /// `scrobble` records a play through it, so the writes share the session
    /// registry the stream service reports into.
    pub fn activity(&self) -> &ActivityService {
        &self.inner.activity
    }

    /// Returns the prefetch scheduler, when the composition root built one.
    ///
    /// `getAlbum` records the listing it answered as the client's implied
    /// queue, and `stream` notes the track it served, so the scheduler can
    /// prefetch what the client is about to reach.
    ///
    /// # Returns
    ///
    /// The scheduler, or `None` for a state built without one, which records
    /// and notes nothing.
    pub fn prefetch(&self) -> Option<&Arc<PrefetchScheduler>> {
        self.inner.prefetch.as_ref()
    }

    /// Returns the acquisition layer, when the composition root built one.
    ///
    /// The vendor acquisition endpoints read an acquisition's progress and its
    /// ledger rows through it, so what a client is told about a job is what the
    /// server's workers are doing.
    ///
    /// # Returns
    ///
    /// The orchestrator, or `None` for a state built without one.
    pub fn acquisitions(&self) -> Option<&Arc<AcquisitionOrchestrator>> {
        self.inner.acquisitions.as_ref()
    }

    /// Returns the provider registry, when the composition root built one.
    ///
    /// The vendor endpoints report which provider layers the server serves
    /// through it.
    ///
    /// # Returns
    ///
    /// The registry, or `None` for a state built without one.
    pub fn registry(&self) -> Option<&Arc<ProviderRegistry>> {
        self.inner.registry.as_ref()
    }

    /// Returns the stream service.
    ///
    /// `/rest/stream` serves the track's bytes through it, so the file lookup,
    /// the range and validator handling, and the stream session all live behind
    /// one service.
    pub fn stream(&self) -> &StreamService {
        &self.inner.stream
    }

    /// Returns the ffmpeg process runner.
    ///
    /// The stream service transcodes through it, and the composition root built
    /// it with the `stream.max_concurrent_transcodes` cap.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.inner.ffmpeg
    }

    /// Returns the managed storage roots.
    ///
    /// A media file that is not inside a library folder resolves against one of
    /// them, which is how the stream and artwork paths turn a stored relative
    /// path into an absolute one.
    pub fn storage_roots(&self) -> &StorageRoots {
        self.inner.stream.roots()
    }

    /// Resolves a request's credentials into a principal.
    ///
    /// # Parameters
    ///
    /// - `client_ip`: the address the request came from, which keys the login
    ///   rate limit.
    /// - `credentials`: the credential parameters of the request.
    ///
    /// # Returns
    ///
    /// The authenticated principal.
    ///
    /// # Errors
    ///
    /// Returns the [`SubsonicError`] to render when the credentials are absent,
    /// contradictory, or wrong, and a generic error when the check itself could
    /// not be completed, which is a server-side failure rather than a
    /// credential error and is logged with its cause.
    pub async fn authenticate(
        &self,
        client_ip: IpAddr,
        credentials: &SubsonicCredentials,
    ) -> Result<Principal, SubsonicError> {
        let settings = self.settings_snapshot();
        match self
            .inner
            .authenticator
            .resolve(&settings, client_ip, credentials)
            .await
        {
            Ok(principal) => Ok(principal),
            Err(ResolveError::Rejected(error)) => {
                tracing::debug!(
                    code = error.code(),
                    "rejected the credentials of a /rest call"
                );
                Err(SubsonicError::authentication(&error))
            }
            Err(ResolveError::Internal(error)) => {
                tracing::error!(error = %error, "could not resolve the credentials of a /rest call");
                Err(SubsonicError::generic())
            }
        }
    }
}
