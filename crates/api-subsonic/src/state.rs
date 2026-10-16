//! The services every `/rest` endpoint shares.
//!
//! [`SubsonicState`] is the router's state: the database later endpoints query,
//! the settings service a response reads its tunables from, the credential
//! resolver, and the proxy list the client address is derived with. It is a
//! cloneable handle over one shared set of services, and reads the current
//! settings snapshot once per call so a change takes effect without a restart.

use std::net::IpAddr;
use std::sync::Arc;

use hearthsound_acquire::prefetch::PrefetchScheduler;
use hearthsound_activity::ActivityService;
use hearthsound_auth::{Authenticator, Principal, ResolveError, SubsonicCredentials, UserSettings};
use hearthsound_catalog::CatalogService;
use hearthsound_config::SettingsStore;
use hearthsound_config::settings::Settings;
use hearthsound_db::Database;
use hearthsound_library::StorageRoots;
use hearthsound_media::FfmpegRunner;

use crate::auth::TrustedProxies;
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
#[derive(Debug)]
struct Inner {
    /// The open database, for the queries later endpoints run.
    database: Arc<Database>,
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
    /// The proxies whose forwarding headers are trusted.
    trusted_proxies: Arc<TrustedProxies>,
}

impl SubsonicState {
    /// Builds the shared state.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
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
    pub fn new(
        database: Arc<Database>,
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
            Arc::clone(&catalog),
            roots,
            Arc::clone(&ffmpeg),
            activity.sessions().clone(),
            None,
        );
        SubsonicState {
            inner: Arc::new(Inner {
                database,
                settings,
                user_settings,
                catalog,
                activity,
                stream,
                ffmpeg,
                authenticator,
                prefetch: None,
                trusted_proxies: Arc::new(TrustedProxies::default()),
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
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                trusted_proxies: Arc::new(trusted_proxies),
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
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone().with_pending_source(pending),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: self.inner.prefetch.clone(),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
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
                settings: self.inner.settings.clone(),
                user_settings: self.inner.user_settings.clone(),
                catalog: Arc::clone(&self.inner.catalog),
                activity: Arc::clone(&self.inner.activity),
                stream: self.inner.stream.clone(),
                ffmpeg: Arc::clone(&self.inner.ffmpeg),
                authenticator: Arc::clone(&self.inner.authenticator),
                prefetch: Some(prefetch),
                trusted_proxies: Arc::clone(&self.inner.trusted_proxies),
            }),
        }
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

    /// Returns the proxies whose forwarding headers are trusted.
    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.inner.trusted_proxies
    }

    /// Returns the catalog layer.
    ///
    /// `search3` searches it for catalog results and the browse endpoints
    /// resolve an unmaterialized `x_` id through it.
    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
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
