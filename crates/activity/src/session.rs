//! The stream-session registry: who is listening, and did they hear the song?
//!
//! Every stream the server opens registers a session for a user and a track.
//! The session carries an [`AudioDelivered`] source, which answers later
//! whether song audio ever reached the client; the scrobble entry point reads
//! that answer to decide whether a submission is a play (see
//! [`crate::ActivityService::scrobble`]).
//!
//! The registry is deliberately small and bounded:
//!
//! - only the latest session per `(user, track)` is kept, so a restarted stream
//!   owns the answer for its own attempt and a stale session cannot mask it;
//! - a session is forgotten [`SessionOptions::ttl`] after it was opened;
//! - at most [`SessionOptions::capacity`] sessions are retained, oldest first.
//!
//! Both bounds are what keep the registry's memory independent of how many
//! streams were opened or how many clients never scrobbled.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// How long a stream session is remembered by default.
///
/// A day is far longer than any client needs to report a play for a stream it
/// started, so the bound only exists to reclaim memory from sessions whose
/// client never scrobbled.
pub const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// How many stream sessions are retained by default.
///
/// Ten thousand sessions of a few dozen bytes each keep the registry in the low
/// megabytes even while thousands of clients stream.
pub const DEFAULT_SESSION_CAPACITY: usize = 10_000;

/// How long stream sessions are kept and how many are retained.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_activity::SessionOptions;
///
/// let options = SessionOptions {
///     ttl: Duration::from_secs(60),
///     ..SessionOptions::default()
/// };
/// assert_eq!(options.ttl, Duration::from_secs(60));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOptions {
    /// How long a session stays valid after it was registered.
    pub ttl: Duration,
    /// How many sessions are retained; the oldest is evicted first.
    pub capacity: usize,
}

impl Default for SessionOptions {
    /// The defaults: [`DEFAULT_SESSION_TTL`] and [`DEFAULT_SESSION_CAPACITY`].
    fn default() -> SessionOptions {
        SessionOptions {
            ttl: DEFAULT_SESSION_TTL,
            capacity: DEFAULT_SESSION_CAPACITY,
        }
    }
}

/// The answer to "did this stream deliver song audio to the client".
///
/// The stream service owns the answer and gives the registry a way to read it
/// later, because the answer is only known once the stream is under way. Two
/// forms are accepted, one per kind of stream:
///
/// - a raw file stream shares an [`AtomicBool`] through [`AudioDelivered::flag`]
///   or [`AudioDelivered::from_flag`] and sets it as soon as bytes reach the
///   client;
/// - a pending stream answers through [`AudioDelivered::probe`] with a closure
///   over its `hearthsound_media::PendingControl`, whose `audio_delivered` is
///   `false` while only silence padding has been played.
///
/// # Examples
///
/// ```
/// use std::sync::atomic::Ordering;
///
/// use hearthsound_activity::AudioDelivered;
///
/// let (delivered, flag) = AudioDelivered::flag();
/// assert!(!delivered.delivered());
/// flag.store(true, Ordering::SeqCst);
/// assert!(delivered.delivered());
/// ```
#[derive(Clone)]
pub struct AudioDelivered {
    source: Source,
}

/// Where an [`AudioDelivered`] reads its answer from.
#[derive(Clone)]
enum Source {
    /// A flag the stream sets once song audio has been written.
    Flag(Arc<AtomicBool>),
    /// A closure the stream answers through.
    Probe(Arc<dyn Fn() -> bool + Send + Sync>),
}

impl AudioDelivered {
    /// Creates a false flag and the handle that sets it.
    ///
    /// The stream service calls this before it starts sending bytes, hands the
    /// returned [`AudioDelivered`] to [`StreamSessions::register`], and stores
    /// `true` in the returned flag as soon as song audio reaches the client.
    ///
    /// # Returns
    ///
    /// The source to register, and the flag the stream sets.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::atomic::Ordering;
    ///
    /// use hearthsound_activity::AudioDelivered;
    ///
    /// let (delivered, flag) = AudioDelivered::flag();
    /// flag.store(true, Ordering::SeqCst);
    /// assert!(delivered.delivered());
    /// ```
    pub fn flag() -> (AudioDelivered, Arc<AtomicBool>) {
        let flag = Arc::new(AtomicBool::new(false));
        (
            AudioDelivered {
                source: Source::Flag(Arc::clone(&flag)),
            },
            flag,
        )
    }

    /// Reads a flag the stream already shares.
    ///
    /// # Parameters
    ///
    /// - `flag`: the flag the stream sets once song audio has been written.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::Arc;
    /// use std::sync::atomic::AtomicBool;
    ///
    /// use hearthsound_activity::AudioDelivered;
    ///
    /// let flag = Arc::new(AtomicBool::new(false));
    /// let delivered = AudioDelivered::from_flag(flag);
    /// assert!(!delivered.delivered());
    /// ```
    pub fn from_flag(flag: Arc<AtomicBool>) -> AudioDelivered {
        AudioDelivered {
            source: Source::Flag(flag),
        }
    }

    /// Answers through a probe.
    ///
    /// # Parameters
    ///
    /// - `probe`: called every time the registry asks whether song audio was
    ///   delivered. It must be cheap and must not block on a lock the caller
    ///   holds, because it runs while the registry is locked.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_activity::AudioDelivered;
    ///
    /// let delivered = AudioDelivered::probe(|| true);
    /// assert!(delivered.delivered());
    /// ```
    pub fn probe(probe: impl Fn() -> bool + Send + Sync + 'static) -> AudioDelivered {
        AudioDelivered {
            source: Source::Probe(Arc::new(probe)),
        }
    }

    /// Reports whether song audio reached the client.
    ///
    /// # Returns
    ///
    /// `true` when the stream delivered song audio; `false` while it has only
    /// played silence padding, or before it started.
    pub fn delivered(&self) -> bool {
        match &self.source {
            Source::Flag(flag) => flag.load(Ordering::SeqCst),
            Source::Probe(probe) => probe(),
        }
    }
}

impl fmt::Debug for AudioDelivered {
    /// Writes whether the source currently reports delivered audio.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioDelivered")
            .field("delivered", &self.delivered())
            .finish()
    }
}

/// A stream session as the registry remembers it.
///
/// The registry keeps only the latest session per user and track, and a lookup
/// answers with what it observed: the client that opened the stream and whether
/// that stream delivered song audio.
///
/// # Examples
///
/// ```
/// use uuid::Uuid;
///
/// use hearthsound_activity::{AudioDelivered, StreamSessions};
///
/// let sessions = StreamSessions::new();
/// let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
/// sessions.register(user, track, Some("dsub"), AudioDelivered::probe(|| true));
///
/// let session = sessions.latest(user, track).expect("the session is known");
/// assert_eq!(session.client.as_deref(), Some("dsub"));
/// assert!(session.audio_delivered);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamSession {
    /// The client that opened the stream, as the request carried it.
    pub client: Option<String>,
    /// Whether song audio reached the client.
    pub audio_delivered: bool,
}

/// What a stream session is keyed by: one listener and one track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SessionKey {
    /// The account that streamed the track.
    user_id: Uuid,
    /// The track that was streamed.
    track_id: Uuid,
}

/// One registered stream.
#[derive(Debug)]
struct Session {
    /// The client that opened the stream.
    client: Option<String>,
    /// The source of the audio-delivered answer.
    audio: AudioDelivered,
    /// When the session is no longer consulted, as a monotonic instant.
    expires_at: Instant,
    /// This session's position in [`Registry::order`].
    seq: u64,
}

/// The registry state, behind one mutex.
#[derive(Debug, Default)]
struct Registry {
    /// The newest session per user and track.
    sessions: HashMap<SessionKey, Session>,
    /// Registration order, oldest first, so eviction never scans.
    order: BTreeMap<(u64, SessionKey), ()>,
    /// The sequence number the next registration gets.
    next_seq: u64,
}

impl Registry {
    /// Drops the sessions that have expired or that exceed the capacity.
    ///
    /// Registration order and expiry order are the same because every session
    /// of a registry has the same lifetime, so the oldest entries are the first
    /// to go.
    ///
    /// # Parameters
    ///
    /// - `now`: the current instant.
    /// - `options`: the registry's bounds.
    fn trim(&mut self, now: Instant, options: SessionOptions) {
        while let Some((&(seq, key), _)) = self.order.iter().next() {
            let expired = self
                .sessions
                .get(&key)
                .is_none_or(|session| session.expires_at <= now);
            if !expired && self.sessions.len() <= options.capacity {
                break;
            }
            self.order.remove(&(seq, key));
            self.sessions.remove(&key);
        }
    }
}

/// The latest stream session per user and track.
///
/// The stream service registers a session when it starts serving a track;
/// [`ActivityService::scrobble`](crate::ActivityService::scrobble) asks whether
/// the latest session for that user and track delivered song audio. Only the
/// latest session per `(user, track)` is kept, so a user who restarts a track
/// replaces the answer of the previous attempt, and the registry is bounded by
/// [`SessionOptions::ttl`] and [`SessionOptions::capacity`].
///
/// Every method is synchronous and takes the registry's lock only for the
/// lookup or the insert, never across an `.await`, so the stream path never
/// blocks on a database or a plugin.
///
/// Cloning shares the same registry.
///
/// # Examples
///
/// ```
/// use uuid::Uuid;
///
/// use hearthsound_activity::{AudioDelivered, StreamSessions};
///
/// let sessions = StreamSessions::new();
/// let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
/// assert_eq!(sessions.audio_delivered(user, track), None);
///
/// sessions.register(user, track, None, AudioDelivered::probe(|| false));
/// assert_eq!(sessions.audio_delivered(user, track), Some(false));
/// ```
#[derive(Debug, Clone)]
pub struct StreamSessions {
    /// The shared registry state.
    inner: Arc<Mutex<Registry>>,
    /// The lifetime and capacity every registration is trimmed to.
    options: SessionOptions,
}

impl StreamSessions {
    /// Builds a registry with the default bounds.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_activity::StreamSessions;
    ///
    /// assert!(StreamSessions::new().is_empty());
    /// ```
    pub fn new() -> StreamSessions {
        StreamSessions::with_options(SessionOptions::default())
    }

    /// Builds a registry with explicit bounds.
    ///
    /// # Parameters
    ///
    /// - `options`: the lifetime and capacity to trim registrations to.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use hearthsound_activity::{SessionOptions, StreamSessions};
    ///
    /// let sessions = StreamSessions::with_options(SessionOptions {
    ///     ttl: Duration::from_secs(30),
    ///     capacity: 4,
    /// });
    /// assert!(sessions.is_empty());
    /// ```
    pub fn with_options(options: SessionOptions) -> StreamSessions {
        StreamSessions {
            inner: Arc::new(Mutex::new(Registry::default())),
            options,
        }
    }

    /// Registers a stream that just started.
    ///
    /// Replaces any session already registered for the same user and track, so
    /// the newest stream owns the audio-delivered answer. Sessions that have
    /// expired are dropped, and when more than [`SessionOptions::capacity`]
    /// sessions are known the oldest is dropped.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the stream is served to.
    /// - `track_id`: the track being streamed.
    /// - `client`: the client that asked for the stream, when the request
    ///   carried one.
    /// - `audio`: how the registry reads the stream's audio-delivered answer.
    ///
    /// # Panics
    ///
    /// Never panics: a registry lock poisoned by a panicking thread is taken
    /// over rather than propagated, because the registry holds no invariant a
    /// failed thread could have broken.
    ///
    /// # Examples
    ///
    /// ```
    /// use uuid::Uuid;
    ///
    /// use hearthsound_activity::{AudioDelivered, StreamSessions};
    ///
    /// let sessions = StreamSessions::new();
    /// let (user, track) = (Uuid::now_v7(), Uuid::now_v7());
    /// sessions.register(user, track, Some("dsub"), AudioDelivered::probe(|| true));
    /// assert_eq!(sessions.len(), 1);
    /// ```
    pub fn register(
        &self,
        user_id: Uuid,
        track_id: Uuid,
        client: Option<&str>,
        audio: AudioDelivered,
    ) {
        let now = Instant::now();
        let key = SessionKey { user_id, track_id };
        let mut registry = self.lock();
        let seq = registry.next_seq;
        registry.next_seq += 1;
        let replaced = registry.sessions.get(&key).map(|previous| previous.seq);
        if let Some(previous_seq) = replaced {
            registry.order.remove(&(previous_seq, key));
        }
        // A lifetime that does not fit an instant is treated as already over;
        // no real options reach this, and it must not panic on the stream path.
        let expires_at = now.checked_add(self.options.ttl).unwrap_or(now);
        registry.order.insert((seq, key), ());
        registry.sessions.insert(
            key,
            Session {
                client: client.map(str::to_owned),
                audio,
                expires_at,
                seq,
            },
        );
        registry.trim(now, self.options);
    }

    /// Returns the latest session for a user and track.
    ///
    /// A session that has outlived [`SessionOptions::ttl`] is forgotten here and
    /// reported as unknown.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the stream was served to.
    /// - `track_id`: the track that was streamed.
    ///
    /// # Returns
    ///
    /// The latest unexpired session, or `None` when no session is known.
    ///
    /// # Panics
    ///
    /// Never panics; see [`StreamSessions::register`].
    pub fn latest(&self, user_id: Uuid, track_id: Uuid) -> Option<StreamSession> {
        let now = Instant::now();
        let key = SessionKey { user_id, track_id };
        let mut registry = self.lock();
        let (expires_at, seq) = {
            let session = registry.sessions.get(&key)?;
            (session.expires_at, session.seq)
        };
        if expires_at <= now {
            registry.order.remove(&(seq, key));
            registry.sessions.remove(&key);
            return None;
        }
        let session = registry.sessions.get(&key)?;
        Some(StreamSession {
            client: session.client.clone(),
            audio_delivered: session.audio.delivered(),
        })
    }

    /// Reports whether the latest session for a user and track delivered song
    /// audio.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the account the stream was served to.
    /// - `track_id`: the track that was streamed.
    ///
    /// # Returns
    ///
    /// `Some(true)` or `Some(false)` for a known session, and `None` when there
    /// is no unexpired session for that user and track.
    ///
    /// # Panics
    ///
    /// Never panics; see [`StreamSessions::register`].
    pub fn audio_delivered(&self, user_id: Uuid, track_id: Uuid) -> Option<bool> {
        self.latest(user_id, track_id)
            .map(|session| session.audio_delivered)
    }

    /// Returns how many sessions the registry holds.
    ///
    /// # Panics
    ///
    /// Never panics; see [`StreamSessions::register`].
    pub fn len(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Reports whether the registry holds no session.
    ///
    /// # Panics
    ///
    /// Never panics; see [`StreamSessions::register`].
    pub fn is_empty(&self) -> bool {
        self.lock().sessions.is_empty()
    }

    /// Locks the registry, recovering from a poisoned lock.
    ///
    /// The registry is plain data with no invariant a panicking thread could
    /// have broken, and the alternative would be to fail a listener's play over
    /// an unrelated panic, so a poisoned lock is taken over.
    fn lock(&self) -> MutexGuard<'_, Registry> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for StreamSessions {
    /// Builds a registry with the default bounds; see [`StreamSessions::new`].
    fn default() -> StreamSessions {
        StreamSessions::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the bounds and the lookup rules of the registry.

    use super::*;

    /// Registers a session whose audio-delivered answer is fixed.
    ///
    /// # Parameters
    ///
    /// - `sessions`: the registry to register with.
    /// - `user_id`: the account the session belongs to.
    /// - `track_id`: the track the session streamed.
    /// - `delivered`: the answer the session reports.
    fn register(sessions: &StreamSessions, user_id: Uuid, track_id: Uuid, delivered: bool) {
        sessions.register(
            user_id,
            track_id,
            None,
            AudioDelivered::probe(move || delivered),
        );
    }

    #[test]
    fn an_unknown_pair_has_no_answer() {
        let sessions = StreamSessions::new();
        assert_eq!(
            sessions.audio_delivered(Uuid::now_v7(), Uuid::now_v7()),
            None
        );
    }

    #[test]
    fn the_latest_session_replaces_the_earlier_one() {
        let sessions = StreamSessions::new();
        let (user, track) = (Uuid::now_v7(), Uuid::now_v7());

        register(&sessions, user, track, false);
        assert_eq!(sessions.audio_delivered(user, track), Some(false));
        assert_eq!(sessions.len(), 1, "a restarted stream replaces its session");

        register(&sessions, user, track, true);
        assert_eq!(sessions.audio_delivered(user, track), Some(true));

        register(&sessions, user, track, false);
        assert_eq!(
            sessions.audio_delivered(user, track),
            Some(false),
            "the newest stream owns the answer"
        );
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn a_session_can_be_looked_up_with_its_client() {
        let sessions = StreamSessions::new();
        let (user, track) = (Uuid::now_v7(), Uuid::now_v7());

        sessions.register(user, track, Some("dsub"), AudioDelivered::probe(|| true));
        let session = sessions.latest(user, track).expect("the session is known");
        assert_eq!(session.client.as_deref(), Some("dsub"));
        assert!(session.audio_delivered);
    }

    #[test]
    fn sessions_of_different_users_do_not_answer_for_each_other() {
        let sessions = StreamSessions::new();
        let (first_user, second_user) = (Uuid::now_v7(), Uuid::now_v7());
        let track = Uuid::now_v7();

        register(&sessions, first_user, track, false);
        register(&sessions, second_user, track, true);

        assert_eq!(sessions.audio_delivered(first_user, track), Some(false));
        assert_eq!(sessions.audio_delivered(second_user, track), Some(true));
    }

    #[test]
    fn a_session_is_forgotten_once_its_lifetime_elapsed() {
        let sessions = StreamSessions::with_options(SessionOptions {
            ttl: Duration::ZERO,
            ..SessionOptions::default()
        });
        let (user, track) = (Uuid::now_v7(), Uuid::now_v7());

        register(&sessions, user, track, true);
        assert_eq!(
            sessions.audio_delivered(user, track),
            None,
            "a session past its lifetime is not consulted"
        );
        assert!(sessions.is_empty(), "and it is dropped on the lookup");
    }

    #[test]
    fn capacity_evicts_the_oldest_session() {
        let sessions = StreamSessions::with_options(SessionOptions {
            capacity: 2,
            ..SessionOptions::default()
        });
        let user = Uuid::now_v7();
        let (first, second, third) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

        register(&sessions, user, first, true);
        register(&sessions, user, second, true);
        register(&sessions, user, third, true);

        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions.audio_delivered(user, first),
            None,
            "the oldest session is evicted first"
        );
        assert_eq!(sessions.audio_delivered(user, second), Some(true));
        assert_eq!(sessions.audio_delivered(user, third), Some(true));
    }

    #[test]
    fn a_replaced_session_does_not_count_towards_capacity() {
        let sessions = StreamSessions::with_options(SessionOptions {
            capacity: 1,
            ..SessionOptions::default()
        });
        let user = Uuid::now_v7();
        let track = Uuid::now_v7();

        for _ in 0..5 {
            register(&sessions, user, track, true);
        }

        assert_eq!(
            sessions.len(),
            1,
            "re-registering leaves one session behind"
        );
        assert_eq!(sessions.audio_delivered(user, track), Some(true));
    }
}
