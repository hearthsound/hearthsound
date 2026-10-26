//! Playback reports: what a client is doing, and the plays they earn.
//!
//! The OpenSubsonic `reportPlayback` endpoint is how a client tells the server
//! about playback without waiting for a scrobble. A report names the state
//! (`starting`, `playing`, `paused`, `stopped`), the position, the rate, and
//! whether the server should ignore the play it implies.
//!
//! The session of one user and client is a [`ListenSession`]: the track, how long
//! it lasts, the state the client last reported, and how much listening time has
//! accumulated. A `starting` report opens a session and shows it as now playing,
//! `playing` and `paused` accumulate the time that passed while the client said
//! it was playing, and `stopped` closes the session: the play is recorded when
//! the client did not ask to ignore it and the session earned one, and the
//! now-playing row is cleared.
//!
//! The sessions live in memory and in the `listen_sessions` table. Memory is what
//! a report is decided against, and the table is a throttled shadow written when
//! a session opens, when its state changes, when it crosses the play threshold,
//! and at most every [`LISTEN_SESSION_PERSIST_MS`] while it keeps playing.
//! [`InMemoryReports::recover`] reads the table once at startup, so a restart
//! mid-track does not lose the listen: a session whose client is still inside its
//! track goes back into memory and continues, and one whose client went quiet is
//! closed, recording the play it earned. The scrobble-dedupe map is not
//! persisted, so a play a report recorded just before a restart can be recorded
//! again by a client scrobble within [`SCROBBLE_DEDUPE_MS`] afterwards; that is
//! the at-least-once trade the outbox makes too.
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
use crate::listen_session::{LISTEN_SESSION_PERSIST_MS, ListenSession, ListenSessions};
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

/// What one report decided, before any row is written.
#[derive(Debug, Default)]
struct Plan {
    /// The play to record, when the report closed a session that earned one.
    record: Option<(EntityId, i64, i64)>,
    /// The now-playing row to write, when the report keeps playing.
    upsert: Option<(EntityId, i64, i64, &'static str)>,
    /// Whether the report clears the now-playing row.
    remove: bool,
    /// What the stored shadow of the session needs.
    persist: Option<Persist>,
}

/// What the report decided to do with a session's stored row.
#[derive(Debug)]
enum Persist {
    /// Write the session.
    Upsert(ListenSession),
    /// Remove the row: the session closed.
    Delete,
}

/// What a recovery of stored sessions did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovery {
    /// The sessions that went back into memory.
    pub restored: usize,
    /// The sessions whose earned play was recorded.
    pub recorded: usize,
    /// The sessions that were closed and removed.
    pub dropped: usize,
}

/// Reports whether a session has to be written now.
///
/// A session is written when it was never written, when it changed something a
/// restart needs to know (its state, or crossing the play threshold), and
/// otherwise at most every [`LISTEN_SESSION_PERSIST_MS`].
///
/// # Parameters
///
/// - `session`: the session as memory has it.
/// - `changed`: whether this report changed the state or crossed the threshold.
/// - `now`: the instant of the report, in unix milliseconds.
///
/// # Returns
///
/// `true` when the stored row should be rewritten.
fn persist_now(session: &ListenSession, changed: bool, now: i64) -> bool {
    session.persisted_at == 0 || changed || now - session.persisted_at >= LISTEN_SESSION_PERSIST_MS
}

/// The state every clone of the reports shares.
#[derive(Debug)]
struct Inner {
    /// The open session of each user and client.
    sessions: HashMap<(Uuid, String), ListenSession>,
    /// When a report recorded a play, by user and track.
    recorded: HashMap<(Uuid, Uuid), i64>,
}

/// The playback reports of every client, and the plays they recorded.
///
/// The sessions live in memory and in the `listen_sessions` table, which
/// [`InMemoryReports::recover`] reads at startup, so a restart mid-track keeps
/// the listening time a client already reported and the play it earned. The
/// scrobble-dedupe map is memory only: a restart can let a client scrobble a play
/// a report recorded just before it.
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
    /// The `listen_sessions` table the sessions are shadowed in.
    sessions: ListenSessions,
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
            sessions: ListenSessions::new(db),
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
    /// The now-playing row is written first, then the play the report earned,
    /// then the stored shadow of the session. On a `stopped` report the play is
    /// recorded before the session row is removed, so a crash between the two
    /// can duplicate the play (which the client's own scrobble then dedupes) but
    /// never lose it.
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
        let recorded = match plan.record {
            Some((track_id, started_at, listened_ms)) => Some(
                self.record(
                    report.user_id,
                    report.client,
                    track_id,
                    started_at,
                    listened_ms,
                    report.now,
                )
                .await?,
            ),
            None => None,
        };
        self.persist(report.user_id, report.client, plan.persist)
            .await;
        Ok(match recorded {
            Some(id) => PlaybackOutcome::Recorded(id),
            None => PlaybackOutcome::Updated,
        })
    }

    /// Restores the sessions a previous run stored, and closes the ones that
    /// went quiet.
    ///
    /// Called once at startup, before any report is applied. A stored session
    /// whose client last reported within its own track plus
    /// [`SESSION_GRACE_MS`] goes back into memory, so the client's next report
    /// continues it exactly as if the restart had not happened. One whose client
    /// waited longer is closed: the play it earned is recorded, and its row is
    /// removed.
    ///
    /// Calling this twice is harmless: the closed rows are gone and the
    /// restored ones are in memory, so the second call restores nothing.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant the server starts, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// How many sessions were restored, how many plays were recorded, and how
    /// many rows were removed.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when a row cannot be read, written or
    /// removed. The caller logs it and starts anyway: a session table problem
    /// must not keep the server down.
    pub async fn recover(&self, now: i64) -> Result<Recovery, ActivityError> {
        let stored = self.sessions.all().await?;
        let mut recovery = Recovery::default();
        for stored in stored {
            let session = stored.session;
            if session.last_at + session.duration_ms.max(0) + SESSION_GRACE_MS > now {
                self.lock()
                    .sessions
                    .insert((stored.user_id, stored.client.clone()), session);
                recovery.restored += 1;
                continue;
            }
            if session.earned() {
                self.record(
                    stored.user_id,
                    &stored.client,
                    session.track_id,
                    session.started_at,
                    session.listened_ms,
                    now,
                )
                .await?;
                recovery.recorded += 1;
            }
            self.sessions.delete(stored.user_id, &stored.client).await?;
            recovery.dropped += 1;
        }
        Ok(recovery)
    }

    /// Records one play and remembers it for the scrobble that may follow.
    ///
    /// Both the report path and the startup recovery record through this, so a
    /// play a recovery records seeds the dedupe map exactly as a reported one
    /// does.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that played the track.
    /// - `client`: the client that played it.
    /// - `track_id`: the track that was played.
    /// - `started_at`: when playback started, in unix milliseconds.
    /// - `listened_ms`: how long was listened, in milliseconds.
    /// - `now`: the instant the play is recorded at, in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The new `plays` row id.
    ///
    /// # Errors
    ///
    /// Returns [`ActivityError::Database`] when the row cannot be written, for
    /// example when the track does not exist.
    async fn record(
        &self,
        user_id: Uuid,
        client: &str,
        track_id: EntityId,
        started_at: i64,
        listened_ms: i64,
        now: i64,
    ) -> Result<Uuid, ActivityError> {
        let id = self
            .plays
            .record_play(
                user_id,
                track_id.uuid(),
                Some(client),
                started_at,
                listened_ms,
                true,
            )
            .await?;
        if let Some(outbox) = &self.scrobble_outbox
            && let Err(error) = outbox
                .listen(user_id, id, track_id.uuid(), Some(client), started_at)
                .await
        {
            tracing::warn!(%error, "could not queue a listen from a playback report");
        }
        let mut inner = self.lock();
        inner.recorded.insert((user_id, track_id.uuid()), now);
        // A play older than the window can never suppress a scrobble again.
        inner
            .recorded
            .retain(|_, at| now - *at <= SCROBBLE_DEDUPE_MS);
        Ok(id)
    }

    /// Applies a persist decision to the stored session, logging a failure.
    ///
    /// The row is a shadow of the session in memory, so a write that fails costs
    /// at most the listening time since the previous one; failing the report
    /// would cost the play the client did report.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account that is playing.
    /// - `client`: the client that reported.
    /// - `persist`: what to write, when there is anything.
    async fn persist(&self, user_id: Uuid, client: &str, persist: Option<Persist>) {
        let Some(persist) = persist else {
            return;
        };
        let result = match &persist {
            Persist::Upsert(session) => self.sessions.upsert(user_id, client, session).await,
            Persist::Delete => self.sessions.delete(user_id, client).await,
        };
        if let Err(error) = result {
            tracing::warn!(%error, "could not store a playback session");
        }
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

        // A session whose client went quiet for longer than its own track is
        // closed rather than continued, and its stored row goes with it. It
        // records its earned play only under the rules below, which is the same
        // decision the startup recovery makes.
        let previous = match previous {
            Some(session)
                if session.last_at + session.duration_ms.max(0) + SESSION_GRACE_MS > report.now =>
            {
                Some(session)
            }
            Some(_) => {
                plan.persist = Some(Persist::Delete);
                None
            }
            None => None,
        };

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
                let (started_at, listened_ms, ignore_scrobble, duration_ms, persisted_at) =
                    match previous {
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
                            let persisted_at = session.persisted_at;
                            if session.track_id == report.track_id && !session.reached_threshold() {
                                (
                                    session.started_at,
                                    session.listened_ms,
                                    session.ignore_scrobble,
                                    duration_ms,
                                    persisted_at,
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
                                (report.now, 0, report.ignore_scrobble, duration_ms, 0)
                            }
                        }
                        None => (report.now, 0, report.ignore_scrobble, report.duration_ms, 0),
                    };
                let mut session = ListenSession {
                    track_id: report.track_id,
                    duration_ms,
                    state: PlaybackState::Playing,
                    started_at,
                    last_at: report.now,
                    listened_ms,
                    ignore_scrobble,
                    persisted_at,
                };
                plan.upsert = Some((
                    report.track_id,
                    started_at,
                    report.position_ms,
                    PlaybackState::Playing.as_str(),
                ));
                // A new session is written at once. A continued one is written
                // when it changed what a restart needs to know.
                if persist_now(&session, false, report.now) {
                    session.persisted_at = report.now;
                    plan.persist = Some(Persist::Upsert(session.clone()));
                }
                inner.sessions.insert(key, session);
            }
            PlaybackState::Playing | PlaybackState::Paused => {
                let fresh = ListenSession {
                    track_id: report.track_id,
                    duration_ms: report.duration_ms,
                    state: report.state,
                    started_at: report.now,
                    last_at: report.now,
                    listened_ms: 0,
                    ignore_scrobble: report.ignore_scrobble,
                    persisted_at: 0,
                };
                let mut session = previous.unwrap_or(fresh);
                let state_changed = session.state != report.state;
                let was_threshold = session.reached_threshold();
                session.accumulate(report.now, report.rate);
                session.state = report.state;
                session.ignore_scrobble |= report.ignore_scrobble;
                if report.duration_ms > 0 {
                    session.duration_ms = report.duration_ms;
                }
                let newly_reached = !was_threshold && session.reached_threshold();
                plan.upsert = Some((
                    session.track_id,
                    session.started_at,
                    report.position_ms,
                    report.state.as_str(),
                ));
                let changed = state_changed || newly_reached;
                if persist_now(&session, changed, report.now) {
                    session.persisted_at = report.now;
                    plan.persist = Some(Persist::Upsert(session.clone()));
                }
                inner.sessions.insert(key, session);
            }
            PlaybackState::Stopped => {
                plan.remove = true;
                plan.persist = Some(Persist::Delete);
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
    fn session(duration_ms: i64, listened_ms: i64) -> ListenSession {
        ListenSession {
            track_id: EntityId::new(hearthsound_core::IdPrefix::Track),
            duration_ms,
            state: PlaybackState::Playing,
            started_at: 0,
            last_at: 0,
            listened_ms,
            ignore_scrobble: false,
            persisted_at: 0,
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
