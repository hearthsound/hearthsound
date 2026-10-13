//! The server composition root.
//!
//! This crate builds the process the rest of the workspace serves: it opens the
//! database, applies migrations, loads the bootstrap and runtime configuration,
//! loads or creates the secret key, and constructs every long-lived service
//! (settings, authentication, the plugin host and provider registry, the ffmpeg
//! runner, the library scanner, the job runner and scheduler, and activity).
//! [`Services`] is that set of services; [`Server`] adds the HTTP listener, the
//! background workers, and shutdown.
//!
//! Mounted surfaces are `/rest` (the Subsonic router), `/healthz` and `/readyz`
//! (liveness and readiness), and `GET /` (a JSON status object until the web
//! client lands). [`hearthsound_api_subsonic`] owns `/rest`; this crate only
//! wires it to the services and adds the request-tracing layer.
//!
//! While it runs, the server watches `PRAGMA data_version` for commits made by
//! another connection, because a command-line command writes the database
//! directly rather than talking to the running process. A detected commit
//! reloads the runtime settings, rebuilds the provider registry from the plugin
//! tables, and reapplies the settings the services cache, so a command-line
//! change takes effect within about a second without a restart.
//!
//! # Where later wiring goes
//!
//! [`Services::build`] is the one place every service is constructed, so the
//! catalog service, the acquisition orchestrator, and the stream service are
//! added there, between the provider registry and the job runner, and their job
//! handlers are registered in the same function. The [`Server`] starts whatever
//! background work the services expose, so a new worker is added by spawning it
//! next to the job runner rather than by a second composition root.
//!
//! # Examples
//!
//! ```no_run
//! use std::collections::BTreeMap;
//!
//! use hearthsound_config::bootstrap::BootstrapConfig;
//! use hearthsound_server::{Server, Services};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = BootstrapConfig::load_process_env(Some(std::path::Path::new("hearthsound.toml")))?;
//! let env = hearthsound_config::bootstrap::process_env();
//! let services = Services::build(config, &env).await?;
//! Server::run(services).await?;
//! # Ok(())
//! # }
//! ```
#![forbid(unsafe_code)]

mod error;
pub mod http;
pub mod jobs;
pub mod paths;
mod reload;
mod server;
mod services;

pub use error::ServerError;
pub use server::Server;
pub use services::Services;
