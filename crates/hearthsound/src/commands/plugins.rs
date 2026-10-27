//! `hearthsound plugins`: install plugins, manage their lifecycle, and list
//! them.
//!
//! Installing records the permissions the plugin's manifest requests as the
//! grant and enables it. A plugin comes from an archive, from an index, or from
//! the URL of an index document a repository publishes. Only an upgrade that
//! asks for a permission the installed version does not hold is staged for
//! review: `approve` applies that upgrade, `decline-upgrade` discards it,
//! `enable` and `disable` switch a plugin, and `remove` deletes the plugin's
//! rows and files. The host still checks the granted set inside every import,
//! so a plugin cannot use a permission its manifest did not request.
//!
//! The index-backed commands are also how a repository is administered:
//! `repositories add`, `list`, and `remove` are the command line's half of the
//! `plugin_repositories` rows, and `available` reports what the reachable
//! indexes publish.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::keys;
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_plugin_host::index::OFFICIAL_INDEX_URL;
use hearthsound_plugin_host::{
    AvailablePlugin, IndexError, InstallError, Manifest, PluginRegistration, PluginRepository,
    PluginSource, RepositoryRef, RepositoryRow, RequestedPermission, requested_permissions,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::{
    AddRepositoryArgs, AvailablePluginsArgs, InstallPluginArgs, PluginIdArgs, PluginsArgs,
    PluginsCommand, RemoveRepositoryArgs, RepositoriesArgs, RepositoriesCommand, SourceSelection,
};
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
        PluginsCommand::Available(args) => available(&context, args).await,
        PluginsCommand::Repositories(args) => repositories(&context, args).await,
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

/// Installs a plugin from an archive, from an index, or from an index URL.
///
/// Every path ends in the same notice: the plugin is installed, the permissions
/// its manifest requests are printed with their risk labels, and its lifecycle
/// state is reported. A version that widens the installed permission set is
/// staged for review instead; the notice then lists the new permissions and how
/// to apply or discard the staged version.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: where to install from, and the source to record it under.
///
/// # Errors
///
/// Returns an error when the archive cannot be installed (an unreadable
/// archive, an invalid manifest, a component that asks for something the host
/// does not offer, or a version that is already installed), when the index
/// cannot be read or does not offer the plugin, or when a row cannot be
/// written.
async fn install(context: &Context, args: InstallPluginArgs) -> anyhow::Result<()> {
    let registration = match args.source {
        None => {
            let source = if args.bundled {
                PluginSource::Bundled
            } else {
                PluginSource::Sideload
            };
            let file = PathBuf::from(&args.target);
            context
                .plugin_admin()?
                .install_archive(&file, None, source, None)
                .await
                .with_context(|| format!("could not install {}", file.display()))?
        }
        Some(SourceSelection::Index) => {
            if let Some(url) = &args.url {
                bail!(
                    "`--source index` covers the official index and the registered repositories; \
                     use `--source url --url <INDEX-URL>` to install from {url} alone"
                );
            }
            install_from_indexes(context, &args.target, args.version.as_deref()).await?
        }
        Some(SourceSelection::Url) => {
            let Some(url) = args.url.as_deref() else {
                bail!("`--source url` needs `--url <INDEX-URL>`");
            };
            context
                .installer()?
                .install_from_url(&args.target, url, args.version.as_deref())
                .await
                .with_context(|| format!("could not install `{}` from {url}", args.target))?
        }
    };
    let repository = repository(context);
    let Some(row) = repository
        .plugin(&registration.id)
        .await
        .with_context(|| format!("could not read `{}` after installing it", registration.id))?
    else {
        bail!(
            "`{}` was not recorded after it was installed",
            registration.id
        );
    };
    let upgrade = repository
        .upgrade(&registration.id)
        .await
        .with_context(|| format!("could not read the staged upgrade of `{}`", registration.id))?;
    print_registration(&registration, row.state(upgrade.is_some()).as_str());
    Ok(())
}

/// Installs a plugin from the official index, then from the registered ones.
///
/// The official index is read first. A source that does not list the plugin, or
/// that cannot be read at all, is passed over in favour of the next one, so a
/// plugin an administrator published in a repository is installable on a
/// machine that cannot reach the official index. Every other failure is about
/// the plugin or the download itself and ends the command, because another
/// index would not change it: a signature that does not verify and a digest
/// that does not match stay fatal.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `plugin_id`: the plugin id from the plugin's manifest.
/// - `version`: the version to install, or `None` for the newest version this
///   server hosts.
///
/// # Returns
///
/// The registration of the installed plugin.
///
/// # Errors
///
/// Returns the first failure that another index cannot fix, and every source's
/// reason when no source offers the plugin.
async fn install_from_indexes(
    context: &Context,
    plugin_id: &str,
    version: Option<&str>,
) -> anyhow::Result<PluginRegistration> {
    let installer = context.installer()?;
    let mut reasons: Vec<String> = Vec::new();
    for (label, reference) in index_sources(context).await? {
        match installer
            .install_from_index(plugin_id, version, Some(reference))
            .await
        {
            Ok(registration) => return Ok(registration),
            Err(error) if another_index_may_offer(&error) => {
                reasons.push(format!("{label}: {error}"));
            }
            Err(error) => {
                let what = format!("could not install `{plugin_id}` from {label}");
                return Err(anyhow::Error::new(error).context(what));
            }
        }
    }
    bail!("no index offers `{plugin_id}`; {}", reasons.join("; "))
}

/// Lists the indexes `--source index` covers, in the order they are tried.
///
/// The official index comes first, then every enabled repository an
/// administrator registered, by URL.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Returns
///
/// Each index with the label a report names it by.
///
/// # Errors
///
/// Returns an error when the repository rows cannot be read.
async fn index_sources(context: &Context) -> anyhow::Result<Vec<(String, RepositoryRef)>> {
    let mut sources = vec![(OFFICIAL_INDEX_URL.to_owned(), RepositoryRef::Official)];
    for row in repository(context)
        .repositories()
        .await
        .context("could not list the plugin repositories")?
    {
        if row.enabled {
            sources.push((row.url.clone(), RepositoryRef::Id(row.id)));
        }
    }
    Ok(sources)
}

/// Reports whether a failed install from one index may succeed from another.
///
/// # Parameters
///
/// - `error`: the failure of one index.
///
/// # Returns
///
/// Whether the next index is tried: `true` for a source that does not list the
/// plugin and for one that cannot be read at all.
fn another_index_may_offer(error: &InstallError) -> bool {
    matches!(
        error,
        InstallError::UnknownPlugin { .. }
            | InstallError::Index(IndexError::Unreachable { .. } | IndexError::Malformed { .. })
    )
}

/// Prints the notice every install path ends with.
///
/// A version that switched in is reported as installed, with the lifecycle
/// state it left behind. A version staged for review is reported as staged
/// instead: the review's new permissions are listed, then how to apply or
/// discard it. No state line is printed for a staged version, because the
/// installed version keeps its own.
///
/// # Parameters
///
/// - `registration`: what the install recorded.
/// - `state`: the lifecycle state the install left the plugin in.
fn print_registration(registration: &PluginRegistration, state: &str) {
    let staged = !registration.new_permissions.is_empty();
    if staged {
        println!(
            "staged {} {} ({}) for review",
            registration.id, registration.version, registration.source
        );
    } else {
        println!(
            "installed {} {} ({})",
            registration.id, registration.version, registration.source
        );
    }
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
    if staged {
        println!("new permissions:");
        print_permission_rows("", &registration.new_permissions);
        println!(
            "apply it with: hearthsound plugins approve {}",
            registration.id
        );
        println!(
            "or discard it with: hearthsound plugins decline-upgrade {}",
            registration.id
        );
    } else {
        println!("state: {state}");
    }
}

/// Lists the plugins the reachable indexes offer.
///
/// A source that cannot be read is printed at the end rather than failing the
/// listing, so one unreachable repository cannot hide the plugins the others
/// publish.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the indexes to list.
///
/// # Errors
///
/// Returns an error when the repository rows or the installed plugins cannot be
/// read, or when the arguments name no index.
async fn available(context: &Context, args: AvailablePluginsArgs) -> anyhow::Result<()> {
    let reference = match (args.source, args.url, args.repository) {
        (_, _, Some(repository)) => Some(repository_reference(&repository)?),
        (None, None, None) | (Some(SourceSelection::Index), None, None) => None,
        (Some(SourceSelection::Index), Some(url), None) => {
            bail!(
                "`--source index` covers the official index and the registered repositories; \
                 use `--source url --url <INDEX-URL>` to list {url} alone"
            );
        }
        (Some(SourceSelection::Url), Some(url), None) => Some(RepositoryRef::Url(url)),
        (Some(SourceSelection::Url), None, None) => {
            bail!("`--source url` needs `--url <INDEX-URL>`")
        }
        (None, Some(_), None) => bail!("`--url` needs `--source url`"),
    };

    let report = context
        .installer()?
        .available(reference)
        .await
        .context("could not list the available plugins")?;
    let installed: BTreeSet<String> = repository(context)
        .plugins()
        .await
        .context("could not list the installed plugins")?
        .into_iter()
        .map(|plugin| plugin.id)
        .collect();
    let urls = repository_urls(context).await?;

    if report.plugins.is_empty() && report.errors.is_empty() {
        println!("no index offers a plugin");
        return Ok(());
    }
    for plugin in &report.plugins {
        println!(
            "{} {} ({})",
            plugin.id,
            plugin.version.as_deref().unwrap_or("-"),
            layers(plugin)
        );
        println!("  name: {}", plugin.name);
        println!("  source: {}", source_of(plugin, &urls));
        println!(
            "  {}",
            if installed.contains(&plugin.id) {
                "installed"
            } else {
                "not installed"
            }
        );
        if let Some(unavailable) = &plugin.unavailable {
            println!("  cannot be installed here: {unavailable}");
        }
    }
    if !report.errors.is_empty() {
        println!("sources that could not be read:");
        for error in &report.errors {
            println!("  {}: {}", error.source, error.detail);
        }
    }
    Ok(())
}

/// Renders the layers one available plugin provides.
///
/// # Parameters
///
/// - `plugin`: the listed plugin.
///
/// # Returns
///
/// The layers, or a note that the index declares none.
fn layers(plugin: &AvailablePlugin) -> String {
    if plugin.provides.is_empty() {
        "no layers".to_owned()
    } else {
        plugin.provides.join(", ")
    }
}

/// Names the index one available plugin came from.
///
/// # Parameters
///
/// - `plugin`: the listed plugin.
/// - `urls`: the registered repositories by id.
///
/// # Returns
///
/// The official index, or the repository's URL when it is registered, with
/// whether the signature of its index was verified.
fn source_of(plugin: &AvailablePlugin, urls: &BTreeMap<Uuid, String>) -> String {
    let trust = if plugin.verified {
        "verified"
    } else {
        "unverified"
    };
    match plugin.repository_id {
        None => format!("official index ({trust})"),
        Some(id) => {
            let url = urls.get(&id).cloned().unwrap_or_else(|| repository_id(id));
            format!("{url} ({trust})")
        }
    }
}

/// Reads the registered repositories, keyed by id.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Errors
///
/// Returns an error when the repository rows cannot be read.
async fn repository_urls(context: &Context) -> anyhow::Result<BTreeMap<Uuid, String>> {
    Ok(repository(context)
        .repositories()
        .await
        .context("could not list the plugin repositories")?
        .into_iter()
        .map(|row| (row.id, row.url))
        .collect())
}

/// Reads an argument that names a repository as a source.
///
/// # Parameters
///
/// - `text`: the repository's URL, or its `pr_` id.
///
/// # Returns
///
/// The reference the installer resolves.
///
/// # Errors
///
/// Returns an error when the text is a client id of another kind.
fn repository_reference(text: &str) -> anyhow::Result<RepositoryRef> {
    match EntityId::parse(text) {
        Ok(id) if id.kind() == IdPrefix::PluginRepository => Ok(RepositoryRef::Id(id.uuid())),
        Ok(_) => bail!("`{text}` is not a repository id; it must be a URL or a `pr_` id"),
        Err(_) => Ok(RepositoryRef::Url(text.to_owned())),
    }
}

/// Reads the repository one argument names, by URL or by `pr_` id.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `text`: the repository's URL, or its `pr_` id.
///
/// # Returns
///
/// The registered row.
///
/// # Errors
///
/// Returns an error when no repository has the URL or the id, or when the row
/// cannot be read.
async fn find_repository(context: &Context, text: &str) -> anyhow::Result<RepositoryRow> {
    if let Ok(id) = EntityId::parse(text)
        && id.kind() == IdPrefix::PluginRepository
    {
        return repository(context)
            .repository(id.uuid())
            .await
            .context("could not read the plugin repository")?
            .with_context(|| format!("no plugin repository has the id `{text}`"));
    }
    repository(context)
        .repository_by_url(text)
        .await
        .context("could not read the plugin repository")?
        .with_context(|| format!("no plugin repository is registered under `{text}`"))
}

/// Runs one `plugins repositories` command.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the subcommand to run.
///
/// # Errors
///
/// Returns the error of the subcommand.
async fn repositories(context: &Context, args: RepositoriesArgs) -> anyhow::Result<()> {
    match args.command {
        RepositoriesCommand::Add(args) => add_repository(context, args).await,
        RepositoriesCommand::List => list_repositories(context).await,
        RepositoriesCommand::Remove(args) => remove_repository(context, args).await,
    }
}

/// Registers a plugin index.
///
/// The repository is queried for its index from then on, and its index is
/// verified against the public key when one is given.
///
/// # Parameters
///
/// - `context`: the open database and runtime settings.
/// - `args`: the URL to register, with its name and public key.
///
/// # Errors
///
/// Returns an error when `plugins.allow_third_party_repositories` is off, when
/// another repository is registered under the same URL, or when the row cannot
/// be written.
async fn add_repository(context: &Context, args: AddRepositoryArgs) -> anyhow::Result<()> {
    let allowed = context
        .settings()
        .current()
        .get(keys::PLUGINS_ALLOW_THIRD_PARTY_REPOSITORIES)
        .context("could not read plugins.allow_third_party_repositories")?;
    if !allowed {
        bail!(
            "adding a plugin repository is turned off; \
             set plugins.allow_third_party_repositories first"
        );
    }
    let row = repository(context)
        .insert_repository(
            &args.url,
            args.name.as_deref(),
            args.public_key.as_deref(),
            true,
        )
        .await
        .with_context(|| format!("could not register {}", args.url))?;

    println!("registered {} {}", repository_id(row.id), row.url);
    if let Some(name) = &row.name {
        println!("  name: {name}");
    }
    println!(
        "  index: {}",
        if row.public_key.is_some() {
            "signed, verified against the public key"
        } else {
            "unsigned, read unverified"
        }
    );
    Ok(())
}

/// Lists the registered plugin indexes.
///
/// # Parameters
///
/// - `context`: the open database.
///
/// # Errors
///
/// Returns an error when the repository rows cannot be read.
async fn list_repositories(context: &Context) -> anyhow::Result<()> {
    let rows = repository(context)
        .repositories()
        .await
        .context("could not list the plugin repositories")?;
    if rows.is_empty() {
        println!("no plugin repositories are registered");
        return Ok(());
    }
    for row in &rows {
        println!("{} {}", repository_id(row.id), row.url);
        if let Some(name) = &row.name {
            println!("  name: {name}");
        }
        println!("  enabled: {}", if row.enabled { "yes" } else { "no" });
        println!(
            "  index: {}",
            if row.public_key.is_some() {
                "signed"
            } else {
                "unsigned"
            }
        );
        println!(
            "  last fetched: {}",
            row.last_fetched_at
                .map_or_else(|| "never".to_owned(), timestamp)
        );
        if let Some(error) = &row.last_error {
            println!("  last error: {error}");
        }
    }
    Ok(())
}

/// Removes a registered plugin index.
///
/// Plugins installed from the repository keep their rows; only the repository
/// itself goes.
///
/// # Parameters
///
/// - `context`: the open database.
/// - `args`: the repository to remove, by URL or by `pr_` id.
///
/// # Errors
///
/// Returns an error when no repository has the URL or the id, or when the row
/// cannot be removed.
async fn remove_repository(context: &Context, args: RemoveRepositoryArgs) -> anyhow::Result<()> {
    let row = find_repository(context, &args.repository).await?;
    let removed = repository(context)
        .delete_repository(row.id)
        .await
        .context("could not remove the plugin repository")?;
    if !removed {
        bail!("{} was removed while the command ran", row.url);
    }
    println!("removed {} {}", repository_id(row.id), row.url);
    Ok(())
}

/// Applies a plugin's staged upgrade.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin to apply the upgrade for.
///
/// # Errors
///
/// Returns an error when the plugin is unknown, has no staged upgrade, or a row
/// cannot be written.
async fn approve(context: &Context, args: &PluginIdArgs) -> anyhow::Result<()> {
    let applied = context
        .plugin_admin()?
        .apply_upgrade(&args.id)
        .await
        .with_context(|| format!("could not apply the upgrade to `{}`", args.id))?;

    println!("applied the upgrade to {} {}", applied.id, applied.version);
    Ok(())
}

/// Switches a plugin on or off.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the id of the plugin to switch.
/// - `enabled`: whether the plugin becomes active.
///
/// # Errors
///
/// Returns an error when the plugin is unknown or the row cannot be written.
async fn set_enabled(context: &Context, args: &PluginIdArgs, enabled: bool) -> anyhow::Result<()> {
    let (verb, done) = if enabled {
        ("enable", "enabled")
    } else {
        ("disable", "disabled")
    };
    context
        .plugin_admin()?
        .set_enabled(&args.id, enabled)
        .await
        .with_context(|| format!("could not {verb} `{}`", args.id))?;

    println!("{done} {}", args.id);
    Ok(())
}

/// Discards the staged upgrade, keeping the installed version in service.
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
    let row = context
        .plugin_admin()?
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

    let row = context
        .plugin_admin()?
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
/// waiting for review.
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
            println!("  upgrade waiting for review: {}", upgrade.version);
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
    print_permission_rows(prefix, requested);
}

/// Prints one line per permission, in the shared row format.
///
/// # Parameters
///
/// - `prefix`: what each line starts with, so a listing can nest the block.
/// - `permissions`: the permissions to print.
fn print_permission_rows(prefix: &str, permissions: &[RequestedPermission]) {
    for permission in permissions {
        println!(
            "{prefix}  {:<10} {} ({})",
            permission.risk.as_str(),
            permission.detail,
            permission.scope
        );
    }
}

/// Renders a repository's id as the client id an administrator types.
///
/// # Parameters
///
/// - `id`: the repository's primary key.
///
/// # Returns
///
/// The `pr_` client id.
fn repository_id(id: Uuid) -> String {
    EntityId::from_uuid(IdPrefix::PluginRepository, id).to_string()
}

/// Renders a timestamp.
///
/// # Parameters
///
/// - `millis`: the instant in unix milliseconds.
///
/// # Returns
///
/// The instant in RFC 3339 UTC, or the raw value when it is out of range.
fn timestamp(millis: i64) -> String {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|instant| instant.format(&Rfc3339).ok())
        .unwrap_or_else(|| millis.to_string())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An index that does not list a plugin is passed over for the next one.
    #[test]
    fn an_index_that_does_not_list_a_plugin_is_skipped() {
        assert!(another_index_may_offer(&InstallError::UnknownPlugin {
            id: "com.example.plugin".to_owned(),
            index: "https://example.com/v1/index.json".to_owned(),
        }));
    }

    /// An index that cannot be read at all is passed over for the next one.
    #[test]
    fn an_index_that_cannot_be_read_is_skipped() {
        for error in [
            IndexError::Unreachable {
                url: "https://example.com/v1/index.json".to_owned(),
                reason: "the connection was refused".to_owned(),
            },
            IndexError::Malformed {
                url: "https://example.com/v1/index.json".to_owned(),
                reason: "expected value".to_owned(),
            },
        ] {
            assert!(another_index_may_offer(&InstallError::Index(error)),);
        }
    }

    /// A signature that does not verify ends the command: another index would
    /// not change which document the official one is supposed to be.
    #[test]
    fn a_signature_that_does_not_verify_ends_the_command() {
        for error in [
            IndexError::SignatureInvalid {
                url: "https://example.com/v1/index.json".to_owned(),
            },
            IndexError::Unsigned {
                url: "https://example.com/v1/index.json".to_owned(),
            },
        ] {
            assert!(!another_index_may_offer(&InstallError::Index(error)));
        }
    }

    /// Everything about the plugin or its download ends the command.
    #[test]
    fn a_plugin_that_is_there_ends_the_command() {
        assert!(!another_index_may_offer(&InstallError::AlreadyInstalled {
            id: "com.example.plugin".to_owned(),
            version: "1.0.0".to_owned(),
        }));
        assert!(!another_index_may_offer(&InstallError::IntegrityFailed {
            expected: "0".repeat(64),
            actual: "1".repeat(64),
        }));
        assert!(!another_index_may_offer(&InstallError::Unavailable(
            hearthsound_plugin_host::Unavailable::NoCompatibleVersion {
                supported: "hearthsound:plugin@0.3.0",
                offered: vec!["hearthsound:plugin@0.1.0".to_owned()],
            },
        )));
        assert!(!another_index_may_offer(
            &InstallError::RepositoriesDisabled
        ));
    }
}
