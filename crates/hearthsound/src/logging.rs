//! Log output for the binary.
//!
//! The process logs at `log.level` in `log.format`, the two bootstrap keys the
//! rest of the server also honours: `text` for a human reading a terminal or a
//! journal, `json` for a log collector. The subscriber is installed once, before
//! any command runs, so a startup failure is logged as well as printed.

use hearthsound_config::bootstrap::{LogConfig, LogFormat, LogLevel};

/// Installs the process-wide log subscriber.
///
/// # Parameters
///
/// - `log`: the `log.level` and `log.format` values.
///
/// # Errors
///
/// Returns an error when the subscriber cannot be installed, which means another
/// subscriber is already set for this process.
pub fn init(log: &LogConfig) -> anyhow::Result<()> {
    let level = level(log.level);
    match log.format {
        LogFormat::Text => tracing_subscriber::fmt()
            .with_max_level(level)
            .try_init()
            .map_err(|error| anyhow::anyhow!("could not install the text log subscriber: {error}")),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_max_level(level)
            .try_init()
            .map_err(|error| anyhow::anyhow!("could not install the JSON log subscriber: {error}")),
    }
}

/// Maps a bootstrap log level onto a tracing level.
///
/// # Parameters
///
/// - `level`: the configured level.
///
/// # Returns
///
/// The equivalent `tracing` level.
pub fn level(level: LogLevel) -> tracing::Level {
    match level {
        LogLevel::Trace => tracing::Level::TRACE,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Error => tracing::Level::ERROR,
    }
}
