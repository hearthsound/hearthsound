//! Noticing the writes made by other connections.
//!
//! A running server is not the only process that opens the database: an operator
//! running a CLI admin command writes settings, users, library folders, plugins
//! and layer assignments directly to the same file. Rather than polling the
//! tables, the server polls `PRAGMA data_version`, whose value changes inside a
//! connection whenever a different connection commits. The pragma is cheap to
//! read, and because the server owns the only writer connection, its own commits
//! never trigger a notification.
//!
//! [`Database::watch_data_version`] starts the polling task;
//! [`DataVersionWatcher`] owns it and [`DataVersionSubscription`]s observe it.

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::Database;

/// The shortest interval the watcher polls at.
///
/// A caller that asks for a shorter interval - including zero - gets this one,
/// so a misconfigured value cannot spin the polling task.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

impl Database {
    /// Starts watching for commits made by other connections.
    ///
    /// The watcher polls `PRAGMA data_version` on the writer pool: SQLite
    /// changes the value a connection sees whenever a different connection
    /// commits to the same database file, and leaves it alone for the
    /// connection's own writes. Each poll takes the writer connection, reads the
    /// pragma and releases the connection again, so a poll never holds the
    /// single writer connection.
    ///
    /// The baseline is read before this method returns, so a commit that lands
    /// after it is always reported to subscribers. A value the watcher cannot
    /// attribute - the pool replacing its writer connection restarts that
    /// connection's local value - is reported like any other change.
    ///
    /// # Parameters
    ///
    /// - `poll_interval`: how long to wait between polls. Values below one
    ///   millisecond are raised to one millisecond. The server passes
    ///   [`DATA_VERSION_POLL_INTERVAL`](crate::DATA_VERSION_POLL_INTERVAL);
    ///   tests use shorter intervals.
    ///
    /// # Returns
    ///
    /// A [`DataVersionWatcher`] that owns the polling task. Dropping it, or
    /// calling [`DataVersionWatcher::shutdown`], stops the task. The watcher
    /// keeps the writer pool alive, so stop it before closing the database.
    ///
    /// # Panics
    ///
    /// Panics when there is no Tokio runtime to spawn the polling task on.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use hearthsound_db::{DATA_VERSION_POLL_INTERVAL, Database};
    ///
    /// # async fn example(db: &Database) {
    /// let watcher = db.watch_data_version(DATA_VERSION_POLL_INTERVAL).await;
    /// let mut changes = watcher.subscribe();
    ///
    /// while changes.changed().await {
    ///     // Reload whatever a CLI command may have written.
    /// }
    /// watcher.shutdown().await;
    /// # }
    /// ```
    pub async fn watch_data_version(&self, poll_interval: Duration) -> DataVersionWatcher {
        let interval = poll_interval.max(MIN_POLL_INTERVAL);

        // Read the baseline here, before the caller can load state of its own,
        // so no commit can slip between the baseline and the caller's load.
        let baseline = match read_data_version(&self.writer).await {
            Ok(version) => Some(version),
            Err(error) => {
                tracing::warn!(%error, "could not read the data version of the database");
                None
            }
        };

        let (changes, _) = watch::channel(0_u64);
        let (shutdown, stopped) = oneshot::channel();
        let task = tokio::spawn(poll(
            self.writer.clone(),
            interval,
            baseline,
            changes.clone(),
            stopped,
        ));

        DataVersionWatcher {
            changes,
            shutdown,
            task,
        }
    }
}

/// A running [`Database::watch_data_version`] task and the handle that stops it.
///
/// Dropping the watcher stops the task; [`DataVersionWatcher::shutdown`] stops
/// it and waits for it to finish.
#[derive(Debug)]
#[must_use = "the polling task stops when the watcher is dropped"]
pub struct DataVersionWatcher {
    changes: watch::Sender<u64>,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl DataVersionWatcher {
    /// Subscribes to the watcher's notifications.
    ///
    /// Every call returns an independent subscription. Commit notifications are
    /// coalesced: a subscriber that is busy while several connections commit
    /// wakes up once, which is all a reload needs. A subscription ends - its
    /// [`changed`](DataVersionSubscription::changed) returns `false` - once the
    /// watcher is shut down or dropped.
    ///
    /// # Returns
    ///
    /// A [`DataVersionSubscription`] that starts out without a pending
    /// notification.
    pub fn subscribe(&self) -> DataVersionSubscription {
        DataVersionSubscription {
            changes: self.changes.subscribe(),
        }
    }

    /// Stops the polling task and waits for it to exit.
    ///
    /// A poll that already runs finishes first; polls do not occupy the writer
    /// connection between statements. Every subscription ends before this method
    /// returns.
    pub async fn shutdown(self) {
        let DataVersionWatcher {
            changes,
            shutdown,
            task,
        } = self;
        // Fails only when the task already stopped, which needs no handling.
        let _ = shutdown.send(());
        let _ = task.await;
        // Dropping the last sender ends every subscription.
        drop(changes);
    }
}

/// A subscriber to a [`DataVersionWatcher`].
///
/// Returned by [`DataVersionWatcher::subscribe`]. Wait on
/// [`changed`](DataVersionSubscription::changed) in a loop and end the loop when
/// it reports that the watcher stopped.
#[derive(Debug)]
pub struct DataVersionSubscription {
    changes: watch::Receiver<u64>,
}

impl DataVersionSubscription {
    /// Waits until another connection commits, or until the watcher stops.
    ///
    /// # Returns
    ///
    /// `true` once the database changed since the last call; `false` when the
    /// watcher stopped, after which no notification can arrive.
    pub async fn changed(&mut self) -> bool {
        self.changes.changed().await.is_ok()
    }
}

/// Polls `PRAGMA data_version` until the shutdown signal arrives.
///
/// `current` holds the value [`Database::watch_data_version`] read, or `None`
/// when that read failed; the first successful poll then only records the
/// baseline. Every later poll whose value differs from the recorded one notifies
/// `changes`, whatever the direction of the difference.
async fn poll(
    pool: SqlitePool,
    interval: Duration,
    mut current: Option<i64>,
    changes: watch::Sender<u64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        // Wait for the next poll, or stop as soon as the watcher asks for it.
        // The receiver also resolves when the watcher is dropped.
        if tokio::time::timeout(interval, &mut shutdown).await.is_ok() {
            break;
        }

        match read_data_version(&pool).await {
            Ok(version) => {
                if current != Some(version) {
                    if current.is_some() {
                        changes.send_modify(|generation| *generation += 1);
                    }
                    current = Some(version);
                }
            }
            Err(error) => tracing::warn!(%error, "could not read the data version of the database"),
        }
    }
}

/// Reads `PRAGMA data_version` on a connection taken from `pool`.
///
/// The connection returns to the pool as soon as the statement is done, so
/// polling does not hold the single writer connection between polls.
///
/// # Errors
///
/// Returns the [`sqlx::Error`] of the pool or of the statement, for example when
/// the pool is closed.
async fn read_data_version(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(pool)
        .await
}
