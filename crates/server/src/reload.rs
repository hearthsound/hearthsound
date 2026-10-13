//! Reacting to writes made by other connections.
//!
//! The command-line commands and the server share one SQLite file, and a
//! command-line command has no way to reach the running process: it opens the
//! database and writes to it. The running server therefore watches
//! `PRAGMA data_version`, which SQLite changes only for a connection when
//! *another* connection commits, and reloads what a command may have changed
//! when it moves.
//!
//! [`Services::watch_external_changes`] is that worker. A detected commit
//! reloads the runtime settings and rebuilds the provider registry, which
//! re-reads the plugin, instance, and layer tables; a change to the resolved
//! settings additionally reapplies the settings the services cache a copy of,
//! namely the ffmpeg concurrency cap. Users and credentials are read per
//! request, so a user a command created authenticates without any reload.
//!
//! The watcher polls once a second ([`DATA_VERSION_POLL_INTERVAL`]), so a
//! command-line change takes effect within about a second without a restart.

use std::sync::Arc;

use hearthsound_db::DATA_VERSION_POLL_INTERVAL;
use hearthsound_jobs::Shutdown;

use crate::services::{Services, log_rebuild, max_concurrent_transcodes};

impl Services {
    /// Runs until `shutdown`, reloading what another connection changed.
    ///
    /// The worker owns the data-version watcher, so stopping it stops the
    /// polling task and every subscription. A reload failure is logged rather
    /// than fatal: the next external commit tries again, and the server keeps
    /// serving the state it has.
    ///
    /// # Parameters
    ///
    /// - `self`: the services, shared with the rest of the process.
    /// - `shutdown`: the signal that stops the worker.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # async fn example(services: Arc<hearthsound_server::Services>) {
    /// let shutdown = hearthsound_jobs::Shutdown::new();
    /// tokio::spawn(Arc::clone(&services).watch_external_changes(shutdown));
    /// # }
    /// ```
    pub async fn watch_external_changes(self: Arc<Self>, shutdown: Shutdown) {
        let watcher = self
            .database()
            .watch_data_version(DATA_VERSION_POLL_INTERVAL)
            .await;
        let mut commits = watcher.subscribe();
        let mut settings_changes = self.settings().subscribe();

        loop {
            tokio::select! {
                changed = commits.changed() => {
                    if !changed {
                        break;
                    }
                    self.reload().await;
                }
                changed = settings_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.apply_cached_settings();
                }
                () = shutdown.wait() => break,
            }
        }

        watcher.shutdown().await;
    }

    /// Reloads the settings and rebuilds the provider registry.
    ///
    /// # Parameters
    ///
    /// - `self`: the services whose stored layers are re-read.
    pub async fn reload(&self) {
        match self.settings().reload().await {
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "could not reload the runtime settings"),
        }
        match self.registry().rebuild().await {
            Ok(report) => log_rebuild(&report),
            Err(error) => tracing::error!(%error, "could not rebuild the provider registry"),
        }
        tracing::info!("reloaded the settings and providers after an external write");
    }

    /// Reapplies the settings the services keep a copy of.
    ///
    /// Only the ffmpeg concurrency cap is cached today; every other reader takes
    /// the current snapshot when it needs it.
    ///
    /// # Parameters
    ///
    /// - `self`: the services whose cached copies are brought up to date.
    pub fn apply_cached_settings(&self) {
        let snapshot = self.settings().current();
        match max_concurrent_transcodes(&snapshot) {
            Ok(cap) if self.ffmpeg().max_concurrent() != cap => {
                self.ffmpeg().set_max_concurrent(cap);
                tracing::info!(cap, "updated the ffmpeg concurrency cap");
            }
            Ok(_) => {}
            Err(error) => {
                tracing::error!(%error, "could not read the ffmpeg concurrency cap");
            }
        }
    }
}
