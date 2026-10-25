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
//! The value is only comparable between two reads on the same connection, so the
//! poll owns one read-only connection of its own: a pool that handed out
//! different connections would report phantom changes or miss real ones, and the
//! writer connection the pragma used to be read on is the one every write needs.
//!
//! A connection of its own sees the server's own commits as well, which a poll on
//! the writer connection did not, so a detected change is attributed: the writer
//! connection's own value stays put across its own commits and moves for
//! everyone else's. Reading it takes no lock and no wait - a free writer is asked
//! with `try_acquire` - and a change that arrives while a write is in flight
//! cannot be attributed, so it is reported: a missed reload of a command-line
//! change is worse than one extra reload.
//!
//! [`Database::watch_data_version`] starts the polling task;
//! [`DataVersionWatcher`] owns it and [`DataVersionSubscription`]s observe it.

use std::time::Duration;

use sqlx::{Connection as _, SqliteConnection};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::{Database, read_only_options};

/// The shortest interval the watcher polls at.
///
/// A caller that asks for a shorter interval - including zero - gets this one,
/// so a misconfigured value cannot spin the polling task.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

impl Database {
    /// Starts watching for commits made by other connections.
    ///
    /// The watcher polls `PRAGMA data_version` on one read-only connection it
    /// owns: SQLite changes the value a connection sees whenever a different
    /// connection commits to the same database file, and leaves it alone for the
    /// connection's own writes. The value is only comparable between two reads on
    /// the same connection, which is why the poll holds a connection of its own
    /// rather than borrowing one from a pool, and why it never touches the
    /// single writer connection every write queues behind.
    ///
    /// The baseline is read before this method returns, so a commit that lands
    /// after it is always reported to subscribers. When the read-only connection
    /// cannot be opened, or the baseline cannot be read, the watcher starts
    /// without a baseline and the first successful poll records one; a watcher
    /// that cannot connect at all never reports anything.
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

        // Read the writer's baseline first, then the poll connection's, so a
        // commit that lands between the two is attributed to the writer rather
        // than mistaken for the server's own.
        let writer_current = match self.writer.acquire().await {
            Ok(mut connection) => read_data_version(&mut connection).await.ok(),
            Err(error) => {
                tracing::warn!(%error, "could not read the data version of the database");
                None
            }
        };
        let connection = SqliteConnection::connect_with(&read_only_options(&self.path)).await;
        let poller = match connection {
            Ok(mut connection) => {
                let current = match read_data_version(&mut connection).await {
                    Ok(version) => Some(version),
                    Err(error) => {
                        tracing::warn!(%error, "could not read the data version of the database");
                        None
                    }
                };
                Some(Poller {
                    connection,
                    current,
                    writer: self.writer.clone(),
                    writer_current,
                })
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not open the read-only connection the data version is polled on; \
                     nothing is watched for external writes"
                );
                None
            }
        };

        let (changes, _) = watch::channel(0_u64);
        let (shutdown, stopped) = oneshot::channel();
        let sender = changes.clone();
        let task = tokio::spawn(async move {
            if let Some(poller) = poller {
                poll(poller, interval, sender, stopped).await;
            }
        });

        DataVersionWatcher {
            changes,
            shutdown,
            task,
        }
    }
}

/// A running [`Database::watch_data_version`] task and the handle that stops it.
///
/// The task owns its read-only connection, so stopping it closes that connection
/// and the writer pool is untouched.
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

/// Polls `PRAGMA data_version` on its own connection until the shutdown signal
/// arrives.
///
/// # Parameters
///
/// - `poller`: the poll's state: its connection, the value it read last and the
///   writer connection it attributes a change against.
/// - `interval`: how long to wait between polls.
/// - `changes`: the sender subscribers are notified through.
/// - `shutdown`: resolves when the watcher stops.
async fn poll(
    mut poller: Poller,
    interval: Duration,
    changes: watch::Sender<u64>,
    mut shutdown: oneshot::Receiver<()>,
) {
    loop {
        // Wait for the next poll, or stop as soon as the watcher asks for it.
        // The receiver also resolves when the watcher is dropped.
        if tokio::time::timeout(interval, &mut shutdown).await.is_ok() {
            break;
        }
        match poller.foreign_commit().await {
            Ok(true) => changes.send_modify(|generation| *generation += 1),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "could not read the data version of the database"),
        }
    }
}

/// One poll's connection and the baselines it compares against.
struct Poller {
    /// The read-only connection the value is read on; its value only means
    /// something to itself.
    connection: SqliteConnection,
    /// The value that connection read last, or `None` when the baseline could
    /// not be read, in which case the first poll only records one.
    current: Option<i64>,
    /// The writer pool, which is touched only to attribute a change.
    writer: sqlx::SqlitePool,
    /// The writer connection's own value at the last attribution.
    writer_current: Option<i64>,
}

/// How often the attribution retries a busy writer before it gives up.
const ATTRIBUTION_RETRIES: u32 = 4;

/// How long the attribution waits between two attempts at a busy writer.
const ATTRIBUTION_RETRY_DELAY: Duration = Duration::from_millis(2);

impl Poller {
    /// Reads the data version and reports whether another connection committed.
    ///
    /// # Returns
    ///
    /// `true` when a connection other than the server's writer committed since
    /// the last poll, `false` when nothing changed or the change was the
    /// server's own write.
    ///
    /// # Errors
    ///
    /// Returns the [`sqlx::Error`] of the read, for example when the connection
    /// is closed.
    async fn foreign_commit(&mut self) -> Result<bool, sqlx::Error> {
        let version = read_data_version(&mut self.connection).await?;
        if self.current == Some(version) {
            return Ok(false);
        }
        let first = self.current.is_none();
        self.current = Some(version);
        if first {
            return Ok(false);
        }
        self.attribute().await
    }

    /// Reports whether the change the poll saw came from another connection.
    ///
    /// The writer connection's own value does not move for its own commits, so a
    /// difference from the last attribution means another connection committed.
    /// A writer that stays busy through every attempt cannot be asked, and the
    /// change is reported: a command-line change that is never acted on is worse
    /// than one unnecessary reload.
    ///
    /// # Returns
    ///
    /// `true` when the change came from another connection.
    ///
    /// # Errors
    ///
    /// Returns the [`sqlx::Error`] of the pragma read.
    async fn attribute(&mut self) -> Result<bool, sqlx::Error> {
        for attempt in 0..=ATTRIBUTION_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(ATTRIBUTION_RETRY_DELAY).await;
            }
            // `try_acquire` never waits: a writer mid-write answers `None`.
            let mut connection = match self.writer.try_acquire() {
                Some(connection) => connection,
                None => continue,
            };
            let version = read_data_version(&mut connection).await?;
            let foreign = self.writer_current != Some(version);
            self.writer_current = Some(version);
            return Ok(foreign);
        }
        Ok(true)
    }
}

/// Reads `PRAGMA data_version` on one connection.
///
/// # Parameters
///
/// - `connection`: the connection to read on; the value only means something to
///   that connection, which is why the poll owns one of its own.
///
/// # Returns
///
/// The connection's data version.
///
/// # Errors
///
/// Returns the [`sqlx::Error`] of the statement, for example when the connection
/// is closed.
async fn read_data_version(connection: &mut SqliteConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("PRAGMA data_version")
        .fetch_one(&mut *connection)
        .await
}
