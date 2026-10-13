//! Per-instance state and the host imports a plugin may call.
//!
//! Each plugin instance gets its own [`InstanceState`]: a WASI context with no
//! preopens, no environment, no arguments and no network, a resource table, the
//! instance's memory limit, its log sink and its server-scoped configuration.
//!
//! The host imports deny everything by default. There is no approval flow yet,
//! so every instance has an empty grant: HTTP is denied outright and key-value
//! writes report that no quota is approved. Denying at the import is what makes
//! a plugin's declared permissions meaningful even before the approval UI
//! exists.

use std::sync::Arc;

use hearthsound_core::Slug;
use wasmtime::component::ResourceTable;
use wasmtime::{StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::hearthsound::plugin::{host_config, host_http, host_kv, host_log, types};
use crate::log::{GuestOutput, LogSink};

/// The state behind one plugin instance's store.
pub struct InstanceState {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    log: Arc<LogSink>,
    server_config: Arc<str>,
    slug: Slug,
}

impl InstanceState {
    /// Builds the state of one instance.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug, used in log fields and denial messages.
    /// - `server_config`: the server-scoped configuration as JSON text.
    /// - `log`: the sink the plugin's log lines and stdout go to.
    /// - `memory_mb`: the instance's memory ceiling in megabytes.
    pub fn new(
        slug: Slug,
        server_config: Arc<str>,
        log: Arc<LogSink>,
        memory_mb: u32,
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
        _request: host_http::Request,
    ) -> wasmtime::Result<Result<host_http::Response, host_http::HttpError>> {
        Ok(Err(host_http::HttpError::Denied(format!(
            "host-http: no hosts are approved for instance `{}`",
            self.slug
        ))))
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
