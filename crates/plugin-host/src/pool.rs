//! The pool of guest stores, with deadlines, trap accounting, and auto-disable.
//!
//! One plugin instance owns a small pool of instantiated stores. A call takes a
//! permit, borrows an idle store or instantiates a new one, runs the guest call
//! under the instance's deadline, and returns the store to the pool when the
//! guest answered. A trap or a timeout discards the store, because a store that
//! trapped may be in an unknown state, and enough traps inside a window disable
//! the instance entirely.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hearthsound_core::{ProviderError, Slug};
use tokio::sync::Semaphore;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};

use crate::bindings::{CatalogProvider, CatalogProviderPre};
use crate::log::LogSink;
use crate::state::InstanceState;

/// How many traps inside [`TRAP_WINDOW`] disable an instance.
const TRAP_LIMIT: usize = 5;

/// The window traps are counted over.
const TRAP_WINDOW: Duration = Duration::from_secs(600);

/// A guest call borrowed from one leased store.
pub(crate) type CallFuture<'a, T, E> =
    Pin<Box<dyn Future<Output = wasmtime::Result<Result<T, E>>> + Send + 'a>>;

/// The result of one attempt, before trap accounting.
enum Attempt<T, E> {
    /// The guest answered with a value.
    Value(T),
    /// The guest answered with its own error.
    Guest(E),
    /// The call trapped, timed out, or could not be set up.
    Trap(String),
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
    /// The instance's memory ceiling in megabytes.
    pub memory_mb: u32,
    /// How many stores may exist at once.
    pub max_instances: u8,
    /// The deadline of one call in milliseconds.
    pub call_timeout_ms: u32,
}

/// The pool of stores for one plugin instance.
pub(crate) struct StorePool {
    inner: Arc<PoolInner>,
}

/// The shared state of a pool.
struct PoolInner {
    engine: Engine,
    pre: CatalogProviderPre<InstanceState>,
    permits: Semaphore,
    idle: Mutex<Vec<Slot>>,
    traps: TrapWindow,
    call_timeout: Duration,
    plugin: String,
    slug: Slug,
    server_config: Arc<str>,
    log: Arc<LogSink>,
    memory_mb: u32,
}

/// One instantiated store and its world handle.
type Slot = (Store<InstanceState>, CatalogProvider);

impl StorePool {
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
    ) -> Result<StorePool, wasmtime::Error> {
        let pre = linker.instantiate_pre(component)?;
        let pre = CatalogProviderPre::new(pre)?;

        Ok(StorePool {
            inner: Arc::new(PoolInner {
                engine: engine.clone(),
                pre,
                permits: Semaphore::new(usize::from(config.max_instances)),
                idle: Mutex::new(Vec::new()),
                traps: TrapWindow::new(),
                call_timeout: Duration::from_millis(u64::from(config.call_timeout_ms)),
                plugin: config.plugin,
                slug: config.slug,
                server_config: config.server_config,
                log: config.log,
                memory_mb: config.memory_mb,
            }),
        })
    }

    /// Runs one guest call under the instance's deadline.
    ///
    /// # Parameters
    ///
    /// - `call`: a function producing the guest call future from a leased store
    ///   and its world handle.
    ///
    /// # Returns
    ///
    /// `Ok(Ok(value))` when the guest answered, `Ok(Err(error))` when the guest
    /// reported its own error, and `Err` when the instance is disabled, the call
    /// trapped, or the deadline expired.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unavailable`] when the instance has been
    /// disabled, and [`ProviderError::Internal`] on a trap or timeout.
    pub(crate) async fn call<T, E, F>(&self, call: F) -> Result<Result<T, E>, ProviderError>
    where
        F: for<'a> FnOnce(
            &'a mut Store<InstanceState>,
            &'a CatalogProvider,
        ) -> CallFuture<'a, T, E>,
    {
        let inner = &self.inner;
        if inner.traps.is_disabled() {
            return Err(ProviderError::Unavailable);
        }

        let attempt = tokio::time::timeout(inner.call_timeout, async {
            let permit = match inner.permits.acquire().await {
                Ok(permit) => permit,
                Err(_) => return Attempt::Trap("the instance pool is closed".to_owned()),
            };

            let (mut store, instance) = match self.take_or_instantiate().await {
                Ok(slot) => slot,
                Err(error) => return Attempt::Trap(format!("{error:#}")),
            };

            match call(&mut store, &instance).await {
                Ok(Ok(value)) => {
                    drop(permit);
                    inner.return_idle(store, instance);
                    Attempt::Value(value)
                }
                Ok(Err(error)) => {
                    drop(permit);
                    inner.return_idle(store, instance);
                    Attempt::Guest(error)
                }
                Err(error) => Attempt::Trap(format!("{error:#}")),
            }
        })
        .await;

        match attempt {
            Ok(Attempt::Value(value)) => Ok(Ok(value)),
            Ok(Attempt::Guest(error)) => Ok(Err(error)),
            Ok(Attempt::Trap(reason)) => Err(inner.record_trap(&reason)),
            Err(_) => Err(inner.record_trap("the call exceeded its deadline")),
        }
    }

    /// Takes an idle store or instantiates a new one.
    async fn take_or_instantiate(&self) -> Result<Slot, wasmtime::Error> {
        if let Some(slot) = lock(&self.inner.idle).pop() {
            return Ok(slot);
        }
        self.instantiate().await
    }

    /// Instantiates a fresh store for the component.
    async fn instantiate(&self) -> Result<Slot, wasmtime::Error> {
        let inner = &self.inner;
        let mut store = Store::new(
            &inner.engine,
            InstanceState::new(
                inner.slug.clone(),
                Arc::clone(&inner.server_config),
                Arc::clone(&inner.log),
                inner.memory_mb,
            ),
        );
        store.limiter(|state| state.limits());
        store.set_epoch_deadline(1);
        store.epoch_deadline_async_yield_and_update(1);

        let instance = inner.pre.instantiate_async(&mut store).await?;
        Ok((store, instance))
    }
}

impl PoolInner {
    /// Returns a store to the idle list.
    fn return_idle(&self, store: Store<InstanceState>, instance: CatalogProvider) {
        lock(&self.idle).push((store, instance));
    }

    /// Records a trap and reports whether it disabled the instance.
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
        if self.traps.record(Instant::now()) {
            tracing::error!(
                target: "hearthsound::plugin",
                plugin = %self.plugin,
                instance = %self.slug,
                "plugin instance disabled after repeated traps"
            );
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
