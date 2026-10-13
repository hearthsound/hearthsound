//! `hearthsound layers`: assign provider instances to layers.
//!
//! A layer is served by an ordered list of instances, and the order is the
//! priority: the first instance that can answer wins, and the next is tried when
//! it cannot. Assigning a layer replaces its whole list, so `layers set` is the
//! one way the order changes.

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Context as _;
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_core::Layer;

use crate::cli::{LayersArgs, LayersCommand, SetLayerArgs};
use crate::context::Context;

/// Runs one `layers` command.
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
    args: LayersArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        LayersCommand::Set(args) => set(&context, args).await,
        LayersCommand::List => list(&context).await,
    }
}

/// Replaces one layer's instances.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
/// - `args`: the layer name and the instance slugs in priority order.
///
/// # Errors
///
/// Returns an error when the layer name is unknown, an instance does not exist,
/// an instance's plugin does not provide the layer, a slug is listed twice, or
/// the rows cannot be written.
async fn set(context: &Context, args: SetLayerArgs) -> anyhow::Result<()> {
    let layer: Layer = args
        .layer
        .parse()
        .with_context(|| format!("`{}` is not a layer name", args.layer))?;

    context
        .instances()?
        .set_layer(layer, &args.slugs)
        .await
        .with_context(|| format!("could not assign the {} layer", layer.as_str()))?;

    println!(
        "assigned {} to the {} layer",
        args.slugs.join(", "),
        layer.as_str()
    );
    Ok(())
}

/// Lists every layer and the instances assigned to it.
///
/// # Parameters
///
/// - `context`: the open database and plugin tree.
///
/// # Errors
///
/// Returns an error when the assignment or instance tables cannot be read.
async fn list(context: &Context) -> anyhow::Result<()> {
    let instances = context.instances()?;
    let views = instances
        .list()
        .await
        .context("could not list the provider instances")?;
    let slugs: HashMap<uuid::Uuid, &str> = views
        .iter()
        .map(|view| (view.id, view.slug.as_str()))
        .collect();
    let assignments = instances
        .assignments()
        .await
        .context("could not list the layer assignments")?;

    for layer in Layer::ALL {
        let assigned: Vec<&str> = assignments
            .iter()
            .filter(|assignment| assignment.layer == layer)
            .filter_map(|assignment| slugs.get(&assignment.instance_id).copied())
            .collect();
        if assigned.is_empty() {
            println!("{}: (none)", layer.as_str());
        } else {
            println!("{}: {}", layer.as_str(), assigned.join(", "));
        }
    }
    Ok(())
}
