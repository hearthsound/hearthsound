//! The server event vocabulary and the event bus.
//!
//! [`ServerEvent`] is the single event vocabulary the whole core publishes: no
//! service invents an event type that is not one of its variants. The event
//! types are a closed set, and the native API's SSE endpoint forwards these
//! variants verbatim, so the `type` strings are part of the API contract.
//!
//! [`EventBus`] publishes events to its subscribers and keeps the last
//! [`REPLAY_CAPACITY`] events so a client that reconnects with a
//! `Last-Event-ID` can be served the events it missed instead of a stale view.
//!
//! # Examples
//!
//! ```
//! use hearthsound_core::events::{EventBus, ServerEvent, SystemNotice};
//!
//! let bus = EventBus::new();
//! let mut receiver = bus.subscribe();
//! let id = bus.publish(ServerEvent::SystemNotice(SystemNotice {
//!     level: "info".to_owned(),
//!     message: "server started".to_owned(),
//!     code: None,
//! }));
//! assert_eq!(id, 1);
//! assert_eq!(receiver.try_recv().unwrap().id, id);
//! ```

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    AcquisitionState, Availability, EntityKind, FailureReason, ParkReason, Priority, QualityTier,
};

/// The number of recent events the bus keeps for replay.
///
/// A client that reconnects with a `Last-Event-ID` older than the buffer's
/// oldest event cannot be caught up from the buffer and is told to refetch
/// state; see [`Replay`].
pub const REPLAY_CAPACITY: usize = 1000;

/// An event the core publishes.
///
/// The enum serializes to the API's event shape: a `type` string naming the
/// event and a `data` object holding the payload, wrapped by [`EventEnvelope`]
/// with the sequence number and timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum ServerEvent {
    /// An acquisition changed state or progress.
    ///
    /// The orchestrator throttles this event to at most one per acquisition per
    /// two seconds.
    #[serde(rename = "acquisition.updated")]
    AcquisitionUpdated(AcquisitionUpdated),
    /// A track's availability changed.
    #[serde(rename = "track.availability")]
    TrackAvailability(TrackAvailability),
    /// A replacement file was imported for a track.
    #[serde(rename = "track.upgraded")]
    TrackUpgraded(TrackUpgraded),
    /// The library gained or lost an entity.
    #[serde(rename = "library.changed")]
    LibraryChanged(LibraryChanged),
    /// A playlist changed.
    #[serde(rename = "playlist.changed")]
    PlaylistChanged(PlaylistChanged),
    /// An upload progressed or changed state.
    #[serde(rename = "upload.updated")]
    UploadUpdated(UploadUpdated),
    /// A library scan progressed.
    #[serde(rename = "scan.progress")]
    ScanProgress(ScanProgress),
    /// A plugin instance changed lifecycle state.
    #[serde(rename = "plugin.status")]
    PluginStatus(PluginStatus),
    /// The prefetch scheduler started or dropped a look-ahead acquisition.
    #[serde(rename = "queue.prefetch")]
    QueuePrefetch(QueuePrefetch),
    /// The server has something to tell the client that no other event covers.
    #[serde(rename = "system.notice")]
    SystemNotice(SystemNotice),
    /// Catalog extras for an entity were fetched and cached.
    #[serde(rename = "catalog.enriched")]
    CatalogEnriched(CatalogEnriched),
}

impl ServerEvent {
    /// Returns the event type string.
    ///
    /// The string is the value of the envelope's `type` member and the SSE
    /// `event:` field, so it is exactly the name [`ServerEvent`] serializes
    /// with.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::{ServerEvent, SystemNotice};
    ///
    /// let event = ServerEvent::SystemNotice(SystemNotice {
    ///     level: "info".to_owned(),
    ///     message: "restarting".to_owned(),
    ///     code: None,
    /// });
    /// assert_eq!(event.type_str(), "system.notice");
    /// ```
    pub fn type_str(&self) -> &'static str {
        match self {
            ServerEvent::AcquisitionUpdated(_) => "acquisition.updated",
            ServerEvent::TrackAvailability(_) => "track.availability",
            ServerEvent::TrackUpgraded(_) => "track.upgraded",
            ServerEvent::LibraryChanged(_) => "library.changed",
            ServerEvent::PlaylistChanged(_) => "playlist.changed",
            ServerEvent::UploadUpdated(_) => "upload.updated",
            ServerEvent::ScanProgress(_) => "scan.progress",
            ServerEvent::PluginStatus(_) => "plugin.status",
            ServerEvent::QueuePrefetch(_) => "queue.prefetch",
            ServerEvent::SystemNotice(_) => "system.notice",
            ServerEvent::CatalogEnriched(_) => "catalog.enriched",
        }
    }
}

/// The payload of an `acquisition.updated` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcquisitionUpdated {
    /// The acquisition's client id.
    pub acquisition_id: String,
    /// The client id of the track or album being acquired, which is an `x_`
    /// catalog id while the target is not materialized.
    pub target_id: String,
    /// The acquisition's current state.
    pub state: AcquisitionState,
    /// The acquisition's current priority.
    pub priority: Priority,
    /// The download progress as a whole percentage, when one is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    /// The estimated number of seconds until the download completes, when one
    /// is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eta_secs: Option<u32>,
    /// The slug of the provider instance serving the acquisition, when one is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Why the acquisition failed, when it has failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<FailureReason>,
    /// Whether the acquisition is waiting for an acquisition source.
    ///
    /// A parked acquisition is `queued` and looks idle; this flag is what makes
    /// a client say so instead of showing a search that never starts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_source: bool,
    /// Why the acquisition is waiting for a source, when it is parked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub park_reason: Option<ParkReason>,
}

/// The payload of a `track.availability` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackAvailability {
    /// The track's client id.
    pub track_id: String,
    /// The track's new availability.
    pub availability: Availability,
    /// The quality tier of the file the track is now served from, when it has
    /// one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualityTier>,
}

/// The payload of a `track.upgraded` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackUpgraded {
    /// The track's client id.
    pub track_id: String,
    /// The quality tier of the replacement file.
    pub quality: QualityTier,
    /// The offset, in milliseconds, by which the replacement file's audio
    /// differs from the file it replaces. A first-party client shifts its
    /// crossfade by this amount; the value is absent when the two files line up
    /// exactly or the offset is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align_offset_ms: Option<i64>,
}

/// The payload of a `library.changed` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibraryChanged {
    /// The kind of the entity that changed.
    pub entity_kind: EntityKind,
    /// The entity's client id.
    pub entity_id: String,
    /// What happened to the entity.
    pub change: LibraryChange,
}

/// The change a `library.changed` event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LibraryChange {
    /// The entity was added to the library.
    Added,
    /// The entity was removed from the library.
    Removed,
}

impl LibraryChange {
    /// Returns the literal of this change.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::LibraryChange;
    ///
    /// assert_eq!(LibraryChange::Removed.as_str(), "removed");
    /// ```
    pub const fn as_str(self) -> &'static str {
        match self {
            LibraryChange::Added => "added",
            LibraryChange::Removed => "removed",
        }
    }
}

/// The payload of a `playlist.changed` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistChanged {
    /// The playlist's client id.
    pub playlist_id: String,
    /// What happened to the playlist.
    pub change: String,
}

/// The payload of an `upload.updated` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadUpdated {
    /// The upload's client id.
    pub upload_id: String,
    /// The upload's state.
    pub state: String,
    /// The number of bytes received so far.
    pub bytes_received: u64,
    /// The number of bytes the upload promised to send.
    pub total_bytes: u64,
}

/// The payload of a `scan.progress` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// The scanned folder's client id.
    pub folder_id: String,
    /// The scan's current phase.
    pub phase: String,
    /// The number of files seen so far.
    pub files_seen: u64,
    /// The number of files processed so far.
    pub files_done: u64,
    /// The problem the scan ended with, when it ended with one: the failure of
    /// a scan that did not finish, or the warning of one that finished without
    /// doing all of its work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The payload of a `plugin.status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginStatus {
    /// The plugin's client id.
    pub plugin_id: String,
    /// The slug of the instance the status belongs to, when it belongs to one
    /// instance rather than to the plugin as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The plugin lifecycle state, or the health state the host reported.
    pub state: String,
    /// A human readable detail, for example the trap that disabled an instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The payload of a `queue.prefetch` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuePrefetch {
    /// The client whose queue the prefetch was inferred from.
    pub client: String,
    /// The prefetched track's client id.
    pub track_id: String,
    /// The priority the prefetch ran at.
    pub priority: Priority,
    /// Whether the prefetch was started or dropped.
    pub state: String,
}

/// The payload of a `system.notice` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemNotice {
    /// The notice's severity.
    pub level: String,
    /// The message shown to the client.
    pub message: String,
    /// A machine readable code, when the notice has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// The payload of a `catalog.enriched` event.
///
/// The event is published when a `catalog.enrich` job has fetched an entity's
/// missing catalog extras, so the page that asked can refetch without a reload.
/// It reaches the listener whose page asked (administrators receive it under the
/// existing audience rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEnriched {
    /// The entity's client id, exactly as the page that asked named it.
    pub entity_id: String,
    /// The entity kind: `artist` or `album`.
    pub kind: EntityKind,
}

/// Who may receive an event over the event stream.
///
/// The stream forwards an envelope to a connection when
/// [`Audience::is_visible_to`] allows it for that connection's principal.
/// Administrators receive every envelope, so the narrower audiences only limit
/// ordinary users.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Audience {
    /// Every authenticated connection receives the event.
    #[default]
    Everyone,
    /// Only administrator connections receive the event.
    Admins,
    /// The user with this id receives the event, as do administrator connections.
    User(Uuid),
}

impl Audience {
    /// Reports whether an event addressed to this audience may reach a connection.
    ///
    /// # Parameters
    ///
    /// - `user`: the id of the user the connection is authenticated as.
    /// - `admin`: whether that user is an administrator.
    ///
    /// # Returns
    ///
    /// `true` when the connection should receive the event.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::Audience;
    /// use uuid::Uuid;
    ///
    /// let owner = Uuid::from_u128(1);
    /// assert!(Audience::User(owner).is_visible_to(owner, false));
    /// assert!(!Audience::User(owner).is_visible_to(Uuid::from_u128(2), false));
    /// assert!(!Audience::Admins.is_visible_to(owner, false));
    /// ```
    pub fn is_visible_to(self, user: Uuid, admin: bool) -> bool {
        match self {
            Audience::Everyone => true,
            Audience::Admins => admin,
            Audience::User(owner) => admin || owner == user,
        }
    }
}

/// One event with the envelope the API exposes it in.
///
/// The envelope carries the sequence number `id`, the `type` of the event, the
/// wall-clock time `at` in unix milliseconds UTC, and the `data` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// The event's monotonically increasing sequence number.
    ///
    /// The first published event has sequence `1`, and the sequence never
    /// repeats.
    pub id: u64,
    /// The time the bus published the event, in unix milliseconds UTC.
    pub at: i64,
    /// The event itself, flattened into the envelope as `type` and `data`.
    #[serde(flatten)]
    pub event: ServerEvent,
    /// Who may receive the event over the stream.
    ///
    /// This is routing metadata for the server's own connections and is never
    /// serialized.
    #[serde(skip)]
    pub audience: Audience,
}

/// The outcome of replaying after a sequence id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// Every buffered event with a sequence id greater than the requested one,
    /// oldest first.
    Events(Vec<EventEnvelope>),
    /// The requested sequence id is older than the buffer's oldest event, so at
    /// least one event between them was evicted and cannot be replayed. The
    /// caller must tell its client to refetch state.
    Evicted,
}

/// The in-process publisher of [`ServerEvent`]s.
///
/// The bus assigns each event a monotonically increasing sequence id, hands it
/// to every live subscriber over a `tokio::sync::broadcast` channel, and keeps
/// the last [`REPLAY_CAPACITY`] events so a reconnecting subscriber can be
/// replayed the events it missed.
///
/// A subscriber that cannot keep up is dropped rather than allowed to block the
/// publisher: publishing never waits for a subscriber, and the subscriber
/// observes [`broadcast::error::RecvError::Lagged`] with the number of events it
/// missed. Services share one bus through an `Arc`; the bus owns the sequence
/// counter, so cloning it would fork the numbering and is not supported.
pub struct EventBus {
    capacity: usize,
    sender: broadcast::Sender<EventEnvelope>,
    state: Mutex<BusState>,
}

/// The mutable half of the bus: the replay buffer and the sequence counter.
struct BusState {
    /// The most recent events, oldest first, at most `capacity` of them.
    events: VecDeque<EventEnvelope>,
    /// The sequence id the next published event receives.
    next_id: u64,
}

impl EventBus {
    /// Creates a bus holding the last [`REPLAY_CAPACITY`] events.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::EventBus;
    ///
    /// let bus = EventBus::new();
    /// assert_eq!(bus.latest_id(), 0);
    /// ```
    pub fn new() -> EventBus {
        EventBus::with_capacity(REPLAY_CAPACITY)
    }

    /// Creates a bus holding the last `capacity` events.
    ///
    /// The same bound applies to the broadcast channel, which rounds it up to a
    /// power of two, so a subscriber that falls more than `capacity` events
    /// behind observes a lag rather than blocking the publisher. A capacity of
    /// zero is treated as one.
    ///
    /// # Parameters
    ///
    /// - `capacity`: the number of recent events kept for replay.
    pub fn with_capacity(capacity: usize) -> EventBus {
        let capacity = capacity.max(1);
        let (sender, _) = broadcast::channel(capacity);
        EventBus {
            capacity,
            sender,
            state: Mutex::new(BusState {
                events: VecDeque::with_capacity(capacity),
                next_id: 1,
            }),
        }
    }

    /// Publishes `event` to the replay buffer and every subscriber, addressed to
    /// [`Audience::Everyone`].
    ///
    /// Publishing never blocks: a subscriber that is not reading is dropped
    /// from the channel rather than waited for. The send happens under the same
    /// lock that assigns the id, so a subscriber receives ids in the order they
    /// were assigned and a `Last-Event-ID` it reports never skips an event.
    ///
    /// # Parameters
    ///
    /// - `event`: the event to publish.
    ///
    /// # Returns
    ///
    /// The sequence id the bus assigned to the event. The first published event
    /// of a bus receives sequence `1`.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::{EventBus, ServerEvent, SystemNotice};
    ///
    /// let bus = EventBus::new();
    /// let first = bus.publish(ServerEvent::SystemNotice(SystemNotice {
    ///     level: "info".to_owned(),
    ///     message: "hello".to_owned(),
    ///     code: None,
    /// }));
    /// let second = bus.publish(ServerEvent::SystemNotice(SystemNotice {
    ///     level: "info".to_owned(),
    ///     message: "again".to_owned(),
    ///     code: None,
    /// }));
    /// assert_eq!((first, second), (1, 2));
    /// ```
    pub fn publish(&self, event: ServerEvent) -> u64 {
        self.publish_for(Audience::Everyone, event)
    }

    /// Publishes `event` to the replay buffer and every subscriber, addressed to
    /// `audience`.
    ///
    /// The bus delivers to every subscriber. The audience travels with the
    /// envelope so the event stream can decide, per connection, whether to
    /// forward it.
    ///
    /// Publishing never blocks: a subscriber that is not reading is dropped
    /// from the channel rather than waited for.
    ///
    /// # Parameters
    ///
    /// - `audience`: who the event is addressed to.
    /// - `event`: the event to publish.
    ///
    /// # Returns
    ///
    /// The sequence id the bus assigned to the event.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::{Audience, EventBus, ServerEvent, SystemNotice};
    /// use uuid::Uuid;
    ///
    /// let bus = EventBus::new();
    /// let mut receiver = bus.subscribe();
    /// let owner = Uuid::from_u128(1);
    /// bus.publish_for(
    ///     Audience::User(owner),
    ///     ServerEvent::SystemNotice(SystemNotice {
    ///         level: "info".to_owned(),
    ///         message: "for one user".to_owned(),
    ///         code: None,
    ///     }),
    /// );
    /// assert_eq!(receiver.try_recv().unwrap().audience, Audience::User(owner));
    /// ```
    pub fn publish_for(&self, audience: Audience, event: ServerEvent) -> u64 {
        let id = {
            let mut state = self.lock();
            let id = state.next_id;
            state.next_id += 1;
            let envelope = EventEnvelope {
                id,
                at: now_millis(),
                audience,
                event,
            };
            state.events.push_back(envelope.clone());
            if state.events.len() > self.capacity {
                state.events.pop_front();
            }
            // The send is made under the same lock that assigned the id, so two
            // publishers cannot deliver one envelope's id before another's: a
            // live subscriber sees the ids in order, exactly as `replay_after`
            // serves them. A send fails only when the bus has no live
            // subscriber, and the event is in the replay buffer either way.
            let _ = self.sender.send(envelope);
            id
        };
        id
    }

    /// Subscribes to the events published from now on.
    ///
    /// The returned receiver sees events published after the call; events
    /// already in the replay buffer are obtained from [`EventBus::replay_after`]
    /// instead.
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.sender.subscribe()
    }

    /// Returns every buffered event published after `id`.
    ///
    /// # Parameters
    ///
    /// - `id`: the last sequence id the caller has seen; `0` asks for every
    ///   buffered event.
    ///
    /// # Returns
    ///
    /// [`Replay::Events`] with the missed events, oldest first and never empty
    /// by mistake when nothing was missed, or [`Replay::Evicted`] when `id` is
    /// older than the buffer's oldest event, which means at least one event in
    /// between can no longer be replayed.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::events::{EventBus, Replay, ServerEvent, SystemNotice};
    ///
    /// let bus = EventBus::new();
    /// bus.publish(ServerEvent::SystemNotice(SystemNotice {
    ///     level: "info".to_owned(),
    ///     message: "hello".to_owned(),
    ///     code: None,
    /// }));
    /// match bus.replay_after(0) {
    ///     Replay::Events(events) => assert_eq!(events.len(), 1),
    ///     Replay::Evicted => unreachable!(),
    /// }
    /// ```
    pub fn replay_after(&self, id: u64) -> Replay {
        let state = self.lock();
        if let Some(oldest) = state.events.front().map(|event| event.id)
            && id < oldest.saturating_sub(1)
        {
            return Replay::Evicted;
        }
        Replay::Events(
            state
                .events
                .iter()
                .filter(|event| event.id > id)
                .cloned()
                .collect(),
        )
    }

    /// Returns the sequence id of the most recently published event.
    ///
    /// # Returns
    ///
    /// The latest sequence id, or `0` when nothing has been published yet. A
    /// subscriber records this value before it starts reading the channel so
    /// that it knows where the live stream picks up.
    pub fn latest_id(&self) -> u64 {
        self.lock().next_id - 1
    }

    /// Locks the mutable state.
    ///
    /// A poisoned lock is recovered rather than propagated: the only writes
    /// under it are an id increment and a buffer push, either of which leaves
    /// the buffer consistent, and a publishing thread must not take the server
    /// down.
    fn lock(&self) -> MutexGuard<'_, BusState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for EventBus {
    /// Creates a bus with the default replay capacity; see [`EventBus::new`].
    fn default() -> EventBus {
        EventBus::new()
    }
}

/// Returns the current time in unix milliseconds UTC.
///
/// The conversion saturates at `i64::MAX`, which no reachable clock reading
/// comes close to.
fn now_millis() -> i64 {
    let nanos = OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn concurrent_publishers_deliver_ids_in_order() {
        let bus = Arc::new(EventBus::new());
        let mut receiver = bus.subscribe();
        let publishers = 4;
        let each = 250;
        let total = publishers * each;

        let threads: Vec<_> = (0..publishers)
            .map(|_| {
                let bus = Arc::clone(&bus);
                std::thread::spawn(move || {
                    for _ in 0..each {
                        bus.publish(notice("concurrent publishing"));
                    }
                })
            })
            .collect();

        // Drain while the publishers run, so a full channel is not what the
        // test measures.
        let mut received: Vec<u64> = Vec::with_capacity(total);
        let mut skipped = 0;
        while received.len() + skipped < total {
            match receiver.blocking_recv() {
                Ok(envelope) => received.push(envelope.id),
                Err(broadcast::error::RecvError::Lagged(count)) => skipped += count as usize,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(received.len() + skipped, total, "every event was published");
        assert!(
            received.windows(2).all(|pair| pair[0] < pair[1]),
            "a live subscriber sees the ids in order"
        );
        let Replay::Events(buffered) = bus.replay_after(0) else {
            panic!("the buffer holds the events of one run");
        };
        let replayed: Vec<u64> = buffered.iter().map(|event| event.id).collect();
        assert_eq!(replayed.len(), total.min(REPLAY_CAPACITY));
        assert!(
            replayed.windows(2).all(|pair| pair[0] < pair[1]),
            "the replay buffer serves the same events in order"
        );
    }

    /// One event of every type, in the order of the event table.
    fn sample_events() -> Vec<ServerEvent> {
        vec![
            ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
                acquisition_id: "ac_01j9z4k7m2q8v3n5r6t7w8x9za".to_owned(),
                target_id: "x_deezer_t_3135556".to_owned(),
                state: AcquisitionState::Downloading,
                priority: Priority::Play,
                percent: Some(42),
                eta_secs: Some(31),
                instance: Some("slskd".to_owned()),
                failure_reason: None,
                awaiting_source: false,
                park_reason: None,
            }),
            ServerEvent::TrackAvailability(TrackAvailability {
                track_id: "tr_01jgn00000e008000000000001".to_owned(),
                availability: Availability::Local,
                quality: Some(QualityTier::Lossless),
            }),
            ServerEvent::TrackUpgraded(TrackUpgraded {
                track_id: "tr_01jgn00000e008000000000001".to_owned(),
                quality: QualityTier::LosslessHires,
                align_offset_ms: Some(-120),
            }),
            ServerEvent::LibraryChanged(LibraryChanged {
                entity_kind: EntityKind::Track,
                entity_id: "tr_01jgn00000e008000000000001".to_owned(),
                change: LibraryChange::Added,
            }),
            ServerEvent::PlaylistChanged(PlaylistChanged {
                playlist_id: "pl_01jgn00000e008000000000001".to_owned(),
                change: "updated".to_owned(),
            }),
            ServerEvent::UploadUpdated(UploadUpdated {
                upload_id: "up_01jgn00000e008000000000001".to_owned(),
                state: "storing".to_owned(),
                bytes_received: 1024,
                total_bytes: 4096,
            }),
            ServerEvent::ScanProgress(ScanProgress {
                folder_id: "fl_01jgn00000e008000000000001".to_owned(),
                phase: "probing".to_owned(),
                files_seen: 120,
                files_done: 40,
                error: None,
            }),
            ServerEvent::PluginStatus(PluginStatus {
                plugin_id: "pl_01jgn00000e008000000000002".to_owned(),
                slug: Some("slskd".to_owned()),
                state: "disabled".to_owned(),
                detail: Some("trapped 5 times".to_owned()),
            }),
            ServerEvent::QueuePrefetch(QueuePrefetch {
                client: "web".to_owned(),
                track_id: "tr_01jgn00000e008000000000001".to_owned(),
                priority: Priority::Next,
                state: "started".to_owned(),
            }),
            ServerEvent::SystemNotice(SystemNotice {
                level: "warning".to_owned(),
                message: "event buffer overrun".to_owned(),
                code: Some("event-buffer-overrun".to_owned()),
            }),
        ]
    }

    /// A minimal event, for tests that only care about sequencing.
    fn notice(message: &str) -> ServerEvent {
        ServerEvent::SystemNotice(SystemNotice {
            level: "info".to_owned(),
            message: message.to_owned(),
            code: None,
        })
    }

    #[test]
    fn event_types_match_the_api_table() {
        let types: Vec<&str> = sample_events().iter().map(ServerEvent::type_str).collect();
        assert_eq!(
            types,
            [
                "acquisition.updated",
                "track.availability",
                "track.upgraded",
                "library.changed",
                "playlist.changed",
                "upload.updated",
                "scan.progress",
                "plugin.status",
                "queue.prefetch",
                "system.notice",
            ]
        );
    }

    #[test]
    fn every_event_serializes_with_its_type_and_round_trips() {
        for event in sample_events() {
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["type"], event.type_str());
            assert!(value["data"].is_object(), "{event:?} has no data object");
            assert_eq!(serde_json::from_value::<ServerEvent>(value).unwrap(), event);
        }
    }

    #[test]
    fn acquisition_updated_serializes_the_documented_shape() {
        let event = ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
            acquisition_id: "ac_01j9z4k7m2q8v3n5r6t7w8x9za".to_owned(),
            target_id: "x_deezer_t_3135556".to_owned(),
            state: AcquisitionState::Downloading,
            priority: Priority::Play,
            percent: Some(42),
            eta_secs: Some(31),
            instance: Some("slskd".to_owned()),
            failure_reason: None,
            awaiting_source: false,
            park_reason: None,
        });
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({
                "type": "acquisition.updated",
                "data": {
                    "acquisitionId": "ac_01j9z4k7m2q8v3n5r6t7w8x9za",
                    "targetId": "x_deezer_t_3135556",
                    "state": "downloading",
                    "priority": "play",
                    "percent": 42,
                    "etaSecs": 31,
                    "instance": "slskd"
                }
            })
        );
    }

    #[test]
    fn a_failed_acquisition_reports_its_reason() {
        let event = ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
            acquisition_id: "ac_01j9z4k7m2q8v3n5r6t7w8x9za".to_owned(),
            target_id: "tr_01jgn00000e008000000000001".to_owned(),
            state: AcquisitionState::Failed,
            priority: Priority::Queue,
            percent: None,
            eta_secs: None,
            instance: None,
            failure_reason: Some(FailureReason::NoCandidates),
            awaiting_source: false,
            park_reason: None,
        });
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["data"]["failureReason"], "no-candidates");
        assert_eq!(value["data"]["state"], "failed");
        assert!(value["data"].get("percent").is_none());
        assert!(value["data"].get("awaitingSource").is_none());
        assert!(value["data"].get("parkReason").is_none());
    }

    #[test]
    fn a_parked_acquisition_reports_why_it_waits() {
        let event = ServerEvent::AcquisitionUpdated(AcquisitionUpdated {
            acquisition_id: "ac_01j9z4k7m2q8v3n5r6t7w8x9za".to_owned(),
            target_id: "tr_01jgn00000e008000000000001".to_owned(),
            state: AcquisitionState::Queued,
            priority: Priority::Queue,
            percent: None,
            eta_secs: None,
            instance: None,
            failure_reason: None,
            awaiting_source: true,
            park_reason: Some(ParkReason::SourceUnavailable),
        });
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["data"]["awaitingSource"], true);
        assert_eq!(value["data"]["parkReason"], "source-unavailable");
        assert_eq!(value["data"]["state"], "queued");
        assert!(value["data"].get("failureReason").is_none());
    }

    #[test]
    fn the_envelope_serializes_the_documented_shape() {
        let envelope = EventEnvelope {
            id: 41_207,
            at: 1_760_000_000_000,
            audience: Audience::Everyone,
            event: ServerEvent::TrackAvailability(TrackAvailability {
                track_id: "tr_01jgn00000e008000000000001".to_owned(),
                availability: Availability::Local,
                quality: None,
            }),
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "id": 41_207,
                "type": "track.availability",
                "at": 1_760_000_000_000i64,
                "data": {
                    "trackId": "tr_01jgn00000e008000000000001",
                    "availability": "local"
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<EventEnvelope>(value).unwrap(),
            envelope
        );
    }

    #[test]
    fn library_changes_use_their_literals() {
        for (change, literal) in [
            (LibraryChange::Added, "added"),
            (LibraryChange::Removed, "removed"),
        ] {
            assert_eq!(change.as_str(), literal);
            let event = ServerEvent::LibraryChanged(LibraryChanged {
                entity_kind: EntityKind::Album,
                entity_id: "al_01jgn00000e008000000000001".to_owned(),
                change,
            });
            let value = serde_json::to_value(&event).unwrap();
            assert_eq!(value["data"]["change"], literal);
            assert_eq!(value["data"]["entityKind"], "album");
        }
    }

    #[tokio::test]
    async fn sequences_and_streams_events_in_order() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        for expected in 1..=3u64 {
            assert_eq!(bus.publish(notice("hello")), expected);
            let envelope = receiver.recv().await.unwrap();
            assert_eq!(envelope.id, expected);
            assert!(envelope.at > 0);
            assert_eq!(envelope.event, notice("hello"));
        }
        assert_eq!(bus.latest_id(), 3);
        assert_eq!(bus.publish(notice("hello")), 4);
    }

    #[test]
    fn replays_only_the_events_after_the_requested_id() {
        let bus = EventBus::with_capacity(4);
        for _ in 1..=4 {
            bus.publish(notice("hello"));
        }
        let Replay::Events(replayed) = bus.replay_after(2) else {
            panic!("the buffer still holds the whole history");
        };
        let ids: Vec<u64> = replayed.iter().map(|event| event.id).collect();
        assert_eq!(ids, [3, 4]);

        let Replay::Events(all) = bus.replay_after(0) else {
            panic!("the buffer still holds the whole history");
        };
        assert_eq!(all.len(), 4);

        let Replay::Events(none) = bus.replay_after(4) else {
            panic!("nothing to replay is not an eviction");
        };
        assert!(none.is_empty());
    }

    #[test]
    fn evicts_past_the_capacity_and_reports_it() {
        let bus = EventBus::with_capacity(4);
        for _ in 1..=5 {
            bus.publish(notice("hello"));
        }
        assert_eq!(bus.replay_after(0), Replay::Evicted);
        let Replay::Events(replayed) = bus.replay_after(1) else {
            panic!("event 1 must be replayable");
        };
        let ids: Vec<u64> = replayed.iter().map(|event| event.id).collect();
        assert_eq!(ids, [2, 3, 4, 5]);
    }

    #[test]
    fn keeps_the_last_thousand_events_by_default() {
        let bus = EventBus::new();
        for _ in 1..=REPLAY_CAPACITY as u64 + 1 {
            bus.publish(notice("hello"));
        }
        assert_eq!(bus.latest_id(), REPLAY_CAPACITY as u64 + 1);
        assert_eq!(bus.replay_after(0), Replay::Evicted);
        let Replay::Events(replayed) = bus.replay_after(1) else {
            panic!("event 1 is the buffer's predecessor and must be servable");
        };
        assert_eq!(replayed.len(), REPLAY_CAPACITY);
        assert_eq!(replayed[0].id, 2);
        assert_eq!(replayed[REPLAY_CAPACITY - 1].id, REPLAY_CAPACITY as u64 + 1);
    }

    #[tokio::test]
    async fn a_lagging_subscriber_does_not_block_publishing() {
        // Four is a power of two, so the channel does not round the capacity
        // up and the fifth event certainly evicts the first.
        let bus = EventBus::with_capacity(4);
        let mut receiver = bus.subscribe();
        for expected in 1..=5u64 {
            assert_eq!(bus.publish(notice("hello")), expected);
        }
        assert_eq!(bus.latest_id(), 5);
        assert!(matches!(
            receiver.recv().await,
            Err(broadcast::error::RecvError::Lagged(1))
        ));
        // The subscriber resynchronizes on the oldest event the channel still
        // holds.
        let envelope = receiver.recv().await.unwrap();
        assert_eq!(envelope.id, 2);
    }

    #[test]
    fn a_bus_without_subscribers_still_buffers() {
        let bus = EventBus::with_capacity(1);
        bus.publish(notice("hello"));
        bus.publish(notice("hello"));
        assert_eq!(bus.latest_id(), 2);
        assert_eq!(bus.replay_after(0), Replay::Evicted);
        let default = EventBus::default();
        assert_eq!(default.latest_id(), 0);
        assert_eq!(default.replay_after(7), Replay::Events(Vec::new()));
    }

    #[test]
    fn publish_stamps_everyone_and_publish_for_stamps_the_audience() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let user = Uuid::from_u128(7);
        bus.publish(notice("everyone"));
        bus.publish_for(Audience::User(user), notice("private"));
        bus.publish_for(Audience::Admins, notice("admins"));
        let first = receiver.try_recv().unwrap();
        let second = receiver.try_recv().unwrap();
        let third = receiver.try_recv().unwrap();
        assert_eq!(first.audience, Audience::Everyone);
        assert_eq!(second.audience, Audience::User(user));
        assert_eq!(third.audience, Audience::Admins);
        assert_eq!(third.id, 3);
    }

    #[test]
    fn replay_keeps_each_envelope_audience() {
        let bus = EventBus::new();
        bus.publish_for(Audience::Admins, notice("first"));
        bus.publish(notice("second"));
        match bus.replay_after(0) {
            Replay::Events(events) => {
                assert_eq!(events[0].audience, Audience::Admins);
                assert_eq!(events[1].audience, Audience::Everyone);
            }
            Replay::Evicted => panic!("nothing was evicted"),
        }
    }

    #[test]
    fn the_audience_never_reaches_the_wire() {
        let bus = EventBus::new();
        bus.publish_for(Audience::User(Uuid::from_u128(1)), notice("private"));
        let Replay::Events(mut events) = bus.replay_after(0) else {
            unreachable!("nothing was evicted");
        };
        let envelope = events.remove(0);
        let value = serde_json::to_value(&envelope).unwrap();
        assert!(value.get("audience").is_none());
        let back: EventEnvelope = serde_json::from_value(value).unwrap();
        assert_eq!(back.event, envelope.event);
        assert_eq!(back.audience, Audience::Everyone);
    }

    #[test]
    fn audiences_gate_who_sees_an_event() {
        let owner = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        assert!(Audience::Everyone.is_visible_to(other, false));
        assert!(!Audience::Admins.is_visible_to(owner, false));
        assert!(Audience::Admins.is_visible_to(owner, true));
        assert!(Audience::User(owner).is_visible_to(owner, false));
        assert!(!Audience::User(owner).is_visible_to(other, false));
        assert!(Audience::User(owner).is_visible_to(other, true));
    }
}
