//! The wasmtime engine every plugin instance runs on.

use std::time::Duration;

use tokio::time::MissedTickBehavior;
use wasmtime::{Config, Engine, EngineWeak};

use crate::error::HostError;

/// How often the engine's epoch advances.
///
/// Every guest store sets an epoch deadline of one, so a plugin call that keeps
/// running is interrupted about this often.
const TICK: Duration = Duration::from_millis(10);

/// The engine plugin components are compiled and instantiated with.
///
/// The engine owns an epoch ticker task. The task holds only a weak reference
/// to the engine, so dropping the [`PluginEngine`] stops the ticker instead of
/// keeping the engine alive forever.
#[derive(Debug, Clone)]
pub struct PluginEngine {
    engine: Engine,
}

impl PluginEngine {
    /// Creates an engine and starts its epoch ticker.
    ///
    /// # Returns
    ///
    /// The engine.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Engine`] when wasmtime rejects the configuration.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, because the ticker task is
    /// spawned immediately.
    pub fn new() -> Result<PluginEngine, HostError> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(HostError::Engine)?;

        let weak: EngineWeak = engine.weak();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                match weak.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => break,
                }
            }
        });

        Ok(PluginEngine { engine })
    }

    /// Returns the underlying wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}
