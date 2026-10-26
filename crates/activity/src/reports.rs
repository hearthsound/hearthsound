//! Playback reports: what a client is doing, and the plays they earn.
//!
//! The OpenSubsonic `reportPlayback` endpoint is how a client tells the server
//! about playback without waiting for a scrobble. A report names the state
//! (`starting`, `playing`, `paused`, `stopped`), the position, the rate, and
//! whether the server should ignore the play it implies.
//!
//! The session of one user and client is kept in memory: the track, how long it
//! lasts, the state the client last reported, and how much listening time has
//! accumulated. A `starting` report opens a session and shows it as now playing,
//! `playing` and `paused` accumulate the time that passed while the client said
//! it was playing, and `stopped` closes the session: the play is recorded when
//! the client did not ask to ignore it and the session earned one, and the
//! now-playing row is cleared.
//!
//! A play is earned when the client listened to at least half the track, or four
//! minutes of it, whichever comes first, and the track is at least thirty
//! seconds long. A session that goes quiet is dropped on the next report, so a
//! client that disappears cannot keep a row in memory or a stale now-playing row
//! alive.
//!
//! Because the same play often arrives twice - once as a report and once as a
//! scrobble - a play recorded from a report is remembered for a while, and
//! [`InMemoryReports::take_recorded`] consumes that memory: the one scrobble of
//! the same play is dropped, and a later submission of the same track is
//! recorded like any other play.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use hearthsound_core::EntityId;
use hearthsound_library::maintenance::MaintenanceSignal;
use uuid::Uuid;

use crate::error::ActivityError;
use crate::now_playing::NowPlaying;
use crate::play::Plays;

/// The half of a track a client must listen to for a play, in milliseconds.
pub const LISTENED_HALF_MS: i64 = 240_000;

/// The shortest track a play is recorded for, in milliseconds.
pub const MIN_TRACK_MS: i64 = 30_000;

/// How long after a report the one scrobble of the same play is suppressed, in
/// milliseconds.
///
/// The window only bounds how late the matching scrobble may arrive; the entry
/// is consumed by the first scrobble it suppresses, so a second submission of
/// the same track is a play of its own.
pub const SCROBBLE_DEDUPE_MS: i64 = 30 * 60 * 1000;

/// How long after a track's end a session is dropped, in milliseconds.
pub const SESSION_GRACE_MS: i64 = 30 * 60 * 1000;

/// The state a client reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    /// The client started playing the track.
    Starting,
    /// The client is playing.
    Playing,
    /// The client paused.
    Paused,
    /// The client stopped.
    Stopped,
}

impl PlaybackState {
    /// Returns the literal stored in `now_playing.state`.
    ///
    /// # Returns
    ///
    /// `starting`, `playing`, `paused`, or `stopped`.
    pub const fn as_str(self) -> &'static str {
        match self {
            PlaybackState::Starting => "starting",
            PlaybackState::Playing => "playing",
            PlaybackState::Paused => "paused",
            PlaybackState::Stopped => "stopped",
        }
    }

    /// Parses the literal a client sent.
    ///
    /// # Parameters
    ///
    /// - `value`: the literal the request carried.
    ///
    /// # Returns
    ///
    /// The state, or `None` when the literal is not one of the four.
    pub fn parse(value: &str) -> Option<PlaybackState> {
        match value {
            "starting" => Some(PlaybackState::Starting),
            "playing" => Some(PlaybackState::Playing),
            "paused" => Some(PlaybackState::Paused),
            "stopped" => Some(PlaybackState::Stopped),
            _ => None,
        }
    }
}

/// One client's playback report.
///
/// # Examples
///
/// ```
/// use hearthsound_activity::{PlaybackReport, PlaybackState};
///
/// let report = PlaybackReport {
///     state: PlaybackState::Stopped,
///     ..PlaybackReport::default()
/// };
/// assert_eq!(report.state.as_str(), "stopped");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct PlaybackReport<'a> {
    /// The account that is playing.
    pub user_id: Uuid,
    /// The client that reported.
    pub client: &'a str,
    /// The track being played.
    pub track_id: EntityId,
    /// How long the track lasts, in milliseconds; 0 when the server does not
    /// know.
    pub duration_ms: i64,
    /// The state the client reported.
    pub state: PlaybackState,
    /// How far into the track the client is.
    pub position_ms: i64,
    /// The playback rate, where 1.0 is normal speed.
    pub rate: f64,
    /// Whether the server should record no play for this session.
    pub ignore_scrobble: bool,
    /// The instant the report arrived, in unix milliseconds.
    pub now: i64,
}

impl Default for PlaybackReport<'_> {
    /// Returns a report for a track the server does not know, in the `stopped`
    /// state at rate 1, so a test or a caller can name only the fields it cares
    /// about.
    fn default() -> PlaybackReport<'static> {
        PlaybackReport {
            user_id: Uuid::nil(),
            client: "",
            track_id: EntityId::new(hearthsound_core::IdPrefix::Track),
            duration_ms: 0,
            state: PlaybackState::Stopped,
            position_ms: 0,
            rate: 1.0,
            ignore_scrobble: false,
            now: 0,
        }
    }
}

/// What a report did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackOutcome {
    /// The session and the now-playing row were updated; no play was recorded.
    Updated,
    /// The session closed and the play it earned was recorded; the value is the
    /// new `plays` row id.
    Recorded(Uuid),
}

/// One client's open playback session.
#[derive(Debug, Clone, PartialEq)]
struct Session {
    /// The track the session is for.
    track_id: EntityId,
    /// How long the track lasts, in milliseconds.
    duration_ms: i64,
    /// The state the client last reported.
    state: PlaybackState,
    /// The instant the session's first report arrived, in unix milliseconds.
    started_at: i64,
    /// The instant the last report arrived, in unix milliseconds.
    last_at: i64,
    /// How much listening time has accumulated, in milliseconds.
    listened_ms: i64,
    /// Whether the client asked for no play to be recorded.
    ignore_scrobble: bool,
}

impl Session {
    /// Adds the time that passed since the last report while the client said it
    /// was playing.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant of the new report, in unix milliseconds.
    /// - `rate`: the playback rate the client reported.
    fn accumulate(&mut self, now: i64, rate: f64) {
        if self.state == PlaybackState::Playing {
            let elapsed = (now - self.last_at).max(0) as f64;
            self.listened_ms += (elapsed * rate.max(0.0)) as i64;
        }
        self.last_at = now;
    }

    /// Reports whether the session has listened to enough to be a play.
    ///
    /// The threshold is half the track, or four minutes of it, whichever comes
    /// first, on a track of at least [`MIN_TRACK_MS`]. It answers about the
    /// listening alone: whether the client asked to be ignored is
    /// [`Session::earned`]'s business, so a session can cross the threshold and
    /// still record nothing.
    ///
    /// # Returns
    ///
    /// `true` when the listened time reached the threshold.
    fn reached_threshold(&self) -> bool {
        self.duration_ms >= MIN_TRACK_MS
            && self.listened_ms >= (self.duration_ms / 2).min(LISTENED_HALF_MS)
    }

    /// Reports whether the session earned a play.
    ///
    /// # Returns
    ///
    /// `true` when the client did not ask to be ignored and the session reached
    /// the play threshold.
    fn earned(&self) -> bool {
        !self.ignore_scrobble && self.reached_threshold()
    }
}

/// What one report decided, before any row is written.
#[derive(Debug, Default)]
struct Plan {
    /// The play to record, when the report closed a session that earned one.
    record: Option<(EntityId, i64, i64)>,
    /// The now-playing row to write, when the report keeps playing.
    upsert: Option<(EntityId, i64, i64, &'static str)>,
    /// Whether the report clears the now-playing row.
    remove: bool,
}

/// The state every clone of the reports shares.
#[derive(Debug)]
struct Inner {
    /// The open session of each user and client.
    sessions: HashMap<(Uuid, String), Session>,
    /// When a report recorded a play, by user and track.
    recorded: HashMap<(Uuid, Uuid), i64>,
}

/// The playback reports of every client, and the plays they recorded.
///
/// The sessions and the recently recorded plays live in memory: a restart loses
/// them, which at worst means a play that is reported twice is recorded twice,
/// or one that was never reported to a scrobble is recorded once.
///
/// # Examples
///
/// ```no_run
/// # async fn example(db: &hearthsound_db::Database) {
/// use hearthsound_activity::InMemoryReports;
///
/// let reports = InMemoryReports::new(db);
/// # let _ = reports;
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct InMemoryReports {
    /// The `plays` table a recorded play goes through.
    plays: Plays,
    /// The `now_playing` table a report keeps current.
    now_playing: NowPlaying,
    /// The sessions and the recently recorded plays.
    inner: Arc<Mutex<Inner>>,
    /// The scrobble outbox a recorded play is queued in, when the server runs
    /// one.
    scrobble_outbox: Option<Arc<crate::scrobble::ScrobbleOutbox>>,
}

impl InMemoryReports {
    /// Builds the reports over a database.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; the repositories share its pools.
    ///
    /// # Returns
    ///
    /// The reports, with no session open.
    pub fn new(db: &hearthsound_db::Database) -> InMemoryReports {
        InMemoryReports {
            plays: Plays::new(db),
            now_playing: NowPlaying::new(db),
            inner: Arc::new(Mutex::new(Inner {
                sessions: HashMap::new(),
                recorded: HashMap::new(),
            })),
            scrobble_outbox: None,
        }
    }

    /// Queues every play a report records in an outbox.
    ///
    /// # Parameters
    ///
    /// - `outbox`: the outbox to enqueue in.
    ///
    /// # Returns
    ///
    /// The reports, sharing the outbox with the activity service.
    pub fn with_scrobble_outbox(
        mut self,
        outbox: Arc<crate::scrobble::ScrobbleOutbox>,
    ) -> InMemoryReports {
        self.scrobble_outbox = Some(outbox);
        self
    }

    /// Wakes the given storage maintenance signal after every play a report
    /// records.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal the maintenance worker waits on.
    ///
    /// # Returns
    ///
    /// The reports, notifying that signal.
    pub fn with_signal(mut self, signal: MaintenanceSignal) -> InMemoryReports {
        self.plays = self.plays.with_signal(signal);
        self
    }

    /// Takes the remembered report of a play, so its scrobble is dropped once.
    ///
    /// A client often reports playback and then scrobbles the same play; the
    /// scrobble is dropped while this answers `true`, and the entry is consumed
    /// by that answer. A later submission of the same track - repeat-one, a
    /// second device, two explicit submissions - is therefore a play of its
    /// own, which is what the client stated.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that played the track.
    /// - `track_id`: the track that was played.
    /// - `now`: the instant to check, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// `true` when a report recorded a play for that account and track within
    /// [`SCROBBLE_DEDUPE_MS`], and that entry was removed.
    pub fn take_recorded(&self, user_id: Uuid, track_id: Uuid, now: i64) -> bool {
        self.lock()
            .recorded
            .remove(&(user_id, track_id))
            .is_some_and(|at| now - at <= SCROBBLE_DEDUPE_MS)
    }

    /// Applies one report.
    ///
    /// # Parameters
    ///
    /// - `report`: the report as the endpoint resolved it.
    ///
    /// # Returns
    ///
    /// [`PlaybackOutcome::Recorded`] when the report closed a session that
    /// earned a play, and [`PlaybackOutcome::Updated`] otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be written, for
    /// example when the track does not exist.
    pub async fn report(
        &self,
        report: &PlaybackReport<'_>,
    ) -> Result<PlaybackOutcome, ActivityError> {
        let plan = self.plan(report);
        if plan.remove {
            self.now_playing
                .remove(report.user_id, report.client)
                .await?;
        }
        if let Some((track_id, started_at, position_ms, state)) = plan.upsert {
            self.now_playing
                .upsert(
                    report.user_id,
                    report.client,
                    track_id,
                    started_at,
                    position_ms,
                    state,
                )
                .await?;
        }
        let Some((track_id, started_at, listened_ms)) = plan.record else {
            return Ok(PlaybackOutcome::Updated);
        };
        let id = self
            .plays
            .record_play(
                report.user_id,
                track_id.uuid(),
                Some(report.client),
                started_at,
                listened_ms,
                true,
            )
            .await?;
        if let Some(outbox) = &self.scrobble_outbox
            && let Err(error) = outbox
                .listen(
                    report.user_id,
                    id,
                    track_id.uuid(),
                    Some(report.client),
                    started_at,
                )
                .await
        {
            tracing::warn!(%error, "could not queue a listen from a playback report");
        }
        let mut inner = self.lock();
        inner
            .recorded
            .insert((report.user_id, track_id.uuid()), report.now);
        // A play older than the window can never suppress a scrobble again.
        inner
            .recorded
            .retain(|_, at| report.now - *at <= SCROBBLE_DEDUPE_MS);
        Ok(PlaybackOutcome::Recorded(id))
    }

    /// Applies one report to the sessions and answers what to write.
    ///
    /// # Parameters
    ///
    /// - `report`: the report to apply.
    ///
    /// # Returns
    ///
    /// The rows the report decided on.
    fn plan(&self, report: &PlaybackReport<'_>) -> Plan {
        let mut inner = self.lock();
        let key = (report.user_id, report.client.to_owned());
        let previous = inner.sessions.remove(&key);
        let mut plan = Plan::default();

        // A session that has gone quiet is dropped rather than closed: the
        // client never said it stopped, so no play is recorded for it.
        let previous = previous.filter(|session| {
            session.last_at + session.duration_ms.max(0) + SESSION_GRACE_MS > report.now
        });

        match report.state {
            PlaybackState::Starting => {
                // A start is the only report that opens a play. It continues
                // the open session when it names the same track and that
                // session has not yet reached the play threshold - a seek, a
                // resume, or a restart mid-track - so the listening time is
                // kept and no second play is recorded. Anything else closes the
                // session: a different track, or the same track started again
                // after the threshold, which is repeat-one and counts as another
                // play. A client that loops without sending a start keeps one
                // play per continuous session.
                let (started_at, listened_ms, ignore_scrobble, duration_ms) = match previous {
                    Some(mut session) => {
                        session.accumulate(report.now, report.rate);
                        session.ignore_scrobble |= report.ignore_scrobble;
                        // A repeat often arrives without the duration, so the
                        // session's own is carried into the new one.
                        let duration_ms = if report.duration_ms > 0 {
                            report.duration_ms
                        } else {
                            session.duration_ms
                        };
                        if session.track_id == report.track_id && !session.reached_threshold() {
                            (
                                session.started_at,
                                session.listened_ms,
                                session.ignore_scrobble,
                                duration_ms,
                            )
                        } else {
                            // `reached_threshold` decides the split and
                            // `earned` decides the record, so a session the
                            // client asked to ignore restarts cleanly and
                            // records nothing.
                            if session.earned() {
                                plan.record = Some((
                                    session.track_id,
                                    session.started_at,
                                    session.listened_ms,
                                ));
                            }
                            (report.now, 0, report.ignore_scrobble, duration_ms)
                        }
                    }
                    None => (report.now, 0, report.ignore_scrobble, report.duration_ms),
                };
                let session = Session {
                    track_id: report.track_id,
                    duration_ms,
                    state: PlaybackState::Playing,
                    started_at,
                    last_at: report.now,
                    listened_ms,
                    ignore_scrobble,
                };
                plan.upsert = Some((
                    report.track_id,
                    started_at,
                    report.position_ms,
                    PlaybackState::Playing.as_str(),
                ));
                inner.sessions.insert(key, session);
            }
            PlaybackState::Playing | PlaybackState::Paused => {
                let fresh = Session {
                    track_id: report.track_id,
                    duration_ms: report.duration_ms,
                    state: report.state,
                    started_at: report.now,
                    last_at: report.now,
                    listened_ms: 0,
                    ignore_scrobble: report.ignore_scrobble,
                };
                let mut session = previous.unwrap_or(fresh);
                session.accumulate(report.now, report.rate);
                session.state = report.state;
                session.ignore_scrobble |= report.ignore_scrobble;
                if report.duration_ms > 0 {
                    session.duration_ms = report.duration_ms;
                }
                plan.upsert = Some((
                    session.track_id,
                    session.started_at,
                    report.position_ms,
                    report.state.as_str(),
                ));
                inner.sessions.insert(key, session);
            }
            PlaybackState::Stopped => {
                plan.remove = true;
                if let Some(mut session) = previous {
                    session.accumulate(report.now, report.rate);
                    session.ignore_scrobble |= report.ignore_scrobble;
                    if report.duration_ms > 0 {
                        session.duration_ms = report.duration_ms;
                    }
                    if session.earned() {
                        plan.record =
                            Some((session.track_id, session.started_at, session.listened_ms));
                    }
                }
            }
        }
        plan
    }

    /// Locks the shared state, ignoring a poisoned lock.
    ///
    /// # Returns
    ///
    /// The guard.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a session that listened to `listened_ms` of a `duration_ms`
    /// track.
    ///
    /// # Parameters
    ///
    /// - `duration_ms`: how long the track lasts.
    /// - `listened_ms`: how much was listened to.
    fn session(duration_ms: i64, listened_ms: i64) -> Session {
        Session {
            track_id: EntityId::new(hearthsound_core::IdPrefix::Track),
            duration_ms,
            state: PlaybackState::Playing,
            started_at: 0,
            last_at: 0,
            listened_ms,
            ignore_scrobble: false,
        }
    }

    #[test]
    fn the_threshold_is_half_the_track_or_four_minutes() {
        assert!(!session(240_000, 119_999).reached_threshold());
        assert!(session(240_000, 120_000).reached_threshold());
        assert!(
            session(3_600_000, 240_000).reached_threshold(),
            "four minutes of an hour is enough"
        );
        assert!(
            !session(20_000, 20_000).reached_threshold(),
            "a track too short to count never reaches it"
        );
    }

    #[test]
    fn an_ignored_session_reaches_the_threshold_without_earning_a_play() {
        let mut ignored = session(240_000, 240_000);
        assert!(ignored.reached_threshold());
        assert!(ignored.earned());
        ignored.ignore_scrobble = true;
        assert!(
            ignored.reached_threshold(),
            "the split still happens when the play is not recorded"
        );
        assert!(!ignored.earned(), "an ignored session records nothing");
    }
}
