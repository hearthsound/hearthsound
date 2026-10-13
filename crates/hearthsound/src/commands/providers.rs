//! `hearthsound providers`: configure provider instances and list them.
//!
//! An instance is a plugin plus the configuration and the directories the
//! administrator gives it. Creating one validates the configuration against the
//! plugin's schema, asks the plugin itself (`lifecycle.validate-config`), seals
//! the write-only fields, and only then stores the row; a rejected configuration
//! is printed with the plugin's own problems.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context as _, bail};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_core::ImportMode;
use hearthsound_plugin_host::{ImportRoot, InstanceHostConfig, InstanceSpec};

use crate::cli::{AddProviderArgs, ProvidersArgs, ProvidersCommand};
use crate::context::Context;

/// Runs one `providers` command.
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
    args: ProvidersArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        ProvidersCommand::Add(args) => add(&context, args).await,
        ProvidersCommand::List => list(&context).await,
    }
}

/// Configures a provider instance from an installed plugin.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the plugin, slug, configuration, and import roots.
///
/// # Errors
///
/// Returns an error when the configuration is not a JSON object, an import root
/// is malformed, the plugin is unknown or disabled, the plugin rejects the
/// configuration, or the row cannot be written.
async fn add(context: &Context, args: AddProviderArgs) -> anyhow::Result<()> {
    let config: serde_json::Value = serde_json::from_str(&args.server_config)
        .with_context(|| format!("--config is not valid JSON: {}", args.server_config))?;
    if !config.is_object() {
        bail!("--config must be a JSON object, for example '{{}}'");
    }
    let host_config = host_config(&args.import_roots)?;
    let spec = InstanceSpec {
        slug: args.slug.clone(),
        plugin_id: args.plugin.clone(),
        display_name: args.display_name.unwrap_or_else(|| args.slug.clone()),
        server_config: config,
        host_config,
    };

    let view = context
        .instances()?
        .create(&spec)
        .await
        .with_context(|| format!("could not configure the instance `{}`", args.slug))?;

    println!(
        "configured provider instance `{}` from plugin {}",
        view.slug, view.plugin_id
    );
    println!("name: {}", view.display_name);
    for root in &view.host_config.import_roots {
        println!(
            "import root: {} = {} ({})",
            root.id,
            root.path.display(),
            root.mode
        );
    }
    Ok(())
}

/// Lists the configured provider instances.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
///
/// # Errors
///
/// Returns an error when the instance table cannot be read.
async fn list(context: &Context) -> anyhow::Result<()> {
    let instances = context
        .instances()?
        .list()
        .await
        .context("could not list the provider instances")?;

    if instances.is_empty() {
        println!("no provider instances are configured");
        return Ok(());
    }
    for instance in &instances {
        println!(
            "{} from {} ({}{})",
            instance.slug,
            instance.plugin_id,
            instance.display_name,
            if instance.enabled { "" } else { ", disabled" }
        );
        for root in &instance.host_config.import_roots {
            println!(
                "  import root: {} = {} ({})",
                root.id,
                root.path.display(),
                root.mode
            );
        }
    }
    Ok(())
}

/// Builds the host-level configuration from the `--import-root` values.
///
/// # Parameters
///
/// - `values`: `<id>=<path>:<mode>` strings, with a mode of `move`, `copy`, or
///   `link`.
///
/// # Returns
///
/// The host configuration as JSON, or `null` when no root was given, which makes
/// the instance use the plugin host's defaults.
///
/// # Errors
///
/// Returns an error for a value without an `=`, without a `:`, or with an
/// unknown mode.
fn host_config(values: &[String]) -> anyhow::Result<serde_json::Value> {
    if values.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    let mut roots = Vec::with_capacity(values.len());
    for value in values {
        let (id, rest) = value
            .split_once('=')
            .with_context(|| format!("`{value}` is not `<id>=<path>:<mode>`"))?;
        let (path, mode) = rest
            .rsplit_once(':')
            .with_context(|| format!("`{value}` is not `<id>=<path>:<mode>`"))?;
        let mode: ImportMode = mode
            .parse()
            .with_context(|| format!("`{mode}` is not `move`, `copy`, or `link`"))?;
        roots.push(ImportRoot {
            id: id.to_owned(),
            path: PathBuf::from(path),
            mode,
            read_only: false,
        });
    }
    let config = InstanceHostConfig {
        import_roots: roots,
        ..InstanceHostConfig::default()
    };
    serde_json::to_value(config).context("could not encode the import roots")
}
