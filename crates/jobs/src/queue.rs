//! The persistent job queue.
//!
//! The queue is the `jobs` table plus the statements that move a row through
//! its three states. Everything a caller needs is on [`JobQueue`]: enqueue a
//! job, claim one, extend the claim while the handler runs, complete it, fail
//! it, cancel it, and list what is queued.
//!
//! Claiming is one `UPDATE ... RETURNING` that takes the most urgent due job,
//! so two workers can never hold the same job; the same statement also takes a
//! job whose worker died, which is recognized by its expired `locked_until`.
//! Delivery is therefore at-least-once: a handler must tolerate running for a
//! job it already ran.

use std::time::Duration;

use hearthsound_core::Priority;
use hearthsound_db::Database;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnection;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::clock::{add, now_millis};
use crate::error::JobsError;
use crate::kind::JobKind;

/// How long a claimed job stays locked and how a failed job is retried.
///
/// The composition root builds this from the settings it owns; the queue itself
/// never reads settings.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_jobs::QueueOptions;
///
/// let options = QueueOptions {
///     lock_duration: Duration::from_secs(120),
///     ..QueueOptions::default()
/// };
/// assert_eq!(options.lock_duration, Duration::from_secs(120));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueOptions {
    /// How long a claim is valid before another worker may take the job.
    ///
    /// The runner heartbeats this deadline while a handler runs, so the value
    /// only has to outlive the gap between two heartbeats.
    pub lock_duration: Duration,
    /// The schedule applied to a failed job by [`JobQueue::fail`].
    pub retry: RetryPolicy,
}

impl Default for QueueOptions {
    /// The defaults: a 60 second lock and the retry schedule of the design
    /// documents.
    fn default() -> QueueOptions {
        QueueOptions {
            lock_duration: Duration::from_secs(60),
            retry: RetryPolicy::default(),
        }
    }
}

/// The delays a failed job waits before each of its retries.
///
/// The default schedule is the one the acquisition and scrobble work uses: one
/// minute, five minutes, thirty minutes, two hours, then twelve hours. A job
/// that fails after the last delay is not retried again and moves to the
/// [`JobState::Failed`] state. An empty schedule means "never retry": the first
/// failure is final.
///
/// The delays are fixed rather than jittered so backoff is reproducible in
/// tests; a job that needs jitter should apply it to its own work.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_jobs::RetryPolicy;
///
/// let policy = RetryPolicy::new(vec![Duration::from_millis(50)]);
/// assert_eq!(policy.max_attempts(), 2);
/// assert_eq!(policy.delay_after(1), Some(Duration::from_millis(50)));
/// assert_eq!(policy.delay_after(2), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    delays: Vec<Duration>,
}

impl RetryPolicy {
    /// Builds a policy from the delay before each retry, in order.
    ///
    /// # Parameters
    ///
    /// - `delays`: the delay before retry `n`, where `n` is the number of
    ///   attempts already made. The schedule therefore allows
    ///   `delays.len() + 1` attempts in total.
    ///
    /// # Returns
    ///
    /// The policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use hearthsound_jobs::RetryPolicy;
    ///
    /// let policy = RetryPolicy::new(vec![Duration::from_secs(1), Duration::from_secs(5)]);
    /// assert_eq!(policy.delays().len(), 2);
    /// ```
    pub fn new(delays: Vec<Duration>) -> RetryPolicy {
        RetryPolicy { delays }
    }

    /// Returns the configured delays, in order.
    pub fn delays(&self) -> &[Duration] {
        &self.delays
    }

    /// Returns how many attempts a job is allowed in total.
    ///
    /// # Returns
    ///
    /// The number of retries plus the first attempt, so at least one.
    pub fn max_attempts(&self) -> i64 {
        i64::try_from(self.delays.len())
            .unwrap_or(i64::MAX)
            .saturating_add(1)
    }

    /// Returns the delay before the retry that follows `attempts` attempts.
    ///
    /// # Parameters
    ///
    /// - `attempts`: how many times the job has been claimed.
    ///
    /// # Returns
    ///
    /// The delay to wait, or `None` when the job has used up every attempt and
    /// must be left in the [`JobState::Failed`] state.
    pub fn delay_after(&self, attempts: i64) -> Option<Duration> {
        if attempts < 1 {
            return self.delays.first().copied();
        }
        let index = usize::try_from(attempts - 1).ok()?;
        self.delays.get(index).copied()
    }
}

impl Default for RetryPolicy {
    /// The documented schedule: one minute, five minutes, thirty minutes, two
    /// hours, twelve hours.
    fn default() -> RetryPolicy {
        RetryPolicy {
            delays: vec![
                Duration::from_secs(60),
                Duration::from_secs(300),
                Duration::from_secs(1800),
                Duration::from_secs(7200),
                Duration::from_secs(43200),
            ],
        }
    }
}

/// The state literals stored in `jobs.state`.
///
/// A job is `ready` while it waits for a worker, `running` while a worker holds
/// it, and `failed` once it has used up every attempt; a job that finishes is
/// deleted instead of being marked, so the table holds only outstanding work.
/// These three literals are the values of `jobs.state`, so they must not be
/// renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_jobs::JobState;
///
/// assert_eq!(JobState::Running.as_str(), "running");
/// assert_eq!(JobState::from_literal("failed"), Some(JobState::Failed));
/// assert_eq!(JobState::from_literal("done"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum JobState {
    /// Waiting to be claimed; `run_at` is the earliest time it may run.
    Ready,
    /// Held by a worker until `locked_until`.
    Running,
    /// Out of attempts; kept with `last_error` for an operator to inspect.
    Failed,
}

impl JobState {
    /// Every state, in a fixed order.
    pub const ALL: [JobState; 3] = [JobState::Ready, JobState::Running, JobState::Failed];

    /// Returns the literal stored in `jobs.state`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_jobs::JobState;
    ///
    /// assert_eq!(JobState::Ready.as_str(), "ready");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            JobState::Ready => "ready",
            JobState::Running => "running",
            JobState::Failed => "failed",
        }
    }

    /// Parses the literal stored in `jobs.state`.
    ///
    /// # Parameters
    ///
    /// - `literal`: the literal read from the column.
    ///
    /// # Returns
    ///
    /// The state, or `None` when the literal is not one of the three states.
    pub fn from_literal(literal: &str) -> Option<JobState> {
        JobState::ALL
            .into_iter()
            .find(|state| state.as_str() == literal)
    }
}

impl std::fmt::Display for JobState {
    /// Writes the literal.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What [`JobQueue::fail`] did with a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureOutcome {
    /// The job was put back in the queue and is due again at `run_at`.
    Retried {
        /// When the job may be claimed again, in unix milliseconds.
        run_at: i64,
    },
    /// The job used up its attempts and is now in the [`JobState::Failed`]
    /// state.
    Dead,
}

/// A job to enqueue.
///
/// The defaults are the ones most callers want: the job becomes due
/// immediately, and it runs at [`Priority::Background`] so interactive work
/// sorts ahead of it.
///
/// # Examples
///
/// ```
/// use hearthsound_core::Priority;
/// use hearthsound_jobs::NewJob;
///
/// let job = NewJob::new("library.scan")
///     .priority(Priority::Queue)
///     .payload(&serde_json::json!({ "folder": "music" }))
///     .unwrap();
/// assert_eq!(job.kind().as_str(), "library.scan");
/// ```
#[derive(Debug, Clone)]
pub struct NewJob {
    kind: JobKind,
    payload: Option<String>,
    priority: Priority,
    run_at: i64,
}

impl NewJob {
    /// Builds a job that is due immediately at [`Priority::Background`].
    ///
    /// # Parameters
    ///
    /// - `kind`: the kind the runner matches against a registered handler.
    ///
    /// # Returns
    ///
    /// The job, ready to be tuned with the builder methods below.
    pub fn new(kind: impl Into<JobKind>) -> NewJob {
        NewJob {
            kind: kind.into(),
            payload: None,
            priority: Priority::Background,
            run_at: now_millis(),
        }
    }

    /// Sets the JSON payload a handler reads back from the claimed job.
    ///
    /// # Parameters
    ///
    /// - `value`: the value to encode as JSON.
    ///
    /// # Returns
    ///
    /// The job with its payload set.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Payload`] when `value` cannot be encoded as JSON,
    /// which a serializable value with string map keys never does.
    pub fn payload<T: Serialize>(mut self, value: &T) -> Result<NewJob, JobsError> {
        self.payload = Some(serde_json::to_string(value)?);
        Ok(self)
    }

    /// Sets the payload from already encoded JSON text.
    ///
    /// The text is stored verbatim, so a caller that forwards a payload it read
    /// from a schedule does not have to round-trip it through a decoder.
    ///
    /// # Parameters
    ///
    /// - `json`: the JSON text to store.
    ///
    /// # Returns
    ///
    /// The job with its payload set.
    pub fn payload_text(mut self, json: impl Into<String>) -> NewJob {
        self.payload = Some(json.into());
        self
    }

    /// Sets the priority the job is claimed at.
    ///
    /// # Parameters
    ///
    /// - `priority`: the priority; a lower level is claimed sooner.
    ///
    /// # Returns
    ///
    /// The job with its priority set.
    pub fn priority(mut self, priority: Priority) -> NewJob {
        self.priority = priority;
        self
    }

    /// Sets the earliest time the job may run.
    ///
    /// # Parameters
    ///
    /// - `unix_ms`: the earliest claim time in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The job with its run time set.
    pub fn run_at(mut self, unix_ms: i64) -> NewJob {
        self.run_at = unix_ms;
        self
    }

    /// Sets the earliest time the job may run to `delay` from now.
    ///
    /// # Parameters
    ///
    /// - `delay`: how far in the future the job becomes due.
    ///
    /// # Returns
    ///
    /// The job with its run time set.
    pub fn delay(mut self, delay: Duration) -> NewJob {
        self.run_at = add(now_millis(), delay);
        self
    }

    /// Returns the kind the job will be enqueued under.
    pub fn kind(&self) -> &JobKind {
        &self.kind
    }

    /// Returns the payload that will be stored, as JSON text.
    pub fn payload_json_text(&self) -> Option<&str> {
        self.payload.as_deref()
    }

    /// Returns the earliest time the job may run, in unix milliseconds.
    pub fn run_at_ms(&self) -> i64 {
        self.run_at
    }
}

/// A job read back from the `jobs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The job's id, the primary key of its row.
    pub id: Uuid,
    /// The kind a handler is registered under.
    pub kind: JobKind,
    /// The JSON payload, as it is stored.
    pub payload: Option<String>,
    /// The state the job is in.
    pub state: JobState,
    /// The priority the job was claimed at; a lower level is more urgent.
    pub priority: Priority,
    /// How many times the job has been claimed.
    pub attempts: i64,
    /// The earliest time the job may be claimed, in unix milliseconds.
    pub run_at: i64,
    /// The end of the current claim, in unix milliseconds.
    pub locked_until: Option<i64>,
    /// The error of the last failed attempt.
    pub last_error: Option<String>,
    /// When the job was enqueued, in unix milliseconds.
    pub created_at: i64,
    /// When the job was last written, in unix milliseconds.
    pub updated_at: i64,
}

impl Job {
    /// Decodes the payload into `T`.
    ///
    /// # Returns
    ///
    /// The decoded payload, or `None` when the job carries no payload.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Payload`] when the stored text is not valid JSON
    /// for `T`. A handler should return that error as its own failure so the
    /// job is retried and, if the payload stays broken, ends in the
    /// [`JobState::Failed`] state.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_jobs::{Job, JobState};
    /// use hearthsound_core::Priority;
    /// use uuid::Uuid;
    ///
    /// let job = Job {
    ///     id: Uuid::now_v7(),
    ///     kind: "library.scan".into(),
    ///     payload: Some(String::from("{\"folder\":7}")),
    ///     state: JobState::Running,
    ///     priority: Priority::Background,
    ///     attempts: 1,
    ///     run_at: 0,
    ///     locked_until: Some(1),
    ///     last_error: None,
    ///     created_at: 0,
    ///     updated_at: 0,
    /// };
    /// let folder: Option<serde_json::Value> = job.payload_json().unwrap();
    /// assert_eq!(folder.unwrap()["folder"], 7);
    /// ```
    pub fn payload_json<T: DeserializeOwned>(&self) -> Result<Option<T>, JobsError> {
        match &self.payload {
            Some(text) => Ok(Some(serde_json::from_str(text)?)),
            None => Ok(None),
        }
    }

    /// Returns the payload as the JSON text it is stored as.
    pub fn payload_text(&self) -> Option<&str> {
        self.payload.as_deref()
    }
}

/// The persistent queue of outstanding jobs.
///
/// One queue is shared by the whole process through an [`Arc`](std::sync::Arc):
/// the runner claims from it, handlers enqueue follow-up work on it, and the
/// scheduler enqueues due schedules on it. A queue built on a
/// [`Database`](hearthsound_db::Database) uses that database's writer pool for
/// every write and its reader pool for lookups.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
///
/// use hearthsound_db::Database;
/// use hearthsound_jobs::{JobQueue, QueueOptions};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let db = Database::open(Path::new("/tmp/hearthsound.db")).await?;
/// let queue = JobQueue::new(&db, QueueOptions::default());
///
/// queue.enqueue(hearthsound_jobs::NewJob::new("library.scan")).await?;
/// while let Some(job) = queue.claim().await? {
///     queue.complete(job.id).await?;
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct JobQueue {
    writer: SqlitePool,
    readers: SqlitePool,
    options: QueueOptions,
    notify: Notify,
}

impl JobQueue {
    /// Builds a queue over `db`'s pools.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; the queue clones its writer and reader pools.
    /// - `options`: the lock duration and retry schedule.
    ///
    /// # Returns
    ///
    /// The queue.
    pub fn new(db: &Database, options: QueueOptions) -> JobQueue {
        JobQueue {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            options,
            notify: Notify::new(),
        }
    }

    /// Returns the options the queue was built with.
    pub fn options(&self) -> &QueueOptions {
        &self.options
    }

    /// Wakes every worker waiting for work.
    ///
    /// Enqueuing through [`JobQueue::enqueue`] calls this itself. A caller that
    /// commits its own transaction with [`JobQueue::enqueue_tx`] must call this
    /// after the commit, so a worker does not wait for its next poll. The call
    /// also leaves a permit behind, which covers a worker that is between a
    /// claim that found nothing and the wait that follows it.
    pub fn wake(&self) {
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// Waits until work may have been enqueued.
    ///
    /// The runner uses this to wake immediately on a local enqueue instead of
    /// waiting for its poll interval. The wait may also return spuriously or
    /// after a notification that another worker consumed, so a caller loops
    /// back to [`JobQueue::claim`] rather than assuming a job is available.
    pub async fn wait_for_work(&self) {
        self.notify.notified().await;
    }

    /// Enqueues a job and wakes the workers.
    ///
    /// # Parameters
    ///
    /// - `job`: the job to enqueue.
    ///
    /// # Returns
    ///
    /// The stored job, including the id the queue generated.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the insert fails.
    pub async fn enqueue(&self, job: NewJob) -> Result<Job, JobsError> {
        let mut connection = self.writer.acquire().await?;
        let row = insert(&mut connection, &job).await?;
        drop(connection);
        self.wake();
        row.into_job()
    }

    /// Enqueues a job inside a caller's transaction without waking the workers.
    ///
    /// The scheduler uses this to insert a due schedule's job and advance the
    /// schedule in one transaction. Calling [`JobQueue::wake`] after the commit
    /// is the caller's job, because workers must not see the job before it is
    /// committed.
    ///
    /// # Parameters
    ///
    /// - `connection`: the connection or transaction to write on.
    /// - `job`: the job to enqueue.
    ///
    /// # Returns
    ///
    /// The job as it was inserted.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the insert fails.
    pub async fn enqueue_tx(
        &self,
        connection: &mut SqliteConnection,
        job: NewJob,
    ) -> Result<Job, JobsError> {
        let row = insert(connection, &job).await?;
        row.into_job()
    }

    /// Claims the most urgent job that is due.
    ///
    /// One statement takes the job: it sets the state to `running`, stamps
    /// `locked_until`, counts the attempt, and returns the row. The same
    /// statement reclaims a job that is still `running` with an expired lock,
    /// which is how work survives a worker that died mid-handler.
    ///
    /// # Returns
    ///
    /// The claimed job, or `None` when nothing is due.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails, and the
    /// decoding errors of [`JobsError`] when a row holds an id, priority, or
    /// state the queue does not recognize.
    pub async fn claim(&self) -> Result<Option<Job>, JobsError> {
        let now = now_millis();
        let locked_until = add(now, self.options.lock_duration);
        let row = sqlx::query_as!(
            JobRow,
            "UPDATE jobs
             SET state = 'running', locked_until = ?, updated_at = ?, attempts = attempts + 1
             WHERE id = (
                 SELECT id FROM jobs
                 WHERE (state = 'ready' AND run_at <= ?)
                    OR (state = 'running' AND locked_until IS NOT NULL AND locked_until <= ?)
                 ORDER BY priority ASC, run_at ASC
                 LIMIT 1
             )
             RETURNING id, kind, payload, state, priority, attempts, run_at, locked_until,
                       last_error, created_at, updated_at",
            locked_until,
            now,
            now,
            now,
        )
        .fetch_optional(&self.writer)
        .await?;
        row.map(JobRow::into_job).transpose()
    }

    /// Pushes the end of the current claim out by the lock duration.
    ///
    /// A worker calls this while its handler runs, so a job that takes longer
    /// than one lock duration is not reclaimed by another worker.
    ///
    /// # Parameters
    ///
    /// - `id`: the job to extend.
    ///
    /// # Returns
    ///
    /// `true` when the claim was extended, `false` when the job is no longer
    /// `running` because it finished, was cancelled, or failed elsewhere.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn extend_lock(&self, id: Uuid) -> Result<bool, JobsError> {
        let now = now_millis();
        let locked_until = add(now, self.options.lock_duration);
        let result = sqlx::query!(
            "UPDATE jobs SET locked_until = ?, updated_at = ? WHERE id = ? AND state = 'running'",
            locked_until,
            now,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes a finished job.
    ///
    /// # Parameters
    ///
    /// - `id`: the job to delete.
    ///
    /// # Returns
    ///
    /// `true` when a row was deleted, `false` when the job no longer exists
    /// because it was cancelled or already completed.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn complete(&self, id: Uuid) -> Result<bool, JobsError> {
        let result = sqlx::query!("DELETE FROM jobs WHERE id = ?", id.as_bytes().as_slice())
            .execute(&self.writer)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Removes a job from the queue.
    ///
    /// A job that a worker is already running is deleted too; the handler is not
    /// interrupted, and its later completion is a no-op. A handler that fails
    /// after its job was cancelled finds no row to fail, so the runner only logs
    /// that the job disappeared.
    ///
    /// # Parameters
    ///
    /// - `id`: the job to cancel.
    ///
    /// # Returns
    ///
    /// `true` when a row was deleted.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn cancel(&self, id: Uuid) -> Result<bool, JobsError> {
        self.complete(id).await
    }

    /// Records a failed attempt, retrying the job or leaving it dead.
    ///
    /// The retry schedule decides: while attempts remain the job returns to the
    /// [`JobState::Ready`] state with `run_at` set to now plus the delay for
    /// this attempt, and once they are used up it moves to the
    /// [`JobState::Failed`] state with `last_error` set. Reading the attempt
    /// count and writing the outcome happen in one short transaction so two
    /// failures of the same job cannot both decide its fate.
    ///
    /// # Parameters
    ///
    /// - `id`: the job that failed.
    /// - `error`: the failure to store as the job's last error.
    ///
    /// # Returns
    ///
    /// What happened to the job, or `None` when the job no longer exists.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when a statement fails.
    pub async fn fail(&self, id: Uuid, error: &str) -> Result<Option<FailureOutcome>, JobsError> {
        let mut transaction = self.writer.begin().await?;
        let attempts = sqlx::query_scalar!(
            "SELECT attempts FROM jobs WHERE id = ?",
            id.as_bytes().as_slice(),
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(attempts) = attempts else {
            return Ok(None);
        };

        let now = now_millis();
        let outcome = match self.options.retry.delay_after(attempts) {
            Some(delay) => {
                let run_at = add(now, delay);
                sqlx::query!(
                    "UPDATE jobs
                     SET state = 'ready', run_at = ?, locked_until = NULL, last_error = ?,
                         updated_at = ?
                     WHERE id = ?",
                    run_at,
                    error,
                    now,
                    id.as_bytes().as_slice(),
                )
                .execute(&mut *transaction)
                .await?;
                FailureOutcome::Retried { run_at }
            }
            None => {
                mark_dead(&mut transaction, id, error, now).await?;
                FailureOutcome::Dead
            }
        };
        transaction.commit().await?;
        Ok(Some(outcome))
    }

    /// Moves a job to the [`JobState::Failed`] state without retrying it.
    ///
    /// The runner uses this for a job whose kind has no registered handler: no
    /// number of retries would give it one, so it is kept with `last_error` for
    /// an operator to see.
    ///
    /// # Parameters
    ///
    /// - `id`: the job to fail.
    /// - `error`: the failure to store as the job's last error.
    ///
    /// # Returns
    ///
    /// `true` when the job was marked failed, `false` when it no longer exists.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn fail_permanently(&self, id: Uuid, error: &str) -> Result<bool, JobsError> {
        let mut connection = self.writer.acquire().await?;
        let changed = mark_dead(&mut connection, id, error, now_millis()).await?;
        Ok(changed)
    }

    /// Reads one job.
    ///
    /// # Parameters
    ///
    /// - `id`: the job to read.
    ///
    /// # Returns
    ///
    /// The job, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns the decoding errors of [`JobsError`] for a row the queue does not
    /// recognize.
    pub async fn get(&self, id: Uuid) -> Result<Option<Job>, JobsError> {
        let row = sqlx::query_as!(
            JobRow,
            "SELECT id, kind, payload, state, priority, attempts, run_at, locked_until,
                    last_error, created_at, updated_at
             FROM jobs WHERE id = ?",
            id.as_bytes().as_slice(),
        )
        .fetch_optional(&self.readers)
        .await?;
        row.map(JobRow::into_job).transpose()
    }

    /// Lists the jobs in one state, most urgent first.
    ///
    /// Ordered by priority and then by the time each job is due, which is the
    /// order they are claimed in.
    ///
    /// # Parameters
    ///
    /// - `state`: the state to list.
    /// - `limit`: the maximum number of rows to return.
    ///
    /// # Returns
    ///
    /// The matching jobs.
    ///
    /// # Errors
    ///
    /// Returns the decoding errors of [`JobsError`] for a row the queue does not
    /// recognize.
    pub async fn list_by_state(&self, state: JobState, limit: u32) -> Result<Vec<Job>, JobsError> {
        let rows = sqlx::query_as!(
            JobRow,
            "SELECT id, kind, payload, state, priority, attempts, run_at, locked_until,
                    last_error, created_at, updated_at
             FROM jobs WHERE state = ?
             ORDER BY priority ASC, run_at ASC, id ASC
             LIMIT ?",
            state.as_str(),
            i64::from(limit),
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter().map(JobRow::into_job).collect()
    }

    /// Counts the jobs in one state.
    ///
    /// # Parameters
    ///
    /// - `state`: the state to count.
    ///
    /// # Returns
    ///
    /// How many jobs are in that state.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn count(&self, state: JobState) -> Result<i64, JobsError> {
        let count =
            sqlx::query_scalar!("SELECT COUNT(*) FROM jobs WHERE state = ?", state.as_str())
                .fetch_one(&self.readers)
                .await?;
        Ok(count)
    }
}

/// Inserts one job and returns its row.
///
/// Shared by [`JobQueue::enqueue`] and [`JobQueue::enqueue_tx`] so both write
/// the same columns with the same defaults.
async fn insert(connection: &mut SqliteConnection, job: &NewJob) -> Result<JobRow, JobsError> {
    let id = hearthsound_core::new_uuid_v7();
    let now = now_millis();
    let row = sqlx::query_as!(
        JobRow,
        "INSERT INTO jobs (id, kind, payload, state, priority, attempts, run_at, locked_until,
                           last_error, created_at, updated_at)
         VALUES (?, ?, ?, 'ready', ?, 0, ?, NULL, NULL, ?, ?)
         RETURNING id, kind, payload, state, priority, attempts, run_at, locked_until,
                   last_error, created_at, updated_at",
        id.as_bytes().as_slice(),
        job.kind.as_str(),
        job.payload.as_deref(),
        i64::from(job.priority.level()),
        job.run_at,
        now,
        now,
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(row)
}

/// Marks a job dead, keeping its error.
///
/// # Returns
///
/// `true` when the job existed.
async fn mark_dead(
    connection: &mut SqliteConnection,
    id: Uuid,
    error: &str,
    now: i64,
) -> Result<bool, JobsError> {
    let result = sqlx::query!(
        "UPDATE jobs
         SET state = 'failed', locked_until = NULL, last_error = ?, updated_at = ?
         WHERE id = ?",
        error,
        now,
        id.as_bytes().as_slice(),
    )
    .execute(&mut *connection)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The columns of one `jobs` row, as SQLite returns them.
///
/// Every statement above selects this exact column list, so one mapping
/// function can turn a row into a [`Job`].
struct JobRow {
    id: Vec<u8>,
    kind: String,
    payload: Option<String>,
    state: String,
    priority: i64,
    attempts: i64,
    run_at: i64,
    locked_until: Option<i64>,
    last_error: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl JobRow {
    /// Decodes the stored literals and ids into a [`Job`].
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::InvalidId`] for an id that is not a 16 byte UUID,
    /// [`JobsError::UnknownState`] for an unknown state literal, and
    /// [`JobsError::UnknownPriority`] for a priority level outside the five
    /// levels.
    fn into_job(self) -> Result<Job, JobsError> {
        let id = Uuid::from_slice(&self.id).map_err(|_| JobsError::InvalidId { table: "jobs" })?;
        let state = JobState::from_literal(&self.state).ok_or_else(|| JobsError::UnknownState {
            state: self.state.clone(),
        })?;
        Ok(Job {
            id,
            kind: JobKind::new(self.kind),
            payload: self.payload,
            state,
            priority: priority_from_level(self.priority)?,
            attempts: self.attempts,
            run_at: self.run_at,
            locked_until: self.locked_until,
            last_error: self.last_error,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Maps a stored priority level back to a [`Priority`].
///
/// # Errors
///
/// Returns [`JobsError::UnknownPriority`] when no priority has that level.
fn priority_from_level(level: i64) -> Result<Priority, JobsError> {
    u8::try_from(level)
        .ok()
        .and_then(|level| {
            Priority::ALL
                .into_iter()
                .find(|priority| priority.level() == level)
        })
        .ok_or(JobsError::UnknownPriority { level })
}
