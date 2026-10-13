//! `hearthsound settings`: read and write the runtime settings.
//!
//! A runtime setting is resolved as environment, config file, database, then
//! default. The stored value is the one this command writes; a key the config
//! file pins, or an environment variable overrides, cannot take a stored value,
//! so `settings set` refuses it with the reason rather than writing a row that
//! would never take effect.

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use hearthsound_config::bootstrap::BootstrapConfig;
use hearthsound_config::settings::SettingsError;
use hearthsound_config::{SettingKind, SettingValue};

use crate::cli::{SetSettingArgs, SettingKeyArgs, SettingsArgs, SettingsCommand};
use crate::context::Context;

/// Runs one `settings` command.
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
    args: SettingsArgs,
) -> anyhow::Result<()> {
    let context = Context::open(config, env).await?;
    match args.command {
        SettingsCommand::Get(args) => get(&context, args),
        SettingsCommand::Set(args) => set(&context, args).await,
        SettingsCommand::Unset(args) => unset(&context, args).await,
        SettingsCommand::List => list(&context),
    }
}

/// Prints one setting's effective value and its source.
///
/// # Parameters
///
/// - `context`: the open settings.
/// - `args`: the setting key.
///
/// # Errors
///
/// Returns an error when no setting has that key.
fn get(context: &Context, args: SettingKeyArgs) -> anyhow::Result<()> {
    let entry = entry(context, &args.key)?;
    println!(
        "{} = {}{} (from {}, default {})",
        entry.key,
        entry.value,
        pin_note(entry.pinned),
        entry.source,
        entry.default
    );
    Ok(())
}

/// Stores a value in the database.
///
/// # Parameters
///
/// - `context`: the open settings.
/// - `args`: the setting key and the new value.
///
/// # Errors
///
/// Returns an error when no setting has that key, the value does not match the
/// setting's kind, the config file pins the key, an environment variable
/// overrides it, or the write fails.
async fn set(context: &Context, args: SetSettingArgs) -> anyhow::Result<()> {
    let def = context
        .settings()
        .registry()
        .def(&args.key)
        .with_context(|| format!("setting `{}` is not registered", args.key))?;
    let value = parse_value(def.kind, &args.value)?;

    context
        .settings()
        .set(&args.key, value.clone(), None)
        .await
        .map_err(writable)
        .with_context(|| format!("could not store `{}`", args.key))?;

    println!("set {} = {}", args.key, value);
    Ok(())
}

/// Removes a stored value, falling back to the layers below it.
///
/// # Parameters
///
/// - `context`: the open settings.
/// - `args`: the setting key.
///
/// # Errors
///
/// Returns an error when no setting has that key, the config file pins the key,
/// an environment variable overrides it, or the delete fails.
async fn unset(context: &Context, args: SettingKeyArgs) -> anyhow::Result<()> {
    context
        .settings()
        .unset(&args.key)
        .await
        .map_err(writable)
        .with_context(|| format!("could not unset `{}`", args.key))?;

    let entry = entry(context, &args.key)?;
    println!(
        "unset {} (now {} from {})",
        args.key, entry.value, entry.source
    );
    Ok(())
}

/// Lists every setting.
///
/// # Parameters
///
/// - `context`: the open settings.
///
/// # Errors
///
/// This function does not fail; it returns `Ok(())` for symmetry with the other
/// commands.
fn list(context: &Context) -> anyhow::Result<()> {
    for entry in context.settings().list() {
        println!(
            "{} = {}{} (from {}, default {})",
            entry.key,
            entry.value,
            pin_note(entry.pinned),
            entry.source,
            entry.default
        );
    }
    Ok(())
}

/// Looks one setting up in the registry.
///
/// # Parameters
///
/// - `context`: the open settings.
/// - `key`: the dotted setting key.
///
/// # Errors
///
/// Returns an error when no setting has that key.
fn entry(
    context: &Context,
    key: &str,
) -> anyhow::Result<hearthsound_config::settings::SettingEntry> {
    context
        .settings()
        .list()
        .into_iter()
        .find(|entry| entry.key == key)
        .with_context(|| format!("setting `{key}` is not registered"))
}

/// Returns the marker a pinned setting carries in a listing.
///
/// # Parameters
///
/// - `pinned`: whether the config file pins the setting.
fn pin_note(pinned: bool) -> &'static str {
    if pinned { " [pinned]" } else { "" }
}

/// Parses a command-line value as the setting's kind.
///
/// # Parameters
///
/// - `kind`: the kind the registry declares for the setting.
/// - `raw`: the value as typed.
///
/// # Returns
///
/// The parsed value. A string list is split on commas and trimmed, and an empty
/// item is dropped.
///
/// # Errors
///
/// Returns an error when the value does not fit the kind, for example a word
/// where a number is expected.
fn parse_value(kind: SettingKind, raw: &str) -> anyhow::Result<SettingValue> {
    Ok(match kind {
        SettingKind::Bool => match raw {
            "true" => SettingValue::Bool(true),
            "false" => SettingValue::Bool(false),
            other => bail!("`{other}` is not `true` or `false`"),
        },
        SettingKind::Integer { min, max } => {
            let value: i64 = raw
                .parse()
                .with_context(|| format!("`{raw}` is not an integer"))?;
            if value < min || value > max {
                bail!("`{value}` is outside {min}..={max}");
            }
            SettingValue::Integer(value)
        }
        SettingKind::Float { min, max } => {
            let value: f64 = raw
                .parse()
                .with_context(|| format!("`{raw}` is not a number"))?;
            if value < min || value > max {
                bail!("`{value}` is outside {min}..={max}");
            }
            SettingValue::Float(value)
        }
        SettingKind::String { .. } => SettingValue::String(raw.to_owned()),
        SettingKind::StringList { .. } => SettingValue::StringList(
            raw.split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect(),
        ),
    })
}

/// Explains why a write was refused, or passes a storage failure through.
///
/// # Parameters
///
/// - `error`: the error the settings service returned.
fn writable(error: SettingsError) -> anyhow::Error {
    match &error {
        SettingsError::Pinned { key } => anyhow::anyhow!(
            "setting `{key}` is pinned by the configuration file, so a stored value would never \
             take effect; remove it from the [settings] table and restart"
        ),
        SettingsError::EnvironmentOverride { key } => anyhow::anyhow!(
            "setting `{key}` is overridden by an environment variable, so a stored value would \
             never take effect"
        ),
        _ => anyhow::Error::new(error),
    }
}
