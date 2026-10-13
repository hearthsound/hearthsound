//! Every service the process runs, built once from the bootstrap configuration.
//!
//! [`Services::build`] performs the startup sequence in the order the services
//! depend on each other:
//!
//! 1. create the data and storage directory trees ([`crate::paths`]);
//! 2. open the database and apply pending migrations, after a `VACUUM INTO`
//!    backup when the database already holds data;
//! 3. load the runtime settings, resolved as environment, config file,
//!    database, then default;
//! 4. load the 32-byte secret key, creating it on first start;
//! 5. build the event bus, the authenticator (with the login rate limit from
//!    `auth.login_rate_limit_per_minute`), the plugin host, the provider
//!    registry (rebuilt from the plugin tables), and the catalog layer over
//!    that registry;
//! 6. build the ffmpeg runner capped by `stream.max_concurrent_transcodes`, the
//!    acquisition orchestrator over the catalog, registry and storage, and start
//!    its scheduler, then the managed storage roots, the library scanner, the
//!    job queue, the job runner, and the cron scheduler;
//! 7. build the activity service and the trusted-proxy list.
//!
//! Every service is shared through an `Arc`, so a handler, a job, or an
//! endpoint clones the handle it needs and holds nothing else.
//!
//! # The plugin host holds a settings snapshot
//!
//! [`PluginHost::new`] takes the resolved settings, which it uses to validate
//! manifests at install and load time. It keeps the snapshot it was built with:
//! a change to a plugin resource setting takes effect for newly loaded
//! components, and a process restart picks it up for everything else.
//!
//! # Where later wiring goes
//!
//! The acquisition job handlers and schedules, and the remaining API surfaces,
//! are constructed in [`Services::build`] next to the services they belong to,
//! and their fields are added to [`Services`] with accessors next to the
//! existing ones. Nothing else needs to change: the domain crates take the
//! handles they are given, the API crates read them through
//! [`SubsonicState`](hearthsound_api_subsonic::SubsonicState), and
//! [`crate::Server`] already starts whatever background work the services
//! expose.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use hearthsound_acquire::orchestrator::{AcquisitionOrchestrator, AcquisitionStorage};
use hearthsound_acquire::prefetch::PrefetchScheduler;
use hearthsound_activity::ActivityService;
use hearthsound_api_subsonic::{PendingSource, SubsonicState, TrustedProxies};
use hearthsound_auth::Authenticator;
use hearthsound_catalog::CatalogService;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::keys;
use hearthsound_config::secret::SecretKey;
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{
    JobQueue, JobRunner, JobRunnerOptions, QueueOptions, ScheduleSpec, Scheduler, SchedulerOptions,
};
use hearthsound_library::StorageRoots;
use hearthsound_library::scan::{LibraryScanner, SCAN_JOB_KIND, ScanJobHandler};
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::instances::InstanceService;
use hearthsound_plugin_host::{PluginHost, PluginRepository, ProviderRegistry};

use crate::error::ServerError;
use crate::jobs::{KvSweepJobHandler, PLUGIN_KV_SWEEP_CRON, PLUGIN_KV_SWEEP_JOB_KIND};

/// Every long-lived service of one server process.
///
/// A `Services` value is built once, wrapped in an `Arc`, and handed to the
/// HTTP routers, the job runner, and the background workers. Cloning the `Arc`
/// is how a caller gets another handle; there is no per-caller state.
pub struct Services {
    /// The bootstrap configuration the process was started with.
    config: BootstrapConfig,
    /// The open database, with its writer and reader pools.
    database: Arc<Database>,
    /// The runtime settings service.
    settings: Settings,
    /// The server event bus.
    events: Arc<EventBus>,
    /// The credential resolver shared by every surface.
    authenticator: Arc<Authenticator>,
    /// The wasmtime plugin host.
    plugins: Arc<PluginHost>,
    /// The provider registry built from the plugin tables.
    registry: Arc<ProviderRegistry>,
    /// The catalog layer over the provider registry.
    catalog: Arc<CatalogService>,
    /// The write side of the plugin instance and layer tables.
    instances: InstanceService,
    /// The ffmpeg process runner.
    ffmpeg: Arc<FfmpegRunner>,
    /// The acquisition orchestrator the pending stream and the admin views use.
    orchestrator: Arc<AcquisitionOrchestrator>,
    /// The prefetch scheduler the listing and stream endpoints feed.
    prefetch: Arc<PrefetchScheduler>,
    /// The library folder scanner.
    scanner: Arc<LibraryScanner>,
    /// The persistent job queue.
    queue: Arc<JobQueue>,
    /// The job runner with every handler registered.
    runner: Arc<JobRunner>,
    /// The cron scheduler with the system schedules stored.
    scheduler: Arc<Scheduler>,
    /// The activity service: play recording and stream sessions.
    activity: Arc<ActivityService>,
    /// The managed storage roots a stored media file resolves against.
    storage_roots: StorageRoots,
    /// The proxies whose forwarding headers are trusted.
    trusted_proxies: TrustedProxies,
}

impl Services {
    /// Builds every service from the bootstrap configuration.
    ///
    /// # Parameters
    ///
    /// - `config`: the resolved bootstrap configuration.
    /// - `env`: the process environment, keyed by variable name. It supplies
    ///   `HEARTHSOUND_SECRET_KEY` and the environment layer of the runtime
    ///   settings; pass [`hearthsound_config::bootstrap::process_env`] for the
    ///   real environment.
    ///
    /// # Returns
    ///
    /// The services, ready to serve.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] for the step that failed: a directory that could
    /// not be created, the database, a setting whose stored value no longer
    /// resolves, the secret key, the plugin host, the provider tables, or a
    /// system schedule that could not be stored.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::collections::BTreeMap;
    ///
    /// use hearthsound_config::bootstrap::BootstrapConfig;
    /// use hearthsound_server::Services;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let env = BTreeMap::new();
    /// let config = BootstrapConfig::load(None, &env)?;
    /// let services = Services::build(config, &env).await?;
    /// println!("{} settings", services.settings().list().len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn build(
        config: BootstrapConfig,
        env: &BTreeMap<String, String>,
    ) -> Result<Arc<Services>, ServerError> {
        crate::paths::prepare(&config)?;

        let database = Arc::new(Database::open(&config.database.path).await?);
        let outcome = database.migrate(&MIGRATOR, &config.paths.backups).await?;
        if outcome.to != outcome.from {
            tracing::info!(from = outcome.from, to = outcome.to, "applied migrations");
        }

        let settings = Settings::load(
            database.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::new(env.clone(), config.settings.clone()),
        )
        .await?;
        let snapshot = settings.current();

        let secret_key = SecretKey::load(&config.secrets.key_file, env)?;
        let secrets = secret_key.secret_box();
        let events = Arc::new(EventBus::new());

        let authenticator = Arc::new(Authenticator::new(
            &database,
            secrets.clone(),
            login_rate_limit_per_minute(&snapshot)?,
        ));

        let plugins = Arc::new(
            PluginHost::new(
                config.paths.plugins.clone(),
                Arc::clone(&snapshot),
                Arc::clone(&database),
            )?
            .with_event_bus(Arc::clone(&events)),
        );

        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&plugins),
            repository.clone(),
            secrets.clone(),
        ));
        log_rebuild(&registry.rebuild().await?);

        let instances = InstanceService::new(Arc::clone(&plugins), repository, secrets);

        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            config.paths.artwork.clone(),
        ));

        let ffmpeg = Arc::new(FfmpegRunner::new(
            None,
            max_concurrent_transcodes(&snapshot)?,
        ));
        let orchestrator = Arc::new(AcquisitionOrchestrator::new(
            Arc::clone(&database),
            settings.clone(),
            Arc::clone(&registry),
            Arc::clone(&catalog),
            Arc::clone(&ffmpeg),
            Arc::clone(&events),
            AcquisitionStorage::new(
                config.paths.staging.clone(),
                StorageRoots::new(config.paths.library.clone(), config.paths.cache.clone()),
            ),
        ));
        orchestrator.start();
        let prefetch = Arc::new(PrefetchScheduler::new(Arc::clone(&orchestrator)));
        let scanner = Arc::new(LibraryScanner::new(&database, Arc::clone(&events)));

        let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));
        let mut runner = JobRunner::new(Arc::clone(&queue), JobRunnerOptions::default());
        runner.register(
            SCAN_JOB_KIND,
            Arc::new(ScanJobHandler::new(Arc::clone(&scanner))),
        );
        runner.register(
            PLUGIN_KV_SWEEP_JOB_KIND,
            Arc::new(KvSweepJobHandler::new(Arc::clone(&database))),
        );
        let runner = Arc::new(runner);

        let scheduler = Arc::new(Scheduler::new(
            &database,
            Arc::clone(&queue),
            SchedulerOptions::default(),
        ));
        scheduler
            .upsert(ScheduleSpec::system(
                PLUGIN_KV_SWEEP_JOB_KIND,
                PLUGIN_KV_SWEEP_CRON,
            ))
            .await?;

        let activity = Arc::new(ActivityService::new(&database));
        let storage_roots =
            StorageRoots::new(config.paths.library.clone(), config.paths.cache.clone());
        let trusted_proxies = TrustedProxies::parse(config.server.trusted_proxies.iter().cloned())?;

        tracing::info!(
            data = %config.paths.data.display(),
            database = %config.database.path.display(),
            "composed the server services"
        );

        Ok(Arc::new(Services {
            config,
            database,
            settings,
            events,
            authenticator,
            plugins,
            registry,
            catalog,
            instances,
            ffmpeg,
            orchestrator,
            prefetch,
            scanner,
            queue,
            runner,
            scheduler,
            activity,
            storage_roots,
            trusted_proxies,
        }))
    }

    /// Returns the bootstrap configuration the process was built with.
    pub fn config(&self) -> &BootstrapConfig {
        &self.config
    }

    /// Returns the open database.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// Returns the runtime settings service.
    ///
    /// A reader takes the current snapshot with `settings().current()`; it never
    /// holds a snapshot across a write.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Returns the server event bus.
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// Returns the credential resolver.
    pub fn authenticator(&self) -> &Arc<Authenticator> {
        &self.authenticator
    }

    /// Returns the wasmtime plugin host.
    pub fn plugins(&self) -> &Arc<PluginHost> {
        &self.plugins
    }

    /// Returns the provider registry.
    pub fn registry(&self) -> &Arc<ProviderRegistry> {
        &self.registry
    }

    /// Returns the catalog layer.
    ///
    /// The `/rest` surface searches it and resolves an unmaterialized `x_` id
    /// through it.
    pub fn catalog(&self) -> &Arc<CatalogService> {
        &self.catalog
    }

    /// Returns the plugin instance and layer service.
    pub fn instances(&self) -> &InstanceService {
        &self.instances
    }

    /// Returns the ffmpeg process runner.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.ffmpeg
    }

    /// Returns the acquisition orchestrator.
    ///
    /// `/rest/stream` asks it for a track the server has not stored, and the
    /// admin views read its ledger; its scheduler runs for the life of the
    /// process.
    pub fn orchestrator(&self) -> &Arc<AcquisitionOrchestrator> {
        &self.orchestrator
    }

    /// Returns the prefetch scheduler.
    ///
    /// `getAlbum` records the listing it answered and `stream` notes the track
    /// it served, so the scheduler can request what the client is about to
    /// reach; the acquisitions it starts are the orchestrator's, and the
    /// `queue.prefetch` events go out on the same bus.
    pub fn prefetch(&self) -> &Arc<PrefetchScheduler> {
        &self.prefetch
    }

    /// Returns the library scanner.
    pub fn scanner(&self) -> &Arc<LibraryScanner> {
        &self.scanner
    }

    /// Returns the job queue.
    pub fn queue(&self) -> &Arc<JobQueue> {
        &self.queue
    }

    /// Returns the job runner.
    pub fn runner(&self) -> &Arc<JobRunner> {
        &self.runner
    }

    /// Returns the cron scheduler.
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.scheduler
    }

    /// Returns the activity service.
    pub fn activity(&self) -> &Arc<ActivityService> {
        &self.activity
    }

    /// Returns the managed storage roots.
    ///
    /// A stored media file that is not inside a library folder resolves against
    /// one of them; the stream and artwork paths turn a relative path into an
    /// absolute one with them.
    pub fn storage_roots(&self) -> &StorageRoots {
        &self.storage_roots
    }

    /// Returns the shared state of the `/rest` surface.
    pub fn subsonic_state(&self) -> SubsonicState {
        let pending: Arc<dyn PendingSource> = self.orchestrator.clone();
        SubsonicState::new(
            Arc::clone(&self.database),
            self.settings.clone(),
            Arc::clone(&self.authenticator),
            Arc::clone(&self.catalog),
            Arc::clone(&self.activity),
            Arc::clone(&self.ffmpeg),
            self.storage_roots.clone(),
        )
        .with_pending_source(pending)
        .with_prefetch(Arc::clone(&self.prefetch))
        .with_trusted_proxies(self.trusted_proxies.clone())
    }

    /// Resolves `server.bind` and `server.port` into a socket address.
    ///
    /// # Returns
    ///
    /// The first address the configured host and port resolve to.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::Listen`] when the pair does not resolve to any
    /// address, for example when `server.bind` is neither an address nor a
    /// resolvable host name.
    pub fn listen_address(&self) -> Result<SocketAddr, ServerError> {
        let addr = format!("{}:{}", self.config.server.bind, self.config.server.port);
        (self.config.server.bind.as_str(), self.config.server.port)
            .to_socket_addrs()
            .map_err(|source| ServerError::Listen {
                addr: addr.clone(),
                source,
            })?
            .next()
            .ok_or_else(|| ServerError::Listen {
                addr,
                source: std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    "the host and port resolved to no address",
                ),
            })
    }

    /// Reports whether the database is open and at the bundled schema version.
    ///
    /// The check reads `_sqlx_migrations` over a reader connection and compares
    /// the applied version with the highest version the bundled migrator
    /// supplies, so a process whose database a later binary migrated answers
    /// that it is not ready rather than serving from an unknown schema.
    ///
    /// # Returns
    ///
    /// `true` when the schema version matches, `false` when the database cannot
    /// be queried (which is logged at debug level) or is at another version.
    pub async fn is_ready(&self) -> bool {
        match self.database.migration_version().await {
            Ok(applied) if applied == bundled_migration_version() => true,
            Ok(applied) => {
                tracing::debug!(
                    applied,
                    bundled = bundled_migration_version(),
                    "the database schema is not the bundled version"
                );
                false
            }
            Err(error) => {
                tracing::debug!(%error, "the readiness probe could not read the database");
                false
            }
        }
    }
}

/// The schema version the bundled migrations bring a database to.
///
/// # Returns
///
/// The highest version in [`MIGRATOR`], or zero when the migrator is empty.
fn bundled_migration_version() -> i64 {
    MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .max()
        .unwrap_or(0)
}

/// Reads `auth.login_rate_limit_per_minute` as the limiter's rate.
///
/// # Errors
///
/// Returns [`ServerError::Setting`] when the setting is missing or negative.
pub(crate) fn login_rate_limit_per_minute(settings: &SettingsStore) -> Result<u32, ServerError> {
    let per_minute = settings.get(keys::AUTH_LOGIN_RATE_LIMIT_PER_MINUTE)?;
    u32::try_from(per_minute).map_err(|_| {
        ServerError::Setting(hearthsound_config::ConfigError::InvalidValue {
            key: keys::AUTH_LOGIN_RATE_LIMIT_PER_MINUTE.name().to_owned(),
            origin: settings
                .source(keys::AUTH_LOGIN_RATE_LIMIT_PER_MINUTE.name())
                .unwrap_or(hearthsound_config::SettingSource::Default),
            value: per_minute.to_string(),
            reason: "must not be negative".to_owned(),
        })
    })
}

/// Reads `stream.max_concurrent_transcodes` as the ffmpeg process cap.
///
/// # Errors
///
/// Returns [`ServerError::Setting`] when the setting is missing or not positive.
pub(crate) fn max_concurrent_transcodes(settings: &SettingsStore) -> Result<usize, ServerError> {
    let cap = settings.get(keys::STREAM_MAX_CONCURRENT_TRANSCODES)?;
    usize::try_from(cap)
        .ok()
        .filter(|cap| *cap > 0)
        .ok_or_else(|| {
            ServerError::Setting(hearthsound_config::ConfigError::InvalidValue {
                key: keys::STREAM_MAX_CONCURRENT_TRANSCODES.name().to_owned(),
                origin: settings
                    .source(keys::STREAM_MAX_CONCURRENT_TRANSCODES.name())
                    .unwrap_or(hearthsound_config::SettingSource::Default),
                value: cap.to_string(),
                reason: "must be a positive number of processes".to_owned(),
            })
        })
}

/// Logs what a provider registry rebuild produced.
///
/// A skipped instance is a warning rather than an error: it stays configured and
/// the next rebuild tries it again, so a server whose plugin is temporarily
/// unloadable still serves everything else.
pub(crate) fn log_rebuild(report: &hearthsound_plugin_host::RegistryReport) {
    tracing::info!(
        instances = report.instances,
        "rebuilt the provider registry"
    );
    for skipped in &report.skipped {
        tracing::warn!(
            instance = %skipped.slug,
            plugin = %skipped.plugin_id,
            reason = %skipped.reason,
            "skipped a provider instance"
        );
    }
}
