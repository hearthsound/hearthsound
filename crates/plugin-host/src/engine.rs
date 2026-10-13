//! The wasmtime engine every plugin instance runs on.

use std::time::Duration;

use wasmtime::{Config, Engine, EngineWeak};

use crate::error::HostError;

/// How often the engine's epoch advances.
///
/// Every guest store sets an epoch deadline of one, so a plugin call that keeps
/// running is interrupted about this often.
const TICK: Duration = Duration::from_millis(10);

/// The engine plugin components are compiled and instantiated with.
///
/// The engine owns an epoch ticker thread. The ticker is an OS thread rather
/// than an async task so that it keeps running while a guest spins on a
/// single-threaded runtime; a spinning guest only yields once the epoch
/// advances. The thread holds only a weak reference to the engine, so dropping
/// every clone of the [`PluginEngine`] stops the ticker instead of keeping the
/// engine alive forever.
#[derive(Debug, Clone)]
pub struct PluginEngine {
    engine: Engine,
}

impl PluginEngine {
    /// Creates an engine and starts its epoch ticker thread.
    ///
    /// # Returns
    ///
    /// The engine.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Engine`] when wasmtime rejects the configuration
    /// or the ticker thread cannot be spawned.
    pub fn new() -> Result<PluginEngine, HostError> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(HostError::Engine)?;

        let weak: EngineWeak = engine.weak();
        std::thread::Builder::new()
            .name("hearthsound-epoch".to_owned())
            .spawn(move || {
                loop {
                    std::thread::sleep(TICK);
                    match weak.upgrade() {
                        Some(engine) => engine.increment_epoch(),
                        None => break,
                    }
                }
            })
            .map_err(|e| HostError::Engine(wasmtime::Error::new(e)))?;

        Ok(PluginEngine { engine })
    }

    /// Returns the underlying wasmtime engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}
