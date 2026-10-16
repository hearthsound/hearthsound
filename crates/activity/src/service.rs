//! The activity service: what a scrobble means and when it is a play.
//!
//! A client reports playback with `scrobble`. When the call carries
//! `submission=true` it claims the song was played, and the server records a
//! play for it - unless the latest stream session for the same user and track
//! reported that it delivered no song audio, which is what a client that gave
//! up before a pending acquisition produced a file would otherwise turn into a
//! false play. When the call carries `submission=false` it is a now-playing
//! report, which is accepted and discarded because now-playing is not stored.

use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::play::Plays;
use crate::play_queues::PlayQueues;
use crate::playlists::Playlists;
use crate::ratings;
use crate::session::StreamSessions;
use crate::{READ_THEN_WRITE, feedback, now_millis};

/// A client's scrobble request.
///
/// The fields are the ones the Subsonic `scrobble` endpoint resolves: the
/// authenticated user, the track, the client identifier the request carried,
/// whether the client is submitting a play or reporting now playing, when
/// playback started, and how long was listened.
///
/// # Examples
///
/// ```
/// use uuid::Uuid;
///
/// use hearthsound_activity::Scrobble;
///
/// let reported = Scrobble {
///     user_id: Uuid::now_v7(),
///     track_id: Uuid::now_v7(),
///     client: Some("dsub"),
///     submission: true,
///     started_at: 1_700_000_000_000,
///     listened_ms: 180_000,
/// };
/// assert!(reported.submission);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scrobble<'a> {
    /// The account that reports the playback.
    pub user_id: Uuid,
    /// The track that was played.
    pub track_id: Uuid,
    /// The client identifier the request carried, when there was one.
    pub client: Option<&'a str>,
    /// `true` to submit a play, `false` to report now playing.
    pub submission: bool,
    /// When playback started, in unix milliseconds.
    pub started_at: i64,
    /// How long the client listened, in milliseconds.
    pub listened_ms: i64,
}

/// What a scrobble did.
///
/// # Examples
///
/// ```
/// use uuid::Uuid;
///
/// use hearthsound_activity::ScrobbleOutcome;
///
/// let id = Uuid::now_v7();
/// assert_ne!(ScrobbleOutcome::Recorded(id), ScrobbleOutcome::Discarded);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleOutcome {
    /// A play was recorded; the value is the new `plays` row id.
    Recorded(Uuid),
    /// The call was a now-playing report: accepted and discarded.
    Discarded,
    /// The call was a submission, but the latest stream session for that user
    /// and track delivered no song audio, so no play was recorded.
    NoSongAudio,
}

/// Play recording and the per-stream guard that decides what counts.
///
/// The service pairs the [`Plays`] repository with the [`StreamSessions`]
/// registry the stream service registers its streams in, and answers scrobbles
/// with [`ActivityService::scrobble`]. Cloning is cheap: every clone shares the
/// same repository and registry.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database, user_id: uuid::Uuid, track_id: uuid::Uuid) {
/// use hearthsound_activity::{ActivityService, AudioDelivered};
///
/// let activity = ActivityService::new(db);
/// activity
///     .sessions()
///     .register(user_id, track_id, Some("dsub"), AudioDelivered::probe(|| true));
/// # let _ = activity;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ActivityService {
    /// The writer pool every mutation goes through.
    writer: SqlitePool,
    /// The `plays` table.
    plays: Plays,
    /// The playlist tables.
    playlists: Playlists,
    /// The saved play queue table.
    queues: PlayQueues,
    /// The stream sessions the guard reads.
    sessions: StreamSessions,
}

impl ActivityService {
    /// Builds the service over a database, with a registry of its own.
    ///
    /// The stream service gets that registry from [`ActivityService::sessions`]
    /// and registers every stream it opens.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    ///
    /// # Returns
    ///
    /// The service.
    pub fn new(db: &Database) -> ActivityService {
        ActivityService {
            writer: db.writer().clone(),
            plays: Plays::new(db),
            playlists: Playlists::new(db),
            queues: PlayQueues::new(db),
            sessions: StreamSessions::new(),
        }
    }

    /// Builds the service with a session registry the caller already shares.
    ///
    /// Use this when the stream service must reach the same registry before the
    /// service is built, or when the registry's bounds are not the defaults.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database.
    /// - `sessions`: the registry the guard reads and the stream service
    ///   registers streams in.
    ///
    /// # Returns
    ///
    /// The service.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// # async fn example(db: &hearthsound_db::Database) {
    /// use hearthsound_activity::{ActivityService, SessionOptions, StreamSessions};
    ///
    /// let sessions = StreamSessions::with_options(SessionOptions {
    ///     ttl: Duration::from_secs(60),
    ///     ..SessionOptions::default()
    /// });
    /// let activity = ActivityService::with_sessions(db, sessions.clone());
    /// assert!(activity.sessions().is_empty());
    /// # let _ = activity;
    /// # }
    /// ```
    pub fn with_sessions(db: &Database, sessions: StreamSessions) -> ActivityService {
        ActivityService {
            writer: db.writer().clone(),
            plays: Plays::new(db),
            playlists: Playlists::new(db),
            queues: PlayQueues::new(db),
            sessions,
        }
    }

    /// Returns the session registry the stream service registers streams in.
    pub fn sessions(&self) -> &StreamSessions {
        &self.sessions
    }

    /// Returns the `plays` repository.
    pub fn plays(&self) -> &Plays {
        &self.plays
    }

    /// Returns the playlist repository.
    pub fn playlists(&self) -> &Playlists {
        &self.playlists
    }

    /// Returns the saved play queue repository.
    pub fn play_queues(&self) -> &PlayQueues {
        &self.queues
    }

    /// Handles a client's scrobble.
    ///
    /// A submission is recorded as a play unless the latest stream session for
    /// the same user and track reported that it delivered no song audio. A
    /// submission with no known session is recorded: the client heard the song
    /// through a stream the server did not serve, or the session has since
    /// expired, and neither is evidence against the play. A now-playing call
    /// (`submission=false`) is accepted and discarded.
    ///
    /// # Parameters
    ///
    /// - `scrobble`: the request as the endpoint resolved it.
    ///
    /// # Returns
    ///
    /// [`ScrobbleOutcome::Recorded`] with the new play id,
    /// [`ScrobbleOutcome::NoSongAudio`] when the guard rejected the submission,
    /// or [`ScrobbleOutcome::Discarded`] for a now-playing report.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the play cannot be inserted,
    /// for example when the user or the track does not exist.
    pub async fn scrobble(
        &self,
        scrobble: &Scrobble<'_>,
    ) -> Result<ScrobbleOutcome, ActivityError> {
        if !scrobble.submission {
            return Ok(ScrobbleOutcome::Discarded);
        }
        if self
            .sessions
            .audio_delivered(scrobble.user_id, scrobble.track_id)
            == Some(false)
        {
            return Ok(ScrobbleOutcome::NoSongAudio);
        }
        // A play that reaches the insert either had a session that delivered
        // song audio or had no session known to the server, which the guard
        // treats as a play the client heard. Either way the row records a
        // delivered play.
        let id = self
            .plays
            .record_play(
                scrobble.user_id,
                scrobble.track_id,
                scrobble.client,
                scrobble.started_at,
                scrobble.listened_ms,
                true,
            )
            .await?;
        Ok(ScrobbleOutcome::Recorded(id))
    }

    /// Sets or clears one user's rating of an entity.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose rating changes.
    /// - `kind`: the kind of entity that is rated.
    /// - `entity_id`: the entity's id.
    /// - `rating`: the rating, 1 to 5, or 0 to clear it.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::InvalidRating`] when `rating` is above
    /// [`crate::MAX_RATING`], and [`ActivityError::Database`] when the row
    /// cannot be written.
    pub async fn set_rating(
        &self,
        user_id: Uuid,
        kind: EntityKind,
        entity_id: Uuid,
        rating: u8,
    ) -> Result<(), ActivityError> {
        let mut conn = self.writer.acquire().await?;
        ratings::set_rating(&mut conn, user_id, kind, entity_id, rating, now_millis()).await
    }

    /// Records likes for the named entities.
    ///
    /// The whole request is one transaction with the pin refresh it implies, so
    /// a like and the personal-library pin either both land or neither does.
    /// Nothing is acquired: a liked track that is not stored stays `remote`.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that likes the entities.
    /// - `entities`: the entities to like, each with its kind.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a like, a pin, or the commit fails.
    pub async fn star(
        &self,
        user_id: Uuid,
        entities: &[(EntityKind, Uuid)],
    ) -> Result<(), ActivityError> {
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for (kind, entity_id) in entities {
            feedback::like(&mut tx, user_id, *kind, *entity_id, at).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Removes likes for the named entities.
    ///
    /// The whole request is one transaction with the pin refresh it implies.
    /// Unliking something the user had not liked is not an error.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that no longer likes the entities.
    /// - `entities`: the entities to unlike, each with its kind.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a like, a pin, or the commit fails.
    pub async fn unstar(
        &self,
        user_id: Uuid,
        entities: &[(EntityKind, Uuid)],
    ) -> Result<(), ActivityError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for (kind, entity_id) in entities {
            feedback::unlike(&mut tx, user_id, *kind, *entity_id).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
