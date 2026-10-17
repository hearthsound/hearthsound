//! `hearthsound plugins`: install plugins and list what is installed.
//!
//! Installing records the manifest's requested permissions as the approved set,
//! which is the command-line administrator's consent; the host still checks the
//! approved set inside every import, so a plugin cannot use a permission the
//! administrator did not see. The permissions are printed with their risk labels
//! so the operator sees what was granted.

use std::collections::BTreeMap;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_plugin_host::{PluginRepository, PluginSource};

use crate::cli::{InstallPluginArgs, PluginsArgs, PluginsCommand};
use crate::context::Context;

/// Runs one `plugins` command.
///
/// # Parameters
///
/// - `config`: the resolved bootstrap configuration.
/// - `env`: the process environment, keyed by variable name.
/// - `args`: the subcommand to run.
///
/// # Errors
///
/// Returns an error when the context cannot be opened, the archive cannot be
/// installed, or the plugin tables cannot be written.
pub async fn run(
    config: BootstrapConfig,
    env: &BTreeMap<String, String>,
    args: PluginsArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        PluginsCommand::Install(args) => install(&context, args).await,
        PluginsCommand::List => list(&context).await,
    }
}

/// Installs an archive and enables the plugin.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the archive and the source to record it under.
///
/// # Errors
///
/// Returns an error when the archive cannot be installed (an unreadable archive,
/// an invalid manifest, a component that asks for something the host does not
/// offer, or a version that is already installed) or the row cannot be written.
async fn install(context: &Context, args: InstallPluginArgs) -> anyhow::Result<()> {
    let host = context.plugin_host()?;
    let installed = host
        .install(&args.file)
        .await
        .with_context(|| format!("could not install {}", args.file.display()))?;
    let source = if args.bundled {
        PluginSource::Bundled
    } else {
        PluginSource::Sideload
    };
    let registration = PluginRepository::new(context.database())
        .register_approved(&installed, source)
        .await
        .context("could not record the installed plugin")?;

    println!(
        "installed {} {} ({})",
        registration.id, registration.version, registration.source
    );
    println!("api: {}", registration.api);
    if registration.provides.is_empty() {
        println!("provides: nothing this server can drive");
    } else {
        let provides: Vec<&str> = registration
            .provides
            .iter()
            .map(|layer| layer.as_str())
            .collect();
        println!("provides: {}", provides.join(", "));
    }
    if registration.requested.is_empty() {
        println!("requests no permissions");
    } else {
        println!("approved permissions:");
        for permission in &registration.requested {
            println!(
                "  {:<10} {} ({})",
                permission.risk.as_str(),
                permission.detail,
                permission.scope
            );
        }
    }
    Ok(())
}

/// Lists the installed plugins.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Errors
///
/// Returns an error when the plugin table cannot be read.
async fn list(context: &Context) -> anyhow::Result<()> {
    let plugins = PluginRepository::new(context.database())
        .plugins()
        .await
        .context("could not list the installed plugins")?;

    if plugins.is_empty() {
        println!("no plugins are installed");
        return Ok(());
    }
    for plugin in &plugins {
        println!(
            "{} {} ({}{})",
            plugin.id,
            plugin.version,
            plugin.source,
            if plugin.enabled { "" } else { ", disabled" }
        );
        println!("  api: {}", plugin.api);
    }
    Ok(())
}
