//! Prefetching the tracks a client is about to play.
//!
//! A Subsonic client reports what it plays when it streams a track, but not what
//! comes next: the protocol has no queue push, and only some clients save their
//! queue. What the server does see is the listing the streamed track came from -
//! `getAlbum` answered with an ordered tracklist a moment earlier - so the
//! implied queue is inferred from it: the most recent listing of the same user
//! and the same client instance that contains the streamed track, and the items
//! after it in that listing.
//!
//! [`PrefetchScheduler`] owns that inference. A listing is kept in memory for
//! [`LISTING_WINDOW`], bounded to one entry per user and client and to
//! [`MAX_CONTEXTS`] entries in total, and a streamed track starts work only when
//! it is in the listing the same user and client last read. The items the
//! scheduler looks ahead to are requested from
//! [`AcquisitionOrchestrator`](crate::orchestrator::AcquisitionOrchestrator) with
//! reason `prefetch`: the first at [`Priority::Next`], the rest at
//! [`Priority::Queue`]. Items that already have a file, and items whose last
//! acquisition failed inside its cooldown, are left to admission, which reports
//! them as available or cooling down and writes nothing.
//!
//! When a later stream moves the upcoming set - the user jumped, or the client
//! read another album - the prefetch acquisitions that left the set are
//! cancelled, unless the track is pinned or the download is more than half done.
//! A pinned track is wanted regardless of the queue, and an acquisition past
//! half its download costs less to finish than to abandon and start again.
//!
//! Prefetching is an optimization, so nothing here fails a request: every
//! failure is logged with the track it concerned, and
//! [`note_stream`](PrefetchScheduler::note_stream) is meant to be spawned off
//! the request path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::events::{QueuePrefetch, ServerEvent};
use hearthsound_core::{ClientId, EntityId, EntityKind, IdPrefix, Priority};
use uuid::Uuid;

use crate::orchestrator::{AcquireOutcome, AcquireRequest, AcquisitionOrchestrator};
use crate::repo::AcquisitionRow;

/// How long a listing stays the implied queue of a user and client.
///
/// A client that read a listing and streamed one of its tracks a moment later
/// meant that listing; a client that read one half an hour ago has moved on, and
/// the listing is dropped.
pub const LISTING_WINDOW: Duration = Duration::from_secs(30 * 60);

/// The number of listing contexts the scheduler keeps at most.
///
/// A context is one user and one client instance, so this bounds the memory of a
/// server that serves many users and clients: when the store is full the oldest
/// context is dropped.
const MAX_CONTEXTS: usize = 1_024;

/// The reason every prefetch acquisition is requested with.
const PREFETCH_REASON: &str = "prefetch";

/// The percentage past which an item that leaves the queue keeps downloading.
const KEEP_PERCENT: u8 = 50;

/// The `queue.prefetch` state that reports a started acquisition.
const STARTED: &str = "started";

/// The `queue.prefetch` state that reports a cancelled one.
const DROPPED: &str = "dropped";

/// Prefetches the tracks a client's implied queue is about to reach.
///
/// The scheduler is fed two facts and does the rest:
/// [`record_listing`](PrefetchScheduler::record_listing) keeps the ordered track
/// ids of a listing, and [`note_stream`](PrefetchScheduler::note_stream) reports
/// a streamed track and looks ahead from it. Both are cheap: a listing stays in
/// memory, and a streamed track only starts work when it is in the listing the
/// same user and client last read.
///
/// The scheduler owns no database rows. The acquisitions it starts are the
/// acquisition layer's, and its own state is the bounded map of listings, so a
/// restart loses only the listings a client can read again.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use hearthsound_acquire::orchestrator::AcquisitionOrchestrator;
/// use hearthsound_acquire::prefetch::PrefetchScheduler;
/// use hearthsound_core::{ClientId, EntityId, IdPrefix};
/// use uuid::Uuid;
///
/// # fn example(orchestrator: Arc<AcquisitionOrchestrator>) {
/// let scheduler = PrefetchScheduler::new(orchestrator);
/// let user = Uuid::now_v7();
/// let listing: Vec<ClientId> = (0..3)
///     .map(|_| ClientId::Entity(EntityId::new(IdPrefix::Track)))
///     .collect();
/// scheduler.record_listing(user, "symfonium", listing.clone());
/// let streamed = listing[0].clone();
/// tokio::spawn(async move {
///     scheduler.note_stream(user, "symfonium", &streamed, true).await;
/// });
/// # }
/// ```
#[derive(Clone)]
pub struct PrefetchScheduler {
    /// The state every clone shares.
    inner: Arc<Inner>,
}

/// The state one scheduler shares with its clones.
struct Inner {
    /// The acquisition layer every look-ahead is requested from.
    orchestrator: Arc<AcquisitionOrchestrator>,
    /// The listings the implied queues are inferred from.
    listings: Mutex<Listings>,
}

/// The listing contexts, keyed by user and then by client instance.
type Contexts = HashMap<Uuid, HashMap<String, Context>>;

/// One listing context: the ordered ids a listing reported, the queue the
/// client saved, and the prefetch acquisitions it started.
///
/// A context lives for one user and client instance. Its listing is the implied
/// queue the scheduler infers, its saved queue is the one a client wrote with
/// `savePlayQueue`, and the saved queue wins when it contains the streamed
/// track, because a client that saves its queue knows it better than a listing
/// does.
struct Context {
    /// When the listing was recorded, in unix milliseconds; `None` when the
    /// client has only saved a queue.
    listed_at: Option<i64>,
    /// The listing's track ids, in the order the client will play them.
    listing: Vec<ClientId>,
    /// The queue the client saved, in the order the client will play it.
    saved: Option<Vec<ClientId>>,
    /// When the context was last touched by a listing or a save, in unix
    /// milliseconds.
    touched_at: i64,
    /// The acquisitions this context started, keyed by the item they were
    /// started for.
    prefetched: HashMap<ClientId, Uuid>,
}

/// The bounded store of listing contexts.
///
/// One entry per user and client instance, holding the most recent listing, the
/// queue the client saved, and the acquisitions the upcoming set started. An
/// entry whose listing is older than [`LISTING_WINDOW`] and that holds no saved
/// queue is dropped on the next call, and the least recently touched entry is
/// dropped when the store is full, so a server that serves many users and
/// clients keeps a fixed amount of memory.
#[derive(Default)]
struct Listings {
    /// The contexts, keyed by user and client.
    contexts: Contexts,
}

impl Listings {
    /// Records a listing as the context's implied queue.
    ///
    /// The context's previous listing is replaced; the saved queue and the
    /// acquisitions it started are kept, so a later stream can still cancel the
    /// ones the new listing left behind.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance.
    /// - `listing`: the ordered track ids of the listing.
    /// - `now`: the current time in unix milliseconds.
    fn record(&mut self, user: Uuid, client: &str, listing: Vec<ClientId>, now: i64) {
        self.prune(now);
        let context = self.contexts.entry(user).or_default().remove(client);
        let context = match context {
            Some(mut context) => {
                context.listed_at = Some(now);
                context.listing = listing;
                context.touched_at = now;
                context
            }
            None => Context {
                listed_at: Some(now),
                listing,
                saved: None,
                touched_at: now,
                prefetched: HashMap::new(),
            },
        };
        self.contexts
            .entry(user)
            .or_default()
            .insert(client.to_owned(), context);
        self.enforce_capacity();
    }

    /// Records the queue a client saved, or clears it.
    ///
    /// The queue is kept until the client saves another one or the context is
    /// evicted, which is what makes it the best source the scheduler has: a
    /// client that saves its queue is telling the server exactly what it will
    /// play next. An empty queue clears the saved one and leaves the listing
    /// alone.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the queue belongs to.
    /// - `client`: the client instance.
    /// - `queue`: the ordered track ids of the queue; empty clears it.
    /// - `now`: the current time in unix milliseconds.
    fn save(&mut self, user: Uuid, client: &str, queue: Vec<ClientId>, now: i64) {
        self.prune(now);
        let contexts = self.contexts.entry(user).or_default();
        let saved = (!queue.is_empty()).then_some(queue);
        match contexts.get_mut(client) {
            Some(context) => {
                context.saved = saved;
                context.touched_at = now;
                if context.saved.is_none() && context.listed_at.is_none_or(|at| !is_recent(at, now))
                {
                    contexts.remove(client);
                }
            }
            None if saved.is_some() => {
                contexts.insert(
                    client.to_owned(),
                    Context {
                        listed_at: None,
                        listing: Vec::new(),
                        saved,
                        touched_at: now,
                        prefetched: HashMap::new(),
                    },
                );
            }
            None => {}
        }
        self.contexts.retain(|_, contexts| !contexts.is_empty());
        self.enforce_capacity();
    }

    /// Returns the context of one user and client, when it is still recent.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance.
    /// - `now`: the current time in unix milliseconds.
    ///
    /// # Returns
    ///
    /// The context, or `None` when the user or the client has none or its
    /// listing is older than [`LISTING_WINDOW`].
    fn recent(&mut self, user: Uuid, client: &str, now: i64) -> Option<&Context> {
        self.prune(now);
        self.contexts.get(&user)?.get(client)
    }

    /// Records the acquisition one context item started.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance.
    /// - `item`: the listing item the acquisition was started for.
    /// - `acquisition`: the acquisition to remember.
    ///
    /// # Returns
    ///
    /// `true` when the item was not tracked before or its acquisition is a new
    /// one, which is when the client has not heard about that acquisition yet.
    fn track(&mut self, user: Uuid, client: &str, item: &ClientId, acquisition: Uuid) -> bool {
        let Some(context) = self
            .contexts
            .get_mut(&user)
            .and_then(|contexts| contexts.get_mut(client))
        else {
            return true;
        };
        context.prefetched.insert(item.clone(), acquisition) != Some(acquisition)
    }

    /// Stops tracking one item of a context.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance.
    /// - `item`: the listing item to forget.
    fn forget(&mut self, user: Uuid, client: &str, item: &ClientId) {
        if let Some(context) = self
            .contexts
            .get_mut(&user)
            .and_then(|contexts| contexts.get_mut(client))
        {
            context.prefetched.remove(item);
        }
    }

    /// Drops the contexts whose listing is older than the window.
    ///
    /// A context that holds a saved queue is kept whatever its listing's age,
    /// because the client saved it and has not replaced it.
    ///
    /// # Parameters
    ///
    /// - `now`: the current time in unix milliseconds.
    fn prune(&mut self, now: i64) {
        for contexts in self.contexts.values_mut() {
            contexts.retain(|_, context| {
                context.saved.is_some() || context.listed_at.is_some_and(|at| is_recent(at, now))
            });
        }
        self.contexts.retain(|_, contexts| !contexts.is_empty());
    }

    /// Drops the oldest contexts until the store is within its capacity.
    fn enforce_capacity(&mut self) {
        while self.count() > MAX_CONTEXTS {
            let Some(oldest) = self.oldest() else {
                return;
            };
            if let Some(contexts) = self.contexts.get_mut(&oldest.0) {
                contexts.remove(&oldest.1);
            }
            self.contexts.retain(|_, contexts| !contexts.is_empty());
        }
    }

    /// Returns how many contexts the store holds.
    fn count(&self) -> usize {
        self.contexts.values().map(HashMap::len).sum()
    }

    /// Returns the user and client of the context that was touched least
    /// recently.
    fn oldest(&self) -> Option<(Uuid, String)> {
        self.contexts
            .iter()
            .flat_map(|(user, contexts)| {
                contexts
                    .iter()
                    .map(move |(client, context)| (*user, client.clone(), context.touched_at))
            })
            .min_by_key(|(_, _, touched_at)| *touched_at)
            .map(|(user, client, _)| (user, client))
    }
}

/// Returns the items a streamed track looks ahead to, from the best source.
///
/// The saved queue of a client wins when it holds the streamed track, because a
/// client that saves its queue knows what it will play next; the listing context
/// fills in for the clients that do not save one.
///
/// # Parameters
///
/// - `queue`: the queue the client saved, when it saved one.
/// - `listing`: the listing the client last read, when it is still recent.
/// - `track`: the streamed track.
/// - `lookahead`: how many items to look ahead to.
///
/// # Returns
///
/// The upcoming items, in playing order, or `None` when neither source holds the
/// streamed track.
fn upcoming_from(
    queue: Option<&[ClientId]>,
    listing: Option<&[ClientId]>,
    track: &ClientId,
    lookahead: usize,
) -> Option<Vec<ClientId>> {
    let items = queue
        .and_then(|queue| position_of(queue, track).map(|position| (queue, position)))
        .or_else(|| {
            listing
                .and_then(|listing| position_of(listing, track).map(|position| (listing, position)))
        })?;
    Some(
        items.0[items.1 + 1..]
            .iter()
            .take(lookahead)
            .cloned()
            .collect(),
    )
}

/// Returns the position of an item in a queue.
///
/// # Parameters
///
/// - `items`: the ordered items.
/// - `track`: the item to find.
///
/// # Returns
///
/// The item's position, or `None` when it is not in the queue.
fn position_of(items: &[ClientId], track: &ClientId) -> Option<usize> {
    items.iter().position(|item| item == track)
}

/// Reports whether a listing recorded at `recorded_at` is inside the window.
///
/// # Parameters
///
/// - `recorded_at`: when the listing was recorded, in unix milliseconds.
/// - `now`: the current time in unix milliseconds.
///
/// # Returns
///
/// `true` when less than [`LISTING_WINDOW`] has passed since the listing was
/// recorded. A listing with a timestamp in the future counts as recent, which is
/// what a clock that moved backwards needs.
fn is_recent(recorded_at: i64, now: i64) -> bool {
    now.saturating_sub(recorded_at) < LISTING_WINDOW.as_millis() as i64
}

impl PrefetchScheduler {
    /// Builds the scheduler over the acquisition layer.
    ///
    /// # Parameters
    ///
    /// - `orchestrator`: the acquisition layer the look-ahead acquisitions are
    ///   requested from. The scheduler reads its settings, its library, its
    ///   catalog and its event bus through that one handle, so a composition
    ///   root needs no other wiring.
    ///
    /// # Returns
    ///
    /// The scheduler, with no listing recorded and no background task: it runs
    /// entirely on the calls it receives.
    pub fn new(orchestrator: Arc<AcquisitionOrchestrator>) -> PrefetchScheduler {
        PrefetchScheduler {
            inner: Arc::new(Inner {
                orchestrator,
                listings: Mutex::new(Listings::default()),
            }),
        }
    }

    /// Returns the orchestrator the scheduler starts its acquisitions through.
    ///
    /// A surface that holds the scheduler reaches the acquisition layer through
    /// it, for example to acquire what a user saved.
    pub fn orchestrator(&self) -> &Arc<AcquisitionOrchestrator> {
        &self.inner.orchestrator
    }

    /// Records a listing as the implied queue of one user and client.
    ///
    /// The listing replaces the one the same user and client recorded before,
    /// which is what makes it "the most recent listing" a later stream is
    /// matched against. Nothing is stored in the database, and an
    /// `x_<slug>_t_<id>` item is kept as it is: a catalog track is materialized
    /// only when it is actually prefetched.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance, the Subsonic `c` parameter; the empty
    ///   string when the client named none, which is one context like any other.
    /// - `listing`: the listing's track ids in playing order. A listing of
    ///   another entity kind, or an empty one, is recorded as it is and simply
    ///   never matches a streamed track.
    ///
    /// # Returns
    ///
    /// Nothing. `prefetch.enabled` off makes this a no-op, and a setting that
    /// cannot be read is logged and treated as off.
    pub fn record_listing(&self, user: Uuid, client: &str, listing: Vec<ClientId>) {
        if !self.enabled() {
            return;
        }
        let now = crate::now_millis();
        lock(&self.inner.listings).record(user, client, listing, now);
        tracing::debug!(
            user = %user,
            client,
            "recorded a listing for prefetch inference"
        );
    }

    /// Reports that a client started streaming a track.
    ///
    /// The scheduler looks the track up in the listing the same user and client
    /// instance last read, takes the next `prefetch.lookahead` items of that
    /// listing as the upcoming set, and then
    ///
    /// 1. cancels the prefetch acquisitions it started that are no longer in it,
    ///    unless their track is pinned or their download is more than half done,
    ///    publishing a `queue.prefetch` event for each one it drops;
    /// 2. requests the upcoming items, the first at [`Priority::Next`] and the
    ///    rest at [`Priority::Queue`], publishing a `queue.prefetch` event for
    ///    each acquisition it starts.
    ///
    /// A track that is in no recorded listing, an item that already has a file,
    /// an item inside its cooldown, and a client that may not acquire all leave
    /// the ledger untouched: admission decides, and the scheduler only reports
    /// what it decided.
    ///
    /// # Parameters
    ///
    /// - `user`: the user whose client streamed the track.
    /// - `client`: the client instance, as in
    ///   [`record_listing`](PrefetchScheduler::record_listing).
    /// - `track`: the streamed track as the client named it: a stored track id
    ///   or an `x_` catalog id.
    /// - `can_acquire`: whether that user may acquire at all; a user without the
    ///   permission gets no prefetch either.
    ///
    /// # Returns
    ///
    /// Nothing. Every failure is logged with the track it concerned, so a caller
    /// can spawn this off the request path without handling anything.
    pub async fn note_stream(&self, user: Uuid, client: &str, track: &ClientId, can_acquire: bool) {
        if !self.enabled() {
            return;
        }
        let now = crate::now_millis();
        let queue = self.queue(user, client);
        let listing = self.listing(user, client, now);
        let Some(upcoming) = upcoming_from(
            queue.as_deref(),
            listing.as_deref(),
            track,
            self.lookahead(),
        ) else {
            tracing::debug!(
                user = %user,
                client,
                track = %track,
                "neither a saved queue nor a recent listing holds the streamed track"
            );
            return;
        };

        self.drop_departed(user, client, &upcoming).await;
        self.start_upcoming(user, client, &upcoming, can_acquire)
            .await;
    }

    /// Records the play queue one client saved.
    ///
    /// The saved queue is the best source the scheduler has, so a client that
    /// saves its queue gets look-ahead for exactly what it will play next. The
    /// queue is kept until the client saves another one or the context is
    /// evicted; an empty queue clears the saved one, which is what a client that
    /// emptied its queue reports.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the queue belongs to.
    /// - `client`: the client instance, as in
    ///   [`record_listing`](PrefetchScheduler::record_listing).
    /// - `queue`: the queue's track ids in playing order; empty clears it.
    ///
    /// # Returns
    ///
    /// Nothing. `prefetch.enabled` off makes this a no-op, and a setting that
    /// cannot be read is logged and treated as off.
    pub fn record_queue(&self, user: Uuid, client: &str, queue: Vec<ClientId>) {
        if !self.enabled() {
            return;
        }
        let now = crate::now_millis();
        lock(&self.inner.listings).save(user, client, queue, now);
        tracing::debug!(user = %user, client, "recorded a saved play queue for prefetch");
    }

    /// Returns the settings in effect.
    fn settings(&self) -> Arc<SettingsStore> {
        self.inner.orchestrator.settings().current()
    }

    /// Reports whether `prefetch.enabled` lets the scheduler work.
    ///
    /// # Returns
    ///
    /// `true` when the setting is on. A setting that cannot be read is logged
    /// and reported as off, because a scheduler that cannot tell must not start
    /// work.
    fn enabled(&self) -> bool {
        match self.settings().get(keys::PREFETCH_ENABLED) {
            Ok(enabled) => enabled,
            Err(error) => {
                tracing::warn!(%error, "could not read prefetch.enabled; prefetching stays off");
                false
            }
        }
    }

    /// Returns how many upcoming items the scheduler looks ahead to.
    ///
    /// # Returns
    ///
    /// `prefetch.lookahead`, or zero when the setting cannot be read, which
    /// disables the look-ahead without disabling the cancellation rule.
    fn lookahead(&self) -> usize {
        match self.settings().get(keys::PREFETCH_LOOKAHEAD) {
            Ok(lookahead) => usize::try_from(lookahead).unwrap_or(0),
            Err(error) => {
                tracing::warn!(%error, "could not read prefetch.lookahead; looking ahead zero items");
                0
            }
        }
    }

    /// Returns the listing recorded for one user and client.
    ///
    /// A context that holds a saved queue but a stale listing reports no
    /// listing, because a listing that old says nothing about what the client
    /// plays next.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the listing belongs to.
    /// - `client`: the client instance.
    /// - `now`: the current time in unix milliseconds.
    fn listing(&self, user: Uuid, client: &str, now: i64) -> Option<Vec<ClientId>> {
        lock(&self.inner.listings)
            .recent(user, client, now)
            .filter(|context| context.listed_at.is_some_and(|at| is_recent(at, now)))
            .map(|context| context.listing.clone())
    }

    /// Returns the queue one user and client saved.
    ///
    /// # Parameters
    ///
    /// - `user`: the user the queue belongs to.
    /// - `client`: the client instance.
    ///
    /// # Returns
    ///
    /// The queue's track ids in playing order, or `None` when the client saved
    /// none or cleared the one it had.
    fn queue(&self, user: Uuid, client: &str) -> Option<Vec<ClientId>> {
        lock(&self.inner.listings)
            .recent(user, client, crate::now_millis())
            .and_then(|context| context.saved.clone())
    }

    /// Cancels the prefetch acquisitions that left the upcoming set.
    ///
    /// # Parameters
    ///
    /// - `user`: the user whose client streamed.
    /// - `client`: the client instance.
    /// - `upcoming`: the items the client is about to reach.
    async fn drop_departed(&self, user: Uuid, client: &str, upcoming: &[ClientId]) {
        let tracked: Vec<(ClientId, Uuid)> = lock(&self.inner.listings)
            .recent(user, client, crate::now_millis())
            .map(|context| {
                context
                    .prefetched
                    .iter()
                    .map(|(item, acquisition)| (item.clone(), *acquisition))
                    .collect()
            })
            .unwrap_or_default();
        for (item, acquisition) in tracked {
            if upcoming.contains(&item) {
                continue;
            }
            self.drop(user, client, &item, acquisition).await;
        }
    }

    /// Ends one prefetch acquisition that left the upcoming set.
    ///
    /// An acquisition that already ended, one whose row is gone, and one whose
    /// track is pinned or past half its download are only forgotten: nothing is
    /// cancelled and no event is published, because nothing changed for the
    /// client.
    ///
    /// # Parameters
    ///
    /// - `user`: the user whose client streamed.
    /// - `client`: the client instance.
    /// - `item`: the listing item the acquisition was started for.
    /// - `acquisition`: the acquisition to cancel.
    async fn drop(&self, user: Uuid, client: &str, item: &ClientId, acquisition: Uuid) {
        let row = match self.inner.orchestrator.repository().get(acquisition).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                lock(&self.inner.listings).forget(user, client, item);
                return;
            }
            Err(error) => {
                tracing::warn!(
                    acquisition = %acquisition,
                    %error,
                    "could not read a prefetch acquisition; it keeps running"
                );
                return;
            }
        };
        let Some(track) = row.track_id else {
            lock(&self.inner.listings).forget(user, client, item);
            return;
        };
        if row.state.is_terminal() || self.keep(track, &row).await {
            lock(&self.inner.listings).forget(user, client, item);
            return;
        }
        match self.inner.orchestrator.cancel(acquisition).await {
            Ok(()) => {
                lock(&self.inner.listings).forget(user, client, item);
                self.publish(client, item, row.priority, DROPPED);
                tracing::debug!(
                    acquisition = %acquisition,
                    track = %track,
                    client,
                    "dropped a prefetch acquisition that left the queue"
                );
            }
            Err(error) => tracing::warn!(
                acquisition = %acquisition,
                %error,
                "could not cancel a prefetch acquisition; it keeps running"
            ),
        }
    }

    /// Reports whether an acquisition that left the queue still finishes.
    ///
    /// # Parameters
    ///
    /// - `track`: the track the acquisition targets.
    /// - `row`: the acquisition's ledger row.
    ///
    /// # Returns
    ///
    /// `true` when the track is pinned, because it is wanted regardless of the
    /// queue, and `true` when the download is more than half done, because
    /// finishing costs less than abandoning and acquiring again. A pin that
    /// cannot be read keeps the acquisition, so an unreadable row never throws
    /// away work.
    async fn keep(&self, track: EntityId, row: &AcquisitionRow) -> bool {
        match self
            .inner
            .orchestrator
            .library()
            .track_is_pinned(track)
            .await
        {
            Ok(true) => return true,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    track = %track,
                    %error,
                    "could not read a track's pins; its prefetch keeps running"
                );
                return true;
            }
        }
        crate::download::stored_progress(row.progress.as_deref())
            .and_then(|progress| progress.percent())
            .is_some_and(|percent| percent > KEEP_PERCENT)
    }

    /// Requests the upcoming items, in order.
    ///
    /// # Parameters
    ///
    /// - `user`: the user whose client streamed.
    /// - `client`: the client instance.
    /// - `upcoming`: the items the client is about to reach.
    /// - `can_acquire`: whether the user may acquire.
    async fn start_upcoming(
        &self,
        user: Uuid,
        client: &str,
        upcoming: &[ClientId],
        can_acquire: bool,
    ) {
        for (index, item) in upcoming.iter().enumerate() {
            let priority = if index == 0 {
                Priority::Next
            } else {
                Priority::Queue
            };
            self.start_item(user, client, item, priority, can_acquire)
                .await;
        }
    }

    /// Requests one upcoming item and reports the acquisition it produced.
    ///
    /// # Parameters
    ///
    /// - `user`: the user whose client streamed.
    /// - `client`: the client instance.
    /// - `item`: the listing item to acquire.
    /// - `priority`: the priority the item is requested at.
    /// - `can_acquire`: whether the user may acquire.
    async fn start_item(
        &self,
        user: Uuid,
        client: &str,
        item: &ClientId,
        priority: Priority,
        can_acquire: bool,
    ) {
        let Some(track) = self.materialize(item).await else {
            return;
        };
        let request = AcquireRequest::new(track, priority, PREFETCH_REASON)
            .with_requested_by(Some(user))
            .with_can_acquire(can_acquire);
        match self.inner.orchestrator.request(request).await {
            Ok(AcquireOutcome::Started(handle)) | Ok(AcquireOutcome::Joined(handle)) => {
                let started = lock(&self.inner.listings).track(user, client, item, handle.id);
                if started {
                    self.publish(client, item, handle.priority, STARTED);
                }
                tracing::debug!(
                    acquisition = %handle.id,
                    track = %track,
                    client,
                    priority = handle.priority.as_str(),
                    "prefetched an upcoming track"
                );
            }
            Ok(AcquireOutcome::AlreadyAvailable { .. }) => {
                tracing::debug!(track = %track, "an upcoming track already has a file");
            }
            Ok(AcquireOutcome::CoolingDown { .. }) => {
                tracing::debug!(track = %track, "an upcoming track is cooling down");
            }
            Ok(AcquireOutcome::Denied { reason, .. }) => {
                tracing::debug!(
                    track = %track,
                    user = %user,
                    reason = reason.as_str(),
                    "a prefetch request was denied"
                );
            }
            Err(error) => tracing::warn!(
                track = %track,
                %error,
                "could not start a prefetch acquisition"
            ),
        }
    }

    /// Resolves a listing item to the stored track to acquire.
    ///
    /// A stored track id is the row itself. An `x_` catalog id is materialized
    /// through the catalog layer, which answers a track the server already
    /// stores without calling its instance, so a catalog listing costs a
    /// provider call only for the tracks that are actually prefetched.
    ///
    /// # Parameters
    ///
    /// - `item`: the listing item.
    ///
    /// # Returns
    ///
    /// The stored track, or `None` when the item is not a track or the catalog
    /// layer could not resolve it, which is logged.
    async fn materialize(&self, item: &ClientId) -> Option<EntityId> {
        match item {
            ClientId::Entity(id) if id.kind() == IdPrefix::Track => Some(*id),
            ClientId::Entity(id) => {
                tracing::debug!(id = %id, "a listing item is not a track");
                None
            }
            ClientId::Catalog(id) if id.kind() == EntityKind::Track => {
                match self
                    .inner
                    .orchestrator
                    .catalog()
                    .materialize_track(id)
                    .await
                {
                    Ok(track) => Some(track),
                    Err(error) => {
                        tracing::warn!(
                            id = %id,
                            %error,
                            "could not materialize a prefetched catalog track"
                        );
                        None
                    }
                }
            }
            ClientId::Catalog(id) => {
                tracing::debug!(id = %id, "a listing item is not a track");
                None
            }
        }
    }

    /// Publishes one `queue.prefetch` event.
    ///
    /// # Parameters
    ///
    /// - `client`: the client instance the prefetch was inferred from.
    /// - `item`: the listing item the event is about.
    /// - `priority`: the priority the prefetch ran at.
    /// - `state`: [`STARTED`] or [`DROPPED`].
    fn publish(&self, client: &str, item: &ClientId, priority: Priority, state: &str) {
        self.inner
            .orchestrator
            .events()
            .publish(ServerEvent::QueuePrefetch(QueuePrefetch {
                client: client.to_owned(),
                track_id: item.to_string(),
                priority,
                state: state.to_owned(),
            }));
    }
}

impl std::fmt::Debug for PrefetchScheduler {
    /// Writes how many listing contexts the scheduler holds.
    ///
    /// The listings themselves are omitted: a debug line wants the size of the
    /// state, not every track id a client read.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrefetchScheduler")
            .field("contexts", &lock(&self.inner.listings).count())
            .finish_non_exhaustive()
    }
}

/// Locks a mutex, taking the value even when a previous holder panicked.
///
/// The scheduler's map is plain data that a panic cannot leave inconsistent, so
/// a poisoned lock must not stop prefetching for the life of the process.
///
/// # Parameters
///
/// - `mutex`: the mutex to lock.
///
/// # Returns
///
/// The guard.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns a track client id, for listings that name stored tracks.
    fn track() -> ClientId {
        ClientId::Entity(EntityId::new(IdPrefix::Track))
    }

    /// Returns a listing of `count` distinct stored tracks.
    fn listing(count: usize) -> Vec<ClientId> {
        (0..count).map(|_| track()).collect()
    }

    #[test]
    fn a_listing_is_recent_until_the_window_elapses() {
        let now = 1_800_000_000_000;
        assert!(is_recent(now, now));
        assert!(is_recent(now - 1_799_999, now));
        assert!(!is_recent(now - 1_800_000, now));
        assert!(is_recent(now + 1_000, now));
    }

    #[test]
    fn a_saved_queue_outlasts_the_listing_window() {
        let mut listings = Listings::default();
        let user = Uuid::now_v7();
        let saved = listing(4);
        listings.save(user, "client", saved.clone(), 1_000);

        // A save with no listing of its own is not pruned by the listing
        // window, because the client has not replaced it.
        let later = 1_000 + 10 * LISTING_WINDOW.as_millis() as i64;
        let context = listings.recent(user, "client", later).unwrap();
        assert_eq!(context.saved.as_ref(), Some(&saved));
        assert!(context.listed_at.is_none());

        // A listing recorded afterwards is what the window applies to.
        listings.record(user, "client", listing(2), later);
        assert!(
            listings
                .recent(user, "client", later + LISTING_WINDOW.as_millis() as i64)
                .is_some(),
            "the saved queue still holds the context"
        );
        listings.save(user, "client", Vec::new(), later);
        assert!(
            listings
                .recent(user, "client", later + LISTING_WINDOW.as_millis() as i64)
                .is_none(),
            "clearing the queue lets the stale listing go"
        );
    }

    #[test]
    fn a_saved_queue_that_holds_the_track_wins_over_the_listing() {
        let queued = listing(4);
        let listed = listing(4);
        let lookahead = 2;

        // The queue holds the track, so its next items are the upcoming set.
        let upcoming = upcoming_from(Some(&queued), Some(&listed), &queued[1], lookahead).unwrap();
        assert_eq!(upcoming, vec![queued[2].clone(), queued[3].clone()]);

        // A queue that does not hold the track leaves the listing to answer.
        let upcoming = upcoming_from(Some(&queued), Some(&listed), &listed[0], lookahead).unwrap();
        assert_eq!(upcoming, vec![listed[1].clone(), listed[2].clone()]);

        // Neither source holding the track yields nothing to prefetch.
        assert!(upcoming_from(Some(&queued), Some(&listed), &track(), lookahead).is_none());
        assert!(upcoming_from(None, None, &queued[0], lookahead).is_none());

        // The last item of a queue has nothing after it.
        assert_eq!(
            upcoming_from(Some(&queued), None, &queued[3], lookahead).unwrap(),
            Vec::<ClientId>::new()
        );
    }

    #[test]
    fn a_later_listing_replaces_the_context_but_keeps_its_tracked_acquisitions() {
        let mut listings = Listings::default();
        let user = Uuid::now_v7();
        let first = track();
        listings.record(user, "client", vec![first.clone()], 1_000);

        let acquisition = Uuid::now_v7();
        listings
            .contexts
            .get_mut(&user)
            .and_then(|contexts| contexts.get_mut("client"))
            .unwrap()
            .prefetched
            .insert(first.clone(), acquisition);

        let second = track();
        listings.record(user, "client", vec![second.clone()], 2_000);
        let context = listings.recent(user, "client", 2_000).unwrap();
        assert_eq!(context.listing, vec![second]);
        assert_eq!(context.prefetched.get(&first), Some(&acquisition));

        listings.forget(user, "client", &first);
        assert!(
            listings
                .recent(user, "client", 2_000)
                .unwrap()
                .prefetched
                .is_empty()
        );
    }

    #[test]
    fn an_expired_listing_is_dropped() {
        let mut listings = Listings::default();
        let user = Uuid::now_v7();
        listings.record(user, "client", listing(3), 1_000);
        assert!(listings.recent(user, "client", 1_000).is_some());
        assert!(
            listings
                .recent(user, "client", 1_000 + LISTING_WINDOW.as_millis() as i64)
                .is_none()
        );
        assert_eq!(listings.count(), 0);
    }

    #[test]
    fn the_store_drops_the_oldest_context_when_it_is_full() {
        let mut listings = Listings::default();
        for index in 0..=MAX_CONTEXTS {
            let user = Uuid::from_u128(index as u128 + 1);
            listings.record(user, "client", listing(1), index as i64);
        }
        assert_eq!(listings.count(), MAX_CONTEXTS);
        assert!(
            listings
                .recent(Uuid::from_u128(1), "client", MAX_CONTEXTS as i64)
                .is_none(),
            "the oldest context is dropped"
        );
        assert!(
            listings
                .recent(
                    Uuid::from_u128(MAX_CONTEXTS as u128 + 1),
                    "client",
                    MAX_CONTEXTS as i64
                )
                .is_some(),
            "the newest context is kept"
        );
    }

    #[test]
    fn contexts_are_separate_per_user_and_client() {
        let mut listings = Listings::default();
        let user = Uuid::now_v7();
        let other = Uuid::now_v7();
        listings.record(user, "one", listing(2), 1_000);
        listings.record(user, "two", listing(3), 1_000);
        listings.record(other, "one", listing(4), 1_000);
        assert_eq!(
            listings.recent(user, "one", 1_000).unwrap().listing.len(),
            2
        );
        assert_eq!(
            listings.recent(user, "two", 1_000).unwrap().listing.len(),
            3
        );
        assert_eq!(
            listings.recent(other, "one", 1_000).unwrap().listing.len(),
            4
        );
        assert_eq!(listings.count(), 3);
    }
}
