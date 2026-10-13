//! Errors of the activity layer.
//!
//! [`ActivityError`] currently covers the one thing that can go wrong while
//! recording or querying a play: the database rejecting the statement.

/// An error from recording or querying plays.
#[derive(Debug, thiserror::Error)]
pub enum ActivityError {
    /// The database rejected a read or a write.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}
