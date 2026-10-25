//! Reacting to writes made by other connections.
//!
//! The command-line commands and the server share one SQLite file, and a
//! command-line command has no way to reach the running process: it opens the
//! database and writes to it. The running server therefore watches
//! `PRAGMA data_version` on a read-only connection of its own, which changes for
//! every commit another connection makes - the server's own included - and asks
//! the writer connection whether the change was its own before it reloads
//! anything, so the server's own writes never reload it.
//!
//! [`Services::watch_external_changes`] is that worker. A detected commit
//! reloads the runtime settings, rebuilds the provider registry, which
//! re-reads the plugin, instance, and layer tables, and reconciles the
//! acquisition ledger, which is how a cancellation written by a command line
//! stops the worker that is still serving that acquisition; a change to the
//! resolved settings additionally reapplies the settings the services cache a
//! copy of, namely the ffmpeg concurrency cap. Users and credentials are read
//! per request, so a user a command created authenticates without any reload.
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
        // An instance the host switches off is written straight to the database
        // by the server's own connection, which `data_version` does not move for,
        // so the host signals it in-process instead.
        let mut disabled = self.plugins().subscribe_instance_disabled();
        // A rebuild that failed after a write committed leaves the registry
        // serving the previous entries; this arm retries it without waiting for
        // another admin write. A sleep rather than an interval, because an
        // interval's first tick is immediate and would rebuild a second time
        // beside the write that just ran.
        let retry_period = DATA_VERSION_POLL_INTERVAL;

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
                    self.reconcile_watchers().await;
                }
                received = disabled.recv() => {
                    match received {
                        Ok(slug) => {
                            tracing::info!(
                                instance = %slug,
                                "an instance was switched off, so the registry is rebuilt"
                            );
                            self.reload().await;
                        }
                        // A lagging receiver missed events; the rebuild below
                        // covers all of them.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            self.reload().await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                () = tokio::time::sleep(retry_period) => {
                    if self.registry().is_stale() {
                        tracing::info!("retrying a provider registry rebuild that failed");
                        self.reload().await;
                    }
                }
                () = shutdown.wait() => break,
            }
        }

        watcher.shutdown().await;
    }

    /// Reloads the settings and rebuilds the provider registry.
    ///
    /// The acquisition ledger is reconciled afterwards, because the same
    /// external write may have cancelled an acquisition: a command line
    /// cancellation is written straight to the database, and only the running
    /// process can stop the worker that still serves that acquisition.
    ///
    /// The settings the services cache a copy of are reapplied here as well,
    /// not only in the settings arm of
    /// [`Services::watch_external_changes`]. The server's own write wakes both
    /// arms at once - the polling connection sees the commit and the store
    /// publishes its new snapshot - and a notification a `select!` branch
    /// observed but did not run is gone, so a reload must not depend on the
    /// other arm still firing.
    ///
    /// # Parameters
    ///
    /// - `self`: the services whose stored layers are re-read.
    pub async fn reload(&self) {
        match self.settings().reload().await {
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "could not reload the runtime settings"),
        }
        self.apply_cached_settings();
        match self.registry().rebuild_or_mark_stale().await {
            Ok(report) => log_rebuild(&report),
            Err(error) => tracing::error!(%error, "could not rebuild the provider registry"),
        }
        match self.orchestrator().reconcile().await {
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "could not reconcile the acquisitions"),
        }
        self.reconcile_watchers().await;
        // A folder owner, a like or a setting written elsewhere can change where
        // a managed file belongs.
        self.maintenance().notify();
        tracing::info!("reloaded the settings and providers after an external write");
    }

    /// Brings the folder watchers in line with the stored folders.
    ///
    /// The watched flag, the folder paths, and the debounce period are all
    /// runtime state, so a write that changes one of them takes effect here
    /// rather than at the next restart. A failure is logged and the watchers
    /// keep watching what they already watch.
    ///
    /// # Parameters
    ///
    /// - `self`: the services whose watchers are reconciled.
    async fn reconcile_watchers(&self) {
        match self.watcher().reconcile().await {
            Ok(()) => {}
            Err(error) => tracing::error!(%error, "could not reconcile the folder watchers"),
        }
    }

    /// Reapplies the settings the services keep a copy of, and wakes the
    /// storage maintenance worker.
    ///
    /// Only the ffmpeg concurrency cap is cached today; every other reader takes
    /// the current snapshot when it needs it. A storage setting such as
    /// `storage.cache.max_bytes` takes effect at the next maintenance pass, so a
    /// settings change asks for one.
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
        self.maintenance().notify();
    }
}
