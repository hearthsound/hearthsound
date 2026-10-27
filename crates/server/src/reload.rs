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
use std::time::Duration;

use hearthsound_db::DATA_VERSION_POLL_INTERVAL;
use hearthsound_jobs::Shutdown;

use crate::error::ServerError;
use crate::server::with_retries;
use crate::services::{Services, log_rebuild, max_concurrent_transcodes};

/// The waits before a failed reload is tried again.
///
/// An external write can land while the database is busy, or while a plugin host
/// is being rebuilt; without a retry the process would serve the previous
/// settings and registry until the next write from another connection, which may
/// never come. The waits are short enough to stay inside one operator action and
/// long enough to outlast a contended write.
const RELOAD_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

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
                    Arc::clone(&self).reload_with_retries(&shutdown).await;
                }
                changed = settings_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    self.apply_cached_settings();
                    self.reconcile_watchers().await;
                    // A precedence change reorders the stored fields of every
                    // entity that holds provider answers, which is a database
                    // pass and not a provider call.
                    self.enqueue_recompute().await;
                }
                received = disabled.recv() => {
                    match received {
                        Ok(slug) => {
                            tracing::info!(
                                instance = %slug,
                                "an instance was switched off, so the registry is rebuilt"
                            );
                            Arc::clone(&self).reload_with_retries(&shutdown).await;
                        }
                        // A lagging receiver missed events; the rebuild below
                        // covers all of them.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            Arc::clone(&self).reload_with_retries(&shutdown).await;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                () = tokio::time::sleep(retry_period) => {
                    if self.registry().is_stale() {
                        tracing::info!("retrying a provider registry rebuild that failed");
                        Arc::clone(&self).reload_with_retries(&shutdown).await;
                    }
                }
                () = shutdown.wait() => break,
            }
        }

        watcher.shutdown().await;
    }

    /// Queues the field recomputation a settings or layer change needs.
    ///
    /// # Errors
    ///
    /// A queue that refuses the job is logged: the next change queues it again,
    /// and the stored fields are still correct for the previous precedence.
    pub async fn enqueue_recompute(&self) {
        if let Err(error) = self
            .queue()
            .enqueue(hearthsound_jobs::NewJob::new(
                crate::index_jobs::INDEX_RECOMPUTE_JOB_KIND,
            ))
            .await
        {
            tracing::warn!(%error, "could not queue the field recomputation");
        }
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
        if let Err(error) = self.reload_once().await {
            tracing::error!(%error, "the reload failed");
        }
    }

    /// Reloads, retrying a failure before leaving it to the next external write.
    ///
    /// The retries run on [`RELOAD_RETRY_DELAYS`], and a stop signal abandons
    /// them. After the last delay the failure is logged once and the watcher goes
    /// back to waiting for the next write from another connection.
    ///
    /// # Parameters
    ///
    /// - `self`: the services whose stored layers are re-read.
    /// - `shutdown`: the signal that abandons the retries.
    async fn reload_with_retries(self: Arc<Self>, shutdown: &Shutdown) {
        let reloaded = with_retries(
            &RELOAD_RETRY_DELAYS,
            shutdown,
            "could not reload after an external write",
            |_attempt| {
                let services = Arc::clone(&self);
                async move { services.reload_once().await }
            },
        )
        .await;
        if reloaded.is_none() {
            tracing::error!("the reload still fails; waiting for the next external write");
        }
    }

    /// Runs one reload and reports the first step that failed.
    ///
    /// Every step runs even after a failure, because they are independent: a
    /// settings reload that failed must not stop the provider registry from
    /// catching up with the plugin tables.
    ///
    /// # Returns
    ///
    /// `Ok(())` when every step succeeded.
    ///
    /// # Errors
    ///
    /// Returns the first failure, after the later steps ran.
    async fn reload_once(&self) -> Result<(), ServerError> {
        let mut failure = None;
        if let Err(error) = self.settings().reload().await {
            failure.get_or_insert(error.into());
        }
        self.apply_cached_settings();
        match self.registry().rebuild_or_mark_stale().await {
            Ok(report) => {
                log_rebuild(&report);
                if let Err(error) = self.orchestrator().providers_changed().await {
                    failure.get_or_insert(error.into());
                }
            }
            Err(error) => {
                failure.get_or_insert(error.into());
            }
        }
        if let Err(error) = self.orchestrator().reconcile().await {
            failure.get_or_insert(error.into());
        }
        self.reconcile_watchers().await;
        // A folder owner, a like or a setting written elsewhere can change where
        // a managed file belongs.
        self.maintenance().notify();
        match failure {
            Some(error) => Err(error),
            None => {
                tracing::info!("reloaded the settings and providers after an external write");
                Ok(())
            }
        }
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Waits as short as the retry helper allows while keeping the schedule's
    /// length, so a test does not wait the real half minute.
    fn short_retry_delays() -> Vec<Duration> {
        vec![Duration::from_millis(1); RELOAD_RETRY_DELAYS.len()]
    }

    #[test]
    fn the_reload_retry_schedule_is_the_documented_one() {
        assert_eq!(
            RELOAD_RETRY_DELAYS,
            [
                Duration::from_secs(1),
                Duration::from_secs(5),
                Duration::from_secs(30)
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_reload_is_retried_within_one_commit() {
        let shutdown = Shutdown::new();
        let calls = AtomicU32::new(0);
        let reloaded: Option<()> = with_retries(
            &short_retry_delays(),
            &shutdown,
            "could not reload after an external write",
            |_attempt| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if attempt == 1 {
                        Err("the database is busy")
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;
        assert_eq!(reloaded, Some(()), "the second attempt succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_reload_that_never_succeeds_uses_up_its_attempts() {
        let shutdown = Shutdown::new();
        let calls = AtomicU32::new(0);
        let reloaded: Option<()> = with_retries(
            &short_retry_delays(),
            &shutdown,
            "could not reload after an external write",
            |_attempt| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { Err("the database is busy") }
            },
        )
        .await;
        assert_eq!(reloaded, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            u32::try_from(RELOAD_RETRY_DELAYS.len()).unwrap() + 1,
            "one delay per retry, plus the first attempt"
        );
    }
}
