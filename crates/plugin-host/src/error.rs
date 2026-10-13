//! Errors of the plugin host, and the conversion from a guest's error.

use std::path::PathBuf;

use hearthsound_config::ConfigError;
use hearthsound_core::{Layer, ProviderError, Slug};

use crate::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::manifest::ManifestError;

/// An error from installing or loading a plugin.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// A file could not be read or written.
    #[error("io error for `{path}`: {source}")]
    Io {
        /// The file the operation touched.
        path: PathBuf,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// The archive is not a readable zip.
    #[error("archive error: {0}")]
    Archive(#[from] zip::result::ZipError),
    /// A required archive entry is absent.
    #[error("the archive has no `{0}` entry")]
    MissingEntry(&'static str),
    /// An archive entry is larger than the host accepts.
    #[error("archive entry `{name}` is {size} bytes, above the {limit} byte limit")]
    EntryTooLarge {
        /// The entry name.
        name: String,
        /// The size the entry declares.
        size: u64,
        /// The largest size the host accepts.
        limit: u64,
    },
    /// The manifest is missing, unreadable, or invalid.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The plugin was built against an API version this server does not host.
    #[error("plugin API `{api}` is not supported; this server hosts hearthsound:plugin@0.1.0")]
    UnsupportedApi {
        /// The API version the plugin declared.
        api: String,
    },
    /// The component failed to compile.
    #[error("failed to compile the component: {0:#}")]
    Compile(wasmtime::Error),
    /// The component imports an interface the host does not provide.
    #[error("the component imports `{0}`, which this host does not provide")]
    DisallowedImport(String),
    /// The component does not export an interface its manifest requires.
    #[error("the component does not export `{interface}`")]
    MissingExport {
        /// The interface that should have been exported.
        interface: String,
    },
    /// The component exports an interface its manifest does not declare.
    #[error("the component exports `{0}`, which its manifest does not declare")]
    UndeclaredExport(String),
    /// The plugin provides a layer this server cannot host yet.
    #[error("this server cannot host `{0}` plugins yet")]
    UnsupportedCapability(Layer),
    /// A plugin with the same id and version is already installed.
    #[error("plugin `{id}` version `{version}` is already installed")]
    AlreadyInstalled {
        /// The plugin id.
        id: String,
        /// The plugin version.
        version: String,
    },
    /// The component does not type-check against the host's world.
    #[error("the component is incompatible with this host: {0}")]
    Incompatible(String),
}

/// An error from setting up or configuring the plugin host itself.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    /// The wasmtime engine or linker could not be built.
    #[error("failed to set up the plugin engine: {0:#}")]
    Engine(wasmtime::Error),
    /// The component does not type-check against the host's world.
    #[error("the component is incompatible with this host: {0}")]
    Incompatible(String),
    /// The instance's outbound HTTP client could not be built.
    #[error("failed to build the instance's HTTP client: {0}")]
    Http(String),
    /// A setting could not be read.
    #[error(transparent)]
    Settings(#[from] ConfigError),
}

/// Converts a guest's error into the core error.
///
/// Every provider world reports the same error variant, so every adapter maps it
/// here. The core error carries no message, so the plugin's own text is logged
/// with the plugin id and instance slug rather than being dropped silently.
///
/// # Parameters
///
/// - `plugin`: the plugin id, used as the log field.
/// - `slug`: the instance slug, used as the log field.
/// - `error`: the error the guest returned.
pub(crate) fn map_provider_error(
    plugin: &str,
    slug: &Slug,
    error: WitProviderError,
) -> ProviderError {
    let (kind, message, mapped) = match error {
        WitProviderError::NotFound => return ProviderError::NotFound,
        WitProviderError::RateLimited(millis) => {
            return ProviderError::RateLimited {
                retry_after_ms: u64::from(millis),
            };
        }
        WitProviderError::Unauthorized(message) => {
            ("unauthorized", message, ProviderError::Unauthorized)
        }
        WitProviderError::InvalidConfig(message) => {
            ("invalid-config", message, ProviderError::InvalidConfig)
        }
        WitProviderError::Unavailable(message) => {
            ("unavailable", message, ProviderError::Unavailable)
        }
        WitProviderError::Internal(message) => ("internal", message, ProviderError::Internal),
    };
    tracing::debug!(
        target: "hearthsound::plugin",
        plugin = %plugin,
        instance = %slug,
        kind,
        message,
        "plugin reported an error"
    );
    mapped
}
