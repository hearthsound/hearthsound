//! Every service the process runs, built once from the bootstrap configuration.
//!
//! [`Services::build`] performs the startup sequence in the order the services
//! depend on each other:
//!
//! 1. resolve the web client's asset source, which fails when `web.root` is
//!    set but is not a directory;
//! 2. create the data and storage directory trees ([`crate::paths`]);
//! 3. open the database and apply pending migrations, after a `VACUUM INTO`
//!    backup when the database already holds data;
//! 4. load the runtime settings, resolved as environment, config file,
//!    database, then default;
//! 5. load the 32-byte secret key, creating it on first start;
//! 6. build the event bus, the authenticator (with the login rate limit from
//!    `auth.login_rate_limit_per_minute`), the plugin host, the provider
//!    registry (rebuilt from the plugin tables), and the catalog layer over
//!    that registry;
//! 7. build the ffmpeg runner capped by `stream.max_concurrent_transcodes`, the
//!    acquisition orchestrator over the catalog, registry and storage, and start
//!    its scheduler, then the library scanner, the activity service, the
//!    managed storage roots and the storage maintainer that keeps their files
//!    in step with the pins, the job queue, the job runner, and the cron
//!    scheduler with the system schedules;
//! 8. build the trusted-proxy list and the CORS policy ([`crate::cors`]).
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
use hearthsound_activity::{
    ActivityService, DISCOVERY_SYNC_JOB_KIND, DISCOVERY_SYNC_SCHEDULE, DiscoverySyncJobHandler,
    EXPIRE_JOB_KIND, EXPIRE_SCHEDULE, ExpireJobHandler, SCROBBLE_DELIVER_JOB_KIND,
    SCROBBLE_DELIVER_SCHEDULE, ScrobbleDeliverJobHandler, ScrobbleDeliverer, ScrobbleOutbox,
    ScrobbleTargets,
};
use hearthsound_api_native::{NativeServices, NativeState};
use hearthsound_api_subsonic::{PendingSource, SubsonicState};
use hearthsound_auth::{Authenticator, Sessions, SetupGate, TrustedProxies, Users};
use hearthsound_catalog::{CatalogService, DiscoveryService, LinkedUserContexts};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::keys;
use hearthsound_config::secret::{SecretBox, SecretKey};
use hearthsound_config::settings::Settings;
use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{
    JobHandler, JobQueue, JobRunner, JobRunnerOptions, QueueOptions, ScheduleSpec, Scheduler,
    SchedulerOptions,
};
use hearthsound_library::StorageRoots;
use hearthsound_library::maintenance::{
    MAINTENANCE_JOB_KIND, MAINTENANCE_SCHEDULE, MaintenanceJobHandler, MaintenanceSignal,
    StorageMaintainer,
};
use hearthsound_library::scan::{LibraryScanner, SCAN_JOB_KIND, ScanJobHandler};
use hearthsound_library::thumbnails::ThumbnailCache;
use hearthsound_library::upload::UploadIngest;
use hearthsound_library::watch::LibraryWatcher;
use hearthsound_media::FfmpegRunner;
use hearthsound_plugin_host::instances::InstanceService;
use hearthsound_plugin_host::{
    IndexClient, PluginAdmin, PluginHost, PluginInstaller, PluginRepository, ProviderRegistry,
    UPDATE_CHECK_JOB_KIND, UPDATE_CHECK_SCHEDULE, UpdateCheckJobHandler, UpdateChecker,
    UserConfigService,
};
use tower_http::cors::CorsLayer;

use crate::error::ServerError;
use crate::jobs::{KvSweepJobHandler, PLUGIN_KV_SWEEP_CRON, PLUGIN_KV_SWEEP_JOB_KIND};
use crate::web::WebAssets;

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
    /// The web session table, shared by the native API and `/rest`.
    sessions: Sessions,
    /// The one-time token that opens the first-run setup, empty once an
    /// account exists.
    setup: Arc<SetupGate>,
    /// The wasmtime plugin host.
    plugins: Arc<PluginHost>,
    /// The provider registry built from the plugin tables.
    registry: Arc<ProviderRegistry>,
    /// The catalog layer over the provider registry.
    catalog: Arc<CatalogService>,
    /// The discovery layer over the provider registry.
    discovery: Arc<DiscoveryService>,
    /// The write side of the plugin instance and layer tables.
    instances: InstanceService,
    /// The plugin tables the admin views and the repository routes read.
    plugin_repository: PluginRepository,
    /// The administrative plugin write path: install, apply a staged upgrade,
    /// switch, remove.
    admin: Arc<PluginAdmin>,
    /// The installer the admin routes install from indexes and repository URLs
    /// with.
    installer: Arc<PluginInstaller>,
    /// The checker the background job and the admin views read.
    update_checker: Arc<UpdateChecker>,
    /// The update-check job, which holds what the last check found.
    update_job: Arc<UpdateCheckJobHandler>,
    /// The ffmpeg process runner.
    ffmpeg: Arc<FfmpegRunner>,
    /// The scaled local cover cache `getCoverArt` serves sizes from.
    thumbnails: Arc<ThumbnailCache>,
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
    /// The library folder watcher.
    watcher: Arc<LibraryWatcher>,
    /// The cron scheduler with the system schedules stored.
    scheduler: Arc<Scheduler>,
    /// The activity service: play recording and stream sessions.
    activity: Arc<ActivityService>,
    /// Delivers the scrobbles the activity service queues.
    scrobble_deliverer: Arc<ScrobbleDeliverer>,
    /// The generated-playlist sync job.
    discovery_sync: Arc<DiscoverySyncJobHandler>,
    /// Keeps managed files in the root their pins imply and the cache within
    /// its limits.
    maintainer: Arc<StorageMaintainer>,
    /// The signal that wakes the maintenance worker; the activity service
    /// notifies it after every pin change.
    maintenance: MaintenanceSignal,
    /// The upload ingest the native API stores uploads through.
    uploads: UploadIngest,
    /// The secret box app passwords are sealed with.
    secrets: SecretBox,
    /// The managed storage roots a stored media file resolves against.
    storage_roots: StorageRoots,
    /// The proxies whose forwarding headers are trusted.
    trusted_proxies: TrustedProxies,
    /// The CORS layer `server.cors_origins` describes, or `None` when it is
    /// empty and cross-origin reads are denied.
    cors: Option<CorsLayer>,
    /// The web client's files, from `web.root` when it is set.
    web: WebAssets,
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
        // Resolved first: a `web.root` that is not a directory is a
        // configuration mistake, and it fails before anything is created on
        // disk.
        let web = WebAssets::resolve(config.web.root.as_deref())?;
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
        let sessions = Sessions::new(&database);
        // The setup link only exists while no account does; the check is the
        // same query the API's own `required` answer reads.
        let setup = Arc::new(SetupGate::new(
            Users::new(&database).list().await?.is_empty(),
        ));

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
            .with_event_bus(Arc::clone(&events))
            .with_live_settings(settings.clone()),
        );

        let repository = PluginRepository::new(&database);
        let registry = Arc::new(ProviderRegistry::new(
            Arc::clone(&plugins),
            repository.clone(),
            secrets.clone(),
        ));
        log_rebuild(&registry.rebuild().await?);

        let instances =
            InstanceService::new(Arc::clone(&plugins), repository.clone(), secrets.clone());

        // reqwest is built without a bundled TLS provider, so ring is installed
        // as the process default before the first client is built; installing it
        // again is harmless, which is why the plugin HTTP import does the same.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(format!("Hearthsound/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(ServerError::HttpClient)?;
        let admin = Arc::new(PluginAdmin::new(
            Arc::clone(&plugins),
            repository.clone(),
            Some(Arc::clone(&events)),
        ));
        let installer = Arc::new(PluginInstaller::new(
            Arc::clone(&admin),
            repository.clone(),
            IndexClient::official(http.clone())?,
            http,
            config.paths.plugins.clone(),
        ));
        let update_checker = Arc::new(UpdateChecker::new(
            Arc::clone(&installer),
            repository.clone(),
        ));
        let update_job = Arc::new(
            UpdateCheckJobHandler::new(Arc::clone(&update_checker))
                .with_registry(Arc::clone(&registry)),
        );

        let catalog = Arc::new(CatalogService::new(
            Arc::clone(&registry),
            Arc::clone(&database),
            settings.clone(),
            config.paths.artwork.clone(),
        ));

        // The listener's per-instance configuration comes from the stored
        // provider links; an instance the listener has not linked is asked
        // without a context, as a public lookup is.
        let user_configs = UserConfigService::new(&database, secrets.clone());
        let discovery = Arc::new(DiscoveryService::new(
            Arc::clone(&registry),
            Arc::clone(&catalog),
            Arc::new(LinkedUserContexts::new(user_configs.clone())),
        ));

        let ffmpeg = Arc::new(FfmpegRunner::new(
            None,
            max_concurrent_transcodes(&snapshot)?,
        ));
        // The thumbs directory sits inside the artwork directory the catalog
        // layer writes its cached images to, so one storage policy covers both.
        let thumbnails = Arc::new(ThumbnailCache::new(
            config.paths.artwork.join("thumbs"),
            Arc::clone(&ffmpeg),
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
        let scanner = Arc::new(LibraryScanner::new(
            &database,
            Arc::clone(&events),
            settings.clone(),
        ));

        let user_configs = UserConfigService::new(&database, secrets.clone());
        let targets: Arc<dyn ScrobbleTargets> = registry.clone();
        let scrobble_outbox = Arc::new(ScrobbleOutbox::new(
            &database,
            targets,
            user_configs.clone(),
        ));
        let activity = Arc::new(
            ActivityService::new(&database).with_scrobble_outbox(Arc::clone(&scrobble_outbox)),
        );
        let scrobble_deliverer = Arc::new(ScrobbleDeliverer::new(Arc::clone(&scrobble_outbox)));
        let storage_roots =
            StorageRoots::new(config.paths.library.clone(), config.paths.cache.clone());
        let maintainer = Arc::new(StorageMaintainer::new(
            &database,
            storage_roots.clone(),
            settings.clone(),
            Arc::clone(&events),
        ));
        let maintenance = activity.maintenance_signal().clone();
        let uploads = UploadIngest::new(
            &database,
            storage_roots.clone(),
            settings.clone(),
            Arc::clone(&events),
        )
        .with_signal(maintenance.clone());

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
        runner.register(
            MAINTENANCE_JOB_KIND,
            Arc::new(MaintenanceJobHandler::new(Arc::clone(&maintainer))),
        );
        runner.register(
            EXPIRE_JOB_KIND,
            Arc::new(ExpireJobHandler::new(activity.playlists().clone())),
        );
        runner.register(
            SCROBBLE_DELIVER_JOB_KIND,
            Arc::new(ScrobbleDeliverJobHandler::new(Arc::clone(
                &scrobble_deliverer,
            ))),
        );
        let discovery_sync = Arc::new(DiscoverySyncJobHandler::new(
            Arc::clone(&discovery),
            Arc::clone(&catalog),
            Arc::clone(&registry),
            user_configs,
            activity.playlists().clone(),
            Arc::clone(&events),
        ));
        runner.register(DISCOVERY_SYNC_JOB_KIND, discovery_sync.clone());
        let update_handler: Arc<dyn JobHandler> = update_job.clone();
        runner.register(UPDATE_CHECK_JOB_KIND, update_handler);
        let runner = Arc::new(runner);

        let watcher = Arc::new(LibraryWatcher::new(
            &database,
            Arc::clone(&scanner),
            Arc::clone(&queue),
            settings.clone(),
        ));

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
        scheduler
            .upsert(ScheduleSpec::system(
                MAINTENANCE_JOB_KIND,
                MAINTENANCE_SCHEDULE,
            ))
            .await?;
        scheduler
            .upsert(ScheduleSpec::system(EXPIRE_JOB_KIND, EXPIRE_SCHEDULE))
            .await?;
        scheduler
            .upsert(ScheduleSpec::system(
                SCROBBLE_DELIVER_JOB_KIND,
                SCROBBLE_DELIVER_SCHEDULE,
            ))
            .await?;
        scheduler
            .upsert(ScheduleSpec::system(
                DISCOVERY_SYNC_JOB_KIND,
                DISCOVERY_SYNC_SCHEDULE,
            ))
            .await?;
        scheduler
            .upsert(ScheduleSpec::system(
                UPDATE_CHECK_JOB_KIND,
                UPDATE_CHECK_SCHEDULE,
            ))
            .await?;

        // The first check of a process runs here rather than on the next tick,
        // so a server that was down for a week looks for plugin updates as soon
        // as it serves. It runs beside the request path: a slow or unreachable
        // index must not hold up the server coming up.
        let startup = Arc::clone(&update_job);
        tokio::spawn(async move {
            startup.check_if_due().await;
        });

        let trusted_proxies = TrustedProxies::parse(config.server.trusted_proxies.iter().cloned())?;
        let cors = crate::cors::layer(&config.server.cors_origins)?;

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
            sessions: sessions.clone(),
            setup: Arc::clone(&setup),
            plugins,
            registry,
            catalog,
            discovery,
            instances,
            plugin_repository: repository.clone(),
            admin,
            installer,
            update_checker,
            update_job,
            ffmpeg,
            thumbnails,
            orchestrator,
            prefetch,
            scanner,
            queue,
            runner,
            watcher,
            scheduler,
            activity,
            scrobble_deliverer,
            discovery_sync,
            maintainer,
            maintenance,
            uploads,
            secrets,
            storage_roots,
            trusted_proxies,
            cors,
            web,
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

    /// Returns the first-run setup gate.
    ///
    /// The native API accepts the setup link through it, and the serving
    /// process logs the URL that carries its token.
    pub fn setup(&self) -> &Arc<SetupGate> {
        &self.setup
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

    /// Returns the discovery layer.
    ///
    /// The `/rest` suggestion endpoints ask it for similar songs, similar
    /// artists and an artist's popular tracks.
    pub fn discovery(&self) -> &Arc<DiscoveryService> {
        &self.discovery
    }

    /// Returns the plugin instance and layer service.
    pub fn instances(&self) -> &InstanceService {
        &self.instances
    }

    /// Returns the plugin tables.
    ///
    /// The administrative plugin views and the repository routes read the
    /// installed rows, their staged upgrades and the repositories an
    /// administrator registered through it.
    pub fn plugin_repository(&self) -> &PluginRepository {
        &self.plugin_repository
    }

    /// Returns the administrative plugin write path.
    ///
    /// The native admin routes install through the installer and apply a staged
    /// upgrade, switch and remove through this service, which is why both are
    /// exposed.
    pub fn admin(&self) -> &Arc<PluginAdmin> {
        &self.admin
    }

    /// Returns the plugin installer.
    ///
    /// An install resolves an index or a repository URL, verifies the download
    /// and records the plugin with the permissions its manifest requests,
    /// enabled.
    pub fn installer(&self) -> &Arc<PluginInstaller> {
        &self.installer
    }

    /// Returns the plugin update checker.
    pub fn update_checker(&self) -> &Arc<UpdateChecker> {
        &self.update_checker
    }

    /// Returns the handler that holds what the last plugin update check found.
    ///
    /// The admin views read the pending updates from it rather than reading
    /// every index again.
    pub fn update_job(&self) -> &Arc<UpdateCheckJobHandler> {
        &self.update_job
    }

    /// Returns the ffmpeg process runner.
    pub fn ffmpeg(&self) -> &Arc<FfmpegRunner> {
        &self.ffmpeg
    }

    /// Returns the scaled local cover cache.
    ///
    /// `getCoverArt` serves a local cover at the requested size through it; the
    /// `/rest` state is built with it so the artwork path never reads the
    /// settings to find it.
    pub fn thumbnails(&self) -> &Arc<ThumbnailCache> {
        &self.thumbnails
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

    /// Returns the library folder watcher.
    ///
    /// `admin set-folder-watch` writes the flag the watcher reconciles against,
    /// and the reload on an external write is what starts or stops the watchers.
    pub fn watcher(&self) -> &Arc<LibraryWatcher> {
        &self.watcher
    }

    /// Returns the cron scheduler.
    pub fn scheduler(&self) -> &Arc<Scheduler> {
        &self.scheduler
    }

    /// Returns the activity service.
    pub fn activity(&self) -> &Arc<ActivityService> {
        &self.activity
    }

    /// Returns the deliverer that drains the scrobble outbox.
    ///
    /// The serving process runs one delivery worker for it, and the
    /// `scrobble.deliver` job runs a pass through it.
    pub fn scrobble_deliverer(&self) -> &Arc<ScrobbleDeliverer> {
        &self.scrobble_deliverer
    }

    /// Returns the generated-playlist sync job.
    ///
    /// The account routes enqueue it when a listener links a discovery
    /// instance, and the system schedule runs it for every linked listener.
    pub fn discovery_sync(&self) -> &Arc<DiscoverySyncJobHandler> {
        &self.discovery_sync
    }

    /// Returns the storage maintainer.
    pub fn maintainer(&self) -> &Arc<StorageMaintainer> {
        &self.maintainer
    }

    /// Returns the signal that wakes the storage maintenance worker.
    pub fn maintenance(&self) -> &MaintenanceSignal {
        &self.maintenance
    }

    /// Returns the shared state of the `/api/v1` surface.
    pub fn native_state(&self) -> NativeState {
        NativeState::new(NativeServices {
            database: Arc::clone(&self.database),
            sessions: self.sessions.clone(),
            setup: Arc::clone(&self.setup),
            base_url: self.config.server.base_url.clone(),
            base_url_is_https: self
                .config
                .server
                .base_url
                .to_ascii_lowercase()
                .starts_with("https://"),
            settings: self.settings.clone(),
            authenticator: Arc::clone(&self.authenticator),
            secrets: self.secrets.clone(),
            trusted_proxies: self.trusted_proxies.clone(),
            registry: Arc::clone(&self.registry),
            orchestrator: Arc::clone(&self.orchestrator),
            activity: Arc::clone(&self.activity),
            maintainer: Arc::clone(&self.maintainer),
            uploads: self.uploads.clone(),
            uploads_staging: self.config.paths.uploads_staging.clone(),
            scanner: Arc::clone(&self.scanner),
            jobs: Arc::clone(&self.queue),
            watcher: Arc::clone(&self.watcher),
            events: Arc::clone(&self.events),
            plugins: Arc::clone(&self.plugins),
            instances: self.instances.clone(),
            admin: Arc::clone(&self.admin),
            installer: Arc::clone(&self.installer),
            update_checker: Arc::clone(&self.update_checker),
            update_job: Arc::clone(&self.update_job),
            plugin_repository: self.plugin_repository.clone(),
        })
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
            self.sessions.clone(),
            self.settings.clone(),
            Arc::clone(&self.authenticator),
            Arc::clone(&self.catalog),
            Arc::clone(&self.activity),
            Arc::clone(&self.ffmpeg),
            self.storage_roots.clone(),
        )
        .with_pending_source(pending)
        .with_prefetch(Arc::clone(&self.prefetch))
        .with_acquisitions(Arc::clone(&self.orchestrator))
        .with_registry(Arc::clone(&self.registry))
        .with_trusted_proxies(self.trusted_proxies.clone())
        .with_scanner(Arc::clone(&self.scanner), Arc::clone(&self.queue))
        .with_thumbnails(Arc::clone(&self.thumbnails))
        .with_discovery(Arc::clone(&self.discovery))
    }

    /// Returns the CORS layer, or `None` when cross-origin reads are denied.
    pub(crate) fn cors(&self) -> Option<&CorsLayer> {
        self.cors.as_ref()
    }

    /// Returns the web client's files.
    ///
    /// The root and the router's fallback serve them; which source they come
    /// from is decided once, when the services are built.
    pub fn web(&self) -> &WebAssets {
        &self.web
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
