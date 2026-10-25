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
use crate::manifest::{ApiVersion, Manifest};

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

/// Returns the interface a plugin must export for the host to drive it.
///
/// # Parameters
///
/// - `api`: the plugin API version whose package the interface name carries.
fn lifecycle_interface(api: ApiVersion) -> String {
    format!("hearthsound:plugin/lifecycle@{}", api.package())
}

/// The layers the host can drive today.
///
/// Every other layer's manifest validates but its component is refused, because
/// the host has no adapter to call it through.
pub const HOSTED_LAYERS: [Layer; 7] = [
    Layer::Catalog,
    Layer::Metadata,
    Layer::Acquisition,
    Layer::Artwork,
    Layer::Verification,
    Layer::Scrobble,
    Layer::Discovery,
];

/// The interface a plugin examines for account linking.
const ACCOUNT_LINK_INTERFACE: &str = "hearthsound:plugin/account-link";

/// Returns whether the component exports the `account-link` interface.
///
/// `account-link` is a capability rather than a layer: a plugin declares it
/// alongside its `scrobble` or `discovery` layer, so the host discovers it from
/// the component instead of from the manifest's `provides`.
///
/// # Parameters
///
/// - `component`: the compiled component.
/// - `api`: the plugin API version the component was built for.
///
/// # Returns
///
/// `true` when the component exports the `account-link` interface at `api`'s
/// package version.
///
/// # Example
///
/// ```ignore
/// let component = Component::new(&engine, bytes)?;
/// if exports_account_link(&component, ApiVersion::V0_4) {
///     // the instance can link accounts
/// }
/// ```
pub fn exports_account_link(component: &Component, api: ApiVersion) -> bool {
    let engine = component.engine();
    component
        .component_type()
        .exports(engine)
        .any(|(name, _item)| {
            let (interface, version) = split_version(name);
            interface == ACCOUNT_LINK_INTERFACE && version == api.package()
        })
}

/// Validates a compiled component against the host's rules.
///
/// # Parameters
///
/// - `component`: the compiled component.
/// - `manifest`: the manifest the plugin shipped with.
/// - `api`: the plugin API version the manifest declared.
///
/// # Errors
///
/// Returns [`InstallError::DisallowedImport`] for an import the host does not
/// provide, [`InstallError::ApiMismatch`] when the component's interfaces carry
/// a different version than the manifest declared,
/// [`InstallError::MissingExport`] when an interface the manifest requires is
/// not exported, [`InstallError::UndeclaredExport`] when an export is not
/// declared, and [`InstallError::UnsupportedCapability`] when the plugin
/// provides a layer this server cannot host yet.
pub fn validate(
    component: &Component,
    manifest: &Manifest,
    api: ApiVersion,
) -> Result<(), InstallError> {
    let engine = component.engine();
    let component_type = component.component_type();

    for (name, item) in component_type.imports(engine) {
        if !matches!(item.ty, ComponentItem::ComponentInstance(_)) {
            return Err(InstallError::DisallowedImport(name.to_owned()));
        }
        let (interface, version) = split_version(name);
        if interface.starts_with("hearthsound:plugin/") && version != api.package() {
            return Err(api_mismatch(api, version));
        }
        if !allowed_import(interface, version, manifest, api) {
            return Err(InstallError::DisallowedImport(name.to_owned()));
        }
    }

    let mut exports_lifecycle = false;
    let mut exports_account_link = false;
    let mut exported_layers = Vec::new();
    for (name, _item) in component_type.exports(engine) {
        let (interface, version) = split_version(name);
        let Some(export) = interface.strip_prefix("hearthsound:plugin/") else {
            return Err(InstallError::UndeclaredExport(name.to_owned()));
        };
        if version != api.package() {
            return Err(api_mismatch(api, version));
        }
        match export {
            "lifecycle" => exports_lifecycle = true,
            "catalog" => exported_layers.push(Layer::Catalog),
            "metadata" => exported_layers.push(Layer::Metadata),
            "acquisition" => exported_layers.push(Layer::Acquisition),
            "artwork" => exported_layers.push(Layer::Artwork),
            "verification" => exported_layers.push(Layer::Verification),
            "scrobble" => exported_layers.push(Layer::Scrobble),
            "discovery" => exported_layers.push(Layer::Discovery),
            "account-link" => exports_account_link = true,
            _ => return Err(InstallError::UndeclaredExport(name.to_owned())),
        }
    }

    if !exports_lifecycle {
        return Err(InstallError::MissingExport {
            interface: lifecycle_interface(api),
        });
    }
    // `account-link` is checked first because it is a capability rather than a
    // layer: a manifest that declares no layer it belongs to is refused for the
    // export itself, not for a layer it names.
    if exports_account_link
        && !manifest
            .provides()
            .iter()
            .any(|layer| matches!(layer, Layer::Scrobble | Layer::Discovery))
    {
        return Err(InstallError::UndeclaredExport(format!(
            "{ACCOUNT_LINK_INTERFACE}@{}",
            api.package()
        )));
    }
    for layer in manifest.provides() {
        if !exported_layers.contains(layer) {
            return Err(InstallError::MissingExport {
                interface: format!("hearthsound:plugin/{layer}@{}", api.package()),
            });
        }
    }
    for layer in &exported_layers {
        if !manifest.provides().contains(layer) {
            return Err(InstallError::UndeclaredExport(format!(
                "hearthsound:plugin/{layer}@{}",
                api.package()
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

/// Builds the error for a component whose interface version disagrees with the
/// manifest.
///
/// # Parameters
///
/// - `api`: the version the manifest declared.
/// - `version`: the version the component's interface carries.
fn api_mismatch(api: ApiVersion, version: &str) -> InstallError {
    InstallError::ApiMismatch {
        declared: api.api().to_owned(),
        component: format!("hearthsound:plugin@{version}"),
    }
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
///
/// `host-files` is only provided to acquisition plugins, and `host-process`
/// only to acquisition plugins whose manifest lists a tool to run.
///
/// # Parameters
///
/// - `interface`: the interface id without its version.
/// - `version`: the version the component's interface carries.
/// - `manifest`: the manifest the plugin shipped with.
/// - `api`: the plugin API version the manifest declared.
fn allowed_import(interface: &str, version: &str, manifest: &Manifest, api: ApiVersion) -> bool {
    if let Some(rest) = interface.strip_prefix("hearthsound:plugin/") {
        if version != api.package() {
            return false;
        }
        return match rest {
            "types" | "host-log" | "host-http" | "host-kv" | "host-config" => true,
            "host-files" => manifest.provides().contains(&Layer::Acquisition),
            "host-process" => {
                manifest.provides().contains(&Layer::Acquisition)
                    && manifest
                        .permissions
                        .process
                        .as_ref()
                        .is_some_and(|process| !process.tools.is_empty())
            }
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
