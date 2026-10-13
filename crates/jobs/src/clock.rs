//! The wall clock the queue and the scheduler read.
//!
//! Times are unix milliseconds UTC everywhere in the database, so this module
//! is the one place that asks the operating system for the current time.

use std::time::Duration;

/// Returns the current time as unix milliseconds.
///
/// # Returns
///
/// Milliseconds since the unix epoch, UTC.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
}

/// Converts a duration to whole milliseconds, saturating at [`i64::MAX`].
///
/// # Parameters
///
/// - `duration`: the duration to convert.
///
/// # Returns
///
/// The duration in milliseconds.
pub(crate) fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// Adds a duration to a unix millisecond timestamp, saturating at [`i64::MAX`].
///
/// # Parameters
///
/// - `unix_ms`: the base timestamp.
/// - `duration`: how far past it to go.
///
/// # Returns
///
/// The resulting timestamp.
pub(crate) fn add(unix_ms: i64, duration: Duration) -> i64 {
    unix_ms.saturating_add(millis(duration))
}
