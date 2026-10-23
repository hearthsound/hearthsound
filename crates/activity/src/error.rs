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

    /// A rating is outside the range a client may set.
    #[error("rating {rating} is not between 0 and {max}")]
    InvalidRating {
        /// The rating the caller asked for.
        rating: u8,
        /// The highest rating a client may set.
        max: u8,
    },

    /// A playlist was named that has no row.
    #[error("playlist `{id}` does not exist")]
    UnknownPlaylist {
        /// The playlist's client id.
        id: String,
    },

    /// An edit was attempted by an account that does not own the playlist.
    #[error("playlist `{id}` belongs to another account")]
    NotOwner {
        /// The playlist's client id.
        id: String,
    },

    /// A playlist name is empty or too long.
    #[error("playlist name `{name}` is empty or longer than {max} bytes")]
    InvalidName {
        /// The name the caller supplied.
        name: String,
        /// The longest name the server stores.
        max: usize,
    },

    /// A reorder did not list every position of the playlist exactly once.
    ///
    /// An order that repeats or omits a position would drop or duplicate an
    /// entry, so it is refused before anything is written.
    #[error("a playlist of {len} entries needs every position from 0 to {len} listed once")]
    InvalidOrder {
        /// How many entries the playlist holds.
        len: usize,
    },

    /// A saved play queue names an entry it does not hold.
    #[error("queue index {index} is outside a queue of {len} entries")]
    InvalidQueueIndex {
        /// The index the caller supplied.
        index: usize,
        /// How many entries the queue holds.
        len: usize,
    },

    /// A stored list of client ids is not valid JSON.
    #[error("stored list is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}
