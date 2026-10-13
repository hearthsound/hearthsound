//! The per-instance budgets the pipeline respects.
//!
//! An administrator configures, per provider instance, how much of that
//! backend's capacity the server may use: `maxConcurrentSearches` and
//! `searchesPerHour` for searches, and `maxConcurrent` for downloads. This
//! module turns those numbers into the objects that enforce them.
//!
//! The budgets are built lazily, on the first search that names an instance, and
//! rebuilt when the instance's configuration changes, so a rebuild of the
//! provider registry - which is what an administrator's change causes - is
//! picked up without a restart. Each budget belongs to one instance id and is
//! shared by every clone of the orchestrator.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard};

use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use hearthsound_plugin_host::RegistryInstance;
use tokio::sync::Semaphore;
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
/// The state behind the lock is a map of budgets, which stays consistent even if
/// a task panicked while holding the lock, so poisoning is not a reason to stop
/// the server.
///
/// # Panics
///
/// Does not panic; the underlying lock's poisoning is ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
