//! `hearthsound serve`: run the server until a termination signal arrives.

use std::collections::BTreeMap;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_server::{Server, Services};

/// Runs the server.
///
/// The composition root creates the directory tree, opens and migrates the
/// database, and builds every service; the server then serves until `SIGINT` or
/// `SIGTERM` arrives, at which point the listener and the background workers
/// stop in an orderly way.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name.
///
/// # Errors
///
/// Returns an error when a service cannot be built or the server cannot bind or
/// stop cleanly.
pub async fn run(config: BootstrapConfig, env: &BTreeMap<String, String>) -> anyhow::Result<()> {
    let services = Services::build(config, env)
        .await
        .context("could not compose the server services")?;
    Server::run(services)
        .await
        .context("the server stopped with an error")?;
    println!("stopped");
    Ok(())
}
