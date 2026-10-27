//! Per-instance state and the services the host imports reach through it.
//!
//! Each plugin instance gets its own [`InstanceState`]: a WASI context with no
//! preopens, no environment, no arguments and no network, a resource table, the
//! instance's memory limit, its log sink, its server-scoped configuration and
//! its outbound HTTP client.
//!
//! The state itself carries no plugin API version: it holds no generated type.
//! The imports a guest can call live in one tree per hosted version
//! ([`crate::api::v5::state`] and [`crate::api::v4::state`]), and each tree
//! reaches the same services through this module, so permissions are enforced
//! identically for every version.
//!
//! Those imports check the instance's granted permissions themselves, never the
//! call site: `host-http` consults the instance's HTTP client, `host-kv` the
//! instance's store, and `host-files` the roots the instance grants. Denying
//! inside the import is what makes a plugin's declared permissions meaningful
//! even when the call site has not been checked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_core::Slug;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::files::{FileAccess, STAGING_ROOT};
use crate::http::HttpClient;
use crate::kv::KvStore;
use crate::log::{GuestOutput, Level, LogSink};
use crate::process::ProcessTable;

/// The host services one instance's stores share.
///
/// The pool clones these handles into every store it instantiates, so a store
/// always reaches the same HTTP client, key-value store, file grant and process
/// table as the rest of its instance.
pub(crate) struct InstanceServices {
    /// The instance's outbound HTTP client.
    pub http: Arc<HttpClient>,
    /// The instance's persistent key-value store.
    pub kv: Arc<KvStore>,
    /// The import roots the instance grants.
    pub files: Arc<FileAccess>,
    /// The instance's process table.
    pub process: Arc<ProcessTable>,
}

/// The state behind one plugin instance's store.
pub struct InstanceState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    /// The instance's log sink, reached by every version's `host-log` import.
    pub(crate) log: Arc<LogSink>,
    /// The server-scoped configuration, reached by every `host-config` import.
    pub(crate) server_config: Arc<str>,
    slug: Slug,
    http: Arc<HttpClient>,
    kv: Arc<KvStore>,
    files: Arc<FileAccess>,
    /// The process table, reached by every version's `host-process` import.
    pub(crate) process: Arc<ProcessTable>,
    /// The staging directory of the call in progress.
    pub(crate) staging: Option<PathBuf>,
    /// The instant the call in progress must finish by.
    pub(crate) deadline: Option<Instant>,
}

impl InstanceState {
    /// Builds the state of one instance.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug, used in log fields and denial messages.
    /// - `server_config`: the server-scoped configuration as JSON text, secrets
    ///   decrypted.
    /// - `log`: the sink the plugin's log lines and stdout go to.
    /// - `memory_mb`: the instance's memory ceiling in megabytes.
    /// - `http`: the instance's outbound HTTP client. Every `host-http` call
    ///   goes through it, so its grant is the only host allowlist that applies.
    /// - `services`: the host services every store of the instance shares.
    pub(crate) fn new(
        slug: Slug,
        server_config: Arc<str>,
        log: Arc<LogSink>,
        memory_mb: u32,
        services: InstanceServices,
    ) -> InstanceState {
        let mut builder = WasiCtxBuilder::new();
        builder.stdout(GuestOutput::new(Arc::clone(&log), Level::Info));
        builder.stderr(GuestOutput::new(Arc::clone(&log), Level::Warn));
        builder.allow_ip_name_lookup(false);
        let wasi = builder.build();

        let limits = StoreLimitsBuilder::new()
            .memory_size(memory_mb as usize * 1024 * 1024)
            .trap_on_grow_failure(true)
            .build();

        InstanceState {
            wasi,
            table: ResourceTable::new(),
            limits,
            log,
            server_config,
            slug,
            http: services.http,
            kv: services.kv,
            files: services.files,
            process: services.process,
            staging: None,
            deadline: None,
        }
    }

    /// Returns the instance's store limits.
    pub fn limits(&mut self) -> &mut StoreLimits {
        &mut self.limits
    }

    /// Returns the instance's log sink.
    pub fn log(&self) -> &Arc<LogSink> {
        &self.log
    }

    /// Returns the instance slug.
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    /// Returns the instance's HTTP client.
    pub fn http(&self) -> &Arc<HttpClient> {
        &self.http
    }

    /// Returns the instance's key-value store.
    pub fn kv(&self) -> &Arc<KvStore> {
        &self.kv
    }

    /// Returns the import roots the instance grants.
    pub fn files(&self) -> &Arc<FileAccess> {
        &self.files
    }

    /// Sets the staging root of the acquisition call about to run.
    ///
    /// The staging directory belongs to one call, so it is set before the call
    /// and cleared after it; `host-files` resolves the reserved root name
    /// `staging` to it.
    ///
    /// # Parameters
    ///
    /// - `staging`: the call's staging directory, or `None` to clear it.
    pub fn set_staging(&mut self, staging: Option<PathBuf>) {
        self.staging = staging;
    }

    /// Returns the directory a root name resolves to, when the instance grants
    /// it.
    ///
    /// # Parameters
    ///
    /// - `name`: the root a plugin named: [`STAGING_ROOT`] or an import-root id.
    pub(crate) fn root_dir(&self, name: &str) -> Option<PathBuf> {
        if name == STAGING_ROOT {
            return self.staging.clone();
        }
        self.files.root(name).map(Path::to_path_buf)
    }

    /// Marks the start of a guest call and the instant it must be finished by.
    ///
    /// The pool calls this before every guest call so imports that can wait
    /// (`host-http`'s rate limiter, for example) never wait past the call's
    /// deadline.
    ///
    /// # Parameters
    ///
    /// - `deadline`: the instant the whole call must be finished by.
    pub fn begin_call(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    /// Returns how much of the current call's budget is left.
    ///
    /// # Returns
    ///
    /// The remaining time, or `None` when no call is in progress.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }
}

impl WasiView for InstanceState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}
