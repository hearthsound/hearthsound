//! The services every `/rest` endpoint shares.
//!
//! [`SubsonicState`] is the router's state: the database later endpoints query,
//! the settings service a response reads its tunables from, the credential
//! resolver, and the proxy list the client address is derived with. It is a
//! cloneable handle over one shared set of services, and reads the current
//! settings snapshot once per call so a change takes effect without a restart.

use std::net::IpAddr;
use std::sync::Arc;

use hearthsound_auth::{Authenticator, Principal, ResolveError, SubsonicCredentials, UserSettings};
use hearthsound_config::SettingsStore;
use hearthsound_config::settings::Settings;
use hearthsound_db::Database;

use crate::auth::TrustedProxies;
use crate::response::SubsonicError;

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
    /// The credential resolver.
    authenticator: Arc<Authenticator>,
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
    ///
    /// # Returns
    ///
    /// The state, which trusts no proxy until
    /// [`with_trusted_proxies`](SubsonicState::with_trusted_proxies) says
    /// otherwise.
    pub fn new(
        database: Arc<Database>,
        settings: Settings,
        authenticator: Arc<Authenticator>,
    ) -> SubsonicState {
        let user_settings = UserSettings::new(&database, settings.registry().clone());
        SubsonicState {
            inner: Arc::new(Inner {
                database,
                settings,
                user_settings,
                authenticator,
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
                authenticator: Arc::clone(&self.inner.authenticator),
                trusted_proxies: Arc::new(trusted_proxies),
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

    /// Returns the credential resolver.
    pub fn authenticator(&self) -> &Authenticator {
        &self.inner.authenticator
    }

    /// Returns the proxies whose forwarding headers are trusted.
    pub fn trusted_proxies(&self) -> &TrustedProxies {
        &self.inner.trusted_proxies
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
