//! Integration tests for the persistent job queue.
//!
//! Every test runs against a temporary migrated database, so the statements
//! under test are the ones the server will run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hearthsound_core::Priority;
use hearthsound_jobs::{FailureOutcome, JobQueue, JobState, NewJob, QueueOptions, RetryPolicy};

use crate::common::{now_millis, open_database};

/// The queue options the timing tests use: a lock short enough to expire inside
/// a test and a retry schedule measured in milliseconds.
fn fast_options() -> QueueOptions {
    QueueOptions {
        lock_duration: Duration::from_millis(60),
        retry: RetryPolicy::new(vec![Duration::from_millis(80), Duration::from_millis(200)]),
    }
}

#[tokio::test]
async fn enqueue_stores_a_job_and_claim_takes_it() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());

    let before = now_millis();
    let enqueued = queue
        .enqueue(
            NewJob::new("library.scan")
                .priority(Priority::Queue)
                .payload(&serde_json::json!({ "folder": "music" }))
                .unwrap(),
        )
        .await
        .unwrap();
    let after = now_millis();

    assert_eq!(enqueued.kind.as_str(), "library.scan");
    assert_eq!(enqueued.state, JobState::Ready);
    assert_eq!(enqueued.priority, Priority::Queue);
    assert_eq!(enqueued.attempts, 0);
    assert!(enqueued.run_at >= before && enqueued.run_at <= after);
    assert!(enqueued.created_at >= before && enqueued.created_at <= after);
    assert_eq!(enqueued.locked_until, None);
    assert_eq!(enqueued.last_error, None);
    assert_eq!(
        enqueued.payload_text(),
        Some(r#"{"folder":"music"}"#),
        "the payload is stored as JSON text"
    );

    let claimed = queue.claim().await.unwrap().expect("a due job is claimed");
    assert_eq!(claimed.id, enqueued.id);
    assert_eq!(claimed.state, JobState::Running);
    assert_eq!(claimed.attempts, 1, "a claim counts as an attempt");
    assert!(claimed.locked_until.unwrap() > now_millis());

    let payload: Option<serde_json::Value> = claimed.payload_json().unwrap();
    assert_eq!(payload.unwrap()["folder"], "music");

    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 0);
    assert_eq!(queue.count(JobState::Running).await.unwrap(), 1);
    assert!(queue.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn claim_waits_for_run_at_and_takes_the_most_urgent_job() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());

    let later = queue
        .enqueue(
            NewJob::new("library.scan")
                .priority(Priority::Play)
                .run_at(now_millis() + 60_000),
        )
        .await
        .unwrap();
    let upgrade = queue
        .enqueue(NewJob::new("acquisition.verify").priority(Priority::Upgrade))
        .await
        .unwrap();
    let play = queue
        .enqueue(NewJob::new("acquisition.run").priority(Priority::Play))
        .await
        .unwrap();

    let first = queue.claim().await.unwrap().unwrap();
    assert_eq!(
        first.id, play.id,
        "the most urgent due job is claimed first"
    );
    let second = queue.claim().await.unwrap().unwrap();
    assert_eq!(second.id, upgrade.id);
    assert!(
        queue.claim().await.unwrap().is_none(),
        "the job that is not due yet stays in the queue: {}",
        later.id
    );
}

#[tokio::test]
async fn concurrent_claimers_never_double_claim() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = Arc::new(JobQueue::new(&database, QueueOptions::default()));

    const JOBS: usize = 50;
    const CLAIMERS: usize = 8;
    for index in 0..JOBS {
        queue
            .enqueue(
                NewJob::new("library.scan")
                    .payload(&serde_json::json!({ "n": index }))
                    .unwrap(),
            )
            .await
            .unwrap();
    }

    let mut tasks = Vec::new();
    for _ in 0..CLAIMERS {
        let queue = Arc::clone(&queue);
        tasks.push(tokio::spawn(async move {
            let mut claimed = Vec::new();
            while let Some(job) = queue.claim().await.unwrap() {
                claimed.push(job.id);
            }
            claimed
        }));
    }
    let mut claimed = Vec::new();
    for task in tasks {
        claimed.extend(task.await.unwrap());
    }

    assert_eq!(claimed.len(), JOBS, "every enqueued job was claimed");
    let mut unique = claimed.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), JOBS, "no job was claimed twice");
    assert_eq!(queue.count(JobState::Running).await.unwrap(), JOBS as i64);
}

#[tokio::test]
async fn a_lock_that_expires_is_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(
        &database,
        QueueOptions {
            lock_duration: Duration::from_millis(50),
            ..QueueOptions::default()
        },
    );

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    let first = queue.claim().await.unwrap().unwrap();
    assert_eq!(first.id, enqueued.id);
    assert_eq!(first.attempts, 1);

    tokio::time::sleep(Duration::from_millis(120)).await;

    let reclaimed = queue
        .claim()
        .await
        .unwrap()
        .expect("the abandoned claim is taken again");
    assert_eq!(reclaimed.id, enqueued.id);
    assert_eq!(reclaimed.attempts, 2);
    assert!(reclaimed.locked_until.unwrap() > now_millis());
}

#[tokio::test]
async fn extend_lock_holds_a_claim_past_its_original_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(
        &database,
        QueueOptions {
            // Wide enough that a slow CI runner's database writes between the
            // sleeps cannot eat the margin; a 60 ms lock failed there.
            lock_duration: Duration::from_millis(1_200),
            ..QueueOptions::default()
        },
    );

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    let claimed = queue.claim().await.unwrap().unwrap();
    let first_deadline = claimed.locked_until.unwrap();

    assert!(queue.extend_lock(enqueued.id).await.unwrap());
    let extended = queue.get(enqueued.id).await.unwrap().unwrap();
    assert!(
        extended.locked_until.unwrap() >= first_deadline,
        "the heartbeat does not move the deadline backwards"
    );

    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(queue.extend_lock(enqueued.id).await.unwrap());
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        queue.claim().await.unwrap().is_none(),
        "1.8 s into a 1.2 s lock the heartbeat still holds the claim"
    );

    assert!(queue.complete(enqueued.id).await.unwrap());
    assert!(
        !queue.extend_lock(enqueued.id).await.unwrap(),
        "a job that is no longer running cannot be extended"
    );
}

#[tokio::test]
async fn complete_deletes_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    queue.claim().await.unwrap().unwrap();

    assert!(queue.complete(enqueued.id).await.unwrap());
    assert_eq!(queue.get(enqueued.id).await.unwrap(), None);
    assert!(!queue.complete(enqueued.id).await.unwrap());
}

#[tokio::test]
async fn fail_retries_with_backoff_then_leaves_a_dead_job() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, fast_options());

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();

    for expected_attempt in 1..=2 {
        let claimed = queue.claim().await.unwrap().unwrap();
        assert_eq!(claimed.attempts, expected_attempt);

        let before = now_millis();
        let outcome = queue
            .fail(claimed.id, "temporarily unavailable")
            .await
            .unwrap();
        let after = now_millis();
        let FailureOutcome::Retried { run_at } = outcome.unwrap() else {
            panic!("attempt {expected_attempt} must be retried");
        };
        let delay = match expected_attempt {
            1 => 80,
            _ => 200,
        };
        assert!(
            run_at >= before + delay && run_at <= after + delay,
            "attempt {expected_attempt} waits its backoff before {run_at}"
        );

        let retried = queue.get(enqueued.id).await.unwrap().unwrap();
        assert_eq!(retried.state, JobState::Ready);
        assert_eq!(retried.locked_until, None);
        assert_eq!(
            retried.last_error.as_deref(),
            Some("temporarily unavailable")
        );
        assert!(
            queue.claim().await.unwrap().is_none(),
            "a job that is backing off is not claimed"
        );

        tokio::time::sleep(Duration::from_millis(u64::try_from(delay).unwrap() + 40)).await;
    }

    let claimed = queue.claim().await.unwrap().unwrap();
    assert_eq!(claimed.attempts, 3);
    let outcome = queue.fail(claimed.id, "still broken").await.unwrap();
    assert_eq!(outcome, Some(FailureOutcome::Dead));

    let dead = queue.get(enqueued.id).await.unwrap().unwrap();
    assert_eq!(dead.state, JobState::Failed);
    assert_eq!(dead.last_error.as_deref(), Some("still broken"));
    assert_eq!(dead.locked_until, None);

    let failed = queue.list_by_state(JobState::Failed, 10).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].id, enqueued.id);
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 0);
    assert!(queue.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn fail_permanently_skips_the_retry_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());

    let enqueued = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    queue.claim().await.unwrap().unwrap();

    assert!(
        queue
            .fail_permanently(enqueued.id, "no handler")
            .await
            .unwrap()
    );
    let dead = queue.get(enqueued.id).await.unwrap().unwrap();
    assert_eq!(dead.state, JobState::Failed);
    assert_eq!(dead.attempts, 1);
    assert_eq!(dead.last_error.as_deref(), Some("no handler"));
    assert!(queue.claim().await.unwrap().is_none());
}

#[tokio::test]
async fn cancel_removes_a_job() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());

    let first = queue.enqueue(NewJob::new("library.scan")).await.unwrap();
    let second = queue.enqueue(NewJob::new("library.scan")).await.unwrap();

    assert!(queue.cancel(first.id).await.unwrap());
    assert!(!queue.cancel(first.id).await.unwrap());
    assert_eq!(queue.get(first.id).await.unwrap(), None);
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 1);

    let listed = queue.list_by_state(JobState::Ready, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, second.id);
}

#[tokio::test]
async fn missing_jobs_are_noops() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queue = JobQueue::new(&database, QueueOptions::default());
    let missing = hearthsound_core::new_uuid_v7();

    assert!(!queue.complete(missing).await.unwrap());
    assert!(!queue.cancel(missing).await.unwrap());
    assert!(!queue.extend_lock(missing).await.unwrap());
    assert!(!queue.fail_permanently(missing, "gone").await.unwrap());
    assert_eq!(queue.fail(missing, "gone").await.unwrap(), None);
    assert_eq!(queue.get(missing).await.unwrap(), None);
}
