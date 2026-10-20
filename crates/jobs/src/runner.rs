//! The job runner: the workers that execute queued jobs.
//!
//! A runner is built once by the composition root, which registers one async
//! handler per job kind, and is then started with [`JobRunner::run`]. That call
//! spawns a fixed number of workers, and each worker repeatedly claims a job
//! from the [`JobQueue`] and hands it to the handler registered for its kind:
//!
//! - a job whose kind has no handler is failed without a retry, because no
//!   number of retries would give it one;
//! - a handler that returns an error fails the job, which the queue retries
//!   according to its retry schedule until the job is left dead;
//! - a handler that takes longer than one lock duration is protected by a
//!   heartbeat that pushes the claim's deadline out;
//! - the workers sleep on a poll interval, but a local enqueue wakes one
//!   immediately, so latency does not come from polling.
//!
//! Shutdown is cooperative: triggering the [`Shutdown`] stops the workers from
//! claiming new work, and [`JobRunner::run`] then waits for the handlers that
//! are already running, up to the configured timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::task::JoinSet;

use crate::error::JobsError;
use crate::kind::JobKind;
use crate::queue::{FailureOutcome, Job, JobQueue};
use crate::shutdown::Shutdown;

/// How a handler reports that a job could not be done.
///
/// The message is stored in `jobs.last_error`, so it must name the failure the
/// way an operator reading the `failed` rows needs it.
///
/// # Examples
///
/// ```
/// use hearthsound_jobs::JobError;
///
/// let error = JobError::new("the folder is not readable");
/// assert_eq!(error.message(), "the folder is not readable");
/// assert_eq!(error.to_string(), "the folder is not readable");
/// ```
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct JobError {
    message: String,
}

impl JobError {
    /// Builds a failure from a message.
    ///
    /// # Parameters
    ///
    /// - `message`: what went wrong, phrased for an operator.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn new(message: impl Into<String>) -> JobError {
        JobError {
            message: message.into(),
        }
    }

    /// Returns the message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<JobsError> for JobError {
    /// Reports a queue or database error as a job failure.
    fn from(error: JobsError) -> JobError {
        JobError::new(error.to_string())
    }
}

/// One job kind's work.
///
/// A handler is registered under exactly one [`JobKind`] and is shared by every
/// worker, so it must be `Send + Sync`. It owns whatever it needs - usually a
/// clone of the [`JobQueue`] for follow-up work and the services the job acts
/// on - because the runner constructs nothing for it.
///
/// # Examples
///
/// ```
/// use async_trait::async_trait;
/// use hearthsound_jobs::{Job, JobError, JobHandler};
///
/// struct Log;
///
/// #[async_trait]
/// impl JobHandler for Log {
///     async fn handle(&self, job: Job) -> Result<(), JobError> {
///         tracing::debug!(kind = %job.kind, "handling a job");
///         Ok(())
///     }
/// }
/// ```
#[async_trait]
pub trait JobHandler: Send + Sync + 'static {
    /// Runs one job.
    ///
    /// # Parameters
    ///
    /// - `job`: the claimed job, including its payload and its attempt count.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the job is done, which deletes it from the queue, or
    /// [`JobError`] when it failed, which fails the job in the queue.
    async fn handle(&self, job: Job) -> Result<(), JobError>;
}

/// How the runner executes jobs.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_jobs::JobRunnerOptions;
///
/// let options = JobRunnerOptions {
///     workers: 2,
///     ..JobRunnerOptions::default()
/// };
/// assert_eq!(options.workers, 2);
/// assert_eq!(options.poll_interval, Duration::from_secs(5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRunnerOptions {
    /// How many jobs run concurrently; a value of zero is treated as one.
    pub workers: usize,
    /// How long a worker waits for work before claiming again.
    ///
    /// This is the fallback for a job enqueued by another process - a CLI
    /// command, or a second server - which cannot wake this process's workers.
    pub poll_interval: Duration,
    /// How often a running job's claim is extended.
    ///
    /// Must be smaller than the queue's lock duration, so a long handler is
    /// never treated as abandoned.
    pub heartbeat_interval: Duration,
    /// How long shutdown waits for in-flight handlers before abandoning them.
    pub shutdown_timeout: Duration,
}

impl Default for JobRunnerOptions {
    /// The defaults: four workers, a five second poll, a twenty second
    /// heartbeat, and a thirty second shutdown grace period.
    fn default() -> JobRunnerOptions {
        JobRunnerOptions {
            workers: 4,
            poll_interval: Duration::from_secs(5),
            heartbeat_interval: Duration::from_secs(20),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// The workers that execute queued jobs.
///
/// A runner borrows nothing: it holds an [`Arc`] of the queue and of its
/// handlers, so it can be started from any task and shut down from another one.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use std::sync::Arc;
///
/// use hearthsound_jobs::{
///     JobQueue, JobRunner, JobRunnerOptions, QueueOptions, Shutdown,
/// };
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let db = hearthsound_db::Database::open(Path::new("/tmp/hearthsound.db")).await?;
/// let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
///
/// let mut runner = JobRunner::new(Arc::clone(&queue), JobRunnerOptions::default());
/// // register one handler per job kind here
///
/// let shutdown = Shutdown::new();
/// let stopping = shutdown.clone();
/// tokio::spawn(async move {
///     tokio::time::sleep(std::time::Duration::from_secs(1)).await;
///     stopping.trigger();
/// });
/// runner.run(shutdown).await;
/// # Ok(())
/// # }
/// ```
pub struct JobRunner {
    queue: Arc<JobQueue>,
    handlers: Arc<HashMap<JobKind, Arc<dyn JobHandler>>>,
    options: JobRunnerOptions,
}

impl JobRunner {
    /// Builds a runner over `queue` with no handlers registered yet.
    ///
    /// # Parameters
    ///
    /// - `queue`: the queue the workers claim from.
    /// - `options`: the worker count, poll interval, heartbeat, and shutdown
    ///   grace period.
    ///
    /// # Returns
    ///
    /// The runner. Register every handler before calling
    /// [`JobRunner::run`].
    pub fn new(queue: Arc<JobQueue>, options: JobRunnerOptions) -> JobRunner {
        JobRunner {
            queue,
            handlers: Arc::new(HashMap::new()),
            options,
        }
    }

    /// Registers the handler for one job kind.
    ///
    /// Registering a kind twice replaces the earlier handler, which makes a
    /// duplicate registration harmless but also easy to miss; the composition
    /// root is the one place that decides which kinds exist.
    ///
    /// # Parameters
    ///
    /// - `kind`: the kind the handler runs.
    /// - `handler`: the handler, shared by every worker.
    ///
    /// # Returns
    ///
    /// The runner, so registrations chain.
    pub fn register(
        &mut self,
        kind: impl Into<JobKind>,
        handler: Arc<dyn JobHandler>,
    ) -> &mut JobRunner {
        Arc::make_mut(&mut self.handlers).insert(kind.into(), handler);
        self
    }

    /// Returns the queue the runner claims from.
    pub fn queue(&self) -> &Arc<JobQueue> {
        &self.queue
    }

    /// Returns the options the runner was built with.
    pub fn options(&self) -> &JobRunnerOptions {
        &self.options
    }

    /// Runs until `shutdown` is triggered, then waits for in-flight handlers.
    ///
    /// The workers claim and execute jobs concurrently. Claim failures - a
    /// locked database, a schema that does not match - are logged and retried
    /// after a poll interval rather than ending the run, so a transient database
    /// problem does not stop the server from doing background work.
    ///
    /// On shutdown the workers finish the jobs they hold; the call returns once
    /// every worker stopped, or once `shutdown_timeout` elapsed, whichever comes
    /// first. A handler abandoned by the timeout keeps its claim until the lock
    /// expires, after which the job is reclaimed and may run a second time.
    ///
    /// # Parameters
    ///
    /// - `shutdown`: the signal that stops the runner.
    pub async fn run(&self, shutdown: Shutdown) {
        let mut workers = JoinSet::new();
        for index in 0..self.options.workers.max(1) {
            workers.spawn(worker(
                Arc::clone(&self.queue),
                Arc::clone(&self.handlers),
                self.options.clone(),
                index,
                shutdown.clone(),
            ));
        }

        shutdown.wait().await;
        tracing::debug!(workers = workers.len(), "the job runner is stopping");

        let deadline = tokio::time::sleep(self.options.shutdown_timeout);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => {
                    tracing::warn!(
                        workers = workers.len(),
                        "the job runner gave up waiting for in-flight handlers",
                    );
                    workers.abort_all();
                    break;
                }
                finished = workers.join_next() => {
                    if finished.is_none() {
                        return;
                    }
                }
            }
        }
        while workers.join_next().await.is_some() {}
    }
}

impl std::fmt::Debug for JobRunner {
    /// Shows the options and the number of registered kinds.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRunner")
            .field("options", &self.options)
            .field("handlers", &self.handlers.len())
            .finish_non_exhaustive()
    }
}

/// Claims and runs jobs until the shutdown signal is triggered.
///
/// # Parameters
///
/// - `queue`: the queue to claim from.
/// - `handlers`: the handlers, keyed by kind.
/// - `options`: the poll interval and heartbeat.
/// - `index`: the worker's number, used in log records only.
/// - `shutdown`: the signal that stops the loop.
async fn worker(
    queue: Arc<JobQueue>,
    handlers: Arc<HashMap<JobKind, Arc<dyn JobHandler>>>,
    options: JobRunnerOptions,
    index: usize,
    shutdown: Shutdown,
) {
    loop {
        if shutdown.is_triggered() {
            return;
        }
        match queue.claim().await {
            Ok(Some(job)) => run_job(&queue, &handlers, &options, job).await,
            Ok(None) => idle(&queue, &options, &shutdown).await,
            Err(error) => {
                tracing::warn!(worker = index, %error, "claiming a job failed");
                idle(&queue, &options, &shutdown).await;
            }
        }
    }
}

/// Waits until there may be work, the poll interval elapses, or shutdown.
///
/// # Parameters
///
/// - `queue`: the queue a wake-up may come from.
/// - `options`: supplies the poll interval.
/// - `shutdown`: ends the wait early.
async fn idle(queue: &JobQueue, options: &JobRunnerOptions, shutdown: &Shutdown) {
    tokio::select! {
        _ = shutdown.wait() => {}
        _ = queue.wait_for_work() => {}
        _ = tokio::time::sleep(options.poll_interval) => {}
    }
}

/// Runs one claimed job and records its outcome.
///
/// # Parameters
///
/// - `queue`: the queue the job belongs to.
/// - `handlers`: the handlers, keyed by kind.
/// - `options`: supplies the heartbeat interval.
/// - `job`: the claimed job.
async fn run_job(
    queue: &Arc<JobQueue>,
    handlers: &HashMap<JobKind, Arc<dyn JobHandler>>,
    options: &JobRunnerOptions,
    job: Job,
) {
    let Some(handler) = handlers.get(&job.kind) else {
        tracing::warn!(job = %job.id, kind = %job.kind, "no handler is registered for this job kind");
        let error = format!("no handler is registered for kind `{}`", job.kind);
        if let Err(error) = queue.fail_permanently(job.id, &error).await {
            tracing::error!(job = %job.id, %error, "failing a job with no handler failed");
        }
        return;
    };

    let started = Instant::now();
    match handle_with_heartbeat(queue, handler.as_ref(), &job, options.heartbeat_interval).await {
        Ok(()) => {
            tracing::debug!(
                job = %job.id,
                kind = %job.kind,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "job completed",
            );
            if let Err(error) = queue.complete(job.id).await {
                tracing::error!(job = %job.id, %error, "completing a job failed");
            }
        }
        Err(error) => {
            tracing::warn!(
                job = %job.id,
                kind = %job.kind,
                attempts = job.attempts,
                elapsed_ms = started.elapsed().as_millis() as u64,
                error = error.message(),
                "job failed",
            );
            match queue.fail(job.id, error.message()).await {
                Ok(Some(FailureOutcome::Retried { run_at })) => {
                    tracing::info!(job = %job.id, run_at, "job rescheduled for another attempt");
                }
                Ok(Some(FailureOutcome::Dead)) => {
                    tracing::warn!(job = %job.id, "job used up its attempts and is now failed");
                }
                Ok(None) => {
                    tracing::warn!(job = %job.id, "the job was cancelled while it ran");
                }
                Err(fail_error) => {
                    tracing::error!(job = %job.id, %fail_error, "failing a job failed");
                }
            }
        }
    }
}

/// Calls a handler while extending the job's claim.
///
/// The claim is extended on a fixed heartbeat so a handler that runs for
/// minutes is not mistaken for an abandoned job. A claim that could not be
/// extended means the job is no longer `running` - it was cancelled, or another
/// worker reclaimed it after the lock expired - so the handler is left to
/// finish and its result is recorded like any other.
///
/// The extension runs in a task of its own rather than as an awaited branch
/// beside the handler. A handler that is inside a write transaction holds the
/// database's single write connection, so an extension waiting for that
/// connection would hold up every later poll of the handler, and a handler that
/// is not polled never commits its transaction: the extension would time out,
/// the handler would only advance between timeouts, and every other write in
/// the process would queue behind a connection that is never released.
///
/// # Parameters
///
/// - `queue`: the queue holding the claim.
/// - `handler`: the handler for the job's kind.
/// - `job`: the claimed job.
/// - `heartbeat_interval`: how often to extend the claim; zero means every
///   millisecond.
///
/// # Returns
///
/// Whatever the handler returned.
async fn handle_with_heartbeat(
    queue: &Arc<JobQueue>,
    handler: &dyn JobHandler,
    job: &Job,
    heartbeat_interval: Duration,
) -> Result<(), JobError> {
    let period = heartbeat_interval.max(Duration::from_millis(1));
    let (extended, mut extensions) = tokio::sync::mpsc::unbounded_channel();
    let extending = {
        let queue = Arc::clone(queue);
        let id = job.id;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(period).await;
                if extended.send(queue.extend_lock(id).await).is_err() {
                    return;
                }
            }
        })
    };

    let handling = handler.handle(job.clone());
    tokio::pin!(handling);
    let mut heartbeat_alive = true;
    let result = loop {
        tokio::select! {
            result = &mut handling => break result,
            result = extensions.recv(), if heartbeat_alive => match result {
                Some(Ok(true)) => {}
                Some(Ok(false)) => {
                    tracing::debug!(job = %job.id, "the job is no longer claimed; the handler runs on");
                }
                Some(Err(error)) => {
                    tracing::warn!(job = %job.id, %error, "extending the job lock failed");
                }
                None => heartbeat_alive = false,
            },
        }
    };
    extending.abort();
    result
}
