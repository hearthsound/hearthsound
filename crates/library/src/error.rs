//! Errors of the library layer.
//!
//! [`LibraryError`] covers everything that can go wrong while reading or
//! writing a library row: the database rejecting a statement, a stored literal
//! that is not one of the known values, an identifier that does not decode, and
//! the filesystem errors a folder add or a scan produces.

use std::path::PathBuf;

use uuid::Uuid;

use crate::model::UnknownOrigin;

/// An error from the library layer.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// The job queue rejected an enqueue.
    #[error("job queue error: {0}")]
    Queue(#[from] hearthsound_jobs::JobsError),
    /// A value stored as a setting was rejected by that setting's kind.
    #[error(transparent)]
    Setting(#[from] hearthsound_config::ConfigError),
    /// A 16-byte identifier stored as a BLOB did not decode as a UUID.
    #[error("stored identifier is not a UUID: {0}")]
    CorruptId(#[from] uuid::Error),
    /// A literal read back from a row is not one of the known values.
    #[error("stored literal `{value}` is not a known {kind}")]
    UnknownLiteral {
        /// The column or literal family the value came from.
        kind: &'static str,
        /// The unexpected value.
        value: String,
    },
    /// An `origin` column held a value outside `local` and `catalog`.
    #[error(transparent)]
    Origin(#[from] UnknownOrigin),
    /// A library folder path does not exist, is not a directory, or cannot be
    /// canonicalized.
    #[error("library folder path `{path}` is not a usable directory: {source}")]
    FolderPath {
        /// The path that was rejected.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A library folder was referenced that has no row.
    #[error("library folder `{folder_id}` does not exist")]
    UnknownFolder {
        /// The id that has no row.
        folder_id: Uuid,
    },
    /// A row was expected but not found.
    #[error("no {kind} with id `{id}`")]
    NotFound {
        /// The kind of row that was missing, for example `track`.
        kind: &'static str,
        /// The client id that was looked up.
        id: String,
    },
    /// A filesystem operation failed while walking a folder.
    #[error("reading `{path}` failed: {source}")]
    Scan {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A media probe or picture extraction failed.
    #[error(transparent)]
    Media(#[from] hearthsound_media::MediaError),
    /// A stored JSON column could not be decoded, or a value could not be
    /// encoded.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// An import path template could not be read, parsed, or rendered.
    #[error(transparent)]
    Naming(#[from] crate::naming::NamingTemplateError),
    /// A blocking task could not be joined.
    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}
