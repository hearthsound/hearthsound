//! The activity service: what a scrobble means and when it is a play.
//!
//! A client reports playback with `scrobble`. When the call carries
//! `submission=true` it claims the song was played, and the server records a
//! play for it - unless the latest stream session for the same user and track
//! reported that it delivered no song audio, which is what a client that gave
//! up before a pending acquisition produced a file would otherwise turn into a
//! false play. When the call carries `submission=false` it is a now-playing
//! report, which is accepted and discarded because now-playing is not stored.

use std::sync::Arc;

use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::maintenance::MaintenanceSignal;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::bookmarks::Bookmarks;
use crate::error::ActivityError;
use crate::now_playing::{DEFAULT_CLIENT, NowPlaying};
use crate::play::Plays;
use crate::play_queues::PlayQueues;
use crate::playlists::Playlists;
use crate::ratings;
use crate::reports::{InMemoryReports, PlaybackState};
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
/// assert_ne!(ScrobbleOutcome::Recorded(id), ScrobbleOutcome::NowPlaying);
/// assert_ne!(
///     ScrobbleOutcome::AlreadyRecorded,
///     ScrobbleOutcome::NoSongAudio
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleOutcome {
    /// A play was recorded; the value is the new `plays` row id.
    Recorded(Uuid),
    /// The call was a now-playing report: what the client is playing is stored.
    NowPlaying,
    /// The call was a submission, but the latest stream session for that user
    /// and track delivered no song audio, so no play was recorded.
    NoSongAudio,
    /// The call was a submission, but a playback report recorded the same play
    /// recently, so this one was not recorded again.
    AlreadyRecorded,
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
    /// The bookmark table.
    bookmarks: Bookmarks,
    /// The now-playing table.
    now_playing: NowPlaying,
    /// The playback reports and the plays they recorded.
    reports: Arc<InMemoryReports>,
    /// The stream sessions the guard reads.
    sessions: StreamSessions,
    /// Woken after every like, unlike, playlist edit and recorded play, so the
    /// storage maintenance worker acts on the pins they change.
    signal: MaintenanceSignal,
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
        ActivityService::build(db, StreamSessions::new())
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
        ActivityService::build(db, sessions)
    }

    /// Builds the service with one maintenance signal shared by every
    /// repository that changes a pin or records a play.
    fn build(db: &Database, sessions: StreamSessions) -> ActivityService {
        let signal = MaintenanceSignal::new();
        ActivityService {
            writer: db.writer().clone(),
            plays: Plays::new(db).with_signal(signal.clone()),
            playlists: Playlists::new(db).with_signal(signal.clone()),
            queues: PlayQueues::new(db),
            bookmarks: Bookmarks::new(db),
            now_playing: NowPlaying::new(db),
            reports: Arc::new(InMemoryReports::new(db).with_signal(signal.clone())),
            sessions,
            signal,
        }
    }

    /// Returns the signal this service notifies after it changed a pin or
    /// recorded a play.
    ///
    /// The server hands it to the storage maintenance worker, which waits on
    /// it.
    pub fn maintenance_signal(&self) -> &MaintenanceSignal {
        &self.signal
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

    /// Returns the bookmark repository.
    pub fn bookmarks(&self) -> &Bookmarks {
        &self.bookmarks
    }

    /// Returns the now-playing repository.
    pub fn now_playing(&self) -> &NowPlaying {
        &self.now_playing
    }

    /// Returns the playback reports.
    ///
    /// `reportPlayback` feeds them, and `scrobble` asks them whether the play it
    /// is submitting was already recorded from a report.
    pub fn reports(&self) -> &Arc<InMemoryReports> {
        &self.reports
    }

    /// Handles a client's scrobble.
    ///
    /// A submission is recorded as a play unless the latest stream session for
    /// the same user and track reported that it delivered no song audio, or a
    /// playback report recorded the same play recently. A submission with no
    /// known session is recorded: the client heard the song through a stream the
    /// server did not serve, or the session has since expired, and neither is
    /// evidence against the play.
    ///
    /// A now-playing call (`submission=false`) stores what the client is playing
    /// so `getNowPlaying` can list it; the association ends when the client
    /// stops reporting, which is what the staleness window in
    /// [`crate::NowPlaying`] is for.
    ///
    /// # Parameters
    ///
    /// - `scrobble`: the request as the endpoint resolved it.
    ///
    /// # Returns
    ///
    /// [`ScrobbleOutcome::Recorded`] with the new play id,
    /// [`ScrobbleOutcome::NoSongAudio`] when the guard rejected the submission,
    /// [`ScrobbleOutcome::AlreadyRecorded`] when a playback report recorded the
    /// play already, or [`ScrobbleOutcome::NowPlaying`] for a now-playing
    /// report.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be written, for
    /// example when the user or the track does not exist.
    pub async fn scrobble(
        &self,
        scrobble: &Scrobble<'_>,
    ) -> Result<ScrobbleOutcome, ActivityError> {
        if !scrobble.submission {
            let track_id = EntityId::from_uuid(IdPrefix::Track, scrobble.track_id);
            self.now_playing
                .upsert(
                    scrobble.user_id,
                    scrobble.client.unwrap_or(DEFAULT_CLIENT),
                    track_id,
                    scrobble.started_at,
                    0,
                    PlaybackState::Playing.as_str(),
                )
                .await?;
            return Ok(ScrobbleOutcome::NowPlaying);
        }
        if self
            .sessions
            .audio_delivered(scrobble.user_id, scrobble.track_id)
            == Some(false)
        {
            return Ok(ScrobbleOutcome::NoSongAudio);
        }
        if self
            .reports
            .recently_recorded(scrobble.user_id, scrobble.track_id, now_millis())
        {
            return Ok(ScrobbleOutcome::AlreadyRecorded);
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

    /// Sets or clears a rating and lets a rating of one stand for a dislike.
    ///
    /// This is the rating `subsonic.rating_one_is_dislike` asks for: rating an
    /// entity 1 also records a dislike of it, unless the user likes it, and any
    /// other rating, or clearing the rating, removes a dislike. Artists, albums
    /// and tracks take a dislike; for any other kind only the rating changes.
    /// The rating and the dislike are written in one transaction.
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
    /// [`crate::MAX_RATING`], and [`ActivityError::Database`] when a row
    /// cannot be written.
    pub async fn set_rating_counting_dislike(
        &self,
        user_id: Uuid,
        kind: EntityKind,
        entity_id: Uuid,
        rating: u8,
    ) -> Result<(), ActivityError> {
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        ratings::set_rating(&mut tx, user_id, kind, entity_id, rating, at).await?;
        if matches!(
            kind,
            EntityKind::Track | EntityKind::Album | EntityKind::Artist
        ) {
            if rating == 1 {
                feedback::dislike_unless_liked(&mut tx, user_id, kind, entity_id, at).await?;
            } else {
                feedback::remove_dislike(&mut tx, user_id, kind, entity_id).await?;
            }
        }
        tx.commit().await?;
        Ok(())
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
        self.signal.notify();
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
        self.signal.notify();
        Ok(())
    }

    /// Saves entities into a user's personal library.
    ///
    /// Each entity becomes a `user_library` row whose origin is `saved`, and the
    /// whole request is one transaction with the pin refresh it implies: saving
    /// an album pins its member tracks. Nothing is acquired here; the
    /// `library.acquire_on_save` decision belongs to the caller that knows the
    /// user's permission and quota.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that saves the entities.
    /// - `entities`: the entities to save, each with its kind.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a row, a pin, or the commit fails.
    pub async fn save(
        &self,
        user_id: Uuid,
        entities: &[(EntityKind, Uuid)],
    ) -> Result<(), ActivityError> {
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for (kind, entity_id) in entities {
            feedback::save(&mut tx, user_id, *kind, *entity_id, at).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        Ok(())
    }

    /// Removes entities from a user's personal library.
    ///
    /// Only the rows a save wrote are removed, so an entity the user also
    /// uploaded keeps the row that upload wrote, and the pin refresh of the same
    /// transaction leaves every track pinned that a like, a playlist or an
    /// upload still holds.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that no longer saves the entities.
    /// - `entities`: the entities to remove, each with its kind.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a row, a pin, or the commit fails.
    pub async fn unsave(
        &self,
        user_id: Uuid,
        entities: &[(EntityKind, Uuid)],
    ) -> Result<(), ActivityError> {
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        for (kind, entity_id) in entities {
            feedback::unsave(&mut tx, user_id, *kind, *entity_id).await?;
        }
        tx.commit().await?;
        self.signal.notify();
        Ok(())
    }
}
