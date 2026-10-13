//! The error type of the job queue, runner, and scheduler.

use croner::errors::CronError;

/// An error from the job queue, the job runner, or the cron scheduler.
#[derive(Debug, thiserror::Error)]
pub enum JobsError {
    /// The database reported an error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A payload could not be encoded to, or decoded from, JSON.
    #[error("job payload is not valid JSON: {0}")]
    Payload(#[from] serde_json::Error),

    /// A stored `jobs.priority` is not one of the priority levels.
    #[error("job priority {level} is not a known priority level")]
    UnknownPriority {
        /// The level that was read from the column.
        level: i64,
    },

    /// A stored `jobs.state` is not one of the state literals.
    #[error("`{state}` is not a known job state")]
    UnknownState {
        /// The literal that was read from the column.
        state: String,
    },

    /// A table held an id that is not a 16 byte UUID.
    #[error("`{table}` holds a row id that is not a 16 byte UUID")]
    InvalidId {
        /// The table the id was read from.
        table: &'static str,
    },

    /// A cron expression could not be parsed or could not be resolved.
    #[error("invalid cron expression `{expression}`: {source}")]
    Cron {
        /// The expression as it is stored in the schedule.
        expression: String,
        /// The parser or search error.
        #[source]
        source: CronError,
    },
}
