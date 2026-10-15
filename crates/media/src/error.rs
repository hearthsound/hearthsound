//! The error type shared by the media operations.

use std::path::PathBuf;

/// Everything a media operation can fail with.
///
/// The variants name the failure mode rather than the underlying library, so
/// callers can map them onto API responses without matching on `lofty` or
/// `symphonia` types. The source error, when there is one, is kept in the
/// `source` field.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// A file could not be opened, read, or written.
    #[error("media io error at `{path}`: {source}")]
    Io {
        /// The path the operation used.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The file is in a format the probe does not understand.
    #[error("unsupported media at `{path}`: {message}")]
    Unsupported {
        /// The path the operation used.
        path: PathBuf,
        /// What was not supported.
        message: String,
    },
    /// The file could not be decoded from end to end.
    #[error("media file at `{path}` does not decode: {message}")]
    Decode {
        /// The path the operation used.
        path: PathBuf,
        /// What the decoder reported.
        message: String,
    },
    /// A tag could not be read or written.
    #[error("tag error at `{path}`: {source}")]
    Tag {
        /// The path the operation used.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Reading an ffmpeg child's output failed.
    #[error("reading an ffmpeg stream failed: {message}")]
    Stream {
        /// What the reader reported.
        message: String,
    },
    /// An ffmpeg process could not be started or exited with a failure.
    #[error("{program} {args} failed: {message}")]
    Process {
        /// The program that ran.
        program: String,
        /// The argument vector, joined for the message.
        args: String,
        /// The exit status plus the captured standard error.
        message: String,
    },
    /// A blocking media task panicked or was cancelled.
    #[error("media background task failed: {message}")]
    TaskFailed {
        /// The panic payload or cancellation reason.
        message: String,
    },
}

impl MediaError {
    /// Builds an [`MediaError::Io`] for a path.
    ///
    /// # Parameters
    ///
    /// - `path`: the path the operation used.
    /// - `source`: the underlying error.
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> MediaError {
        MediaError::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Builds a [`MediaError::Unsupported`] for a path.
    ///
    /// # Parameters
    ///
    /// - `path`: the path the operation used.
    /// - `message`: what was not supported.
    pub(crate) fn unsupported(path: &std::path::Path, message: impl Into<String>) -> MediaError {
        MediaError::Unsupported {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }

    /// Builds a [`MediaError::Decode`] for a path.
    ///
    /// # Parameters
    ///
    /// - `path`: the path the operation used.
    /// - `message`: what the decoder reported.
    pub(crate) fn decode(path: &std::path::Path, message: impl Into<String>) -> MediaError {
        MediaError::Decode {
            path: path.to_path_buf(),
            message: message.into(),
        }
    }

    /// Builds a [`MediaError::Tag`] for a path.
    ///
    /// # Parameters
    ///
    /// - `path`: the path the operation used.
    /// - `source`: the underlying error.
    pub(crate) fn tag(
        path: &std::path::Path,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> MediaError {
        MediaError::Tag {
            path: path.to_path_buf(),
            source: Box::new(source),
        }
    }

    /// Converts the result of a blocking task join into a [`MediaError`].
    ///
    /// # Parameters
    ///
    /// - `error`: the join error.
    pub(crate) fn task_failed<E: std::fmt::Display>(error: E) -> MediaError {
        MediaError::TaskFailed {
            message: error.to_string(),
        }
    }
}
