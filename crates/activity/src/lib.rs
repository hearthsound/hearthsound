//! Listener activity: play history and the guard that decides what counts.
//!
//! The crate owns the `plays` table. A play is recorded by
//! [`ActivityService::scrobble`] when a client submits one, but only if the
//! stream the client listened to delivered song audio: a client in `pad` mode
//! that was fed silence padding while an acquisition failed still calls
//! `scrobble`, and recording a play for a song the user never heard would
//! corrupt play history and the discovery input built on it.
//!
//! The evidence for that decision lives in [`StreamSessions`]. The stream
//! service registers a session with [`StreamSessions::register`] when it starts
//! serving a track for a user, handing over an [`AudioDelivered`] that answers
//! later whether any song audio reached the client: a raw file stream sets a
//! shared flag once it has written bytes, and a pending stream answers from its
//! `hearthsound_media::PendingControl::audio_delivered` handle. Only the latest
//! session per user and track is kept, and the registry is bounded by a lifetime
//! and a capacity so a client that never scrobbles cannot grow it forever.
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
mod play;
mod service;
mod session;

pub use error::ActivityError;
pub use play::Plays;
pub use service::{ActivityService, Scrobble, ScrobbleOutcome};
pub use session::{AudioDelivered, SessionOptions, StreamSession, StreamSessions};
