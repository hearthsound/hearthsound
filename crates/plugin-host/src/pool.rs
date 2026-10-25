//! The pool of guest stores, with deadlines, trap accounting, and auto-disable.
//!
//! One plugin instance owns a small pool of instantiated stores. A call takes a
//! permit, borrows an idle store or instantiates a new one, runs the guest call
//! under the instance's deadline, and returns the store to the pool when the
//! guest answered. A trap or a timeout discards the store, because a store that
//! trapped may be in an unknown state, and enough traps inside a window disable
//! the instance entirely. Only the guest call is the plugin's own work: waiting
//! for a permit and instantiating a store are host-side, so neither counts
//! toward that limit and a busy instance is not switched off.
//!
//! A pool is generic over the [`World`] it drives, so every provider adapter
//! shares this one implementation while each still gets its own world handle
//! back from the store. Each hosted plugin API version contributes its own
//! `World` impls - the `v4` worlds keep their plain names and the `v3` worlds
//! carry a `3` suffix - so trap accounting, the permits and the deadlines are
//! the same code for both.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hearthsound_core::{ProviderError, Slug};
use tokio::sync::{Semaphore, SemaphorePermit};
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use crate::api::v3::bindings::{
    AccountLinkProvider as AccountLinkProvider3, AccountLinkProviderPre as AccountLinkProviderPre3,
    AcquisitionProvider as AcquisitionProvider3, AcquisitionProviderPre as AcquisitionProviderPre3,
    ArtworkProvider as ArtworkProvider3, ArtworkProviderPre as ArtworkProviderPre3,
    CatalogProvider as CatalogProvider3, CatalogProviderPre as CatalogProviderPre3,
    DiscoveryProvider as DiscoveryProvider3, DiscoveryProviderPre as DiscoveryProviderPre3,
    MetadataProvider as MetadataProvider3, MetadataProviderPre as MetadataProviderPre3,
    ScrobbleProvider as ScrobbleProvider3, ScrobbleProviderPre as ScrobbleProviderPre3,
    VerificationProvider as VerificationProvider3,
    VerificationProviderPre as VerificationProviderPre3,
};
use crate::api::v4::bindings::{
    AccountLinkProvider, AccountLinkProviderPre, AcquisitionProvider, AcquisitionProviderPre,
    ArtworkProvider, ArtworkProviderPre, CatalogProvider, CatalogProviderPre, DiscoveryProvider,
    DiscoveryProviderPre, MetadataProvider, MetadataProviderPre, ScrobbleProvider,
    ScrobbleProviderPre, VerificationProvider, VerificationProviderPre,
};
use crate::files::FileAccess;
use crate::http::HttpClient;
use crate::kv::KvStore;
use crate::log::LogSink;
use crate::process::ProcessTable;
use crate::repo::InstanceDisabler;
use crate::state::InstanceState;

/// How many traps inside [`TRAP_WINDOW`] disable an instance.
const TRAP_LIMIT: usize = 5;

/// The window traps are counted over.
const TRAP_WINDOW: Duration = Duration::from_secs(600);

/// A guest call borrowed from one leased store.
pub(crate) type CallFuture<'a, T, E> =
    Pin<Box<dyn Future<Output = wasmtime::Result<Result<T, E>>> + Send + 'a>>;

/// A provider world the host can drive.
///
/// Implementing this for a generated world struct is what lets [`StorePool`]
/// instantiate and hand back that world's handle without knowing which world it
/// is.
pub(crate) trait World: Send + Sync + 'static {
    /// The prepared instance type of the world.
    type Pre: Send + Sync + 'static;

    /// The world handle an instantiated store returns.
    type Instance: Send + Sync + 'static;

    /// Prepares `component` for instantiation against this world.
    ///
    /// # Errors
    ///
    /// Returns a wasmtime error when the component does not export this world.
    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error>;

    /// Instantiates the world inside `store`.
    ///
    /// # Errors
    ///
    /// Returns a wasmtime error when instantiation traps.
    fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> impl Future<Output = Result<Self::Instance, wasmtime::Error>> + Send;
}

impl World for CatalogProvider {
    type Pre = CatalogProviderPre<InstanceState>;
    type Instance = CatalogProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        CatalogProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for MetadataProvider {
    type Pre = MetadataProviderPre<InstanceState>;
    type Instance = MetadataProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        MetadataProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for ArtworkProvider {
    type Pre = ArtworkProviderPre<InstanceState>;
    type Instance = ArtworkProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        ArtworkProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for AcquisitionProvider {
    type Pre = AcquisitionProviderPre<InstanceState>;
    type Instance = AcquisitionProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        AcquisitionProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for VerificationProvider {
    type Pre = VerificationProviderPre<InstanceState>;
    type Instance = VerificationProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        VerificationProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for ScrobbleProvider {
    type Pre = ScrobbleProviderPre<InstanceState>;
    type Instance = ScrobbleProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        ScrobbleProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for DiscoveryProvider {
    type Pre = DiscoveryProviderPre<InstanceState>;
    type Instance = DiscoveryProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        DiscoveryProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for AccountLinkProvider {
    type Pre = AccountLinkProviderPre<InstanceState>;
    type Instance = AccountLinkProvider;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        AccountLinkProviderPre::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for CatalogProvider3 {
    type Pre = CatalogProviderPre3<InstanceState>;
    type Instance = CatalogProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        CatalogProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for MetadataProvider3 {
    type Pre = MetadataProviderPre3<InstanceState>;
    type Instance = MetadataProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        MetadataProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for ArtworkProvider3 {
    type Pre = ArtworkProviderPre3<InstanceState>;
    type Instance = ArtworkProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        ArtworkProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for AcquisitionProvider3 {
    type Pre = AcquisitionProviderPre3<InstanceState>;
    type Instance = AcquisitionProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        AcquisitionProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for VerificationProvider3 {
    type Pre = VerificationProviderPre3<InstanceState>;
    type Instance = VerificationProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        VerificationProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for ScrobbleProvider3 {
    type Pre = ScrobbleProviderPre3<InstanceState>;
    type Instance = ScrobbleProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        ScrobbleProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for DiscoveryProvider3 {
    type Pre = DiscoveryProviderPre3<InstanceState>;
    type Instance = DiscoveryProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        DiscoveryProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

impl World for AccountLinkProvider3 {
    type Pre = AccountLinkProviderPre3<InstanceState>;
    type Instance = AccountLinkProvider3;

    fn prepare(
        linker: &Linker<InstanceState>,
        component: &Component,
    ) -> Result<Self::Pre, wasmtime::Error> {
        AccountLinkProviderPre3::new(linker.instantiate_pre(component)?)
    }

    async fn instantiate(
        pre: &Self::Pre,
        store: &mut Store<InstanceState>,
    ) -> Result<Self::Instance, wasmtime::Error> {
        pre.instantiate_async(store).await
    }
}

/// The admission state of one pool: its permits, its trap window and its call
/// deadline.
///
/// Admission and the guest call have separate budgets. Waiting for a permit and
/// failing to instantiate a store are host-side conditions rather than plugin
/// misbehaviour, so neither reaches [`TRAP_LIMIT`]: a merely busy plugin keeps
/// the full deadline for its own call instead of being switched off.
struct CallGate {
    /// The stores the pool may lease at once.
    permits: Semaphore,
    /// The traps that count toward [`TRAP_LIMIT`].
    traps: TrapWindow,
    /// The deadline of one call, reused as the bound on the queue wait.
    call_timeout: Duration,
}

impl CallGate {
    /// Builds the gate of one pool.
    ///
    /// # Parameters
    ///
    /// - `max_instances`: how many stores the pool may lease at once.
    /// - `call_timeout`: the deadline of one call, also the bound on the queue
    ///   wait.
    fn new(max_instances: u8, call_timeout: Duration) -> CallGate {
        CallGate {
            permits: Semaphore::new(usize::from(max_instances)),
            traps: TrapWindow::new(),
            call_timeout,
        }
    }

    /// Returns whether the instance has been disabled after repeated traps.
    fn is_disabled(&self) -> bool {
        self.traps.is_disabled()
    }

    /// Waits for a permit to lease one store, bounded by the call deadline.
    ///
    /// Queueing behind other calls is not the plugin's fault: a plugin that is
    /// already serving as many calls as its pool allows is working, not
    /// trapping. This wait therefore never records a trap and never counts
    /// toward [`TRAP_LIMIT`]; it only reports the instance as unavailable for
    /// this attempt.
    ///
    /// # Returns
    ///
    /// The permit, held for as long as the caller leases a store. Dropping it
    /// admits the next waiting call.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unavailable`] when the wait exceeded the call
    /// deadline or the pool was closed.
    async fn enter(&self) -> Result<SemaphorePermit<'_>, ProviderError> {
        match tokio::time::timeout(self.call_timeout, self.permits.acquire()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_)) => {
                // A closed pool means the host is shutting the instance down.
                // This gate carries no plugin or instance identifiers, so the
                // warning deliberately names neither.
                tracing::warn!(
                    target: "hearthsound::plugin",
                    "the instance pool closed while a call waited"
                );
                Err(ProviderError::Unavailable)
            }
            Err(_) => {
                // Waiting is host-side queueing, not a trap: no trap is
                // recorded and the trap window stays untouched.
                tracing::warn!(target: "hearthsound::plugin", "the instance stayed busy");
                Err(ProviderError::Unavailable)
            }
        }
    }

    /// Records a trap and reports whether it disabled the instance.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant the trap happened.
    ///
    /// # Returns
    ///
    /// `true` when this trap reached the limit and disabled the instance.
    fn record_trap(&self, now: Instant) -> bool {
        self.traps.record(now)
    }
}

/// Everything one instance's pool needs to build stores.
pub(crate) struct PoolConfig {
    /// The plugin id, used in log fields.
    pub plugin: String,
    /// The instance slug.
    pub slug: Slug,
    /// The server-scoped configuration, as JSON text.
    pub server_config: Arc<str>,
    /// The instance's log sink.
    pub log: Arc<LogSink>,
    /// The instance's outbound HTTP client.
    pub http: Arc<HttpClient>,
    /// The instance's persistent key-value store.
    pub kv: Arc<KvStore>,
    /// The import roots the instance grants.
    pub files: Arc<FileAccess>,
    /// The instance's process table, shared by every store it hands out.
    pub process: Arc<ProcessTable>,
    /// The instance's memory ceiling in megabytes.
    pub memory_mb: u32,
    /// How many stores may exist at once.
    pub max_instances: u8,
    /// The deadline of one call in milliseconds.
    pub call_timeout_ms: u32,
    /// What to write when the instance is disabled after repeated traps.
    pub auto_disable: InstanceDisabler,
}

/// The pool of stores for one plugin instance.
pub(crate) struct StorePool<W: World> {
    inner: Arc<PoolInner<W>>,
}

/// The shared state of a pool.
struct PoolInner<W: World> {
    engine: Engine,
    pre: W::Pre,
    gate: CallGate,
    idle: Mutex<Vec<Slot<W>>>,
    plugin: String,
    slug: Slug,
    server_config: Arc<str>,
    log: Arc<LogSink>,
    http: Arc<HttpClient>,
    kv: Arc<KvStore>,
    files: Arc<FileAccess>,
    process: Arc<ProcessTable>,
    memory_mb: u32,
    auto_disable: InstanceDisabler,
}

/// One instantiated store and its world handle.
type Slot<W> = (Store<InstanceState>, <W as World>::Instance);

impl<W: World> StorePool<W> {
    /// Builds a pool for one instance.
    ///
    /// # Parameters
    ///
    /// - `engine`: the engine the component was compiled with.
    /// - `linker`: the host linker, used to type-check the component.
    /// - `component`: the installed component.
    /// - `config`: the instance's configuration.
    ///
    /// # Errors
    ///
    /// Returns a wasmtime error when the component cannot be prepared for
    /// instantiation.
    pub(crate) fn new(
        engine: &Engine,
        linker: &Linker<InstanceState>,
        component: &Component,
        config: PoolConfig,
    ) -> Result<StorePool<W>, wasmtime::Error> {
        let pre = W::prepare(linker, component)?;

        Ok(StorePool {
            inner: Arc::new(PoolInner {
                engine: engine.clone(),
                pre,
                gate: CallGate::new(
                    config.max_instances,
                    Duration::from_millis(u64::from(config.call_timeout_ms)),
                ),
                idle: Mutex::new(Vec::new()),
                plugin: config.plugin,
                slug: config.slug,
                server_config: config.server_config,
                log: config.log,
                http: config.http,
                kv: config.kv,
                files: config.files,
                process: config.process,
                memory_mb: config.memory_mb,
                auto_disable: config.auto_disable,
            }),
        })
    }

    /// Runs one guest call under the instance's deadline.
    ///
    /// Admission happens before the deadline starts. Waiting for a permit and
    /// instantiating a store are host-side work, so neither is the plugin's
    /// fault and neither counts toward [`TRAP_LIMIT`]; only a guest trap or a
    /// guest call that runs past the deadline does. The guest call therefore
    /// gets its whole budget from the moment it starts, however long the
    /// queueing took.
    ///
    /// # Parameters
    ///
    /// - `call`: a function producing the guest call future from a leased store
    ///   and its world handle.
    ///
    /// # Returns
    ///
    /// `Ok(Ok(value))` when the guest answered, `Ok(Err(error))` when the guest
    /// reported its own error, and `Err` when the instance is disabled, stayed
    /// busy past the queue deadline, could not instantiate a store, trapped, or
    /// ran past the deadline.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unavailable`] when the instance has been
    /// disabled, stayed busy past the queue deadline, or could not instantiate a
    /// store, and [`ProviderError::Internal`] on a trap or timeout.
    pub(crate) async fn call<T, E, F>(&self, call: F) -> Result<Result<T, E>, ProviderError>
    where
        F: for<'a> FnOnce(&'a mut Store<InstanceState>, &'a W::Instance) -> CallFuture<'a, T, E>,
    {
        let inner = &self.inner;
        if inner.gate.is_disabled() {
            return Err(ProviderError::Unavailable);
        }

        // The permit bounds leased stores, not the guest call: it is released
        // only once the store is back in the idle list, so the pool never holds
        // more stores than `max_instances`.
        let permit = inner.gate.enter().await?;

        let (mut store, instance) = match self.take_or_instantiate().await {
            Ok(slot) => slot,
            Err(error) => {
                tracing::error!(
                    target: "hearthsound::plugin",
                    plugin = %inner.plugin,
                    instance = %inner.slug,
                    error = %format!("{error:#}"),
                    "plugin instance could not be instantiated"
                );
                drop(permit);
                return Err(ProviderError::Unavailable);
            }
        };

        let outcome = tokio::time::timeout(inner.gate.call_timeout, async {
            store
                .data_mut()
                .begin_call(Instant::now() + inner.gate.call_timeout);
            call(&mut store, &instance).await
        })
        .await;

        match outcome {
            Ok(Ok(Ok(value))) => {
                inner.return_idle(store, instance);
                drop(permit);
                Ok(Ok(value))
            }
            Ok(Ok(Err(error))) => {
                inner.return_idle(store, instance);
                drop(permit);
                Ok(Err(error))
            }
            Ok(Err(error)) => Err(inner.record_trap(&format!("{error:#}"))),
            Err(_) => Err(inner.record_trap("the call exceeded its deadline")),
        }
    }

    /// Takes an idle store or instantiates a new one.
    async fn take_or_instantiate(&self) -> Result<Slot<W>, wasmtime::Error> {
        if let Some(slot) = lock(&self.inner.idle).pop() {
            return Ok(slot);
        }
        self.instantiate().await
    }

    /// Instantiates a fresh store for the component.
    async fn instantiate(&self) -> Result<Slot<W>, wasmtime::Error> {
        let inner = &self.inner;
        let mut store = Store::new(
            &inner.engine,
            InstanceState::new(
                inner.slug.clone(),
                Arc::clone(&inner.server_config),
                Arc::clone(&inner.log),
                inner.memory_mb,
                crate::state::InstanceServices {
                    http: Arc::clone(&inner.http),
                    kv: Arc::clone(&inner.kv),
                    files: Arc::clone(&inner.files),
                    process: Arc::clone(&inner.process),
                },
            ),
        );
        store.limiter(|state| state.limits());
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);

        let instance = W::instantiate(&inner.pre, &mut store).await?;
        Ok((store, instance))
    }
}

impl<W: World> PoolInner<W> {
    /// Returns a store to the idle list.
    fn return_idle(&self, store: Store<InstanceState>, instance: W::Instance) {
        lock(&self.idle).push((store, instance));
    }

    /// Records a trap and reports whether it disabled the instance.
    ///
    /// Reaching the trap limit disables the instance for this run and hands the
    /// persistence to a task: the flip is written to `provider_instances` so a
    /// restart does not bring the instance back, and a `plugin.status` event
    /// tells every connected client. Both happen off the call path, because the
    /// caller is already returning an error and must not wait on a database
    /// write.
    ///
    /// # Parameters
    ///
    /// - `reason`: a human readable description of the failure.
    ///
    /// # Returns
    ///
    /// [`ProviderError::Internal`], so callers can return it directly.
    fn record_trap(&self, reason: &str) -> ProviderError {
        tracing::error!(
            target: "hearthsound::plugin",
            plugin = %self.plugin,
            instance = %self.slug,
            error = reason,
            "plugin instance failed"
        );
        if self.gate.record_trap(Instant::now()) {
            tracing::error!(
                target: "hearthsound::plugin",
                plugin = %self.plugin,
                instance = %self.slug,
                "plugin instance disabled after repeated traps"
            );
            let disabler = self.auto_disable.clone();
            let detail = reason.to_owned();
            tokio::spawn(async move { disabler.disable(&detail).await });
        }
        ProviderError::Internal
    }
}

/// The trap counter and the disabled flag.
struct TrapWindow {
    traps: Mutex<VecDeque<Instant>>,
    disabled: AtomicBool,
}

impl TrapWindow {
    /// Creates an empty window.
    fn new() -> TrapWindow {
        TrapWindow {
            traps: Mutex::new(VecDeque::new()),
            disabled: AtomicBool::new(false),
        }
    }

    /// Returns whether the instance has been disabled.
    fn is_disabled(&self) -> bool {
        self.disabled.load(Ordering::SeqCst)
    }

    /// Records a trap.
    ///
    /// # Parameters
    ///
    /// - `now`: the instant the trap happened.
    ///
    /// # Returns
    ///
    /// `true` when this trap reached the limit and disabled the instance.
    fn record(&self, now: Instant) -> bool {
        let mut traps = lock(&self.traps);
        traps.push_back(now);
        while let Some(oldest) = traps.front().copied() {
            if now.duration_since(oldest) > TRAP_WINDOW {
                traps.pop_front();
            } else {
                break;
            }
        }
        if traps.len() >= TRAP_LIMIT {
            self.disabled.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
}

/// Locks a mutex, ignoring poisoning.
///
/// A poisoned lock means another thread panicked while holding it; the data is
/// still structurally sound, and refusing to run would turn a panic in one call
/// into a permanently unusable plugin.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Queueing behind other calls is the host's admission, not plugin
    /// misbehaviour, so a busy gate must never reach the trap limit.
    #[tokio::test]
    async fn queue_timeouts_do_not_count_as_traps() {
        let gate = CallGate::new(1, Duration::from_millis(20));
        let held = gate
            .permits
            .try_acquire()
            .expect("a fresh gate has its one permit free");

        for attempt in 0..=TRAP_LIMIT {
            let error = gate.enter().await.expect_err("the only permit is held");
            assert_eq!(error, ProviderError::Unavailable);
            assert!(
                !gate.is_disabled(),
                "queue timeout {attempt} must not disable the instance"
            );
        }

        drop(held);
        let _permit = gate.enter().await.expect("the released permit is free");
    }

    /// A guest trap still counts: the trap limit disables the instance.
    #[test]
    fn guest_traps_still_disable_the_instance() {
        let gate = CallGate::new(1, Duration::from_millis(20));
        let now = Instant::now();

        // The first `TRAP_LIMIT - 1` traps count but do not disable.
        for _ in 0..TRAP_LIMIT - 1 {
            assert!(!gate.record_trap(now));
            assert!(!gate.is_disabled());
        }
        assert!(gate.record_trap(now));
        assert!(gate.is_disabled());
    }
}
