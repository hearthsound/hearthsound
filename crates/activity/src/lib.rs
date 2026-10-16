//! Listener activity: play history, likes, and the guard that decides what
//! counts.
//!
//! The crate owns the `plays` table. A play is recorded by
//! [`ActivityService::scrobble`] when a client submits one, but only if the
//! stream the client listened to delivered song audio: a client that gave up
//! before a pending acquisition produced a file still calls `scrobble`, and
//! recording a play for a song the user never heard would corrupt play history
//! and the discovery input built on it.
//!
//! The evidence for that decision lives in [`StreamSessions`]. The stream
//! service registers a session with [`StreamSessions::register`] when it starts
//! serving a track for a user, handing over an [`AudioDelivered`] that answers
//! later whether any song audio reached the client: the body of a served stream
//! sets a shared flag once it has written bytes, or a probe answers the question
//! itself. Only the latest session per user and track is kept, and the registry
//! is bounded by a lifetime and a capacity so a client that never scrobbles
//! cannot grow it forever.
//!
//! The crate also owns the per-user rows a Subsonic client writes directly:
//! [`ActivityService::star`] and [`ActivityService::unstar`] record likes and
//! keep the personal-library pin of every track they affect in step, so a liked
//! track is permanent in that user's library.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//!
//! use hearthsound_activity::{ActivityService, Scrobble, ScrobbleOutcome};
//! use hearthsound_db::Database;
//!
//! # async fn example(user_id: uuid::Uuid, track_id: uuid::Uuid) -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open(Path::new("/tmp/hearthsound.db")).await?;
//! let activity = ActivityService::new(&db);
//!
//! let (delivered, flag) = hearthsound_activity::AudioDelivered::flag();
//! activity.sessions().register(user_id, track_id, Some("dsub"), delivered);
//! flag.store(true, std::sync::atomic::Ordering::SeqCst);
//!
//! let outcome = activity
//!     .scrobble(&Scrobble {
//!         user_id,
//!         track_id,
//!         client: Some("dsub"),
//!         submission: true,
//!         started_at: 0,
//!         listened_ms: 180_000,
//!     })
//!     .await?;
//! assert!(matches!(outcome, ScrobbleOutcome::Recorded(_)));
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod error;
mod feedback;
mod play;
mod service;
mod session;

pub use error::ActivityError;
pub use feedback::{like, refresh_user_library_pin, unlike};
pub use play::Plays;
pub use service::{ActivityService, Scrobble, ScrobbleOutcome};
pub use session::{AudioDelivered, SessionOptions, StreamSession, StreamSessions};

/// The statement a transaction that reads before it writes starts with.
///
/// Every statement of the activity tables reaches its decision by reading a row
/// first, so the transaction takes the write lock immediately instead of
/// upgrading it later, which is what keeps two of them from deadlocking on
/// SQLite's single writer.
pub(crate) const READ_THEN_WRITE: &str = "BEGIN IMMEDIATE";

/// Returns the current time in unix milliseconds.
///
/// Every timestamp this crate writes shares one clock and one unit.
///
/// # Returns
///
/// The number of milliseconds since the Unix epoch.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
