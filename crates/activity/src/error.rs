//! Errors of the activity layer.
//!
//! [`ActivityError`] covers the two things that can go wrong while recording or
//! querying activity: the database rejecting the statement, and the library
//! layer refusing a pin or an availability write.

/// An error from recording or querying activity.
#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    /// The library layer refused a pin or an availability write.
    #[error(transparent)]
    Library(#[from] hearthsound_library::LibraryError),

    /// A 16-byte identifier stored as a BLOB did not decode as a UUID.
    #[error("stored identifier is not a UUID: {0}")]
    CorruptId(#[from] uuid::Error),
}
