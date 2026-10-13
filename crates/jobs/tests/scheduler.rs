//! Integration tests for the cron scheduler.
//!
//! The tests arrange due schedules by hand so they can assert, without waiting
//! for a real minute boundary, that a due schedule fires exactly once per tick,
//! that runs missed during downtime are not replayed, and that the next fire
//! time is persisted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, Timelike};
use hearthsound_core::Priority;
use hearthsound_db::Database;
use hearthsound_jobs::{
    JobQueue, JobState, JobsError, QueueOptions, Schedule, ScheduleSpec, Scheduler,
    SchedulerOptions, Shutdown,
};
use uuid::Uuid;

use common::{insert_user, now_millis, open_database, set_cron, set_next_run_at};

/// Builds a queue and a scheduler over one database.
fn build(database: &Database, options: SchedulerOptions) -> (Arc<JobQueue>, Scheduler) {
    let queue = Arc::new(JobQueue::new(database, QueueOptions::default()));
    let scheduler = Scheduler::new(database, Arc::clone(&queue), options);
    (queue, scheduler)
}

/// Reads one schedule back out of the table.
async fn stored(scheduler: &Scheduler, id: Uuid) -> Schedule {
    scheduler
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|schedule| schedule.id == id)
        .expect("the schedule still exists")
}

/// A system schedule for the tests.
async fn system_schedule(scheduler: &Scheduler, cron: &str) -> Schedule {
    scheduler
        .upsert(ScheduleSpec::system("library.scan", cron))
        .await
        .unwrap()
}

#[tokio::test]
async fn fires_a_due_schedule_once_per_tick() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = scheduler
        .upsert(
            ScheduleSpec::system("library.scan", "0 3 * * *")
                .payload(&serde_json::json!({ "full": true }))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(schedule.enabled);
    assert!(schedule.next_run_at.unwrap() > now_millis());
    assert_eq!(schedule.last_run_at, None);

    let due_at = now_millis() - 5_000;
    set_next_run_at(&database, schedule.id, Some(due_at)).await;

    assert_eq!(scheduler.tick().await.unwrap(), 1);

    let jobs = queue.list_by_state(JobState::Ready, 10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind.as_str(), "library.scan");
    assert_eq!(jobs[0].priority, Priority::Background);
    assert_eq!(jobs[0].payload_text(), schedule.payload_text());
    assert!(jobs[0].run_at <= now_millis());

    let fired = stored(&scheduler, schedule.id).await;
    assert_eq!(fired.last_run_at, Some(due_at));
    assert!(
        fired.next_run_at.unwrap() > now_millis(),
        "the next fire time moved past now"
    );

    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(
        queue.count(JobState::Ready).await.unwrap(),
        1,
        "the schedule fires once per due tick"
    );
}

#[tokio::test]
async fn skips_the_runs_missed_while_the_server_was_down() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = system_schedule(&scheduler, "0 3 * * *").await;
    let three_days_ago = now_millis() - 3 * 24 * 60 * 60 * 1000;
    set_next_run_at(&database, schedule.id, Some(three_days_ago)).await;

    assert_eq!(scheduler.tick().await.unwrap(), 1);
    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(
        queue.count(JobState::Ready).await.unwrap(),
        1,
        "three missed runs produce one job, not a catch-up storm"
    );

    let next_run_at = stored(&scheduler, schedule.id).await.next_run_at.unwrap();
    let day = 25 * 60 * 60 * 1000;
    assert!(
        next_run_at > now_millis() && next_run_at <= now_millis() + day,
        "the next fire time is the next occurrence after now: {next_run_at}"
    );
}

#[tokio::test]
async fn the_next_fire_time_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = system_schedule(&scheduler, "0 3 * * *").await;
    set_next_run_at(&database, schedule.id, Some(now_millis() - 1_000)).await;
    assert_eq!(scheduler.tick().await.unwrap(), 1);
    assert_eq!(
        queue.count(JobState::Ready).await.unwrap(),
        1,
        "the schedule fired once before the restart"
    );
    let next_run_at = stored(&scheduler, schedule.id).await.next_run_at.unwrap();

    // A restart builds new pools, a new queue, and a new scheduler over the
    // same database, exactly as the composition root does.
    let (restarted_queue, restarted) = build(&database, SchedulerOptions::default());
    assert_eq!(restarted.tick().await.unwrap(), 0);
    assert_eq!(
        restarted_queue.count(JobState::Ready).await.unwrap(),
        1,
        "the job enqueued before the restart is still queued"
    );

    let again = restarted
        .upsert(ScheduleSpec::system("library.scan", "0 3 * * *"))
        .await
        .unwrap();
    assert_eq!(
        again.next_run_at,
        Some(next_run_at),
        "registering a system schedule again does not move its fire time"
    );
    assert_eq!(restarted.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn upsert_updates_the_expression_and_recomputes_only_then() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (_queue, scheduler) = build(&database, SchedulerOptions::default());

    let first = system_schedule(&scheduler, "0 3 * * *").await;
    let second = scheduler
        .upsert(ScheduleSpec::system("library.scan", "0 3 * * *"))
        .await
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(
        second.next_run_at, first.next_run_at,
        "an unchanged expression keeps the fire time it had"
    );
    assert_eq!(scheduler.list().await.unwrap().len(), 1);

    let third = system_schedule(&scheduler, "0 5 * * *").await;
    assert_eq!(third.id, first.id);
    assert_eq!(third.cron, "0 5 * * *");
    assert_ne!(
        third.next_run_at, first.next_run_at,
        "a changed expression moves the fire time"
    );
    assert!(third.next_run_at.unwrap() > now_millis());
}

#[tokio::test]
async fn a_user_schedule_is_separate_from_the_system_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (_queue, scheduler) = build(&database, SchedulerOptions::default());

    let user = hearthsound_core::new_uuid_v7();
    insert_user(&database, user).await;

    let system = system_schedule(&scheduler, "0 3 * * *").await;
    let owned = scheduler
        .upsert(ScheduleSpec::user(user, "library.scan", "0 4 * * *"))
        .await
        .unwrap();
    assert_ne!(owned.id, system.id);
    assert_eq!(owned.owner_user_id, Some(user));
    assert_eq!(scheduler.list().await.unwrap().len(), 2);

    let again = scheduler
        .upsert(ScheduleSpec::user(user, "library.scan", "0 4 * * *"))
        .await
        .unwrap();
    assert_eq!(
        again.id, owned.id,
        "a user schedule is keyed by owner and kind"
    );
    assert_eq!(scheduler.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_disabled_schedule_does_not_fire() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = system_schedule(&scheduler, "0 3 * * *").await;
    set_next_run_at(&database, schedule.id, Some(now_millis() - 1_000)).await;

    assert!(scheduler.set_enabled(schedule.id, false).await.unwrap());
    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 0);

    assert!(scheduler.set_enabled(schedule.id, true).await.unwrap());
    assert_eq!(scheduler.tick().await.unwrap(), 1);
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 1);

    assert!(
        !scheduler
            .set_enabled(hearthsound_core::new_uuid_v7(), true)
            .await
            .unwrap()
    );
    assert!(
        !scheduler
            .delete(hearthsound_core::new_uuid_v7())
            .await
            .unwrap()
    );
    assert!(scheduler.delete(schedule.id).await.unwrap());
    assert!(scheduler.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_expression_that_cannot_be_parsed_disables_the_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let due = system_schedule(&scheduler, "0 3 * * *").await;
    set_cron(&database, due.id, "not a cron").await;
    set_next_run_at(&database, due.id, Some(now_millis() - 1_000)).await;
    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert!(!stored(&scheduler, due.id).await.enabled);

    let unarmed = scheduler
        .upsert(ScheduleSpec::system("acquisition.run", "0 3 * * *"))
        .await
        .unwrap();
    set_cron(&database, unarmed.id, "not a cron").await;
    set_next_run_at(&database, unarmed.id, None).await;
    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert!(!stored(&scheduler, unarmed.id).await.enabled);

    assert_eq!(
        queue.count(JobState::Ready).await.unwrap(),
        0,
        "a schedule the scheduler cannot parse enqueues nothing"
    );
}

#[tokio::test]
async fn a_schedule_without_a_fire_time_is_armed_without_firing() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = system_schedule(&scheduler, "0 3 * * *").await;
    set_next_run_at(&database, schedule.id, None).await;

    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 0);

    let armed = stored(&scheduler, schedule.id).await.next_run_at.unwrap();
    assert!(armed > now_millis());

    assert_eq!(scheduler.tick().await.unwrap(), 0);
    assert_eq!(
        stored(&scheduler, schedule.id).await.next_run_at,
        Some(armed)
    );
}

#[tokio::test]
async fn an_upsert_with_a_broken_expression_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (_queue, scheduler) = build(&database, SchedulerOptions::default());

    let error = scheduler
        .upsert(ScheduleSpec::system("library.scan", "not a cron"))
        .await
        .unwrap_err();
    assert!(matches!(error, JobsError::Cron { .. }), "{error}");
    assert!(scheduler.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_next_fire_time_is_in_server_local_time() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (_queue, scheduler) = build(&database, SchedulerOptions::default());

    let schedule = system_schedule(&scheduler, "0 4 * * *").await;
    let next = DateTime::from_timestamp_millis(schedule.next_run_at.unwrap())
        .unwrap()
        .with_timezone(&Local);
    assert_eq!((next.hour(), next.minute(), next.second()), (4, 0, 0));
    assert!(schedule.next_run_at.unwrap() > now_millis());
}

#[tokio::test]
async fn run_ticks_until_it_is_shut_down() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let (queue, scheduler) = build(
        &database,
        SchedulerOptions {
            tick_interval: Duration::from_millis(20),
        },
    );

    let schedule = system_schedule(&scheduler, "0 3 * * *").await;
    set_next_run_at(&database, schedule.id, Some(now_millis() - 1_000)).await;

    let shutdown = Shutdown::new();
    let running = {
        let scheduler = Arc::new(scheduler);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { scheduler.run(shutdown).await })
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    while queue.count(JobState::Ready).await.unwrap() < 1 {
        assert!(Instant::now() < deadline, "the tick loop never fired");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    shutdown.trigger();
    running.await.unwrap();

    set_next_run_at(&database, schedule.id, Some(now_millis() - 1_000)).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        queue.count(JobState::Ready).await.unwrap(),
        1,
        "the tick loop stopped with the shutdown signal"
    );
}
