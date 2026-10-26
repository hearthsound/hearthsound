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

use hearthsound_core::providers::FeedbackValue;
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
use crate::scrobble::ScrobbleOutbox;
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

/// What one user thinks of one entity.
///
/// The three values are the like, the dislike, and the absence of either, which
/// is what the vendor extension's `like`, `dislike`, and `none` name.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::Feedback;
///
/// assert_ne!(Feedback::Like, Feedback::Dislike);
/// assert_ne!(Feedback::Dislike, Feedback::None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    /// A like, the operation `star` performs.
    Like,
    /// A dislike, which replaces a like and pins nothing.
    Dislike,
    /// Neither: whichever feedback the user held is removed.
    None,
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
    /// The scrobble outbox plays, now-playing reports and feedback are queued
    /// in, when the server runs one.
    scrobble_outbox: Option<Arc<ScrobbleOutbox>>,
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

    /// Queues every play, now-playing report and feedback change in an outbox.
    ///
    /// Without one the service records plays and opinions locally only, which is
    /// what a test or a tool that does not deliver scrobbles wants.
    ///
    /// # Parameters
    ///
    /// - `outbox`: the outbox to enqueue in.
    ///
    /// # Returns
    ///
    /// The service, sharing the outbox with its playback reports.
    pub fn with_scrobble_outbox(mut self, outbox: Arc<ScrobbleOutbox>) -> ActivityService {
        self.reports = Arc::new(
            self.reports
                .as_ref()
                .clone()
                .with_scrobble_outbox(Arc::clone(&outbox)),
        );
        self.scrobble_outbox = Some(outbox);
        self
    }

    /// Returns the scrobble outbox, when the server configured one.
    pub fn scrobble_outbox(&self) -> Option<&Arc<ScrobbleOutbox>> {
        self.scrobble_outbox.as_ref()
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
            scrobble_outbox: None,
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
            self.queue_now_playing(scrobble).await;
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
        self.queue_listen(
            scrobble.user_id,
            id,
            scrobble.track_id,
            scrobble.client,
            scrobble.started_at,
        )
        .await;
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
        self.queue_feedback(user_id, entities, FeedbackValue::Love);
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
        self.queue_feedback(user_id, entities, FeedbackValue::Clear);
        Ok(())
    }

    /// Sets what one user thinks of one entity.
    ///
    /// The three values are the ones the vendor extension expresses: a like,
    /// which is the `star` operation and pins the entity, a dislike, which
    /// replaces a like and pins nothing, and neither, which removes whichever
    /// the user held. The whole request is one transaction with the
    /// personal-library pin refresh it implies. Nothing is acquired here; the
    /// caller that knows the user's permission and quota asks the acquisition
    /// layer for what a like saves.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account whose feedback changes.
    /// - `kind`: the kind of entity.
    /// - `entity_id`: the entity's id.
    /// - `feedback`: the like, dislike, or removal to record.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError`] when a row, a pin, or the commit fails.
    pub async fn set_feedback(
        &self,
        user_id: Uuid,
        kind: EntityKind,
        entity_id: Uuid,
        feedback: Feedback,
    ) -> Result<(), ActivityError> {
        let at = now_millis();
        let mut tx = self.writer.begin_with(READ_THEN_WRITE).await?;
        match feedback {
            Feedback::Like => feedback::like(&mut tx, user_id, kind, entity_id, at).await?,
            Feedback::Dislike => feedback::dislike(&mut tx, user_id, kind, entity_id, at).await?,
            Feedback::None => feedback::clear(&mut tx, user_id, kind, entity_id).await?,
        }
        tx.commit().await?;
        self.signal.notify();
        if kind == EntityKind::Track {
            let value = match feedback {
                Feedback::Like => FeedbackValue::Love,
                Feedback::Dislike => FeedbackValue::Hate,
                Feedback::None => FeedbackValue::Clear,
            };
            self.queue_feedback(user_id, &[(kind, entity_id)], value);
        }
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

    /// Queues a now-playing report, logging rather than failing the request.
    ///
    /// # Parameters
    ///
    /// - `scrobble`: the request that reported playback.
    async fn queue_now_playing(&self, scrobble: &Scrobble<'_>) {
        let Some(outbox) = &self.scrobble_outbox else {
            return;
        };
        if let Err(error) = outbox
            .now_playing(
                scrobble.user_id,
                scrobble.track_id,
                scrobble.client,
                scrobble.started_at,
            )
            .await
        {
            tracing::warn!(%error, "could not queue a now-playing report");
        }
    }

    /// Queues a recorded play as a listen, logging rather than failing the
    /// request.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener who played the track.
    /// - `play_id`: the new play's row id.
    /// - `track_id`: the track that was played.
    /// - `client`: the client that played it, when known.
    /// - `started_at`: when playback started, in unix milliseconds.
    async fn queue_listen(
        &self,
        user_id: Uuid,
        play_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        started_at: i64,
    ) {
        let Some(outbox) = &self.scrobble_outbox else {
            return;
        };
        if let Err(error) = outbox
            .listen(user_id, play_id, track_id, client, started_at)
            .await
        {
            tracing::warn!(%error, "could not queue a listen");
        }
    }

    /// Queues an opinion of every track among the named entities.
    ///
    /// An album, an artist or any other kind carries no listen, so it produces
    /// no feedback row.
    ///
    /// The enqueue runs off the request path in a task of its own: it may have
    /// to ask a plugin whether it accepts feedback, which can take as long as
    /// the instance's call deadline, and a like must answer as soon as its own
    /// transaction committed. A failure is logged, which is the same contract
    /// the enqueue has when it runs inline.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener whose opinion changed.
    /// - `entities`: the entities the request named, each with its kind.
    /// - `value`: the opinion to report.
    fn queue_feedback(&self, user_id: Uuid, entities: &[(EntityKind, Uuid)], value: FeedbackValue) {
        let Some(outbox) = self.scrobble_outbox.clone() else {
            return;
        };
        let tracks: Vec<Uuid> = entities
            .iter()
            .filter(|(kind, _)| *kind == EntityKind::Track)
            .map(|(_, entity_id)| *entity_id)
            .collect();
        if tracks.is_empty() {
            return;
        }
        tokio::spawn(async move {
            for track_id in tracks {
                if let Err(error) = outbox.feedback(user_id, track_id, value).await {
                    tracing::warn!(%error, "could not queue feedback");
                }
            }
        });
    }
}
