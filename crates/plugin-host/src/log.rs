//! Plugin logging: a rate limited sink and the guest's stdout and stderr.
//!
//! A plugin can log as much as it likes; the host decides how much of it
//! reaches the server's tracing subscriber. Lines are capped per second and per
//! message, and the dropped count is reported once the window rolls over so an
//! operator can see that a plugin is noisy rather than silently losing output.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hearthsound_core::Slug;

/// How long one rate limit window lasts.
const WINDOW: Duration = Duration::from_secs(1);

/// How many lines may be emitted in one window.
const WINDOW_LIMIT: u32 = 100;

/// The largest message forwarded to tracing, in bytes.
const MESSAGE_LIMIT: usize = 4096;

/// How much unterminated output is buffered before it is forwarded.
const CHUNK: usize = 4096;

/// The tracing target every plugin log line uses.
const TARGET: &str = "hearthsound::plugin";

/// The severity of a plugin log line.
///
/// Each plugin API version generates its own `host-log` level enum, and the
/// generated types never reach this module: every version's `host-log` import
/// maps its level onto this one, so the sink and the guest's stdout are written
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Finest-grained diagnostic output.
    Trace,
    /// Diagnostic output useful while developing a plugin.
    Debug,
    /// A routine event.
    Info,
    /// Something the operator should look at.
    Warn,
    /// A failure the plugin recovered from or reported.
    Error,
}

/// A rate limited log sink for one plugin instance.
#[derive(Debug)]
pub struct LogSink {
    plugin: String,
    instance: Slug,
    window: Mutex<LogWindow>,
}

/// The state of the current rate limit window.
#[derive(Debug)]
struct LogWindow {
    started: Instant,
    emitted: u32,
    dropped: u32,
}

impl LogSink {
    /// Creates a sink for one plugin instance.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin id, reported as the `plugin` field.
    /// - `instance`: the instance slug, reported as the `instance` field.
    pub fn new(plugin: String, instance: Slug) -> LogSink {
        LogSink {
            plugin,
            instance,
            window: Mutex::new(LogWindow {
                started: Instant::now(),
                emitted: 0,
                dropped: 0,
            }),
        }
    }

    /// Returns the plugin id.
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// Returns the instance slug.
    pub fn instance(&self) -> &Slug {
        &self.instance
    }

    /// Forwards one message, applying the rate limit and truncation.
    ///
    /// # Parameters
    ///
    /// - `level`: the level the plugin logged at.
    /// - `message`: the message text.
    pub fn write(&self, level: Level, message: &str) {
        let mut window = match self.window.lock() {
            Ok(window) => window,
            Err(poisoned) => poisoned.into_inner(),
        };

        if window.started.elapsed() >= WINDOW {
            if window.dropped > 0 {
                let dropped = window.dropped;
                tracing::warn!(
                    target: TARGET,
                    plugin = %self.plugin,
                    instance = %self.instance,
                    dropped,
                    "plugin log lines dropped by rate limit"
                );
            }
            *window = LogWindow {
                started: Instant::now(),
                emitted: 0,
                dropped: 0,
            };
        }

        if window.emitted >= WINDOW_LIMIT {
            window.dropped += 1;
            return;
        }
        window.emitted += 1;
        drop(window);

        let message = truncate(message, MESSAGE_LIMIT);
        match level {
            Level::Trace => {
                tracing::trace!(target: TARGET, plugin = %self.plugin, instance = %self.instance, "{message}")
            }
            Level::Debug => {
                tracing::debug!(target: TARGET, plugin = %self.plugin, instance = %self.instance, "{message}")
            }
            Level::Info => {
                tracing::info!(target: TARGET, plugin = %self.plugin, instance = %self.instance, "{message}")
            }
            Level::Warn => {
                tracing::warn!(target: TARGET, plugin = %self.plugin, instance = %self.instance, "{message}")
            }
            Level::Error => {
                tracing::error!(target: TARGET, plugin = %self.plugin, instance = %self.instance, "{message}")
            }
        }
    }
}

/// Cuts `message` to `limit` bytes at a character boundary.
///
/// # Parameters
///
/// - `message`: the text to cut.
/// - `limit`: the largest number of bytes to keep.
///
/// # Returns
///
/// The original text when it is short enough, otherwise a prefix followed by
/// ` [truncated]`.
fn truncate(message: &str, limit: usize) -> String {
    if message.len() <= limit {
        return message.to_owned();
    }
    let mut end = limit;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = message[..end].to_owned();
    truncated.push_str(" [truncated]");
    truncated
}

/// The guest's stdout or stderr, forwarded to a [`LogSink`].
#[derive(Debug)]
pub struct GuestOutput {
    sink: Arc<LogSink>,
    level: Level,
}

impl GuestOutput {
    /// Creates an output stream that logs at `level`.
    ///
    /// # Parameters
    ///
    /// - `sink`: the sink to write lines to.
    /// - `level`: the level to log at.
    pub fn new(sink: Arc<LogSink>, level: Level) -> GuestOutput {
        GuestOutput { sink, level }
    }
}

impl wasmtime_wasi::cli::IsTerminal for GuestOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl wasmtime_wasi::cli::StdoutStream for GuestOutput {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(GuestWriter {
            sink: Arc::clone(&self.sink),
            level: self.level,
            buffer: Vec::new(),
        })
    }
}

/// The writer behind [`GuestOutput`].
#[derive(Debug)]
struct GuestWriter {
    sink: Arc<LogSink>,
    level: Level,
    buffer: Vec<u8>,
}

impl GuestWriter {
    /// Forwards every complete line and every full chunk in the buffer.
    fn drain(&mut self) {
        while let Some(index) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=index).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            self.sink.write(self.level, &text);
        }
        while self.buffer.len() >= CHUNK {
            let chunk: Vec<u8> = self.buffer.drain(..CHUNK).collect();
            let text = String::from_utf8_lossy(&chunk).into_owned();
            self.sink.write(self.level, &text);
        }
    }

    /// Forwards whatever is left in the buffer.
    fn flush_remaining(&mut self) {
        if !self.buffer.is_empty() {
            let text = String::from_utf8_lossy(&self.buffer).into_owned();
            self.buffer.clear();
            self.sink.write(self.level, &text);
        }
    }
}

impl tokio::io::AsyncWrite for GuestWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.buffer.extend_from_slice(buf);
        this.drain();
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().flush_remaining();
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.get_mut().flush_remaining();
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    /// Locks the capture buffer, ignoring poisoning.
    fn buffer(capture: &Capture) -> std::sync::MutexGuard<'_, Vec<u8>> {
        match capture.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// A `MakeWriter` that appends everything written into a shared buffer.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            buffer(self).extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn sink() -> LogSink {
        LogSink::new("com.example.test".to_owned(), Slug::new("test").unwrap())
    }

    #[test]
    fn rate_limits_and_reports_drops() {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let sink = sink();

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..150 {
                sink.write(Level::Info, "line");
            }
        });

        let output = String::from_utf8(buffer(&capture).clone()).unwrap();
        let lines = output.lines().count();
        assert_eq!(lines, 100, "expected 100 lines, got {lines}");

        // Roll the window over so the dropped count is reported.
        std::thread::sleep(WINDOW + Duration::from_millis(50));
        tracing::subscriber::with_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_ansi(false)
                .finish(),
            || {
                sink.write(Level::Info, "line");
            },
        );

        let output = String::from_utf8(buffer(&capture).clone()).unwrap();
        assert!(
            output.contains("dropped=50"),
            "expected a dropped=50 line, got:\n{output}"
        );
    }

    #[test]
    fn truncates_long_messages() {
        let message = "a".repeat(5000);
        let truncated = truncate(&message, MESSAGE_LIMIT);
        assert_eq!(truncated.len(), MESSAGE_LIMIT + " [truncated]".len());
        assert!(truncated.ends_with(" [truncated]"));
    }
}
