//! The Hearthsound command line.
//!
//! The binary is a thin wrapper: each command resolves the bootstrap
//! configuration, opens what it needs, and calls into `hearthsound-server` or
//! the domain crate that owns the work. The admin commands write the database
//! directly rather than talking to a running server, which is why the server
//! watches for commits made by another connection and reloads what changed.
//!
//! `--config` is the only way to name a configuration file; every bootstrap key
//! can also be supplied as a `HEARTHSOUND__SECTION__KEY` environment variable,
//! which wins over the file exactly as it does for the server.
//!
//! `--version` and `--help` are answered before anything is opened, so they work
//! on a machine where the data directory does not exist yet.
#![forbid(unsafe_code)]

mod cli;
mod commands;
mod context;
mod logging;

use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser;
use hearthsound_config::bootstrap::{BootstrapConfig, process_env};

use crate::cli::{Cli, Command};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Resolves the configuration, installs logging, and runs the command.
///
/// # Errors
///
/// Returns the error of the step that failed, with the context of what was
/// being done: loading the configuration and its environment, installing the
/// log subscriber, or the command itself.
async fn run(cli: Cli) -> anyhow::Result<()> {
    let env = process_env();
    let config = BootstrapConfig::load(cli.config.as_deref(), &env)
        .context("could not load the bootstrap configuration")?;
    logging::init(&config.log).context("could not install the log subscriber")?;
    tracing::debug!(
        data = %config.paths.data.display(),
        database = %config.database.path.display(),
        "resolved the bootstrap configuration"
    );

    match cli.command {
        Command::Serve => commands::serve::run(config, &env).await,
        Command::Migrate => commands::migrate::run(config).await,
        Command::Scan(args) => commands::scan::run(config, args).await,
    }
}
