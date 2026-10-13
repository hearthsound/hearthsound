//! Per-instance state and the host imports a plugin may call.
//!
//! Each plugin instance gets its own [`InstanceState`]: a WASI context with no
//! preopens, no environment, no arguments and no network, a resource table, the
//! instance's memory limit, its log sink, its server-scoped configuration and
//! its outbound HTTP client.
//!
//! The host imports check the instance's approved permissions themselves, never
//! the call site: `host-http` consults the instance's HTTP client, and the
//! key-value imports report that no quota is approved until the store is backed
//! by the database. Denying inside the import is what makes a plugin's declared
//! permissions meaningful even before the approval UI exists.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_core::Slug;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::hearthsound::plugin::{host_config, host_http, host_kv, host_log, types};
use crate::http::{HttpClient, HttpError, HttpRequest};
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
    pub fn new(
        slug: Slug,
        server_config: Arc<str>,
        log: Arc<LogSink>,
        memory_mb: u32,
        http: Arc<HttpClient>,
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
    async fn get(&mut self, _key: String) -> wasmtime::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    async fn set(
        &mut self,
        _key: String,
        _value: Vec<u8>,
        _ttl_ms: Option<u64>,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(Err(format!(
            "host-kv: no storage quota is approved for instance `{}`",
            self.slug
        )))
    }

    async fn delete(&mut self, _key: String) -> wasmtime::Result<()> {
        Ok(())
    }
}
