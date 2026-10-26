//! The cron scheduler.
//!
//! Recurring work is a row in the `schedules` table: a job kind, a cron
//! expression, an optional JSON payload, and an owner that is either `NULL` for
//! a system schedule or a user id for that user's own schedule. Each row also
//! carries `next_run_at`, the materialized next fire time, so a tick finds the
//! due schedules with one indexed read.
//!
//! [`Scheduler::upsert`] is how the server declares the schedules it owns: it is
//! idempotent by kind (and owner), so calling it on every startup neither
//! duplicates a schedule nor moves its next fire time. [`Scheduler::tick`]
//! enqueues the schedules that are due and advances them; [`Scheduler::run`]
//! ticks on an interval until it is shut down.
//!
//! Downtime never causes a catch-up storm: the next fire time is always computed
//! from the moment of the tick rather than from the time that was missed, so a
//! server that was down for a week enqueues each schedule once and then resumes
//! its normal timetable.
//!
//! Cron expressions are evaluated in the server's local time zone, which is what
//! makes `0 3 * * *` mean three in the morning for the operator who wrote it.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local};
use croner::Cron;
use hearthsound_core::Priority;
use hearthsound_db::Database;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::clock::now_millis;
use crate::error::JobsError;
use crate::kind::JobKind;
use crate::queue::{JobQueue, NewJob};
use crate::shutdown::Shutdown;

/// The priority a scheduled job is enqueued at.
///
/// The `schedules` table has no priority column, and every schedule is by
/// nature background work, so scheduled jobs sort behind interactive work.
const SCHEDULED_PRIORITY: Priority = Priority::Background;

/// How often [`Scheduler::run`] looks for due schedules.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_jobs::SchedulerOptions;
///
/// let options = SchedulerOptions {
///     tick_interval: Duration::from_secs(5),
/// };
/// assert_eq!(options.tick_interval, Duration::from_secs(5));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerOptions {
    /// How often the scheduler looks for due schedules.
    ///
    /// Values below one millisecond are treated as one millisecond, so a
    /// misconfigured interval cannot spin the loop.
    pub tick_interval: Duration,
}

impl Default for SchedulerOptions {
    /// The default: a one second tick, which keeps a schedule that fires on a
    /// second boundary close to punctual.
    fn default() -> SchedulerOptions {
        SchedulerOptions {
            tick_interval: Duration::from_secs(1),
        }
    }
}

/// One row of the `schedules` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// The schedule's id.
    pub id: Uuid,
    /// The user the schedule belongs to, or `None` for a system schedule.
    pub owner_user_id: Option<Uuid>,
    /// The kind of job the schedule enqueues.
    pub kind: JobKind,
    /// The cron expression, evaluated in server-local time.
    pub cron: String,
    /// The JSON payload handed to every job the schedule enqueues.
    pub payload: Option<String>,
    /// Whether the schedule fires.
    pub enabled: bool,
    /// When the schedule fires next, in unix milliseconds.
    pub next_run_at: Option<i64>,
    /// When the schedule last fired, in unix milliseconds.
    pub last_run_at: Option<i64>,
}

impl Schedule {
    /// Decodes the payload into `T`.
    ///
    /// # Returns
    ///
    /// The decoded payload, or `None` when the schedule carries none.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Payload`] when the stored text is not valid JSON
    /// for `T`.
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

/// A schedule to create or update.
///
/// A system schedule ([`ScheduleSpec::system`]) is identified by its kind, so
/// upserting it again updates that one row. A user schedule
/// ([`ScheduleSpec::user`]) is identified by its owner and its kind, so two
/// users may each own a schedule of the same kind.
///
/// # Examples
///
/// ```
/// use hearthsound_jobs::ScheduleSpec;
///
/// let spec = ScheduleSpec::system("library.scan", "0 3 * * *")
///     .payload(&serde_json::json!({ "full": true }))
///     .unwrap();
/// assert_eq!(spec.kind().as_str(), "library.scan");
/// ```
#[derive(Debug, Clone)]
pub struct ScheduleSpec {
    owner_user_id: Option<Uuid>,
    kind: JobKind,
    cron: String,
    payload: Option<serde_json::Value>,
}

impl ScheduleSpec {
    /// Builds a system schedule, owned by no user.
    ///
    /// # Parameters
    ///
    /// - `kind`: the kind of job the schedule enqueues.
    /// - `cron`: the cron expression, evaluated in server-local time.
    ///
    /// # Returns
    ///
    /// The spec.
    pub fn system(kind: impl Into<JobKind>, cron: impl Into<String>) -> ScheduleSpec {
        ScheduleSpec {
            owner_user_id: None,
            kind: kind.into(),
            cron: cron.into(),
            payload: None,
        }
    }

    /// Builds a schedule that belongs to one user.
    ///
    /// # Parameters
    ///
    /// - `owner_user_id`: the user the schedule belongs to.
    /// - `kind`: the kind of job the schedule enqueues.
    /// - `cron`: the cron expression, evaluated in server-local time.
    ///
    /// # Returns
    ///
    /// The spec.
    pub fn user(
        owner_user_id: Uuid,
        kind: impl Into<JobKind>,
        cron: impl Into<String>,
    ) -> ScheduleSpec {
        ScheduleSpec {
            owner_user_id: Some(owner_user_id),
            kind: kind.into(),
            cron: cron.into(),
            payload: None,
        }
    }

    /// Sets the JSON payload every job from this schedule carries.
    ///
    /// # Parameters
    ///
    /// - `value`: the value to encode as JSON.
    ///
    /// # Returns
    ///
    /// The spec with its payload set.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Payload`] when `value` cannot be encoded as JSON.
    pub fn payload<T: Serialize>(mut self, value: &T) -> Result<ScheduleSpec, JobsError> {
        self.payload = Some(serde_json::to_value(value)?);
        Ok(self)
    }

    /// Returns the kind of job the schedule enqueues.
    pub fn kind(&self) -> &JobKind {
        &self.kind
    }

    /// Returns the cron expression.
    pub fn cron(&self) -> &str {
        &self.cron
    }

    /// Returns the owner, or `None` for a system schedule.
    pub fn owner_user_id(&self) -> Option<Uuid> {
        self.owner_user_id
    }
}

/// The schedules that create recurring jobs.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
/// use std::sync::Arc;
///
/// use hearthsound_jobs::{JobQueue, QueueOptions, ScheduleSpec, Scheduler, SchedulerOptions};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let db = hearthsound_db::Database::open(Path::new("/tmp/hearthsound.db")).await?;
/// let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
/// let scheduler = Scheduler::new(&db, queue, SchedulerOptions::default());
///
/// let schedule = scheduler
///     .upsert(ScheduleSpec::system("library.scan", "0 3 * * *"))
///     .await?;
/// assert!(schedule.next_run_at.is_some());
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Scheduler {
    queue: Arc<JobQueue>,
    writer: SqlitePool,
    readers: SqlitePool,
    options: SchedulerOptions,
}

impl Scheduler {
    /// Builds a scheduler over `db`'s pools.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; the scheduler clones its writer and reader
    ///   pools.
    /// - `queue`: the queue a due schedule enqueues its job into.
    /// - `options`: the tick interval.
    ///
    /// # Returns
    ///
    /// The scheduler.
    pub fn new(db: &Database, queue: Arc<JobQueue>, options: SchedulerOptions) -> Scheduler {
        Scheduler {
            queue,
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            options,
        }
    }

    /// Returns the options the scheduler was built with.
    pub fn options(&self) -> &SchedulerOptions {
        &self.options
    }

    /// Creates or updates a schedule and returns the stored row.
    ///
    /// The schedule is identified by its owner and kind, so upserting the system
    /// schedules on every startup is idempotent: an existing schedule keeps its
    /// id, its enabled flag, and its next fire time unless the expression
    /// changed or a fire time was never computed. The payload is always
    /// replaced.
    ///
    /// # Parameters
    ///
    /// - `spec`: the schedule to create or update.
    ///
    /// # Returns
    ///
    /// The stored schedule.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Cron`] when the expression cannot be parsed, which
    /// also means nothing is written, and [`JobsError::Database`] when a
    /// statement fails. A schedule deleted while this call runs stays deleted;
    /// the next tick simply does not find it.
    pub async fn upsert(&self, spec: ScheduleSpec) -> Result<Schedule, JobsError> {
        let ScheduleSpec {
            owner_user_id,
            kind,
            cron: expression,
            payload,
        } = spec;
        let cron = parse_cron(&expression)?;
        let now = now_millis();
        let payload = match &payload {
            Some(value) => Some(serde_json::to_string(value)?),
            None => None,
        };
        let owner = owner_user_id.map(|id| id.as_bytes().to_vec());

        let existing = sqlx::query_as!(
            ScheduleRow,
            "SELECT id, owner_user_id, kind, cron, payload, enabled, next_run_at, last_run_at
             FROM schedules
             WHERE kind = ? AND owner_user_id IS ?",
            kind.as_str(),
            owner.as_deref(),
        )
        .fetch_optional(&self.readers)
        .await?;

        match existing {
            Some(row) => {
                let stored = row.into_schedule()?;
                let next_run_at = if stored.cron != expression || stored.next_run_at.is_none() {
                    Some(next_occurrence_ms(&cron, now, &expression)?)
                } else {
                    stored.next_run_at
                };
                sqlx::query!(
                    "UPDATE schedules SET cron = ?, payload = ?, next_run_at = ? WHERE id = ?",
                    expression,
                    payload,
                    next_run_at,
                    stored.id.as_bytes().as_slice(),
                )
                .execute(&self.writer)
                .await?;
                Ok(Schedule {
                    cron: expression,
                    payload,
                    next_run_at,
                    ..stored
                })
            }
            None => {
                let id = hearthsound_core::new_uuid_v7();
                let next_run_at = next_occurrence_ms(&cron, now, &expression)?;
                // The row is built from the values written rather than read
                // back with RETURNING: a NULL blob parameter comes back from a
                // SQLite RETURNING row as an empty blob, which a schedule
                // without an owner has.
                sqlx::query!(
                    "INSERT INTO schedules
                         (id, owner_user_id, kind, cron, payload, enabled, next_run_at, last_run_at)
                     VALUES (?, ?, ?, ?, ?, 1, ?, NULL)",
                    id.as_bytes().as_slice(),
                    owner.as_deref(),
                    kind.as_str(),
                    expression,
                    payload,
                    next_run_at,
                )
                .execute(&self.writer)
                .await?;
                Ok(Schedule {
                    id,
                    owner_user_id,
                    kind,
                    cron: expression,
                    payload,
                    enabled: true,
                    next_run_at: Some(next_run_at),
                    last_run_at: None,
                })
            }
        }
    }

    /// Lists every schedule, ordered by kind.
    ///
    /// # Returns
    ///
    /// The schedules, system and per-user together.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the query fails.
    pub async fn list(&self) -> Result<Vec<Schedule>, JobsError> {
        let rows = sqlx::query_as!(
            ScheduleRow,
            "SELECT id, owner_user_id, kind, cron, payload, enabled, next_run_at, last_run_at
             FROM schedules
             ORDER BY kind ASC, owner_user_id ASC, id ASC",
        )
        .fetch_all(&self.readers)
        .await?;
        rows.into_iter().map(ScheduleRow::into_schedule).collect()
    }

    /// Enables or disables a schedule.
    ///
    /// A schedule that is enabled again keeps the fire time it already had, so
    /// one that was disabled across its next fire time fires once on the
    /// following tick and then resumes its timetable.
    ///
    /// # Parameters
    ///
    /// - `id`: the schedule to change.
    /// - `enabled`: whether it should fire.
    ///
    /// # Returns
    ///
    /// `true` when the schedule exists.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<bool, JobsError> {
        let result = sqlx::query!(
            "UPDATE schedules SET enabled = ? WHERE id = ?",
            i64::from(u8::from(enabled)),
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes a schedule.
    ///
    /// # Parameters
    ///
    /// - `id`: the schedule to delete.
    ///
    /// # Returns
    ///
    /// `true` when a row was deleted.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when the statement fails.
    pub async fn delete(&self, id: Uuid) -> Result<bool, JobsError> {
        let result = sqlx::query!(
            "DELETE FROM schedules WHERE id = ?",
            id.as_bytes().as_slice()
        )
        .execute(&self.writer)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Enqueues every schedule that is due and advances it.
    ///
    /// Each due schedule is handled in its own short transaction: the next fire
    /// time and the last fire time are written with a compare-and-set on the old
    /// fire time, and only a tick that advanced the row enqueues the job. A
    /// second tick racing this one therefore advances nothing and enqueues
    /// nothing, and a crash between the two writes rolls both back.
    ///
    /// The new fire time is computed from now, not from the fire time that was
    /// missed, so a schedule that was due many times during downtime is
    /// enqueued once.
    ///
    /// A schedule whose expression cannot be parsed is disabled and logged
    /// rather than failing the tick, so one bad row cannot stop the other
    /// schedules from firing. A schedule that has never had a fire time computed
    /// gets one on this tick but does not fire yet.
    ///
    /// # Returns
    ///
    /// How many schedules fired.
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::Database`] when a statement fails.
    pub async fn tick(&self) -> Result<usize, JobsError> {
        let now = now_millis();
        let mut fired = 0;

        let due = sqlx::query_as!(
            DueSchedule,
            "SELECT id, kind, cron, payload, next_run_at
             FROM schedules
             WHERE enabled = 1 AND next_run_at IS NOT NULL AND next_run_at <= ?
             ORDER BY next_run_at ASC",
            now,
        )
        .fetch_all(&self.readers)
        .await?;

        for schedule in due {
            let id = decode_id(&schedule.id, "schedules")?;
            let Some(previous_run_at) = schedule.next_run_at else {
                continue;
            };
            let next_run_at = match due_next_run(&schedule.cron, now) {
                Ok(next_run_at) => next_run_at,
                Err(error) => {
                    tracing::error!(schedule = %id, %error, "disabling a schedule whose expression is invalid");
                    self.set_enabled(id, false).await?;
                    continue;
                }
            };

            let mut transaction = self.writer.begin().await?;
            let advanced = sqlx::query!(
                "UPDATE schedules SET next_run_at = ?, last_run_at = ?
                 WHERE id = ? AND next_run_at = ?",
                next_run_at,
                previous_run_at,
                schedule.id,
                previous_run_at,
            )
            .execute(&mut *transaction)
            .await?;
            if advanced.rows_affected() == 0 {
                transaction.rollback().await?;
                continue;
            }

            let mut job = NewJob::new(schedule.kind.clone()).priority(SCHEDULED_PRIORITY);
            if let Some(payload) = &schedule.payload {
                job = job.payload_text(payload.clone());
            }
            self.queue.enqueue_tx(&mut transaction, job).await?;
            transaction.commit().await?;

            fired += 1;
            tracing::info!(
                schedule = %id,
                kind = %schedule.kind,
                run_at = previous_run_at,
                "scheduled job enqueued",
            );
        }

        self.queue.wake();
        self.arm_unscheduled(now).await?;
        Ok(fired)
    }

    /// Runs the tick loop until `shutdown` is triggered.
    ///
    /// A failed tick is logged and the loop continues, because a database that
    /// is temporarily busy or locked must not stop the scheduler from trying
    /// again a second later.
    ///
    /// # Parameters
    ///
    /// - `shutdown`: the signal that stops the loop.
    pub async fn run(&self, shutdown: Shutdown) {
        let interval = self.options.tick_interval.max(Duration::from_millis(1));
        loop {
            if shutdown.is_triggered() {
                return;
            }
            match self.tick().await {
                Ok(0) => {}
                Ok(fired) => tracing::debug!(fired, "schedules fired"),
                Err(error) => tracing::warn!(%error, "a scheduler tick failed"),
            }
            tokio::select! {
                _ = shutdown.wait() => return,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// Gives schedules that have never had a fire time computed one.
    ///
    /// A schedule created by an admin, or by an older build, may have no
    /// `next_run_at`. Computing it here rather than firing immediately keeps a
    /// schedule from running the moment it is created.
    ///
    /// # Parameters
    ///
    /// - `now`: the current time in unix milliseconds.
    async fn arm_unscheduled(&self, now: i64) -> Result<(), JobsError> {
        let unscheduled = sqlx::query_as!(
            UnscheduledSchedule,
            "SELECT id, cron FROM schedules WHERE enabled = 1 AND next_run_at IS NULL",
        )
        .fetch_all(&self.readers)
        .await?;

        for schedule in unscheduled {
            let id = decode_id(&schedule.id, "schedules")?;
            let next_run_at = match due_next_run(&schedule.cron, now) {
                Ok(next_run_at) => next_run_at,
                Err(error) => {
                    tracing::error!(schedule = %id, %error, "disabling a schedule whose expression is invalid");
                    self.set_enabled(id, false).await?;
                    continue;
                }
            };
            sqlx::query!(
                "UPDATE schedules SET next_run_at = ? WHERE id = ? AND next_run_at IS NULL",
                next_run_at,
                schedule.id,
            )
            .execute(&self.writer)
            .await?;
        }
        Ok(())
    }
}

/// The columns of one `schedules` row, as SQLite returns them.
struct ScheduleRow {
    id: Vec<u8>,
    owner_user_id: Option<Vec<u8>>,
    kind: String,
    cron: String,
    payload: Option<String>,
    enabled: i64,
    next_run_at: Option<i64>,
    last_run_at: Option<i64>,
}

impl ScheduleRow {
    /// Decodes the stored id and flag into a [`Schedule`].
    ///
    /// # Errors
    ///
    /// Returns [`JobsError::InvalidId`] for an id that is not a 16 byte UUID.
    fn into_schedule(self) -> Result<Schedule, JobsError> {
        let owner_user_id = match self.owner_user_id {
            Some(bytes) => Some(decode_id(&bytes, "users")?),
            None => None,
        };
        Ok(Schedule {
            id: decode_id(&self.id, "schedules")?,
            owner_user_id,
            kind: JobKind::new(self.kind),
            cron: self.cron,
            payload: self.payload,
            enabled: self.enabled != 0,
            next_run_at: self.next_run_at,
            last_run_at: self.last_run_at,
        })
    }
}

/// The columns a due schedule is handled with.
struct DueSchedule {
    id: Vec<u8>,
    kind: String,
    cron: String,
    payload: Option<String>,
    next_run_at: Option<i64>,
}

/// The columns a schedule with no fire time is armed with.
struct UnscheduledSchedule {
    id: Vec<u8>,
    cron: String,
}

/// Parses a cron expression.
///
/// # Errors
///
/// Returns [`JobsError::Cron`] when the expression is not a valid cron pattern.
fn parse_cron(expression: &str) -> Result<Cron, JobsError> {
    Cron::from_str(expression).map_err(|source| JobsError::Cron {
        expression: expression.to_owned(),
        source,
    })
}

/// Computes the next fire time of an expression, in unix milliseconds.
///
/// The occurrence is computed in the server's local time zone and is strictly
/// after `after_ms`, so a schedule that is due now fires once and then waits for
/// its next occurrence rather than repeating the one it missed.
///
/// The local zone is the process's own, which `TZ` selects on a container: a
/// housekeeping schedule such as `0 3 * * *` means 03:00 by the operator's wall
/// clock. A clock change therefore moves the run: when the clocks jump forward
/// the wall-clock time inside the skipped hour does not exist that day, so the
/// run is missed, and when they jump back it occurs twice, so it runs once.
///
/// # Parameters
///
/// - `expression`: the cron expression, used in error messages only.
/// - `after_ms`: the time to search from, in unix milliseconds.
///
/// # Errors
///
/// Returns [`JobsError::Cron`] when the expression cannot be parsed or when the
/// search leaves the range croner can represent.
fn due_next_run(expression: &str, after_ms: i64) -> Result<i64, JobsError> {
    let cron = parse_cron(expression)?;
    next_occurrence_ms(&cron, after_ms, expression)
}

/// Computes the next occurrence of a parsed expression.
///
/// # Parameters
///
/// - `cron`: the parsed expression.
/// - `after_ms`: the time to search from, in unix milliseconds.
/// - `expression`: the source text, used in error messages only.
///
/// # Errors
///
/// Returns [`JobsError::Cron`] when `after_ms` is outside the range a date and
/// time can represent, or when the search leaves that range.
fn next_occurrence_ms(cron: &Cron, after_ms: i64, expression: &str) -> Result<i64, JobsError> {
    let invalid = || JobsError::Cron {
        expression: expression.to_owned(),
        source: croner::errors::CronError::InvalidDate,
    };
    let after = DateTime::from_timestamp_millis(after_ms).ok_or_else(invalid)?;
    let after: DateTime<Local> = after.with_timezone(&Local);
    let next = cron
        .find_next_occurrence(&after, false)
        .map_err(|source| JobsError::Cron {
            expression: expression.to_owned(),
            source,
        })?;
    Ok(next.timestamp_millis())
}

/// Decodes a stored 16 byte UUID.
///
/// # Parameters
///
/// - `bytes`: the stored column value.
/// - `table`: the table the value came from, used in the error.
///
/// # Errors
///
/// Returns [`JobsError::InvalidId`] when the value is not a UUID.
fn decode_id(bytes: &[u8], table: &'static str) -> Result<Uuid, JobsError> {
    Uuid::from_slice(bytes).map_err(|_| JobsError::InvalidId { table })
}
