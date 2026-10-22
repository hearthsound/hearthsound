//! Per-instance state and the host imports a plugin may call.
//!
//! Each plugin instance gets its own [`InstanceState`]: a WASI context with no
//! preopens, no environment, no arguments and no network, a resource table, the
//! instance's memory limit, its log sink, its server-scoped configuration and
//! its outbound HTTP client.
//!
//! The host imports check the instance's granted permissions themselves, never
//! the call site: `host-http` consults the instance's HTTP client, `host-kv`
//! the instance's store, and `host-files` the roots the instance grants. Denying
//! inside the import is what makes a plugin's declared permissions meaningful
//! even when the call site has not been checked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_core::Slug;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::hearthsound::plugin::{
    host_config, host_files, host_http, host_kv, host_log, types,
};
use crate::files::{self, FileAccess, STAGING_ROOT};
use crate::http::{HttpClient, HttpError, HttpRequest};
use crate::kv::KvStore;
use crate::log::{GuestOutput, LogSink};

/// The state behind one plugin instance's store.
pub struct InstanceState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    log: Arc<LogSink>,
    server_config: Arc<str>,
    slug: Slug,
    http: Arc<HttpClient>,
    kv: Arc<KvStore>,
    files: Arc<FileAccess>,
    staging: Option<PathBuf>,
    deadline: Option<Instant>,
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
    /// - `kv`: the instance's persistent key-value store.
    /// - `files`: the import roots the instance grants.
    pub fn new(
        slug: Slug,
        server_config: Arc<str>,
        log: Arc<LogSink>,
        memory_mb: u32,
        http: Arc<HttpClient>,
        kv: Arc<KvStore>,
        files: Arc<FileAccess>,
    ) -> InstanceState {
        let mut builder = WasiCtxBuilder::new();
        builder.stdout(GuestOutput::new(Arc::clone(&log), host_log::Level::Info));
        builder.stderr(GuestOutput::new(Arc::clone(&log), host_log::Level::Warn));
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
            http,
            kv,
            files,
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
    fn root_dir(&self, name: &str) -> Option<PathBuf> {
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

impl types::Host for InstanceState {}

impl host_files::Host for InstanceState {
    async fn stat(
        &mut self,
        root: String,
        path: String,
    ) -> wasmtime::Result<Option<host_files::FileStat>> {
        let Some(dir) = self.root_dir(&root) else {
            tracing::debug!(
                target: "hearthsound::plugin",
                instance = %self.slug,
                root = %root,
                "host-files: the root is not granted",
            );
            return Ok(None);
        };
        Ok(match files::stat(&dir, &path).await {
            Ok(Some(stat)) => Some(host_files::FileStat {
                size: stat.size,
                modified_ms: stat.modified_ms,
                is_dir: stat.is_dir,
            }),
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(
                    target: "hearthsound::plugin",
                    instance = %self.slug,
                    root = %root,
                    path = %path,
                    error = %error,
                    "host-files stat refused",
                );
                None
            }
        })
    }

    async fn list_dir(
        &mut self,
        root: String,
        path: String,
    ) -> wasmtime::Result<Result<Vec<String>, String>> {
        let Some(dir) = self.root_dir(&root) else {
            return Ok(Err(format!(
                "no import root `{root}` is granted to this instance"
            )));
        };
        Ok(match files::list_dir(&dir, &path).await {
            Ok(entries) => Ok(entries),
            Err(error) => Err(error.reason()),
        })
    }
}

impl host_log::Host for InstanceState {
    async fn log(&mut self, level: host_log::Level, message: String) -> wasmtime::Result<()> {
        self.log.write(level, &message);
        Ok(())
    }
}

impl host_config::Host for InstanceState {
    async fn server_config(&mut self) -> wasmtime::Result<String> {
        Ok(self.server_config.to_string())
    }
}

impl host_http::Host for InstanceState {
    async fn send(
        &mut self,
        request: host_http::Request,
    ) -> wasmtime::Result<Result<host_http::Response, host_http::HttpError>> {
        let request = HttpRequest {
            method: request.method,
            url: request.url,
            headers: request.headers,
            body: request.body,
            timeout_ms: request.timeout_ms,
        };

        Ok(match self.http.send(&request, self.deadline).await {
            Ok(response) => Ok(host_http::Response {
                status: response.status,
                headers: response.headers,
                body: response.body,
            }),
            Err(error) => Err(to_wit_error(error)),
        })
    }
}

/// Maps the host's HTTP error to the plugin's vocabulary.
fn to_wit_error(error: HttpError) -> host_http::HttpError {
    match error {
        HttpError::Denied(reason) => host_http::HttpError::Denied(reason),
        HttpError::Timeout => host_http::HttpError::Timeout,
        HttpError::BodyTooLarge(_) => host_http::HttpError::BodyTooLarge,
        HttpError::Network(reason) => host_http::HttpError::Network(reason),
    }
}

impl host_kv::Host for InstanceState {
    async fn get(&mut self, key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        match self.kv.get(&key).await {
            Ok(value) => Ok(value),
            Err(error) => {
                // The interface has no error channel on a read, so a storage
                // failure is reported as a miss and logged instead.
                tracing::error!(
                    target: "hearthsound::plugin",
                    instance = %self.slug,
                    key = %key,
                    error = %error,
                    "host-kv read failed",
                );
                Ok(None)
            }
        }
    }

    async fn set(
        &mut self,
        key: String,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(match self.kv.set(&key, &value, ttl_ms).await {
            Ok(()) => Ok(()),
            Err(error) => Err(error.reason()),
        })
    }

    async fn delete(&mut self, key: String) -> wasmtime::Result<()> {
        if let Err(error) = self.kv.delete(&key).await {
            tracing::error!(
                target: "hearthsound::plugin",
                instance = %self.slug,
                key = %key,
                error = %error,
                "host-kv delete failed",
            );
        }
        Ok(())
    }
}
