//! Log output for the binary.
//!
//! The process logs in `log.format`, with two levels: `log.level` for
//! Hearthsound's own records and `log.dependencies` for every other crate. The
//! split matters because a third-party crate logs about every file the server
//! touches - a tag reader warns about the header of every scanned MP3, a watcher
//! about every event it delivers - and one global level cannot admit the
//! server's `info` records without admitting those warnings too.
//!
//! The subscriber is installed once, before any command runs, so a startup
//! failure is logged as well as printed.

use hearthsound_config::bootstrap::{LogConfig, LogFormat, LogLevel};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The target prefix every workspace crate logs under.
///
/// One `Targets` directive covers the whole crate tree: `hearthsound`,
/// `hearthsound_library`, `hearthsound_plugin_host` and the rest all start with
/// this, and no third-party crate does.
const WORKSPACE_TARGET: &str = "hearthsound";

/// Installs the process-wide log subscriber.
///
/// # Parameters
///
/// - `log`: the `log.level`, `log.format`, and `log.dependencies` values.
///
/// # Errors
///
/// Returns an error when the subscriber cannot be installed, which means another
/// subscriber is already set for this process.
pub fn init(log: &LogConfig) -> anyhow::Result<()> {
    let filter = filter(log);
    // The two formats build different subscriber types, so each is installed
    // from its own arm rather than from a value both could hold.
    let installed = match log.format {
        LogFormat::Text => tracing_subscriber::fmt().finish().with(filter).try_init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .finish()
            .with(filter)
            .try_init(),
    };
    installed.map_err(|error| anyhow::anyhow!("could not install the log subscriber: {error}"))
}

/// Builds the level filter the subscriber is installed with.
///
/// The workspace's own records follow `log.level`; every other target - a tag
/// reader, a watcher, a codec, an HTTP client - follows `log.dependencies`. A
/// target-bearing directive takes precedence over the default one, which is what
/// makes the split work.
///
/// # Parameters
///
/// - `log`: the two configured levels.
///
/// # Returns
///
/// The filter.
pub fn filter(log: &LogConfig) -> Targets {
    Targets::new()
        .with_default(level(log.dependencies))
        .with_target(WORKSPACE_TARGET, level(log.level))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(level: LogLevel, dependencies: LogLevel) -> LogConfig {
        LogConfig {
            level,
            format: LogFormat::Text,
            dependencies,
        }
    }

    #[test]
    fn the_default_filter_hides_third_party_warnings() {
        let filter = filter(&config(LogLevel::Info, LogLevel::Error));

        assert!(
            !filter.would_enable("lofty::mpeg::header", &tracing::Level::WARN),
            "a tag reader's per-file warning is not logged"
        );
        assert!(
            !filter.would_enable("notify::inotify", &tracing::Level::WARN),
            "a watcher's warning is not logged"
        );
        assert!(
            filter.would_enable("hearthsound_library::scan", &tracing::Level::INFO),
            "the server's own records are logged"
        );
        assert!(
            filter.would_enable("hearthsound", &tracing::Level::INFO),
            "the binary's own records are logged"
        );
    }

    #[test]
    fn a_raised_dependency_level_admits_third_party_records() {
        let filter = filter(&config(LogLevel::Info, LogLevel::Trace));

        assert!(filter.would_enable("lofty::mpeg::header", &tracing::Level::TRACE));
        assert!(filter.would_enable("notify::inotify", &tracing::Level::DEBUG));
    }

    #[test]
    fn the_workspace_level_does_not_raise_the_third_party_one() {
        let filter = filter(&config(LogLevel::Debug, LogLevel::Error));

        assert!(filter.would_enable("hearthsound_library::scan", &tracing::Level::DEBUG));
        assert!(!filter.would_enable("lofty::mpeg::header", &tracing::Level::WARN));
    }
}
