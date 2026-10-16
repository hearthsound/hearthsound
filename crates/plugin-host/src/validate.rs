//! The component import and export rules.
//!
//! A plugin component may only import what the host provides and may only
//! export the capability interfaces its manifest declares. These rules are
//! checked before a plugin is installed and again when it is loaded, so a
//! component swapped on disk after installation is still rejected.

use hearthsound_core::Layer;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

use crate::error::InstallError;
use crate::manifest::Manifest;

/// The only API version the host provides.
const API_VERSION: &str = "0.2.0";

/// The `wasi` packages whose 0.2 interfaces the host provides.
///
/// This is exactly the set `wasmtime_wasi::p2::add_to_linker_async` links.
const WASI_PACKAGES: [&str; 6] = [
    "wasi:cli",
    "wasi:clocks",
    "wasi:filesystem",
    "wasi:io",
    "wasi:random",
    "wasi:sockets",
];

/// The interface a catalog plugin must export for the host to drive it.
pub const LIFECYCLE_INTERFACE: &str = "hearthsound:plugin/lifecycle@0.2.0";

/// The layers the host can drive today.
///
/// Every other layer's manifest validates but its component is refused, because
/// the host has no adapter to call it through.
pub const HOSTED_LAYERS: [Layer; 4] = [
    Layer::Catalog,
    Layer::Metadata,
    Layer::Acquisition,
    Layer::Artwork,
];

/// Validates a compiled component against the host's rules.
///
/// # Parameters
///
/// - `component`: the compiled component.
/// - `manifest`: the manifest the plugin shipped with.
///
/// # Errors
///
/// Returns [`InstallError::DisallowedImport`] for an import the host does not
/// provide, [`InstallError::MissingExport`] when an interface the manifest
/// requires is not exported, [`InstallError::UndeclaredExport`] when an export
/// is not declared, and [`InstallError::UnsupportedCapability`] when the plugin
/// provides a layer this server cannot host yet.
pub fn validate(component: &Component, manifest: &Manifest) -> Result<(), InstallError> {
    let engine = component.engine();
    let component_type = component.component_type();

    for (name, item) in component_type.imports(engine) {
        if !matches!(item.ty, ComponentItem::ComponentInstance(_)) {
            return Err(InstallError::DisallowedImport(name.to_owned()));
        }
        let (interface, version) = split_version(name);
        if !allowed_import(interface, version, manifest) {
            return Err(InstallError::DisallowedImport(name.to_owned()));
        }
    }

    let mut exports_lifecycle = false;
    let mut exported_layers = Vec::new();
    for (name, _item) in component_type.exports(engine) {
        let (interface, version) = split_version(name);
        let Some(export) = interface.strip_prefix("hearthsound:plugin/") else {
            return Err(InstallError::UndeclaredExport(name.to_owned()));
        };
        if version != API_VERSION {
            return Err(InstallError::UndeclaredExport(name.to_owned()));
        }
        match export {
            "lifecycle" => exports_lifecycle = true,
            "catalog" => exported_layers.push(Layer::Catalog),
            "metadata" => exported_layers.push(Layer::Metadata),
            "acquisition" => exported_layers.push(Layer::Acquisition),
            "artwork" => exported_layers.push(Layer::Artwork),
            _ => return Err(InstallError::UndeclaredExport(name.to_owned())),
        }
    }

    if !exports_lifecycle {
        return Err(InstallError::MissingExport {
            interface: LIFECYCLE_INTERFACE.to_owned(),
        });
    }
    for layer in manifest.provides() {
        if !exported_layers.contains(layer) {
            return Err(InstallError::MissingExport {
                interface: format!("hearthsound:plugin/{layer}@{API_VERSION}"),
            });
        }
    }
    for layer in &exported_layers {
        if !manifest.provides().contains(layer) {
            return Err(InstallError::UndeclaredExport(format!(
                "hearthsound:plugin/{layer}@{API_VERSION}"
            )));
        }
    }

    for layer in manifest.provides() {
        if !HOSTED_LAYERS.contains(layer) {
            return Err(InstallError::UnsupportedCapability(*layer));
        }
    }

    Ok(())
}

/// Splits an interface name into its id and version.
///
/// Names carry the version after the last `@`, for example
/// `wasi:cli/environment@0.2.6`. A name without an `@` has an empty version.
fn split_version(name: &str) -> (&str, &str) {
    match name.rsplit_once('@') {
        Some((interface, version)) => (interface, version),
        None => (name, ""),
    }
}

/// Returns whether the host provides `interface` at `version`.
fn allowed_import(interface: &str, version: &str, manifest: &Manifest) -> bool {
    if let Some(rest) = interface.strip_prefix("hearthsound:plugin/") {
        if version != API_VERSION {
            return false;
        }
        return match rest {
            "types" | "host-log" | "host-http" | "host-kv" | "host-config" => true,
            "host-files" => manifest.provides().contains(&Layer::Acquisition),
            _ => false,
        };
    }

    if let Some((package, _)) = interface.split_once('/')
        && WASI_PACKAGES.contains(&package)
    {
        return version.starts_with("0.2.");
    }

    false
}
