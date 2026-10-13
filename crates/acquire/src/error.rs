//! Errors of the acquisition layer.

use std::path::PathBuf;

use hearthsound_core::{AcquisitionState, EntityId};
use uuid::Uuid;

/// An error from the acquisition layer.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A library read or write failed.
    #[error(transparent)]
    Library(#[from] hearthsound_library::LibraryError),
    /// A setting could not be read.
    #[error(transparent)]
    Config(#[from] hearthsound_config::ConfigError),
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
    /// A JSON column could not be decoded, or a value could not be encoded.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// A filesystem operation failed.
    #[error("`{path}` could not be used: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A request named a track that has no row.
    #[error("no track with id `{0}`")]
    UnknownTrack(EntityId),
    /// A request named an album that has no tracks.
    #[error("no album with id `{0}`")]
    UnknownAlbum(EntityId),
    /// An acquisition id has no row.
    #[error("no acquisition with id `{0}`")]
    UnknownAcquisition(Uuid),
    /// The acquisition state machine forbids a transition that was attempted.
    #[error("an acquisition cannot move from `{from}` to `{to}`")]
    IllegalTransition {
        /// The state the acquisition is in.
        from: AcquisitionState,
        /// The state the caller tried to move it to.
        to: AcquisitionState,
    },
}
