//! The services every `/api/v1` handler shares.

use std::path::PathBuf;
use std::sync::Arc;

use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
use hearthsound_activity::ActivityService;
use hearthsound_auth::{
    ApiKeys, AppPasswords, Authenticator, Quotas, Sessions, SetupGate, TrustedProxies,
    UserSettings, Users,
};
use hearthsound_config::secret::SecretBox;
use hearthsound_config::settings::Settings;
use hearthsound_core::events::EventBus;
use hearthsound_db::Database;
use hearthsound_jobs::JobQueue;
use hearthsound_library::folders::ServerPaths;
use hearthsound_library::maintenance::StorageMaintainer;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_library::upload::UploadIngest;
use hearthsound_library::watch::LibraryWatcher;
use hearthsound_plugin_host::{
    InstanceService, PluginAdmin, PluginHost, PluginInstaller, PluginRepository, ProviderRegistry,
    UpdateCheckJobHandler, UpdateChecker, UserConfigService,
};

use crate::tickets::Tickets;

/// The long-lived services the composition root hands the native API.
///
/// Every field is the same instance the rest of the process uses, so a change
/// made through `/api/v1` is the change every other surface sees.
pub struct NativeServices {
    /// The open database.
    pub database: Arc<Database>,
    /// The web session table, shared with the `/rest` surface so a session a
    /// client signed in with there is the one this surface resolves.
    pub sessions: Sessions,
    /// The one-time token the first-run setup link carries, shared with the
    /// composition root that logs the URL for it.
    pub setup: Arc<SetupGate>,
    /// `server.base_url` as the administrator configured it, or the empty string
    /// when it is unset. A browser link flow starts from this address when it is
    /// set, so the service sends the listener back to the address the operator
    /// published.
    pub base_url: String,
    /// Whether `server.base_url` names an `https://` address, computed once at
    /// startup. The session cookie carries `Secure` when this is set or when the
    /// request came through a trusted proxy over HTTPS.
    pub base_url_is_https: bool,
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
    /// The activity service: the repositories the caller's own playlists and
    /// play history are read and written through.
    pub activity: Arc<ActivityService>,
    /// The storage maintainer.
    pub maintainer: Arc<StorageMaintainer>,
    /// The upload ingest.
    pub uploads: UploadIngest,
    /// `<storage>/uploads-staging`, where an upload is written before ingest.
    pub uploads_staging: PathBuf,
    /// The server's own files and directories, which a library folder may not
    /// be, contain, or sit inside.
    pub paths: ServerPaths,
    /// The library scanner.
    pub scanner: Arc<LibraryScanner>,
    /// The job queue scans are enqueued on.
    pub jobs: Arc<JobQueue>,
    /// The folder watcher.
    pub watcher: Arc<LibraryWatcher>,
    /// The server event bus.
    pub events: Arc<EventBus>,
    /// The wasmtime plugin host, which owns the plugins directory and the
    /// files of every installed plugin.
    pub plugins: Arc<PluginHost>,
    /// The write side of the plugin instance and layer tables.
    pub instances: InstanceService,
    /// The administrative plugin lifecycle: install, apply a staged upgrade,
    /// switch, remove.
    pub admin: Arc<PluginAdmin>,
    /// The installer that resolves plugin indexes and repository URLs.
    pub installer: Arc<PluginInstaller>,
    /// The checker the admin views read pending plugin updates from.
    pub update_checker: Arc<UpdateChecker>,
    /// The update-check job, which holds what the last completed check found.
    pub update_job: Arc<UpdateCheckJobHandler>,
    /// The plugin tables: the installed rows, their staged upgrades, and the
    /// repositories an administrator registered.
    pub plugin_repository: PluginRepository,
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
    /// The per-user provider configuration table.
    user_configs: UserConfigService,
    /// The browser link flows waiting for a service's callback.
    link_states: crate::routes::me_providers::LinkStates,
    /// The event-stream ticket store.
    tickets: Tickets,
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
                sessions: services.sessions.clone(),
                user_settings: UserSettings::new(&database, services.settings.registry().clone()),
                quotas: Quotas::new(&database),
                api_keys: ApiKeys::new(&database),
                app_passwords: AppPasswords::new(&database, services.secrets.clone()),
                user_configs: UserConfigService::new(&database, services.secrets.clone()),
                link_states: crate::routes::me_providers::LinkStates::new(),
                tickets: Tickets::default(),
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

    /// Returns the server's own files and directories.
    ///
    /// The folder routes check a new path against them, so a folder can never
    /// be the data directory, contain it, or sit inside it.
    pub fn paths(&self) -> &ServerPaths {
        &self.inner.services.paths
    }

    /// Returns the settings service.
    pub fn settings(&self) -> &Settings {
        &self.inner.services.settings
    }

    /// Returns the activity service.
    ///
    /// The playlist routes read and write the caller's own playlists through
    /// it, so a playlist edited here is the one `/rest/getPlaylist` answers.
    pub fn activity(&self) -> &ActivityService {
        &self.inner.services.activity
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

    /// Returns the first-run setup gate.
    pub fn setup(&self) -> &Arc<SetupGate> {
        &self.inner.services.setup
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

    /// Returns the per-user provider configuration table.
    ///
    /// The provider routes read and write a listener's link through it, so a
    /// link written here is the one the scrobble outbox and the discovery
    /// surfaces call the plugin with.
    pub fn user_configs(&self) -> &UserConfigService {
        &self.inner.user_configs
    }

    /// Returns the browser link flows waiting for a service's callback.
    pub(crate) fn link_states(&self) -> &crate::routes::me_providers::LinkStates {
        &self.inner.link_states
    }

    /// Returns the store of outstanding event-stream tickets.
    pub(crate) fn tickets(&self) -> &Tickets {
        &self.inner.tickets
    }
}
