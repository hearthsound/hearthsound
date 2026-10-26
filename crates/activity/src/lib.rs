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
//! track is permanent in that user's library, and
//! [`ActivityService::set_rating`] stores a rating of 1 to 5 or clears one.
//! [`ActivityService::save`] and [`ActivityService::unsave`] are the same
//! operation for library membership itself, and
//! [`ActivityService::set_feedback`] writes a like, a dislike, or neither.
//! [`Playlists`] creates and edits playlists, keeping the playlist pin of every
//! track whose membership changed in step, [`PlayQueues`] stores the queue
//! each client saved so another client can pick it up, [`Bookmarks`] stores
//! the position a user left a track at, and [`NowPlaying`] stores what each
//! client is playing, which [`InMemoryReports`] keeps current from the playback
//! reports that also earn plays. A report's open session is a [`ListenSession`],
//! stored in the database so a restart mid-track keeps the listen. [`load_annotations`] reads what a user has done
//! with a whole page of entities at once, for the listing endpoints that report
//! stars, ratings, and play counts.
//!
//! A play, a now-playing report and a changed opinion are also scrobbles for the
//! services the listener linked: [`ScrobbleOutbox`] queues one row per linked
//! instance and [`ScrobbleDeliverer`] drains it with the service's own batching,
//! pacing and retry rules, so a service that is down loses nothing. An outcome
//! is retried when it is transient - the service is unavailable or asked for a
//! delay - and parked when it is terminal: a listen the service does not know, a
//! rejected credential or configuration, or a plugin that failed internally is
//! recorded as `failed` and shown to the listener instead of retried for a week.
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

mod annotations;
mod bookmarks;
mod discovery_sync;
mod error;
mod feedback;
mod listen_session;
mod now_playing;
mod play;
mod play_queues;
mod playlists;
mod ratings;
mod reports;
mod scrobble;
mod service;
mod session;

pub use annotations::{Annotation, MAX_IDS_PER_QUERY, disliked_ids, load as load_annotations};
pub use bookmarks::{Bookmark, Bookmarks};
pub use discovery_sync::{
    DISCOVERY_SYNC_JOB_KIND, DISCOVERY_SYNC_SCHEDULE, DiscoverySyncJobHandler,
    DiscoverySyncPayload, SyncReport,
};
pub use error::ActivityError;
pub use feedback::{clear, dislike, like, refresh_user_library_pin, unlike};
pub use listen_session::{LISTEN_SESSION_PERSIST_MS, ListenSession, ListenSessions, StoredSession};
pub use now_playing::{
    DEFAULT_CLIENT, NowPlaying, NowPlayingEntry, PLAYING_GRACE_MS, PLAYING_GRACE_UNKNOWN_MS,
};
pub use play::Plays;
pub use play_queues::{PlayQueues, SavedQueue};
pub use playlists::{
    EXPIRE_JOB_KIND, EXPIRE_SCHEDULE, ExpireJobHandler, MAX_PLAYLIST_NAME, Playlist, PlaylistEntry,
    PlaylistUpdate, Playlists, SyncedPlaylist, refresh_playlist_pin,
};
pub use ratings::{MAX_RATING, set_rating};
pub use reports::{
    InMemoryReports, LISTENED_HALF_MS, MIN_TRACK_MS, PlaybackOutcome, PlaybackReport,
    PlaybackState, Recovery, SCROBBLE_DEDUPE_MS,
};
pub use scrobble::{
    DeliveryReport, NOW_PLAYING_TTL_MS, OUTBOX_MAX_AGE_MS, OutboxCounters,
    SCROBBLE_DELIVER_JOB_KIND, SCROBBLE_DELIVER_SCHEDULE, ScrobbleDeliverJobHandler,
    ScrobbleDeliverer, ScrobbleOutbox, ScrobbleTarget, ScrobbleTargets,
};
pub use service::{ActivityService, Feedback, Scrobble, ScrobbleOutcome};
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
/// Every timestamp this crate writes shares one clock and one unit, so a caller
/// that drives a repository from outside - the server starting up and recovering
/// the open playback sessions, for example - stamps the same way.
///
/// # Returns
///
/// The number of milliseconds since the Unix epoch.
pub fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
