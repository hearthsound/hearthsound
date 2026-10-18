//! `hearthsound plugins`: install plugins, manage their lifecycle, and list
//! them.
//!
//! Installing records the plugin as pending approval: the manifest's requested
//! permissions are what the administrator is being asked for, not what they
//! have granted. `approve` grants them, `enable` and `disable` switch an
//! approved plugin, `decline-upgrade` discards a newer version that asks for
//! more than the installed one, and `remove` deletes the plugin's rows and
//! files. The host still checks the approved set inside every import, so a
//! plugin cannot use a permission the administrator did not approve.

use std::collections::BTreeMap;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_plugin_host::{
    Manifest, PluginAdmin, PluginRepository, PluginSource, RequestedPermission,
    requested_permissions,
};
use uuid::Uuid;

use crate::cli::{InstallPluginArgs, PluginIdArgs, PluginsArgs, PluginsCommand};
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
/// Returns an error when the context cannot be opened or the subcommand fails.
pub async fn run(
    config: BootstrapConfig,
    env: &BTreeMap<String, String>,
    args: PluginsArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        PluginsCommand::Install(args) => install(&context, args).await,
        PluginsCommand::Approve(args) => approve(&context, &args).await,
        PluginsCommand::Enable(args) => set_enabled(&context, &args, true).await,
        PluginsCommand::Disable(args) => set_enabled(&context, &args, false).await,
        PluginsCommand::DeclineUpgrade(args) => decline_upgrade(&context, &args).await,
        PluginsCommand::Remove(args) => remove(&context, &args).await,
        PluginsCommand::List => list(&context).await,
    }
}

/// Builds the plugin tables of the context's database.
///
/// # Parameters
///
/// - `context`: the open database.
fn repository(context: &Context) -> PluginRepository {
    PluginRepository::new(context.database())
}

/// Builds the administrative service over the context's plugin tree.
///
/// The service is given no event bus: a command-line process has no subscriber,
/// and the running server notices the writes through its data-version watcher.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
///
/// # Errors
///
/// Returns an error when the plugin host cannot be built.
fn admin(context: &Context) -> anyhow::Result<PluginAdmin> {
    Ok(PluginAdmin::new(
        context.plugin_host()?,
        repository(context),
        None,
    ))
}

/// Installs an archive and records it as pending approval.
///
/// The requested permissions are printed with their risk labels, followed by
/// the approval command, so the operator sees what they would be granting.
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
    let source = if args.bundled {
        PluginSource::Bundled
    } else {
        PluginSource::Sideload
    };
    let registration = admin(context)?
        .install_archive(&args.file, None, source, None, false)
        .await
        .with_context(|| format!("could not install {}", args.file.display()))?;

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
    print_permissions("", &registration.requested);
    println!(
        "approve it with: hearthsound plugins approve {}",
        registration.id
    );
    Ok(())
}

/// Approves a plugin's requested permissions, or applies its staged upgrade.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin to approve.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, is not waiting for approval, or
/// a row cannot be written.
async fn approve(context: &Context, args: &PluginIdArgs) -> anyhow::Result<()> {
    let upgrade = repository(context)
        .upgrade(&args.id)
        .await
        .with_context(|| format!("could not read the staged upgrade of `{}`", args.id))?;
    let approved = admin(context)?
        .approve(&args.id)
        .await
        .with_context(|| format!("could not approve `{}`", args.id))?;

    if upgrade.is_some() {
        println!(
            "approved the upgrade to {} {}",
            approved.id, approved.version
        );
    } else {
        println!(
            "approved {} {} and enabled it",
            approved.id, approved.version
        );
    }
    Ok(())
}

/// Switches an approved plugin on or off.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin to switch.
/// - `enabled`: whether the plugin becomes active.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, is waiting for approval, or the
/// row cannot be written.
async fn set_enabled(context: &Context, args: &PluginIdArgs, enabled: bool) -> anyhow::Result<()> {
    let (verb, done) = if enabled {
        ("enable", "enabled")
    } else {
        ("disable", "disabled")
    };
    admin(context)?
        .set_enabled(&args.id, enabled)
        .await
        .with_context(|| format!("could not {verb} `{}`", args.id))?;

    println!("{done} {}", args.id);
    Ok(())
}

/// Discards the newer version waiting for approval, keeping the installed one.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin whose staged upgrade is declined.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, has no staged upgrade, or the
/// staged files cannot be removed.
async fn decline_upgrade(context: &Context, args: &PluginIdArgs) -> anyhow::Result<()> {
    let staged = repository(context)
        .upgrade(&args.id)
        .await
        .with_context(|| format!("could not read the staged upgrade of `{}`", args.id))?;
    let row = admin(context)?
        .decline_upgrade(&args.id)
        .await
        .with_context(|| format!("could not decline the upgrade to `{}`", args.id))?;

    let declined = staged
        .as_ref()
        .map_or(row.version.as_str(), |upgrade| upgrade.version.as_str());
    println!(
        "declined the upgrade to {} {declined}; {} {} stays in service",
        row.id, row.id, row.version
    );
    Ok(())
}

/// Removes a plugin, reporting the rows and files that went with it.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin to remove.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, or its row or files cannot be
/// removed.
async fn remove(context: &Context, args: &PluginIdArgs) -> anyhow::Result<()> {
    let repository = repository(context);
    let instance_ids: Vec<Uuid> = repository
        .instances()
        .await
        .context("could not list the provider instances")?
        .into_iter()
        .filter(|instance| instance.plugin_id == args.id)
        .map(|instance| instance.id)
        .collect();
    let assignments = repository
        .assignments()
        .await
        .context("could not list the layer assignments")?;
    let cascaded = assignments
        .iter()
        .filter(|assignment| instance_ids.contains(&assignment.instance_id))
        .count();

    let row = admin(context)?
        .remove(&args.id)
        .await
        .with_context(|| format!("could not remove `{}`", args.id))?;

    println!("removed {} {} ({})", row.id, row.version, row.source);
    println!(
        "cascaded {} and {}",
        count(instance_ids.len(), "provider instance"),
        count(cascaded, "layer assignment")
    );
    println!(
        "removed the files in {}",
        context.plugins_dir().join(&row.id).display()
    );
    Ok(())
}

/// Lists the installed plugins.
///
/// Each plugin is reported with its source and lifecycle state, its API version,
/// the permissions its manifest requests, and, when one is parked, the version
/// waiting for approval.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Errors
///
/// Returns an error when the plugin table, a staged upgrade, or a stored
/// manifest cannot be read.
async fn list(context: &Context) -> anyhow::Result<()> {
    let repository = repository(context);
    let plugins = repository
        .plugins()
        .await
        .context("could not list the installed plugins")?;

    if plugins.is_empty() {
        println!("no plugins are installed");
        return Ok(());
    }
    for plugin in &plugins {
        let upgrade = repository
            .upgrade(&plugin.id)
            .await
            .with_context(|| format!("could not read the staged upgrade of `{}`", plugin.id))?;
        println!(
            "{} {} ({}, {})",
            plugin.id,
            plugin.version,
            plugin.source,
            plugin.state(upgrade.is_some()).as_str()
        );
        println!("  api: {}", plugin.api);
        let manifest: Manifest = serde_json::from_str(&plugin.manifest)
            .with_context(|| format!("the stored manifest of `{}` cannot be read", plugin.id))?;
        print_permissions("  ", &requested_permissions(&manifest));
        if let Some(upgrade) = &upgrade {
            println!("  waiting for approval: {}", upgrade.version);
        }
    }
    Ok(())
}

/// Prints the permissions a plugin requests, labelled with their risk.
///
/// # Parameters
///
/// - `prefix`: what each line starts with, so a listing can nest the block.
/// - `requested`: the permissions, as the manifest requests them.
fn print_permissions(prefix: &str, requested: &[RequestedPermission]) {
    if requested.is_empty() {
        println!("{prefix}requests no permissions");
        return;
    }
    println!("{prefix}requested permissions:");
    for permission in requested {
        println!(
            "{prefix}  {:<10} {} ({})",
            permission.risk.as_str(),
            permission.detail,
            permission.scope
        );
    }
}

/// Renders a count with its singular or plural noun.
///
/// # Parameters
///
/// - `count`: how many there are.
/// - `noun`: the singular noun.
fn count(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}
