//! The persistent job queue and the cron scheduler.
//!
//! Every piece of background work in the server is a job: a row in the `jobs`
//! table holding a kind, an optional JSON payload, a priority and the earliest
//! time it may run. [`JobQueue`] owns that table: it enqueues jobs, claims them
//! with one atomic statement, keeps the claim alive while a handler runs,
//! retries failures with backoff, and deletes a job once it is done.
//!
//! The queue is deliberately storage-only. What runs a job is the
//! [`JobRunner`], which matches the job's kind against the handlers the
//! composition root registered; what creates jobs on a timetable is the
//! [`Scheduler`], which reads the `schedules` table and enqueues the schedules
//! that are due.
//!
//! # Guarantees
//!
//! - A job is handed to at most one worker at a time. A claim writes a
//!   `locked_until` deadline in the same statement that takes the job, so a
//!   second worker cannot take it while the first holds the lock.
//! - A claim by a worker that then crashes is recoverable: a job that is still
//!   `running` with an expired `locked_until` is claimed again, which is why
//!   delivery is at-least-once and handlers must tolerate running twice.
//! - A completed job is deleted, so the table holds only outstanding work.
//! - A job that fails is retried until its retry schedule is exhausted and then
//!   kept in the `failed` state with its last error, for an operator to inspect.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use std::sync::Arc;
//!
//! use hearthsound_db::Database;
//! use hearthsound_jobs::{JobQueue, QueueOptions};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open(Path::new("/tmp/hearthsound.db")).await?;
//! let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
//!
//! let job = queue.enqueue(hearthsound_jobs::NewJob::new("library.scan")).await?;
//! assert_eq!(job.kind.as_str(), "library.scan");
//!
//! queue.complete(job.id, job.attempts).await?;
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod clock;
mod error;
mod kind;
mod queue;
mod runner;
mod scheduler;
mod shutdown;

pub use error::JobsError;
pub use kind::JobKind;
pub use queue::{FailureOutcome, Job, JobQueue, JobState, NewJob, QueueOptions, RetryPolicy};
pub use runner::{JobError, JobHandler, JobRunner, JobRunnerOptions};
pub use scheduler::{Schedule, ScheduleSpec, Scheduler, SchedulerOptions};
pub use shutdown::Shutdown;
