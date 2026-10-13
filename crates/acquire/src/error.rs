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
    /// A media task failed.
    #[error(transparent)]
    Media(#[from] hearthsound_media::MediaError),
    /// The import naming template could not be read or parsed.
    #[error(transparent)]
    NamingTemplate(#[from] hearthsound_library::naming::NamingTemplateError),
    /// The import naming template could not be rendered.
    #[error(transparent)]
    Naming(#[from] hearthsound_library::naming::NamingError),
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
    /// A download reported an output file under a root the instance does not
    /// grant.
    #[error("instance `{instance}` has no import root `{root}`")]
    UnknownRoot {
        /// The slug of the instance that reported the file.
        instance: String,
        /// The root name the plugin used.
        root: String,
    },
    /// A download reported an output path that leaves its root.
    #[error("the reported output path `{path}` leaves the root `{root}`")]
    UnsafeOutputPath {
        /// The root name the plugin used.
        root: String,
        /// The path the plugin reported.
        path: String,
    },
    /// An acquisition id has no row.
    #[error("no acquisition with id `{0}`")]
    UnknownAcquisition(Uuid),
    /// No free destination path could be found for an imported file.
    #[error("no free import path could be found under `{path}`")]
    ImportPath {
        /// The destination path whose names are all taken.
        path: PathBuf,
    },
    /// The acquisition state machine forbids a transition that was attempted.
    #[error("an acquisition cannot move from `{from}` to `{to}`")]
    IllegalTransition {
        /// The state the acquisition is in.
        from: AcquisitionState,
        /// The state the caller tried to move it to.
        to: AcquisitionState,
    },
}
