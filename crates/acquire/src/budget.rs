//! The per-instance budgets the pipeline respects.
//!
//! An administrator configures, per provider instance, how much of that
//! backend's capacity the server may use: `maxConcurrentSearches` and
//! `searchesPerHour` for searches, and `maxConcurrent` for downloads. This
//! module turns those numbers into the objects that enforce them.
//!
//! The budgets are built lazily, on the first search or download that names an
//! instance, and rebuilt when the instance's configuration changes, so a rebuild
//! of the provider registry - which is what an administrator's change causes -
//! is picked up without a restart. Each budget belongs to one instance id and is
//! shared by every clone of the orchestrator.
//!
//! The download side also carries the in-flight registry: the orchestrator
//! records which acquisition is downloading from which instance, at which
//! priority, so a `play` request that finds every download slot of an instance
//! busy can cancel the least urgent bulk download and take its place.

use std::collections::HashMap;
use std::num::NonZeroU32;
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
    /// Wakes the download's task when a `play` request preempts it.
    preempted: Arc<Notify>,
}

/// The download budget of every instance, with the in-flight registry.
#[derive(Default)]
pub(crate) struct DownloadBudgets {
    /// The download semaphore of each instance, keyed by instance id.
    slots: Mutex<HashMap<Uuid, Arc<Semaphore>>>,
    /// The configuration each semaphore was built from, keyed by instance id.
    configured: Mutex<HashMap<Uuid, u32>>,
    /// The downloads in flight, keyed by acquisition id.
    in_flight: Mutex<HashMap<Uuid, InFlightDownload>>,
}

impl DownloadBudgets {
    /// Builds an empty budget map.
    pub(crate) fn new() -> DownloadBudgets {
        DownloadBudgets::default()
    }

    /// Returns the download semaphore of an instance.
    ///
    /// The semaphore is built on first use and rebuilt when the instance's
    /// `maxConcurrent` changed, so a configuration change takes effect without a
    /// restart.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance whose budget is wanted.
    pub(crate) fn slots(&self, instance: &RegistryInstance) -> Arc<Semaphore> {
        let max_concurrent = instance.host_config().max_concurrent.max(1);
        let configured = *lock(&self.configured).get(&instance.id()).unwrap_or(&0);
        if configured == max_concurrent
            && let Some(semaphore) = lock(&self.slots).get(&instance.id())
        {
            return Arc::clone(semaphore);
        }
        let semaphore = Arc::new(Semaphore::new(max_concurrent as usize));
        lock(&self.slots).insert(instance.id(), Arc::clone(&semaphore));
        lock(&self.configured).insert(instance.id(), max_concurrent);
        semaphore
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
                preempted: Arc::clone(&preempted),
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
        let victim = lock(&self.in_flight)
            .values()
            .filter(|download| {
                download.instance_id == instance_id && download.priority >= Priority::Background
            })
            .max_by_key(|download| download.priority)
            .map(|download| (download.acquisition_id, Arc::clone(&download.preempted)))?;
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
