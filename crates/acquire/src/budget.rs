//! The per-instance budgets the pipeline respects.
//!
//! An administrator configures, per provider instance, how much of that
//! backend's capacity the server may use: `maxConcurrentSearches` and
//! `searchesPerHour` for searches, and `maxConcurrent` for downloads. This
//! module turns those numbers into the objects that enforce them.
//!
//! The budgets are built lazily, on the first search or download that names an
//! instance, and follow the instance's configuration from then on, so a rebuild
//! of the provider registry - which is what an administrator's change causes -
//! is picked up without a restart. A search budget is rebuilt when its numbers
//! change; the download slots keep the permits they already handed out and only
//! change their limit, so raising or lowering `maxConcurrent` cannot let more
//! downloads run than the new limit allows. Each budget belongs to one instance
//! id and is shared by every clone of the orchestrator.
//!
//! The download side also carries the in-flight registry: the orchestrator
//! records which acquisition is downloading from which instance, at which
//! priority, so a `play` request that finds every download slot of an instance
//! busy can cancel the least urgent bulk download and take its place.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use hearthsound_core::Priority;
use hearthsound_plugin_host::RegistryInstance;
use tokio::sync::{Notify, Semaphore};
use uuid::Uuid;

/// How many searches an instance may start back to back before its hourly
/// budget paces them.
///
/// The burst exists so an interactive search never queues behind a handful of
/// background ones; the design fixes it at ten.
pub const SEARCH_BURST: u32 = 10;

/// One instance's search budget.
///
/// The semaphore bounds how many searches run at once and the limiter bounds how
/// many start per hour. The limiter is only consulted for `queue`, `background`
/// and `upgrade` searches; a `play` or `next` search is limited by the
/// semaphore alone, because a user is waiting and the account's search rate is
/// protected by throttling the bulk work instead.
#[derive(Clone)]
pub(crate) struct SearchBudget {
    /// How many searches may run at once.
    pub(crate) searches: Arc<Semaphore>,
    /// How many searches may start per hour, with a burst of [`SEARCH_BURST`].
    pub(crate) hourly: Arc<DefaultDirectRateLimiter>,
    /// How many searches run at once, as configured.
    max_concurrent_searches: u32,
    /// How many searches start per hour, as configured.
    searches_per_hour: u32,
}

impl SearchBudget {
    /// Builds the search budget of an instance.
    ///
    /// # Parameters
    ///
    /// - `max_concurrent_searches`: how many searches may run at once; zero is
    ///   treated as one, because a budget that admits nothing could never
    ///   recover.
    /// - `searches_per_hour`: how many searches may start per hour; zero is
    ///   treated as one.
    fn new(max_concurrent_searches: u32, searches_per_hour: u32) -> SearchBudget {
        let permits = max_concurrent_searches.max(1) as usize;
        let per_hour = NonZeroU32::new(searches_per_hour.max(1)).unwrap_or(NonZeroU32::MIN);
        let burst =
            NonZeroU32::new(SEARCH_BURST.min(searches_per_hour.max(1))).unwrap_or(NonZeroU32::MIN);
        SearchBudget {
            searches: Arc::new(Semaphore::new(permits)),
            hourly: Arc::new(RateLimiter::direct(
                Quota::per_hour(per_hour).allow_burst(burst),
            )),
            max_concurrent_searches,
            searches_per_hour,
        }
    }

    /// Returns whether the budget was built from these settings.
    fn matches(&self, max_concurrent_searches: u32, searches_per_hour: u32) -> bool {
        self.max_concurrent_searches == max_concurrent_searches
            && self.searches_per_hour == searches_per_hour
    }
}

/// One download that is in flight on an instance.
///
/// The record is what preemption reads: a `play` request that needs a slot
/// cancels the least urgent in-flight download whose priority is `background` or
/// `upgrade`, and the download's own task is woken through its
/// [`Notify`](tokio::sync::Notify) so it releases the slot immediately instead
/// of waiting for its next poll.
struct InFlightDownload {
    /// The acquisition whose download is in flight.
    acquisition_id: Uuid,
    /// The instance the download runs on.
    instance_id: Uuid,
    /// The acquisition's priority.
    priority: Priority,
    /// Whether a `play` request already preempted this attempt.
    ///
    /// A victim stays registered until its task notices the preemption and
    /// unregisters, so a second interactive request must be able to see that it
    /// is already paying for a slot and pick another download. The mark is
    /// cleared by registering the attempt again, which is what the retry does.
    preempted: AtomicBool,
    /// Wakes the download's task when a `play` request preempts it.
    preempted_signal: Arc<Notify>,
}

/// One instance's download slots.
///
/// The limit is read on every use rather than baked into a semaphore, so an
/// administrator raising or lowering `maxConcurrent` takes effect at once: a
/// raise wakes the waiters, and a lower limit simply stops handing out permits
/// until enough attempts have ended. A permit is a count of one download, so
/// the permits already taken keep counting against the new limit.
pub(crate) struct InstanceSlots {
    /// How many downloads may run at once, as configured.
    limit: Mutex<u32>,
    /// How many downloads are running.
    in_use: Mutex<u32>,
    /// Wakes the waiters when a slot frees up or the limit rises.
    freed: Notify,
}

impl InstanceSlots {
    /// Builds the slots of an instance.
    ///
    /// # Parameters
    ///
    /// - `limit`: how many downloads may run at once; zero is treated as one,
    ///   because a budget that admits nothing could never recover.
    fn new(limit: u32) -> Arc<InstanceSlots> {
        Arc::new(InstanceSlots {
            limit: Mutex::new(limit.max(1)),
            in_use: Mutex::new(0),
            freed: Notify::new(),
        })
    }

    /// Records a new configured limit.
    ///
    /// A raise has spare capacity straight away, so every waiter is woken to
    /// look; a lower limit needs no wake-up, because the permits already taken
    /// stop new ones until they are given back.
    ///
    /// # Parameters
    ///
    /// - `limit`: the configured `maxConcurrent`.
    fn set_limit(&self, limit: u32) {
        let limit = limit.max(1);
        let previous = std::mem::replace(&mut *lock(&self.limit), limit);
        if limit > previous {
            self.freed.notify_waiters();
        }
    }

    /// Takes a slot when one is free.
    ///
    /// # Returns
    ///
    /// The permit, which the caller holds for the whole attempt, or `None` when
    /// the instance is at its limit.
    pub(crate) fn try_acquire(self: &Arc<Self>) -> Option<SlotPermit> {
        let limit = *lock(&self.limit);
        let mut in_use = lock(&self.in_use);
        if *in_use >= limit {
            return None;
        }
        *in_use += 1;
        Some(SlotPermit {
            slots: Arc::clone(self),
        })
    }

    /// Waits for a free slot.
    ///
    /// # Returns
    ///
    /// The permit, once a slot was taken.
    pub(crate) async fn acquire(self: &Arc<Self>) -> SlotPermit {
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            // Registering before the attempt is what closes the window in which
            // a release would be lost between the check and the wait.
            freed.as_mut().enable();
            if let Some(permit) = self.try_acquire() {
                return permit;
            }
            freed.await;
        }
    }
}

/// One download's hold on an instance's slots.
///
/// Dropping the permit gives the slot back and wakes one waiter, which is what
/// lets a preempted or finished attempt hand its place to the next one.
pub(crate) struct SlotPermit {
    /// The slots the permit was taken from.
    slots: Arc<InstanceSlots>,
}

impl Drop for SlotPermit {
    /// Gives the slot back.
    fn drop(&mut self) {
        let mut in_use = lock(&self.slots.in_use);
        *in_use = in_use.saturating_sub(1);
        drop(in_use);
        self.slots.freed.notify_one();
    }
}

/// The download budget of every instance, with the in-flight registry.
#[derive(Default)]
pub(crate) struct DownloadBudgets {
    /// The download slots of each instance, keyed by instance id.
    slots: Mutex<HashMap<Uuid, Arc<InstanceSlots>>>,
    /// The downloads in flight, keyed by acquisition id.
    in_flight: Mutex<HashMap<Uuid, InFlightDownload>>,
}

impl DownloadBudgets {
    /// Builds an empty budget map.
    pub(crate) fn new() -> DownloadBudgets {
        DownloadBudgets::default()
    }

    /// Returns the download slots of an instance.
    ///
    /// The slots are built on first use and their limit is updated from the
    /// instance's `maxConcurrent` on every call, so a configuration change takes
    /// effect without a restart and without losing the permits already held.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance whose budget is wanted.
    pub(crate) fn slots(&self, instance: &RegistryInstance) -> Arc<InstanceSlots> {
        let mut slots = lock(&self.slots);
        let entry = slots
            .entry(instance.id())
            .or_insert_with(|| InstanceSlots::new(instance.host_config().max_concurrent));
        entry.set_limit(instance.host_config().max_concurrent);
        Arc::clone(entry)
    }

    /// Records a download that is starting.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose download is in flight.
    /// - `instance_id`: the instance serving it.
    /// - `priority`: the acquisition's priority.
    ///
    /// # Returns
    ///
    /// The notifier the download's task waits on to learn that a `play` request
    /// took its slot.
    pub(crate) fn register(
        &self,
        acquisition_id: Uuid,
        instance_id: Uuid,
        priority: Priority,
    ) -> Arc<Notify> {
        let preempted = Arc::new(Notify::new());
        lock(&self.in_flight).insert(
            acquisition_id,
            InFlightDownload {
                acquisition_id,
                instance_id,
                priority,
                preempted: AtomicBool::new(false),
                preempted_signal: Arc::clone(&preempted),
            },
        );
        preempted
    }

    /// Forgets a download that ended.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition whose download ended.
    pub(crate) fn unregister(&self, acquisition_id: Uuid) {
        lock(&self.in_flight).remove(&acquisition_id);
    }

    /// Preempts the least urgent bulk download of an instance.
    ///
    /// Only a `play` request preempts, and only a download whose priority is
    /// `background` or `upgrade` can be preempted: an interactive or queued
    /// download is never cancelled for another caller.
    ///
    /// Each in-flight download is preempted at most once per attempt. A victim
    /// keeps its registration until its task notices the preemption, so without
    /// the mark two interactive requests arriving together would both pick the
    /// same victim and the other bulk download would keep its slot. Registering
    /// the attempt again - which the retry does - clears the mark.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the instance whose downloads are in flight.
    /// - `requester_priority`: the priority of the request that needs the slot.
    ///
    /// # Returns
    ///
    /// The acquisition whose download was preempted, or `None` when nothing
    /// could be preempted.
    pub(crate) fn preempt(&self, instance_id: Uuid, requester_priority: Priority) -> Option<Uuid> {
        if requester_priority != Priority::Play {
            return None;
        }
        let in_flight = lock(&self.in_flight);
        let victim = in_flight
            .values()
            .filter(|download| {
                download.instance_id == instance_id
                    && download.priority >= Priority::Background
                    && !download.preempted.load(Ordering::Acquire)
            })
            .max_by_key(|download| download.priority)
            .map(|download| {
                download.preempted.store(true, Ordering::Release);
                (
                    download.acquisition_id,
                    Arc::clone(&download.preempted_signal),
                )
            })?;
        drop(in_flight);
        // `notify_one` stores a permit when nobody is waiting yet, so the wake-up
        // cannot be lost between the registration and the download's next poll.
        victim.1.notify_one();
        Some(victim.0)
    }
}

/// The budgets of every instance the pipeline has used.
#[derive(Default)]
pub(crate) struct InstanceBudgets {
    /// The search budget of each instance, keyed by instance id.
    searches: Mutex<HashMap<Uuid, SearchBudget>>,
}

impl InstanceBudgets {
    /// Builds an empty budget map.
    pub(crate) fn new() -> InstanceBudgets {
        InstanceBudgets::default()
    }

    /// Returns the search budget of an instance.
    ///
    /// The budget is built on first use and rebuilt when the instance's
    /// `maxConcurrentSearches` or `searchesPerHour` changed, so the budget
    /// always reflects the configuration the registry currently serves. A
    /// rebuild replaces the budget, which resets the hourly tokens of that
    /// instance; that is deliberate, because the limit the administrator just
    /// chose is the one that should take effect.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance whose budget is wanted.
    pub(crate) fn search_budget(&self, instance: &RegistryInstance) -> SearchBudget {
        let config = instance.host_config();
        let mut budgets = lock(&self.searches);
        if let Some(budget) = budgets.get(&instance.id())
            && budget.matches(config.max_concurrent_searches, config.searches_per_hour)
        {
            return budget.clone();
        }
        let budget = SearchBudget::new(config.max_concurrent_searches, config.searches_per_hour);
        budgets.insert(instance.id(), budget.clone());
        budget
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// The state behind the lock is a map of budgets and in-flight downloads, which
/// stays consistent even if a task panicked while holding the lock, so poisoning
/// is not a reason to stop the server.
///
/// # Panics
///
/// Does not panic; the underlying lock's poisoning is ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
