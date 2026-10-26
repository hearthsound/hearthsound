//! Integration tests for the job runner.
//!
//! Each test drives a real runner over a temporary database, so the claim,
//! heartbeat, retry, and shutdown paths are the ones the server uses.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use hearthsound_jobs::{
    Job, JobError, JobHandler, JobQueue, JobRunner, JobRunnerOptions, JobState, NewJob,
    QueueOptions, RetryPolicy, Shutdown,
};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::common::open_database;

/// Sends every job it handles to a channel and succeeds.
struct Recorder {
    handled: UnboundedSender<Job>,
}

#[async_trait]
impl JobHandler for Recorder {
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let _ = self.handled.send(job);
        Ok(())
    }
}

/// Fails its first `failures` attempts and signals the attempt that succeeded.
struct Flaky {
    calls: AtomicUsize,
    failures: usize,
    succeeded_on: UnboundedSender<usize>,
}

#[async_trait]
impl JobHandler for Flaky {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call <= self.failures {
            return Err(JobError::new("not yet"));
        }
        let _ = self.succeeded_on.send(call);
        Ok(())
    }
}

/// Signals that it started, waits, then signals that it finished.
struct Slow {
    started: UnboundedSender<()>,
    finished: UnboundedSender<()>,
    delay: Duration,
}

#[async_trait]
impl JobHandler for Slow {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let _ = self.started.send(());
        tokio::time::sleep(self.delay).await;
        let _ = self.finished.send(());
        Ok(())
    }
}

/// Holds the database's writer connection in a transaction while it works, the
/// way a scan batch or an import does.
struct HoldsTheWriter {
    /// The database whose writer connection the handler keeps checked out.
    database: Arc<hearthsound_db::Database>,
    /// Signals that the handler committed its transaction.
    finished: UnboundedSender<()>,
}

#[async_trait]
impl JobHandler for HoldsTheWriter {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let mut transaction = self
            .database
            .writer()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        for index in 0..10 {
            // The awaits are what a claim extension must not starve: a handler
            // that is not polled never commits, so the connection it holds is
            // never released either.
            tokio::time::sleep(Duration::from_millis(10)).await;
            let id = uuid::Uuid::now_v7();
            sqlx::query(
                "INSERT INTO users (id, username, display_name, password_hash, created_at, \
                                    updated_at) VALUES (?, ?, 'User', 'x', 0, 0)",
            )
            .bind(id.as_bytes().as_slice())
            .bind(format!("writer-{index}-{id}"))
            .execute(&mut *transaction)
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| JobError::new(error.to_string()))?;
        let _ = self.finished.send(());
        Ok(())
    }
}

/// Signals that it started and then blocks far past any test's shutdown grace
/// period.
struct Stuck {
    started: UnboundedSender<()>,
}

#[async_trait]
impl JobHandler for Stuck {
    async fn handle(&self, _job: Job) -> Result<(), JobError> {
        let _ = self.started.send(());
        tokio::time::sleep(Duration::from_secs(10)).await;
        Ok(())
    }
}

/// Panics on the payload named `boom` and succeeds on everything else.
struct Panicker {
    /// Signals the job it was handed, so the test knows both ran.
    handled: UnboundedSender<Job>,
}

#[async_trait]
impl JobHandler for Panicker {
    async fn handle(&self, job: Job) -> Result<(), JobError> {
        let _ = self.handled.send(job.clone());
        if job.payload_text().is_some_and(|text| text.contains("boom")) {
            panic!("the handler gave up");
        }
        Ok(())
    }
}

/// Starts a runner on its own task.
fn start(runner: Arc<JobRunner>, shutdown: &Shutdown) -> tokio::task::JoinHandle<()> {
    let shutdown = shutdown.clone();
    tokio::spawn(async move { runner.run(shutdown).await })
}

/// Receives from a channel, failing the test when nothing arrives in time.
async fn receive<T>(receiver: &mut UnboundedReceiver<T>, what: &str) -> T {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap_or_else(|_| panic!("{what} did not arrive in time"))
        .unwrap_or_else(|| panic!("{what} was never sent"))
}

/// Waits until `state` holds at least `expected` jobs.
async fn wait_for_count(queue: &JobQueue, state: JobState, expected: i64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while queue.count(state).await.unwrap() < expected {
        assert!(
            Instant::now() < deadline,
            "no job reached the {state} state in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn runs_a_registered_handler_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let (handled, mut handled_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 2,
            poll_interval: Duration::from_millis(20),
            ..JobRunnerOptions::default()
        },
    );
    runner.register("library.scan", Arc::new(Recorder { handled }));
    let runner = Arc::new(runner);

    let shutdown = Shutdown::new();
    let running = start(Arc::clone(&runner), &shutdown);

    let enqueued = queue
        .enqueue(
            NewJob::new("library.scan")
                .payload(&serde_json::json!({ "folder": "music" }))
                .unwrap(),
        )
        .await
        .unwrap();

    let handled = receive(&mut handled_rx, "the handled job").await;
    assert_eq!(handled.id, enqueued.id);
    assert_eq!(handled.attempts, 1);
    assert_eq!(handled.state, JobState::Running);

    shutdown.trigger();
    running.await.unwrap();

    assert_eq!(
        queue.get(enqueued.id).await.unwrap(),
        None,
        "a job a handler completed is deleted"
    );
}

#[tokio::test]
async fn wakes_immediately_on_a_local_enqueue() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let (handled, mut handled_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_secs(60),
            ..JobRunnerOptions::default()
        },
    );
    runner.register("library.scan", Arc::new(Recorder { handled }));

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    queue.enqueue(NewJob::new("library.scan")).await.unwrap();

    receive(&mut handled_rx, "the handled job").await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the worker woke on the enqueue instead of polling a minute later"
    );

    shutdown.trigger();
    running.await.unwrap();
}

/// A handler that panics fails its own job permanently and leaves the worker
/// running: the next job on the same worker still completes.
#[tokio::test]
async fn a_panicking_handler_fails_its_job_and_keeps_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let (handled, mut handled_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(20),
            ..JobRunnerOptions::default()
        },
    );
    runner.register("library.scan", Arc::new(Panicker { handled }));

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let broken = queue
        .enqueue(NewJob::new("library.scan").payload(&"boom").unwrap())
        .await
        .unwrap();
    let healthy = queue.enqueue(NewJob::new("library.scan")).await.unwrap();

    // Both jobs are handed to the one worker, so the panic cannot have ended
    // it.
    let mut seen = Vec::new();
    for _ in 0..2 {
        seen.push(receive(&mut handled_rx, "a handled job").await.id);
    }
    assert!(seen.contains(&broken.id) && seen.contains(&healthy.id));

    wait_for_count(&queue, JobState::Failed, 1).await;
    shutdown.trigger();
    running.await.unwrap();

    let failed = queue.get(broken.id).await.unwrap().unwrap();
    assert_eq!(failed.state, JobState::Failed);
    assert_eq!(failed.attempts, 1, "a panicking handler is not retried");
    let error = failed.last_error.clone().unwrap();
    assert!(
        error.contains("panicked") && error.contains("the handler gave up"),
        "the stored error says the handler panicked: {error}"
    );
    assert_eq!(
        queue.get(healthy.id).await.unwrap(),
        None,
        "the next job on the same worker completed"
    );
}

#[tokio::test]
async fn an_unknown_kind_fails_without_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let runner = Arc::new(JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(20),
            ..JobRunnerOptions::default()
        },
    ));

    let shutdown = Shutdown::new();
    let running = start(runner, &shutdown);

    let enqueued = queue.enqueue(NewJob::new("mystery.kind")).await.unwrap();

    wait_for_count(&queue, JobState::Failed, 1).await;
    shutdown.trigger();
    running.await.unwrap();

    let failed = queue.list_by_state(JobState::Failed, 10).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].id, enqueued.id);
    assert_eq!(
        failed[0].attempts, 1,
        "no retry is attempted for a kind with no handler"
    );
    let error = failed[0].last_error.clone().unwrap();
    assert!(
        error.contains("mystery.kind"),
        "the stored error names the kind: {error}"
    );
}

#[tokio::test]
async fn a_failing_handler_is_retried_until_it_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(
        &database,
        QueueOptions {
            retry: RetryPolicy::new(vec![Duration::from_millis(30), Duration::from_millis(30)]),
            ..QueueOptions::default()
        },
    ));

    let (succeeded, mut succeeded_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            ..JobRunnerOptions::default()
        },
    );
    runner.register(
        "library.scan",
        Arc::new(Flaky {
            calls: AtomicUsize::new(0),
            failures: 2,
            succeeded_on: succeeded,
        }),
    );

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();

    let succeeded_on = receive(&mut succeeded_rx, "the successful attempt").await;
    assert_eq!(succeeded_on, 3, "two failures were retried");

    shutdown.trigger();
    running.await.unwrap();

    assert_eq!(
        queue.get(enqueued.id).await.unwrap(),
        None,
        "the job was deleted once a retry succeeded"
    );
}

#[tokio::test]
async fn a_long_handler_keeps_its_claim_alive() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(
        &database,
        QueueOptions {
            lock_duration: Duration::from_millis(100),
            ..QueueOptions::default()
        },
    ));

    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (finished, mut finished_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_millis(20),
            ..JobRunnerOptions::default()
        },
    );
    runner.register(
        "library.scan",
        Arc::new(Slow {
            started,
            finished,
            delay: Duration::from_millis(400),
        }),
    );

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    receive(&mut started_rx, "the handler start").await;
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        queue.claim().await.unwrap().is_none(),
        "the heartbeat keeps a running handler's claim past the lock duration"
    );

    receive(&mut finished_rx, "the handler result").await;
    shutdown.trigger();
    running.await.unwrap();

    assert_eq!(queue.get(enqueued.id).await.unwrap(), None);
}

/// A handler that keeps the writer connection inside a transaction finishes even
/// while its claim is being extended.
///
/// The extension needs that same connection, so it has to run beside the
/// handler: awaited in the runner's own select loop it parks the handler, the
/// handler never commits, and every other write queues behind the connection for
/// the pool's acquire timeout. That is the wedge a large watched folder caused.
#[tokio::test]
async fn a_handler_holding_the_writer_is_not_starved_by_its_heartbeat() {
    let dir = tempfile::tempdir().unwrap();
    let database = Arc::new(open_database(dir.path()).await);
    let queue = Arc::new(JobQueue::new(
        &database,
        QueueOptions {
            lock_duration: Duration::from_millis(100),
            ..QueueOptions::default()
        },
    ));

    let (finished, mut finished_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_millis(20),
            ..JobRunnerOptions::default()
        },
    );
    runner.register(
        "library.scan",
        Arc::new(HoldsTheWriter {
            database: Arc::clone(&database),
            finished,
        }),
    );

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), finished_rx.recv())
        .await
        .expect("the handler finishes while its claim is extended")
        .expect("the handler signalled its result");

    shutdown.trigger();
    running.await.unwrap();
    assert_eq!(queue.get(enqueued.id).await.unwrap(), None);
}

#[tokio::test]
async fn shutdown_waits_for_an_in_flight_handler() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let (started, mut started_rx) = mpsc::unbounded_channel();
    let (finished, mut finished_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_millis(20),
            shutdown_timeout: Duration::from_secs(5),
        },
    );
    runner.register(
        "library.scan",
        Arc::new(Slow {
            started,
            finished,
            delay: Duration::from_millis(200),
        }),
    );

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    receive(&mut started_rx, "the handler start").await;

    let started_at = Instant::now();
    shutdown.trigger();
    running.await.unwrap();
    let elapsed = started_at.elapsed();

    assert!(
        elapsed >= Duration::from_millis(150),
        "shutdown waited for the handler, which took {elapsed:?}"
    );
    assert!(
        finished_rx.try_recv().is_ok(),
        "the handler ran to completion during shutdown"
    );
    assert_eq!(
        queue.get(enqueued.id).await.unwrap(),
        None,
        "the handler's success was recorded before the runner stopped"
    );
}

#[tokio::test]
async fn shutdown_gives_up_after_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    let (started, mut started_rx) = mpsc::unbounded_channel();
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_millis(20),
            shutdown_timeout: Duration::from_millis(150),
        },
    );
    runner.register("library.scan", Arc::new(Stuck { started }));

    let shutdown = Shutdown::new();
    let running = start(Arc::new(runner), &shutdown);

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    receive(&mut started_rx, "the handler start").await;

    let started_at = Instant::now();
    shutdown.trigger();
    running.await.unwrap();
    let elapsed = started_at.elapsed();

    assert!(
        elapsed >= Duration::from_millis(150),
        "shutdown gave the handler its grace period, which took {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "shutdown did not wait for the stuck handler, which took {elapsed:?}"
    );

    let abandoned = queue.get(enqueued.id).await.unwrap().unwrap();
    assert_eq!(abandoned.state, JobState::Running);
    assert!(
        abandoned.locked_until.unwrap() > crate::common::now_millis(),
        "an abandoned job keeps its claim until the lock expires"
    );
}
