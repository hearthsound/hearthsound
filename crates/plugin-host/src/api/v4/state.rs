//! The host imports a `hearthsound:plugin@0.4.0` guest may call.
//!
//! The permissions each import enforces live in the shared module it reaches
//! ([`crate::http`], [`crate::files`], [`crate::kv`], [`crate::process`]), so
//! they apply to this version and to the frozen one alike. This module only
//! turns the generated types into shared ones and back.
//!
//! `host-kv`'s read is the one import whose signature differs between the
//! versions: `get` returns `result<option<list<u8>>, string>` here, so a
//! storage failure reaches the guest as `err(reason)` instead of a silent miss.

use crate::api::v4::bindings::hearthsound::plugin::{
    host_config, host_files, host_http, host_kv, host_log, host_process, types,
};
use crate::files;
use crate::http::{HttpError, HttpRequest};
use crate::log::Level;
use crate::process::{ProcessError, ProcessState, SpawnOptions, StdoutMode};
use crate::state::InstanceState;

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
                instance = %self.slug(),
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
                    instance = %self.slug(),
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
        self.log.write(to_level(level), &message);
        Ok(())
    }
}

/// Maps the 0.4.0 `host-log` level to the shared one.
///
/// # Parameters
///
/// - `level`: the level the guest logged at.
fn to_level(level: host_log::Level) -> Level {
    match level {
        host_log::Level::Trace => Level::Trace,
        host_log::Level::Debug => Level::Debug,
        host_log::Level::Info => Level::Info,
        host_log::Level::Warn => Level::Warn,
        host_log::Level::Error => Level::Error,
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

        Ok(match self.http().send(&request, self.deadline).await {
            Ok(response) => Ok(host_http::Response {
                status: response.status,
                headers: response.headers,
                body: response.body,
            }),
            Err(error) => Err(to_http_error(error)),
        })
    }
}

/// Maps the host's HTTP error to the plugin's vocabulary.
///
/// # Parameters
///
/// - `error`: the error the host's client produced.
fn to_http_error(error: HttpError) -> host_http::HttpError {
    match error {
        HttpError::Denied(reason) => host_http::HttpError::Denied(reason),
        HttpError::Timeout => host_http::HttpError::Timeout,
        HttpError::BodyTooLarge(_) => host_http::HttpError::BodyTooLarge,
        HttpError::Network(reason) => host_http::HttpError::Network(reason),
    }
}

impl host_kv::Host for InstanceState {
    async fn get(&mut self, key: String) -> wasmtime::Result<Result<Option<Vec<u8>>, String>> {
        Ok(match self.kv().get(&key).await {
            Ok(value) => Ok(value),
            Err(error) => {
                tracing::error!(
                    target: "hearthsound::plugin",
                    instance = %self.slug(),
                    key = %key,
                    error = %error,
                    "host-kv read failed",
                );
                Err(error.reason())
            }
        })
    }

    async fn set(
        &mut self,
        key: String,
        value: Vec<u8>,
        ttl_ms: Option<u64>,
    ) -> wasmtime::Result<Result<(), String>> {
        Ok(match self.kv().set(&key, &value, ttl_ms).await {
            Ok(()) => Ok(()),
            Err(error) => Err(error.reason()),
        })
    }

    async fn delete(&mut self, key: String) -> wasmtime::Result<()> {
        if let Err(error) = self.kv().delete(&key).await {
            tracing::error!(
                target: "hearthsound::plugin",
                instance = %self.slug(),
                key = %key,
                error = %error,
                "host-kv delete failed",
            );
        }
        Ok(())
    }
}

impl host_process::Host for InstanceState {
    async fn spawn(
        &mut self,
        tool: String,
        args: Vec<String>,
        options: host_process::SpawnOptions,
    ) -> wasmtime::Result<Result<host_process::ProcessHandle, host_process::ProcessError>> {
        let options = SpawnOptions {
            cwd: options.cwd,
            env: options.env,
            stdout: match options.stdout {
                host_process::StdoutMode::Capture => StdoutMode::Capture,
                host_process::StdoutMode::File(path) => StdoutMode::File(path),
            },
            timeout_ms: options.timeout_ms,
        };
        Ok(self
            .process
            .spawn(self.staging.as_deref(), &tool, args, options)
            .await
            .map_err(to_process_error))
    }

    async fn poll(
        &mut self,
        handle: host_process::ProcessHandle,
    ) -> wasmtime::Result<Result<host_process::ProcessState, host_process::ProcessError>> {
        Ok(self
            .process
            .poll(handle)
            .await
            .map(to_process_state)
            .map_err(to_process_error))
    }

    async fn read_stdout(
        &mut self,
        handle: host_process::ProcessHandle,
    ) -> wasmtime::Result<Result<Vec<u8>, host_process::ProcessError>> {
        Ok(self
            .process
            .read_stdout(handle)
            .await
            .map_err(to_process_error))
    }

    async fn read_stderr(
        &mut self,
        handle: host_process::ProcessHandle,
    ) -> wasmtime::Result<Result<Vec<u8>, host_process::ProcessError>> {
        Ok(self
            .process
            .read_stderr(handle)
            .await
            .map_err(to_process_error))
    }

    async fn kill(&mut self, handle: host_process::ProcessHandle) -> wasmtime::Result<()> {
        self.process.kill(handle).await;
        Ok(())
    }
}

/// Maps the shared process error to the 0.4.0 `host-process` error.
///
/// # Parameters
///
/// - `error`: the error the process table produced.
fn to_process_error(error: ProcessError) -> host_process::ProcessError {
    match error {
        ProcessError::Denied(reason) => host_process::ProcessError::Denied(reason),
        ProcessError::NotConfigured(reason) => host_process::ProcessError::NotConfigured(reason),
        ProcessError::SpawnFailed(reason) => host_process::ProcessError::SpawnFailed(reason),
        ProcessError::UnknownHandle => host_process::ProcessError::UnknownHandle,
    }
}

/// Maps the shared process state to the 0.4.0 `host-process` state.
///
/// # Parameters
///
/// - `state`: the state the process table reported.
fn to_process_state(state: ProcessState) -> host_process::ProcessState {
    match state {
        ProcessState::Running => host_process::ProcessState::Running,
        ProcessState::Exited(code) => host_process::ProcessState::Exited(code),
        ProcessState::Killed => host_process::ProcessState::Killed,
        ProcessState::TimedOut => host_process::ProcessState::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hearthsound_core::Slug;

    use super::*;
    use crate::files::FileAccess;
    use crate::http::{HttpClient, HttpGrant};
    use crate::kv::{DEFAULT_QUOTA_BYTES, KvStore};
    use crate::log::LogSink;
    use crate::state::{InstanceServices, InstanceState};

    /// Builds an instance whose key-value store reads through `pool`.
    fn state(pool: sqlx::SqlitePool) -> InstanceState {
        let slug = Slug::new("probe").unwrap();
        let http = Arc::new(
            HttpClient::new(
                "com.example.probe".to_owned(),
                "0.1.0",
                slug.clone(),
                HttpGrant::new(None, "{}"),
            )
            .unwrap(),
        );
        let log = Arc::new(LogSink::new("com.example.probe".to_owned(), slug.clone()));
        InstanceState::new(
            slug.clone(),
            Arc::from("{}"),
            log,
            64,
            InstanceServices {
                http,
                kv: Arc::new(KvStore::new(
                    pool,
                    KvStore::new_instance_id(),
                    DEFAULT_QUOTA_BYTES,
                )),
                files: Arc::new(FileAccess::default()),
                process: Arc::new(crate::process::ProcessTable::without_tools(
                    "com.example.probe",
                    &slug,
                )),
            },
        )
    }

    /// Creates a migrated temporary database and returns its pool.
    async fn migrated_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        let database = hearthsound_db::Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        hearthsound_db::MIGRATOR
            .run(database.writer())
            .await
            .unwrap();
        let pool = database.writer().clone();
        (dir, pool)
    }

    /// A storage failure reaches a 0.4.0 guest as an error.
    #[tokio::test]
    async fn reports_a_read_failure_as_an_error() {
        let (_dir, pool) = migrated_pool().await;
        pool.close().await;
        let mut state = state(pool);

        let outcome = <InstanceState as host_kv::Host>::get(&mut state, "probe".to_owned())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            Err("the plugin's storage is unavailable".to_owned())
        );
    }

    /// A key that was never written is a miss, not an error.
    #[tokio::test]
    async fn reports_a_missing_key_as_none() {
        let (_dir, pool) = migrated_pool().await;
        let mut state = state(pool);

        let outcome = <InstanceState as host_kv::Host>::get(&mut state, "probe".to_owned())
            .await
            .unwrap();
        assert_eq!(outcome, Ok(None));
    }
}
