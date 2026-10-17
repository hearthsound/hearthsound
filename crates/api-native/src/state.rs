//! The services every `/api/v1` handler shares.

use std::path::PathBuf;
use std::sync::Arc;

use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_auth::{
    ApiKeys, AppPasswords, Authenticator, Quotas, Sessions, TrustedProxies, UserSettings, Users,
};
use hearthsound_config::secret::SecretBox;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::EventBus;
use hearthsound_db::Database;
use hearthsound_jobs::JobQueue;
use hearthsound_library::maintenance::StorageMaintainer;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_library::upload::UploadIngest;
use hearthsound_library::watch::LibraryWatcher;
use hearthsound_plugin_host::ProviderRegistry;

/// The long-lived services the composition root hands the native API.
///
/// Every field is the same instance the rest of the process uses, so a change
/// made through `/api/v1` is the change every other surface sees.
pub struct NativeServices {
    /// The open database.
    pub database: Arc<Database>,
    /// The runtime settings service.
    pub settings: Settings,
    /// The credential resolver shared with `/rest`, so one login rate limiter
    /// covers both surfaces.
    pub authenticator: Arc<Authenticator>,
    /// The secret box app passwords are sealed with.
    pub secrets: SecretBox,
    /// The proxies whose `X-Forwarded-For` header names the client.
    pub trusted_proxies: TrustedProxies,
    /// The provider registry, for the layer summary.
    pub registry: Arc<ProviderRegistry>,
    /// The acquisition orchestrator.
    pub orchestrator: Arc<AcquisitionOrchestrator>,
    /// The storage maintainer.
    pub maintainer: Arc<StorageMaintainer>,
    /// The upload ingest.
    pub uploads: UploadIngest,
    /// `<storage>/uploads-staging`, where an upload is written before ingest.
    pub uploads_staging: PathBuf,
    /// The library scanner.
    pub scanner: Arc<LibraryScanner>,
    /// The job queue scans are enqueued on.
    pub jobs: Arc<JobQueue>,
    /// The folder watcher.
    pub watcher: Arc<LibraryWatcher>,
    /// The server event bus.
    pub events: Arc<EventBus>,
}

/// The shared state of the native router.
///
/// Cloning is cheap: every clone shares the same services.
#[derive(Clone)]
pub struct NativeState {
    /// The services this state shares with its clones.
    inner: Arc<Inner>,
}

/// What one [`NativeState`] holds.
struct Inner {
    /// The services the composition root built.
    services: NativeServices,
    /// The web session table.
    sessions: Sessions,
    /// The per-user setting overrides.
    user_settings: UserSettings,
    /// The per-user quotas.
    quotas: Quotas,
    /// The API key table.
    api_keys: ApiKeys,
    /// The app password table.
    app_passwords: AppPasswords,
}

impl std::fmt::Debug for NativeState {
    /// Reports the type without its services.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeState")
    }
}

impl NativeState {
    /// Builds the state over the composition root's services.
    ///
    /// # Parameters
    ///
    /// - `services`: the shared services.
    ///
    /// # Returns
    ///
    /// The state.
    pub fn new(services: NativeServices) -> NativeState {
        let database = Arc::clone(&services.database);
        NativeState {
            inner: Arc::new(Inner {
                sessions: Sessions::new(&database),
                user_settings: UserSettings::new(&database, services.settings.registry().clone()),
                quotas: Quotas::new(&database),
                api_keys: ApiKeys::new(&database),
                app_passwords: AppPasswords::new(&database, services.secrets.clone()),
                services,
            }),
        }
    }

    /// Returns the composition root's services.
    pub fn services(&self) -> &NativeServices {
        &self.inner.services
    }

    /// Returns the open database.
    pub fn database(&self) -> &Database {
        &self.inner.services.database
    }

    /// Returns the settings service.
    pub fn settings(&self) -> &Settings {
        &self.inner.services.settings
    }

    /// Returns the credential resolver.
    pub fn authenticator(&self) -> &Authenticator {
        &self.inner.services.authenticator
    }

    /// Returns the accounts table.
    pub fn users(&self) -> &Users {
        self.inner.services.authenticator.users()
    }

    /// Returns the web session table.
    pub fn sessions(&self) -> &Sessions {
        &self.inner.sessions
    }

    /// Returns the per-user setting overrides.
    pub fn user_settings(&self) -> &UserSettings {
        &self.inner.user_settings
    }

    /// Returns the per-user quotas.
    pub fn quotas(&self) -> &Quotas {
        &self.inner.quotas
    }

    /// Returns the API key table.
    pub fn api_keys(&self) -> &ApiKeys {
        &self.inner.api_keys
    }

    /// Returns the app password table.
    pub fn app_passwords(&self) -> &AppPasswords {
        &self.inner.app_passwords
    }
}
