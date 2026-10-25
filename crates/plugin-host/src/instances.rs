//! Creating, updating, and assigning provider instances.
//!
//! An instance is the unit an administrator configures, and configuring it is
//! not just a write: the server-scoped configuration is checked against the
//! plugin's own JSON Schema, missing defaults are filled in, fields the schema
//! marks `writeOnly` are sealed with the server's secret key, the host-level
//! settings are checked for import roots that exist, and finally the plugin
//! itself is asked whether the configuration works through
//! `lifecycle.validate-config`. Only a configuration that passes all of that is
//! stored.
//!
//! Secrets are handled so that plaintext exists in as few places as possible.
//! A sealed field is stored as `{"$secret": {"ciphertext": <base64>,
//! "nonce": <base64>}}`, the ciphertext being the AES-256-GCM seal of the
//! field's JSON text. Reading a configuration back for display replaces every
//! sealed object with [`SECRET_PLACEHOLDER`] without decrypting anything, and
//! the only place plaintext is produced is the JSON handed to the plugin. A
//! validation failure names the field it came from and never prints a restored
//! secret, and [`seal_plaintext_write_only`] upgrades a field an older plugin
//! version left in the clear.

use std::collections::HashSet;
use std::sync::Arc;

use data_encoding::BASE64;
use hearthsound_config::secret::{SecretBox, SecretError};
use hearthsound_core::{InvalidSlug, Layer, ProviderError, Slug};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::{HostError, InstallError};
use crate::files::STAGING_ROOT;
use crate::host::{InstalledPlugin, PluginHost};
use crate::instance::{ImportRoot, InstanceConfig, InstanceHostConfig};
use crate::manifest::Manifest;
use crate::repo::{Assignment, InstanceRow, PluginRepository, RepoError};
use crate::repo::{new_instance_id, now_millis};
use crate::validate::HOSTED_LAYERS;

/// The key a sealed secret is stored under.
const SEALED_KEY: &str = "$secret";

/// The key of a sealed secret's ciphertext.
const CIPHERTEXT_KEY: &str = "ciphertext";

/// The key of a sealed secret's nonce.
const NONCE_KEY: &str = "nonce";

/// What a read of a stored secret returns in place of the secret.
pub const SECRET_PLACEHOLDER: &str = "[sealed]";

/// What an administrator supplies to create or reconfigure an instance.
///
/// The plugin is fixed at creation: an instance is an instance of one plugin.
/// `server_config` and `host_config` are the JSON objects an administrator
/// would type, without the plugin's defaults applied; validation fills those in.
#[derive(Clone, PartialEq)]
pub struct InstanceSpec {
    /// The instance slug, `^[a-z0-9]{2,24}$`, unique among the server's
    /// instances and immutable once created.
    pub slug: String,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// The name shown in the admin UI.
    pub display_name: String,
    /// The plugin's server-scoped configuration, as an object.
    pub server_config: Value,
    /// The host-level settings, as an object, or `null` for the defaults.
    pub host_config: Value,
}

impl std::fmt::Debug for InstanceSpec {
    /// Reports the specification without the plugin's configuration, which may
    /// hold secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceSpec")
            .field("slug", &self.slug)
            .field("plugin_id", &self.plugin_id)
            .field("display_name", &self.display_name)
            .field("server_config", &"<redacted>")
            .field("host_config", &self.host_config)
            .finish()
    }
}

/// One configured instance, as read back for display.
///
/// The server-scoped configuration carries [`SECRET_PLACEHOLDER`] for every
/// field the plugin's schema marks `writeOnly`, so a view is safe to render,
/// log, and hand to a client.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceView {
    /// The instance id.
    pub id: Uuid,
    /// The instance slug.
    pub slug: String,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// The name shown in the admin UI.
    pub display_name: String,
    /// Whether the instance takes part in its layers.
    pub enabled: bool,
    /// The server-scoped configuration, with secrets replaced.
    pub server_config: Value,
    /// The host-level settings.
    pub host_config: InstanceHostConfig,
    /// When the instance was created, in unix milliseconds.
    pub created_at: i64,
    /// When the instance was last changed, in unix milliseconds.
    pub updated_at: i64,
}

/// An error from configuring an instance or a layer.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    /// The slug is not a legal slug.
    #[error(transparent)]
    InvalidSlug(#[from] InvalidSlug),
    /// The database reported an error.
    #[error(transparent)]
    Repository(#[from] RepoError),
    /// No plugin with that id is installed.
    #[error("plugin `{0}` is not installed")]
    UnknownPlugin(String),
    /// The plugin is installed but disabled, so no instance can be configured
    /// from it.
    #[error("plugin `{0}` is disabled")]
    PluginDisabled(String),
    /// Another instance already uses the slug.
    #[error("slug `{0}` is already taken")]
    SlugTaken(String),
    /// No instance with that slug exists.
    #[error("no provider instance with slug `{0}`")]
    UnknownInstance(String),
    /// An update tried to move an instance to another plugin.
    #[error("instance `{instance}` is configured from plugin `{plugin}`, which cannot be changed")]
    PluginMismatch {
        /// The instance slug.
        instance: String,
        /// The plugin the instance is configured from.
        plugin: String,
    },
    /// The stored version is not a version this host can load.
    #[error("plugin `{plugin}` has stored version `{version}`, which is not SemVer")]
    Version {
        /// The plugin id.
        plugin: String,
        /// The stored version.
        version: String,
    },
    /// The installed plugin could not be loaded.
    #[error(transparent)]
    Load(#[from] InstallError),
    /// The plugin host refused to drive the plugin.
    #[error(transparent)]
    Host(#[from] HostError),
    /// The server-scoped configuration is not a usable object.
    #[error("server configuration: {0}")]
    ServerConfig(String),
    /// The server-scoped configuration does not match the plugin's schema.
    #[error("server configuration does not match the plugin's schema: {}", .0.join("; "))]
    Schema(Vec<String>),
    /// The plugin rejected the configuration in `lifecycle.validate-config`.
    #[error("the plugin rejected the configuration: {}", .0.join("; "))]
    ValidateConfig(Vec<String>),
    /// The plugin's `lifecycle.validate-config` could not be called.
    #[error("calling `lifecycle.validate-config` failed: {0}")]
    ValidateFailed(ProviderError),
    /// The host-level settings are not usable.
    #[error("host configuration: {0}")]
    HostConfig(String),
    /// A secret could not be sealed or opened.
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// The same instance was listed twice in one layer.
    #[error("instance `{0}` is assigned to the layer twice")]
    DuplicateInstance(String),
    /// The plugin's stored manifest cannot be read.
    #[error("the stored manifest of plugin `{plugin}` cannot be read: {reason}")]
    StoredManifest {
        /// The plugin id.
        plugin: String,
        /// Why the manifest could not be read.
        reason: String,
    },
    /// The plugin's stored manifest does not provide the layer.
    #[error("plugin `{plugin}` does not provide the `{layer}` layer")]
    LayerNotProvided {
        /// The plugin id.
        plugin: String,
        /// The layer that was requested.
        layer: Layer,
    },
    /// This server cannot host the plugin's layers.
    #[error("this server cannot host `{0}` plugins yet")]
    UnsupportedCapability(Layer),
    /// The plugin provides no layer this server can drive.
    #[error("plugin `{0}` provides no layer this server can drive")]
    NoHostedLayer(String),
}

/// Configures provider instances and the layers they are assigned to.
///
/// The service reads and writes the plugin tables through a
/// [`PluginRepository`], loads the plugin's component through the
/// [`PluginHost`] when it has to run `lifecycle.validate-config`, and seals and
/// opens secrets with the server's [`SecretBox`].
#[derive(Clone)]
pub struct InstanceService {
    host: Arc<PluginHost>,
    repository: PluginRepository,
    secrets: SecretBox,
}

impl InstanceService {
    /// Builds the service over a host, a repository, and the server's secret key.
    ///
    /// # Parameters
    ///
    /// - `host`: the plugin host, used to load the component a configuration is
    ///   validated against.
    /// - `repository`: the plugin tables.
    /// - `secrets`: the box that seals and opens the write-only configuration
    ///   fields.
    pub fn new(host: Arc<PluginHost>, repository: PluginRepository, secrets: SecretBox) -> Self {
        InstanceService {
            host,
            repository,
            secrets,
        }
    }

    /// Returns the repositories the service works through.
    pub fn repository(&self) -> &PluginRepository {
        &self.repository
    }

    /// Creates an instance and enables it.
    ///
    /// The plugin must be installed and enabled, the slug must be free, the
    /// server-scoped configuration must satisfy the plugin's schema (with the
    /// schema's defaults filled in and its write-only fields sealed), the
    /// import roots must be existing absolute directories, and the plugin must
    /// accept the configuration in `lifecycle.validate-config`.
    ///
    /// # Parameters
    ///
    /// - `spec`: what the administrator configured.
    ///
    /// # Returns
    ///
    /// The created instance, with secrets replaced by
    /// [`SECRET_PLACEHOLDER`].
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError`] when the slug is not legal or already taken,
    /// the plugin is missing or disabled, the configuration is rejected, or the
    /// write fails.
    pub async fn create(&self, spec: &InstanceSpec) -> Result<InstanceView, InstanceError> {
        let slug = Slug::new(&spec.slug)?;
        if self
            .repository
            .instance_by_slug(slug.as_str())
            .await?
            .is_some()
        {
            return Err(InstanceError::SlugTaken(spec.slug.clone()));
        }

        let plugin = self.load_plugin(&spec.plugin_id).await?;
        let prepared = self.prepare(spec, &plugin, &Value::Null).await?;

        let now = now_millis();
        let row = InstanceRow {
            id: new_instance_id(),
            plugin_id: spec.plugin_id.clone(),
            slug: slug.as_str().to_owned(),
            display_name: spec.display_name.clone(),
            server_config: Some(prepared.server_config),
            host_config: Some(prepared.host_config_json),
            enabled: true,
            health: None,
            created_at: now,
            updated_at: now,
        };
        self.repository.insert_instance(&row).await?;
        Ok(view(&row, prepared.host_config))
    }

    /// Replaces one instance's name and configuration.
    ///
    /// The configuration is validated exactly as it is on creation, against the
    /// same plugin and the same schema, and it is applied as a patch: a field it
    /// leaves out keeps the value the instance stores, so a caller changing one
    /// setting does not have to send every other one back and cannot lose a
    /// secret by forgetting it. A read returns [`SECRET_PLACEHOLDER`] in place of
    /// a secret, so an update may echo the placeholder back: such a field keeps
    /// the secret the instance stores, which is opened for the plugin and sealed
    /// again at rest. An explicit empty string on such a field removes it. A
    /// field that echoes the placeholder while nothing is stored for it is
    /// refused.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance to update.
    /// - `spec`: the new configuration. Its `plugin_id` must name the instance's
    ///   plugin.
    ///
    /// # Returns
    ///
    /// The updated instance.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError`] for the same reasons [`InstanceService::create`]
    /// does, and [`InstanceError::PluginMismatch`] when the specification names
    /// another plugin.
    pub async fn update(
        &self,
        slug: &str,
        spec: &InstanceSpec,
    ) -> Result<InstanceView, InstanceError> {
        let row = self.instance_row(slug).await?;
        if row.plugin_id != spec.plugin_id {
            return Err(InstanceError::PluginMismatch {
                instance: row.slug,
                plugin: row.plugin_id,
            });
        }

        let previous = match row.server_config.as_deref() {
            Some(text) => serde_json::from_str(text).map_err(|error| {
                InstanceError::ServerConfig(format!(
                    "the stored configuration cannot be read: {error}"
                ))
            })?,
            None => Value::Null,
        };
        let plugin = self.load_plugin(&row.plugin_id).await?;
        let prepared = self.prepare(spec, &plugin, &previous).await?;

        self.repository
            .update_instance(
                row.id,
                &spec.display_name,
                &prepared.server_config,
                &prepared.host_config_json,
                now_millis(),
            )
            .await?;

        let updated = InstanceRow {
            display_name: spec.display_name.clone(),
            server_config: Some(prepared.server_config),
            host_config: Some(prepared.host_config_json),
            updated_at: now_millis(),
            ..row
        };
        Ok(view(&updated, prepared.host_config))
    }

    /// Returns every instance, ordered by slug.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Repository`] when the rows cannot be read and
    /// [`InstanceError::HostConfig`] when a stored host configuration cannot be
    /// read.
    pub async fn list(&self) -> Result<Vec<InstanceView>, InstanceError> {
        let mut views = Vec::new();
        for row in self.repository.instances().await? {
            let host_config = parse_host_config_text(row.host_config.as_deref())?;
            views.push(view(&row, host_config));
        }
        Ok(views)
    }

    /// Returns one instance by slug.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownInstance`] when no instance has the slug.
    pub async fn get(&self, slug: &str) -> Result<InstanceView, InstanceError> {
        let row = self.instance_row(slug).await?;
        let host_config = parse_host_config_text(row.host_config.as_deref())?;
        Ok(view(&row, host_config))
    }

    /// Enables or disables one instance.
    ///
    /// A disabled instance keeps its configuration, its key-value store, and its
    /// layer position; it simply does not take part in its layers until it is
    /// enabled again.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance to change.
    /// - `enabled`: whether the instance takes part in its layers.
    ///
    /// # Returns
    ///
    /// The instance after the change.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownInstance`] when no instance has the slug.
    pub async fn set_enabled(
        &self,
        slug: &str,
        enabled: bool,
    ) -> Result<InstanceView, InstanceError> {
        let row = self.instance_row(slug).await?;
        self.repository
            .set_instance_enabled(row.id, enabled, now_millis())
            .await?;
        let host_config = parse_host_config_text(row.host_config.as_deref())?;
        Ok(view(
            &InstanceRow {
                enabled,
                updated_at: now_millis(),
                ..row
            },
            host_config,
        ))
    }

    /// Deletes one instance.
    ///
    /// The instance's key-value store and its layer assignments go with it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance to delete.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownInstance`] when no instance has the slug.
    pub async fn delete(&self, slug: &str) -> Result<(), InstanceError> {
        let row = self.instance_row(slug).await?;
        self.repository.delete_instance(row.id).await?;
        Ok(())
    }

    /// Replaces one layer's ordered instances.
    ///
    /// The order is the order the layer's policy tries the instances in, so the
    /// first slug is the primary. Every instance must exist and its plugin must
    /// provide the layer. The whole layer is written in one transaction.
    ///
    /// # Parameters
    ///
    /// - `layer`: the layer to configure.
    /// - `slugs`: the instance slugs, in order.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownInstance`] for a slug no instance uses,
    /// [`InstanceError::DuplicateInstance`] when a slug appears twice,
    /// [`InstanceError::LayerNotProvided`] when an instance's plugin does not
    /// provide the layer, and [`InstanceError::Repository`] when the write
    /// fails.
    pub async fn set_layer(&self, layer: Layer, slugs: &[String]) -> Result<(), InstanceError> {
        let mut seen = HashSet::new();
        let mut ids = Vec::with_capacity(slugs.len());
        for slug in slugs {
            if !seen.insert(slug.as_str()) {
                return Err(InstanceError::DuplicateInstance(slug.clone()));
            }
            let row = self.instance_row(slug).await?;
            let plugin = self
                .repository
                .plugin(&row.plugin_id)
                .await?
                .ok_or_else(|| InstanceError::UnknownPlugin(row.plugin_id.clone()))?;
            if !stored_provides(&plugin.id, &plugin.manifest)?.contains(&layer) {
                return Err(InstanceError::LayerNotProvided {
                    plugin: row.plugin_id,
                    layer,
                });
            }
            ids.push(row.id);
        }

        self.repository.set_layer_assignments(layer, &ids).await?;
        Ok(())
    }

    /// Returns every layer assignment, ordered by layer and position.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Repository`] when the rows cannot be read.
    pub async fn assignments(&self) -> Result<Vec<Assignment>, InstanceError> {
        Ok(self.repository.assignments().await?)
    }

    /// Loads the installed plugin of an enabled plugin row.
    async fn load_plugin(&self, plugin_id: &str) -> Result<InstalledPlugin, InstanceError> {
        let row = self
            .repository
            .plugin(plugin_id)
            .await?
            .ok_or_else(|| InstanceError::UnknownPlugin(plugin_id.to_owned()))?;
        if !row.enabled {
            return Err(InstanceError::PluginDisabled(plugin_id.to_owned()));
        }
        let version = semver::Version::parse(&row.version).map_err(|_| InstanceError::Version {
            plugin: row.id.clone(),
            version: row.version.clone(),
        })?;
        Ok(self.host.load(&row.id, &version).await?)
    }

    /// Reads one instance row or reports that it does not exist.
    async fn instance_row(&self, slug: &str) -> Result<InstanceRow, InstanceError> {
        self.repository
            .instance_by_slug(slug)
            .await?
            .ok_or_else(|| InstanceError::UnknownInstance(slug.to_owned()))
    }

    /// Validates a specification and produces the values to store.
    ///
    /// # Parameters
    ///
    /// - `spec`: what the administrator configured.
    /// - `plugin`: the installed plugin the configuration belongs to.
    /// - `previous`: the configuration the instance stores now, which fills in
    ///   the secrets a configuration echoes as the sealed placeholder, or
    ///   `null` when there is none.
    ///
    /// # Errors
    ///
    /// Returns what sealing the configuration and validating it against the
    /// plugin can return.
    async fn prepare(
        &self,
        spec: &InstanceSpec,
        plugin: &InstalledPlugin,
        previous: &Value,
    ) -> Result<Prepared, InstanceError> {
        let manifest = plugin.manifest();
        let sealed = seal_server_config(manifest, &spec.server_config, &self.secrets, previous)?;
        let (host_config, host_config_json) = parse_host_config(&spec.host_config)?;

        let layer = hosted_layer(manifest)
            .ok_or_else(|| InstanceError::NoHostedLayer(manifest.id.clone()))?;
        let temporary = InstanceConfig::new(Slug::new(&spec.slug)?)
            .with_id(new_instance_id())
            .with_server_config(sealed.plain)
            .with_permissions(manifest.permissions.clone())
            .with_host_config(host_config.clone());
        // The plugin is handed the restored secrets, so its own complaints can
        // quote them; every problem is scrubbed before it leaves this module.
        match self.validate_config(plugin, &temporary, layer).await {
            Ok(()) => {}
            Err(InstanceError::ValidateConfig(mut problems)) => {
                for problem in &mut problems {
                    redact_secrets(problem, &sealed.restored);
                }
                return Err(InstanceError::ValidateConfig(problems));
            }
            Err(error) => return Err(error),
        }

        Ok(Prepared {
            server_config: sealed.stored,
            host_config,
            host_config_json,
        })
    }

    /// Asks the plugin whether a configuration works.
    async fn validate_config(
        &self,
        plugin: &InstalledPlugin,
        instance: &InstanceConfig,
        layer: Layer,
    ) -> Result<(), InstanceError> {
        let config = instance.server_config.as_str();
        let problems = match layer {
            Layer::Catalog => {
                let provider = self.host.catalog_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Metadata => {
                let provider = self.host.metadata_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Acquisition => {
                let provider = self.host.acquisition_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Artwork => {
                let provider = self.host.artwork_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Verification => {
                let provider = self.host.verification_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Scrobble => {
                let provider = self.host.scrobble_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            Layer::Discovery => {
                let provider = self.host.discovery_provider(plugin, instance.clone())?;
                provider.validate_config(config).await
            }
            other => return Err(InstanceError::UnsupportedCapability(other)),
        };

        match problems {
            Ok(problems) if problems.is_empty() => Ok(()),
            Ok(problems) => Err(InstanceError::ValidateConfig(problems)),
            Err(error) => Err(InstanceError::ValidateFailed(error)),
        }
    }
}

/// The values one validated specification produces.
struct Prepared {
    server_config: String,
    host_config: InstanceHostConfig,
    host_config_json: String,
}

/// Builds the view of one row.
fn view(row: &InstanceRow, host_config: InstanceHostConfig) -> InstanceView {
    InstanceView {
        id: row.id,
        slug: row.slug.clone(),
        plugin_id: row.plugin_id.clone(),
        display_name: row.display_name.clone(),
        enabled: row.enabled,
        server_config: redact_server_config(row.server_config.as_deref()),
        host_config,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Returns the first layer of the plugin that this server can drive.
fn hosted_layer(manifest: &Manifest) -> Option<Layer> {
    HOSTED_LAYERS
        .iter()
        .copied()
        .find(|layer| manifest.provides().contains(layer))
}

/// The layers a stored manifest declares.
#[derive(Deserialize)]
struct StoredProvides {
    #[serde(default)]
    provides: Vec<String>,
}

/// Reads the layers a stored manifest declares.
///
/// Layer assignment does not need the component, only the manifest, and the
/// manifest is in the plugin row, so assigning a layer works even when the
/// plugin's files are temporarily unreadable.
fn stored_provides(plugin: &str, manifest: &str) -> Result<Vec<Layer>, InstanceError> {
    let stored: StoredProvides =
        serde_json::from_str(manifest).map_err(|error| InstanceError::StoredManifest {
            plugin: plugin.to_owned(),
            reason: error.to_string(),
        })?;
    stored
        .provides
        .iter()
        .map(|name| {
            name.parse::<Layer>()
                .map_err(|_| InstanceError::StoredManifest {
                    plugin: plugin.to_owned(),
                    reason: format!("`{name}` is not a known layer"),
                })
        })
        .collect()
}

/// Validates the host-level settings and serializes them.
///
/// Import root paths must be absolute and must exist, because a root the
/// administrator names must be a directory the server can hand to a plugin; ids
/// must be unique and may not be the reserved [`STAGING_ROOT`], because the
/// host resolves that id to the current call's staging directory before any
/// import root is consulted, which would make a root with that id unreachable.
fn parse_host_config(input: &Value) -> Result<(InstanceHostConfig, String), InstanceError> {
    let value = if input.is_null() {
        Value::Object(Map::new())
    } else {
        input.clone()
    };
    let config: InstanceHostConfig = serde_json::from_value(value)
        .map_err(|error| InstanceError::HostConfig(error.to_string()))?;

    let mut ids = HashSet::new();
    for root in &config.import_roots {
        if root.id.is_empty() {
            return Err(InstanceError::HostConfig(
                "an import root has an empty id".to_owned(),
            ));
        }
        if root.id == STAGING_ROOT {
            return Err(InstanceError::HostConfig(format!(
                "import root id `{}` is reserved",
                root.id
            )));
        }
        if !ids.insert(root.id.as_str()) {
            return Err(InstanceError::HostConfig(format!(
                "import root id `{}` is used twice",
                root.id
            )));
        }
        validate_root(root)?;
    }

    let json = serde_json::to_string(&config)
        .map_err(|error| InstanceError::HostConfig(error.to_string()))?;
    Ok((config, json))
}

/// Checks one import root's path.
fn validate_root(root: &ImportRoot) -> Result<(), InstanceError> {
    if !root.path.is_absolute() {
        return Err(InstanceError::HostConfig(format!(
            "import root `{}` has the relative path `{}`",
            root.id,
            root.path.display()
        )));
    }
    match std::fs::metadata(&root.path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(InstanceError::HostConfig(format!(
            "import root `{}` path `{}` is not a directory",
            root.id,
            root.path.display()
        ))),
        Err(error) => Err(InstanceError::HostConfig(format!(
            "import root `{}` path `{}`: {error}",
            root.id,
            root.path.display()
        ))),
    }
}

/// Reads the host-level settings back out of a stored instance.
fn parse_host_config_text(text: Option<&str>) -> Result<InstanceHostConfig, InstanceError> {
    let Some(text) = text else {
        return Ok(InstanceHostConfig::default());
    };
    serde_json::from_str(text).map_err(|error| InstanceError::HostConfig(error.to_string()))
}

/// Validates, merges, defaults, and seals one instance's server-scoped
/// configuration.
///
/// The configuration is a patch, not a replacement: a field the input leaves
/// out keeps the value the stored configuration holds, so a caller that changes
/// one setting does not have to send every other one back and cannot lose a
/// secret by forgetting it. A write-only field that carries
/// [`SECRET_PLACEHOLDER`] keeps the secret the stored configuration holds for
/// it, and a write-only field whose input is an empty string is removed, which
/// is how a caller clears a secret. Every other value replaces the stored one.
///
/// A validation failure names the field and never prints a restored secret: the
/// schema's messages are scrubbed of every restored value, and a failure of the
/// restored field itself is reported by its pointer and keyword only.
///
/// # Parameters
///
/// - `manifest`: the plugin's manifest, whose server-scoped schema the
///   configuration is checked against and which names the write-only fields.
/// - `input`: the configuration the administrator sent.
/// - `secrets`: the box the write-only fields are sealed with.
/// - `previous`: the configuration the instance stores now, or `null` when it
///   has none.
///
/// # Returns
///
/// The configuration to store, with write-only fields sealed, the same
/// configuration with those fields in plaintext for
/// `lifecycle.validate-config`, and the values that were restored from the
/// stored configuration by pointer.
///
/// # Errors
///
/// Returns [`InstanceError::ServerConfig`] when the configuration is not an
/// object, the plugin declares no server-scoped section, or a field echoes the
/// placeholder while no secret is stored for it,
/// [`InstanceError::Schema`] when it does not match the plugin's schema, and
/// [`InstanceError::Secret`] when a stored secret cannot be opened or a field
/// cannot be sealed.
fn seal_server_config(
    manifest: &Manifest,
    input: &Value,
    secrets: &SecretBox,
    previous: &Value,
) -> Result<SealedConfig, InstanceError> {
    let Some(section) = manifest.config.server.as_ref() else {
        if input.as_object().is_some_and(Map::is_empty) {
            return Ok(SealedConfig {
                stored: "{}".to_owned(),
                plain: "{}".to_owned(),
                restored: Vec::new(),
            });
        }
        return Err(InstanceError::ServerConfig(
            "the plugin declares no server-scoped configuration".to_owned(),
        ));
    };
    let schema = &section.schema;

    if !input.is_object() {
        return Err(InstanceError::ServerConfig(
            "the configuration must be a JSON object".to_owned(),
        ));
    }

    let mut config = merge_config(previous, input);
    apply_defaults(schema, &mut config);

    // A configuration that was read, edited and sent back carries the
    // placeholder where a secret belongs; those fields keep the secret the
    // instance stores, so validation sees the configuration as it will be used
    // and the plugin is handed the secret in the clear.
    let mut restored: Vec<(String, Value)> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut failure = None;
    visit_write_only(schema, &mut config, &mut |path, value| {
        if value.as_str() == Some("") {
            // An explicit empty string removes the field, which is how a caller
            // clears a secret it no longer wants stored.
            removed.push(path.to_owned());
            return;
        }
        // A field the patch left out arrives still sealed from the stored
        // configuration, and one it sent as the placeholder has to be looked up
        // there; both are opened for validation and for the plugin.
        let sealed = if sealed_value(value) {
            Some(value.clone())
        } else if value.as_str() == Some(SECRET_PLACEHOLDER) {
            previous
                .pointer(path)
                .filter(|stored| sealed_value(stored))
                .cloned()
        } else {
            None
        };
        let Some(sealed) = sealed else {
            if value.as_str() == Some(SECRET_PLACEHOLDER) {
                failure = Some(InstanceError::ServerConfig(format!(
                    "`{path}` is the sealed placeholder, but the instance stores no secret for it"
                )));
            }
            return;
        };
        match open_value(&sealed, secrets) {
            Ok(opened) => {
                restored.push((path.to_owned(), opened.clone()));
                *value = opened;
            }
            Err(error) => failure = Some(error),
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    for path in &removed {
        remove_pointer(&mut config, path);
    }

    let validator = jsonschema::draft7::new(schema).map_err(|error| {
        InstanceError::ServerConfig(format!("the plugin's schema does not compile: {error}"))
    })?;
    let problems: Vec<String> = validator
        .iter_errors(&config)
        .map(|error| describe_schema_problem(&error, &restored))
        .collect();
    if !problems.is_empty() {
        return Err(InstanceError::Schema(problems));
    }

    let plain = serde_json::to_string(&config).map_err(RepoError::from)?;
    let mut stored = config;
    let mut failure = None;
    visit_write_only(
        schema,
        &mut stored,
        &mut |_path, value| match seal_value(value, secrets) {
            Ok(sealed) => *value = sealed,
            Err(error) => failure = Some(error),
        },
    );
    if let Some(error) = failure {
        return Err(error);
    }

    let stored = serde_json::to_string(&stored).map_err(RepoError::from)?;
    Ok(SealedConfig {
        stored,
        plain,
        restored,
    })
}

/// What sealing one configuration produced.
#[derive(Debug)]
struct SealedConfig {
    /// The configuration to store, with write-only fields sealed.
    stored: String,
    /// The same configuration with those fields in plaintext, for the plugin.
    plain: String,
    /// The values restored from the stored configuration, by JSON pointer.
    restored: Vec<(String, Value)>,
}

/// Merges a configuration patch onto the stored configuration.
///
/// Objects merge key by key, so a field the patch omits keeps its stored value;
/// every other value - an array, a string, a number - replaces the stored one.
/// A stored configuration that is not an object is ignored, so a patch for a new
/// instance is taken as it stands.
///
/// # Parameters
///
/// - `previous`: the configuration the instance stores, or `null`.
/// - `input`: the configuration the administrator sent.
///
/// # Returns
///
/// The merged configuration.
fn merge_config(previous: &Value, input: &Value) -> Value {
    match (previous.as_object(), input.as_object()) {
        (Some(stored), Some(given)) => {
            let mut merged = stored.clone();
            for (name, value) in given {
                let merged_value = match merged.get(name) {
                    Some(previous) => merge_config(previous, value),
                    None => value.clone(),
                };
                merged.insert(name.clone(), merged_value);
            }
            Value::Object(merged)
        }
        _ => input.clone(),
    }
}

/// Removes the value one JSON pointer addresses.
///
/// A pointer that names the whole document, or one whose parent is not an
/// object, removes nothing.
///
/// # Parameters
///
/// - `value`: the document to change.
/// - `pointer`: the JSON pointer of the value to remove.
fn remove_pointer(value: &mut Value, pointer: &str) {
    let Some((parent, name)) = pointer.rsplit_once('/') else {
        return;
    };
    let Some(object) = value.pointer_mut(parent).and_then(Value::as_object_mut) else {
        return;
    };
    object.remove(&unescape_segment(name));
}

/// Reads one JSON pointer segment back into the property name it came from.
///
/// # Parameters
///
/// - `segment`: the escaped segment.
///
/// # Returns
///
/// The property name.
fn unescape_segment(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

/// Describes one schema violation without printing a restored secret.
///
/// A violation whose instance location is the restored field, or inside it, is
/// reported by its pointer and keyword only, because the value the schema saw is
/// the secret. Every other violation starts from the schema's own message and is
/// scrubbed of every restored value, which covers a parent-object failure such
/// as `oneOf` that embeds the whole object in its text.
///
/// # Parameters
///
/// - `error`: the violation.
/// - `restored`: the values restored from the stored configuration.
///
/// # Returns
///
/// The message to report.
fn describe_schema_problem(
    error: &jsonschema::ValidationError<'_>,
    restored: &[(String, Value)],
) -> String {
    let at = error.instance_path().as_str();
    for (pointer, _) in restored {
        let inside = at == pointer
            || (at.len() > pointer.len()
                && at.starts_with(pointer)
                && at.as_bytes().get(pointer.len()) == Some(&b'/'));
        if inside {
            return format!(
                "{pointer}: the stored secret does not satisfy the plugin's schema (`{}`), so re-enter the value instead of `{SECRET_PLACEHOLDER}`",
                error.kind().keyword()
            );
        }
    }
    let mut text = error.to_string();
    redact_secrets(&mut text, restored);
    text
}

/// Replaces every occurrence of a restored secret in one message.
///
/// Both the JSON text of a value and the value's own display are replaced, and
/// an empty needle is skipped so a restored empty string cannot blank a message.
///
/// # Parameters
///
/// - `text`: the message to scrub, in place.
/// - `restored`: the values restored from the stored configuration.
fn redact_secrets(text: &mut String, restored: &[(String, Value)]) {
    for (_, value) in restored {
        let mut needles: Vec<String> = serde_json::to_string(value).into_iter().collect();
        needles.push(value.to_string());
        if let Some(literal) = value.as_str() {
            needles.push(literal.to_owned());
        }
        // Longest first, so a JSON-quoted needle is replaced before its bare text
        // can split it.
        needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
        for needle in needles {
            if needle.is_empty() || needle == "\"\"" {
                continue;
            }
            if text.contains(&needle) {
                *text = text.replace(&needle, SECRET_PLACEHOLDER);
            }
        }
    }
}

/// Seals every write-only field of a stored configuration that is still in the
/// clear.
///
/// A plugin that starts marking a field `writeOnly` - or one that marked it only
/// after an instance was configured - leaves the stored configuration holding
/// the value as plain JSON. This walk upgrades such a field in place, so a
/// rebuild after a plugin upgrade needs no operator step. It is idempotent: a
/// field that is already the stored representation of a secret is left byte for
/// byte as it is.
///
/// # Parameters
///
/// - `schema`: the plugin's server-scoped schema, which names the write-only
///   fields.
/// - `config`: the stored configuration, changed in place.
/// - `secrets`: the box the fields are sealed with.
///
/// # Returns
///
/// How many fields were sealed.
///
/// # Errors
///
/// Returns [`InstanceError::Secret`] when a field cannot be sealed, and
/// [`InstanceError::ServerConfig`] when the configuration cannot be serialized.
pub(crate) fn seal_plaintext_write_only(
    schema: &Value,
    config: &mut Value,
    secrets: &SecretBox,
) -> Result<usize, InstanceError> {
    let mut sealed = 0usize;
    let mut failure = None;
    visit_write_only(schema, config, &mut |_path, value| {
        if sealed_value(value) || value.is_null() {
            return;
        }
        match seal_value(value, secrets) {
            Ok(wrapper) => {
                *value = wrapper;
                sealed += 1;
            }
            Err(error) => failure = Some(error),
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(sealed)
}

/// Seals one value into the stored representation of a secret.
pub(crate) fn seal_value(value: &Value, secrets: &SecretBox) -> Result<Value, InstanceError> {
    let plaintext = serde_json::to_vec(value).map_err(RepoError::from)?;
    let (ciphertext, nonce) = secrets.seal(&plaintext)?;
    let mut sealed = Map::new();
    sealed.insert(
        CIPHERTEXT_KEY.to_owned(),
        Value::String(BASE64.encode(&ciphertext)),
    );
    sealed.insert(NONCE_KEY.to_owned(), Value::String(BASE64.encode(&nonce)));
    let mut wrapper = Map::new();
    wrapper.insert(SEALED_KEY.to_owned(), Value::Object(sealed));
    Ok(Value::Object(wrapper))
}

/// Returns whether a value is the stored representation of a secret.
fn sealed_value(value: &Value) -> bool {
    value
        .get(SEALED_KEY)
        .and_then(|sealed| sealed.get(CIPHERTEXT_KEY))
        .and_then(Value::as_str)
        .is_some()
}

/// Replaces every stored secret in a configuration with
/// [`SECRET_PLACEHOLDER`].
///
/// Nothing is decrypted: a read shows the administrator that a field is set
/// without ever holding its plaintext.
fn redact_server_config(stored: Option<&str>) -> Value {
    let Some(stored) = stored else {
        return Value::Object(Map::new());
    };
    let mut config: Value =
        serde_json::from_str(stored).unwrap_or_else(|_| Value::Object(Map::new()));
    replace_sealed(&mut config, &mut |value| {
        *value = Value::String(SECRET_PLACEHOLDER.to_owned());
    });
    config
}

/// Opens a stored configuration into the JSON a plugin receives.
///
/// Every sealed field is decrypted and put back in place, so the result is the
/// configuration as the administrator saved it.
///
/// # Parameters
///
/// - `stored`: the instance's `server_config` column, as stored.
/// - `secrets`: the box the configuration was sealed with.
///
/// # Errors
///
/// Returns [`InstanceError::Secret`] when a sealed field does not authenticate,
/// and [`InstanceError::ServerConfig`] when the stored text is not the JSON this
/// module wrote.
pub(crate) fn open_server_config(
    stored: Option<&str>,
    secrets: &SecretBox,
) -> Result<String, InstanceError> {
    let Some(stored) = stored else {
        return Ok("{}".to_owned());
    };
    let mut config: Value = serde_json::from_str(stored)
        .map_err(|error| InstanceError::ServerConfig(error.to_string()))?;

    let mut failure = None;
    replace_sealed(&mut config, &mut |value| match open_value(value, secrets) {
        Ok(opened) => *value = opened,
        Err(error) => failure = Some(error),
    });
    if let Some(error) = failure {
        return Err(error);
    }

    serde_json::to_string(&config).map_err(|error| InstanceError::ServerConfig(error.to_string()))
}

/// Opens one stored secret back into its plaintext value.
pub(crate) fn open_value(value: &Value, secrets: &SecretBox) -> Result<Value, InstanceError> {
    let sealed = value.get(SEALED_KEY);
    let ciphertext = sealed
        .and_then(|sealed| sealed.get(CIPHERTEXT_KEY))
        .and_then(Value::as_str);
    let nonce = sealed
        .and_then(|sealed| sealed.get(NONCE_KEY))
        .and_then(Value::as_str);
    let (Some(ciphertext), Some(nonce)) = (ciphertext, nonce) else {
        return Err(InstanceError::ServerConfig(
            "a sealed field is missing its ciphertext or nonce".to_owned(),
        ));
    };
    let ciphertext = BASE64.decode(ciphertext.as_bytes()).map_err(|error| {
        InstanceError::ServerConfig(format!("a sealed field is not base64: {error}"))
    })?;
    let nonce = BASE64
        .decode(nonce.as_bytes())
        .map_err(|error| InstanceError::ServerConfig(format!("a nonce is not base64: {error}")))?;
    let plaintext = secrets.open(&ciphertext, &nonce)?;
    serde_json::from_slice(&plaintext).map_err(|error| {
        InstanceError::ServerConfig(format!("a sealed field is not JSON: {error}"))
    })
}

/// Fills in the defaults a schema declares for missing object properties.
///
/// Defaults are applied before validation, so a property the schema gives a
/// default is never reported missing, and the plugin sees the same value the
/// administrator's form showed.
///
/// # Parameters
///
/// - `schema`: the JSON Schema to read defaults from.
/// - `config`: the configuration to fill in.
pub(crate) fn apply_defaults(schema: &Value, config: &mut Value) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        if let (Some(item_schema), Some(items)) = (schema.get("items"), config.as_array_mut()) {
            for item in items {
                apply_defaults(item_schema, item);
            }
        }
        return;
    };
    let Some(object) = config.as_object_mut() else {
        return;
    };
    for (name, property) in properties {
        match object.get_mut(name) {
            Some(value) => apply_defaults(property, value),
            None => {
                if let Some(default) = property.get("default") {
                    object.insert(name.clone(), default.clone());
                }
            }
        }
    }
}

/// Replaces every value the schema marks `writeOnly`.
///
/// The walk follows `properties` and array `items`, so a secret may be a
/// property of the object, of a nested object, or of an array element. The
/// closure is handed the JSON pointer of the field it is given, so a caller can
/// find the same field in another configuration.
fn visit_write_only(schema: &Value, value: &mut Value, replace: &mut impl FnMut(&str, &mut Value)) {
    visit_write_only_at(schema, value, "", replace);
}

/// Walks one level of a configuration, carrying the pointer of its position.
fn visit_write_only_at(
    schema: &Value,
    value: &mut Value,
    pointer: &str,
    replace: &mut impl FnMut(&str, &mut Value),
) {
    if let Some(object) = value.as_object_mut() {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return;
        };
        for (name, property) in properties {
            let Some(field) = object.get_mut(name) else {
                continue;
            };
            let pointer = pointer_child(pointer, name);
            if property.get("writeOnly").and_then(Value::as_bool) == Some(true) {
                replace(&pointer, field);
            } else {
                visit_write_only_at(property, field, &pointer, replace);
            }
        }
        return;
    }
    if let Some(items) = value.as_array_mut()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in items.iter_mut().enumerate() {
            let pointer = pointer_child(pointer, &index.to_string());
            visit_write_only_at(item_schema, item, &pointer, replace);
        }
    }
}

/// Appends one segment to a JSON pointer.
///
/// The escapes are the ones JSON pointers define, so a property name holding a
/// `/` or a `~` still addresses the field it came from.
fn pointer_child(pointer: &str, name: &str) -> String {
    let mut child = String::with_capacity(pointer.len() + name.len() + 1);
    child.push_str(pointer);
    child.push('/');
    for character in name.chars() {
        match character {
            '~' => child.push_str("~0"),
            '/' => child.push_str("~1"),
            other => child.push(other),
        }
    }
    child
}

/// Replaces every stored secret in a configuration.
fn replace_sealed(value: &mut Value, replace: &mut impl FnMut(&mut Value)) {
    if let Some(object) = value.as_object_mut() {
        for (_, field) in object.iter_mut() {
            if sealed_value(field) {
                replace(field);
            } else {
                replace_sealed(field, replace);
            }
        }
        return;
    }
    if let Some(items) = value.as_array_mut() {
        for item in items {
            if sealed_value(item) {
                replace(item);
            } else {
                replace_sealed(item, replace);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_config::secret::SecretKey;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": {"type": "string"},
                "apiKey": {"type": "string", "writeOnly": true},
                "timeout": {"type": "integer", "default": 15000},
                "nested": {
                    "type": "object",
                    "properties": {"token": {"type": "string", "writeOnly": true}}
                }
            }
        })
    }

    #[test]
    fn defaults_are_applied_to_missing_properties() {
        let mut config = json!({"url": "http://example.com"});
        apply_defaults(&schema(), &mut config);
        assert_eq!(config["timeout"], json!(15000));
    }

    #[test]
    fn defaults_do_not_overwrite_given_values() {
        let mut config = json!({"url": "http://example.com", "timeout": 5});
        apply_defaults(&schema(), &mut config);
        assert_eq!(config["timeout"], json!(5));
    }

    #[test]
    fn write_only_fields_round_trip_through_the_secret_box() {
        let secrets = SecretKey::generate().secret_box();
        let mut config = json!({
            "url": "http://example.com",
            "apiKey": "hunter2",
            "nested": {"token": "abc"}
        });
        let mut failure = None;
        visit_write_only(
            &schema(),
            &mut config,
            &mut |_path, value| match seal_value(value, &secrets) {
                Ok(sealed) => *value = sealed,
                Err(error) => failure = Some(error),
            },
        );
        assert!(failure.is_none());
        assert!(sealed_value(&config["apiKey"]));
        assert!(sealed_value(&config["nested"]["token"]));
        assert!(!config.to_string().contains("hunter2"));

        let stored = config.to_string();
        let config = redact_server_config(Some(&stored));
        assert_eq!(config["apiKey"], json!(SECRET_PLACEHOLDER));
        assert_eq!(config["nested"]["token"], json!(SECRET_PLACEHOLDER));

        let opened: Value =
            serde_json::from_str(&open_server_config(Some(&stored), &secrets).unwrap()).unwrap();
        assert_eq!(opened["apiKey"], json!("hunter2"));
        assert_eq!(opened["nested"]["token"], json!("abc"));
    }

    #[test]
    fn a_configuration_that_breaks_the_schema_is_reported() {
        let error = seal_server_config(&manifest(), &json!({"url": 7}), &boxed(), &Value::Null)
            .unwrap_err();
        assert!(matches!(error, InstanceError::Schema(_)), "{error}");
    }

    #[test]
    fn an_echoed_placeholder_keeps_the_stored_secret() {
        let secrets = boxed();
        let sealed = seal_server_config(
            &manifest(),
            &json!({
                "url": "http://example.com",
                "apiKey": "hunter2",
                "nested": {"token": "abc"}
            }),
            &secrets,
            &Value::Null,
        )
        .unwrap();
        let previous: Value = serde_json::from_str(&sealed.stored).unwrap();

        // The configuration an administrator read and sent back carries the
        // placeholder where the secrets are.
        let echoed = json!({
            "url": "http://other.example",
            "apiKey": SECRET_PLACEHOLDER,
            "nested": {"token": SECRET_PLACEHOLDER}
        });
        let sealed = seal_server_config(&manifest(), &echoed, &secrets, &previous).unwrap();

        // The plugin is handed the secrets in the clear, and they are stored
        // sealed again rather than as the placeholder.
        let plain: Value = serde_json::from_str(&sealed.plain).unwrap();
        assert_eq!(plain["apiKey"], json!("hunter2"));
        assert_eq!(plain["nested"]["token"], json!("abc"));
        let stored = sealed.stored;
        let opened: Value =
            serde_json::from_str(&open_server_config(Some(&stored), &secrets).unwrap()).unwrap();
        assert_eq!(opened["apiKey"], json!("hunter2"));
        assert_eq!(opened["nested"]["token"], json!("abc"));
        assert!(!stored.contains(SECRET_PLACEHOLDER), "{stored}");

        // A field that is given a new value is sealed like any other.
        let changed = json!({
            "url": "http://example.com",
            "apiKey": "hunter3",
            "nested": {"token": SECRET_PLACEHOLDER}
        });
        let sealed = seal_server_config(&manifest(), &changed, &secrets, &previous).unwrap();
        let plain: Value = serde_json::from_str(&sealed.plain).unwrap();
        assert_eq!(plain["apiKey"], json!("hunter3"));
        assert_eq!(plain["nested"]["token"], json!("abc"));
        let opened: Value =
            serde_json::from_str(&open_server_config(Some(&sealed.stored), &secrets).unwrap())
                .unwrap();
        assert_eq!(opened["apiKey"], json!("hunter3"));
    }

    #[test]
    fn an_omitted_field_keeps_its_stored_value() {
        let secrets = boxed();
        let sealed = seal_server_config(
            &manifest(),
            &json!({
                "url": "http://example.com/one",
                "apiKey": "hunter2",
                "nested": {"token": "abc"}
            }),
            &secrets,
            &Value::Null,
        )
        .unwrap();
        let previous: Value = serde_json::from_str(&sealed.stored).unwrap();

        // A patch that changes one setting and says nothing about the secrets
        // keeps them: forgetting a field cannot destroy a credential.
        let patch = json!({"url": "http://example.com/two"});
        let sealed = seal_server_config(&manifest(), &patch, &secrets, &previous).unwrap();
        let plain: Value = serde_json::from_str(&sealed.plain).unwrap();
        assert_eq!(plain["url"], json!("http://example.com/two"));
        assert_eq!(plain["apiKey"], json!("hunter2"));
        assert_eq!(plain["nested"]["token"], json!("abc"));
        // A value the schema gives a default keeps the stored one too.
        assert_eq!(plain["timeout"], json!(15000));
        assert!(!sealed.stored.contains("hunter2"), "{}", sealed.stored);
    }

    #[test]
    fn an_empty_string_removes_a_write_only_field() {
        let secrets = boxed();
        let sealed = seal_server_config(
            &manifest(),
            &json!({"url": "http://example.com", "apiKey": "hunter2"}),
            &secrets,
            &Value::Null,
        )
        .unwrap();
        let previous: Value = serde_json::from_str(&sealed.stored).unwrap();

        let patch = json!({"apiKey": ""});
        let sealed = seal_server_config(&manifest(), &patch, &secrets, &previous).unwrap();
        let plain: Value = serde_json::from_str(&sealed.plain).unwrap();
        assert!(plain.get("apiKey").is_none(), "{plain}");
        let opened: Value =
            serde_json::from_str(&open_server_config(Some(&sealed.stored), &secrets).unwrap())
                .unwrap();
        assert!(opened.get("apiKey").is_none(), "{opened}");
        assert_eq!(opened["url"], json!("http://example.com"));
    }

    #[test]
    fn a_schema_failure_never_prints_a_restored_secret() {
        let secrets = boxed();
        let lenient = manifest_with(json!({
            "type": "object",
            "properties": {"apiKey": {"type": "string", "writeOnly": true}}
        }));
        let sealed = seal_server_config(
            &lenient,
            &json!({"apiKey": "short-secret"}),
            &secrets,
            &Value::Null,
        )
        .unwrap();
        let previous: Value = serde_json::from_str(&sealed.stored).unwrap();

        // The plugin's next version tightens the schema; the stored secret no
        // longer satisfies it, and the plugin is handed the secret in the
        // clear, so the schema's own message would quote it.
        let strict = manifest_with(json!({
            "type": "object",
            "properties": {"apiKey": {"type": "string", "writeOnly": true, "minLength": 40}}
        }));
        let error = seal_server_config(
            &strict,
            &json!({"apiKey": SECRET_PLACEHOLDER}),
            &secrets,
            &previous,
        )
        .unwrap_err();
        assert!(matches!(error, InstanceError::Schema(_)), "{error}");
        let text = error.to_string();
        assert!(text.contains("/apiKey"), "{text}");
        assert!(text.contains(SECRET_PLACEHOLDER), "{text}");
        assert!(!text.contains("short-secret"), "{text}");
    }

    #[test]
    fn a_nested_schema_failure_never_prints_a_restored_secret() {
        let secrets = boxed();
        let lenient = manifest_with(json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": {"token": {"type": "string", "writeOnly": true}}
                }
            }
        }));
        let sealed = seal_server_config(
            &lenient,
            &json!({"nested": {"token": "plain-token"}}),
            &secrets,
            &Value::Null,
        )
        .unwrap();
        let previous: Value = serde_json::from_str(&sealed.stored).unwrap();

        let strict = manifest_with(json!({
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": {
                        "token": {"type": "string", "writeOnly": true, "pattern": "^[0-9]+$"}
                    }
                }
            }
        }));
        let error = seal_server_config(
            &strict,
            &json!({"nested": {"token": SECRET_PLACEHOLDER}}),
            &secrets,
            &previous,
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("/nested/token"), "{text}");
        assert!(!text.contains("plain-token"), "{text}");
    }

    #[test]
    fn a_restored_secret_is_scrubbed_in_both_of_its_forms() {
        let restored = vec![("/nested/token".to_owned(), json!("se\"cret"))];
        let json_form = serde_json::to_string(&json!("se\"cret")).unwrap();
        let mut text = format!("the value se\"cret and {json_form} are wrong");
        redact_secrets(&mut text, &restored);
        assert!(!text.contains("se\\\"cret"), "{text}");
        assert!(!text.contains("se\"cret"), "{text}");
        assert!(text.contains(SECRET_PLACEHOLDER), "{text}");
    }

    #[test]
    fn plaintext_write_only_fields_are_sealed_once() {
        let secrets = boxed();
        let mut config = json!({
            "url": "http://example.com",
            "apiKey": "hunter2",
            "nested": {"token": "abc"}
        });
        let sealed = seal_value(&json!("kept"), &secrets).unwrap();
        config["nested"]["token"] = sealed.clone();

        // One plaintext field and one already sealed: only the plaintext one is
        // sealed, and the sealed wrapper is untouched byte for byte.
        let count = seal_plaintext_write_only(&schema(), &mut config, &secrets).unwrap();
        assert_eq!(count, 1);
        assert!(sealed_value(&config["apiKey"]));
        assert_eq!(config["nested"]["token"], sealed);
        assert!(!config.to_string().contains("hunter2"), "{config}");

        // Idempotent: a second pass finds nothing to seal.
        let count = seal_plaintext_write_only(&schema(), &mut config, &secrets).unwrap();
        assert_eq!(count, 0);
        assert_eq!(config["nested"]["token"], sealed);
    }

    #[test]
    fn a_placeholder_without_a_stored_secret_is_refused() {
        let error = seal_server_config(
            &manifest(),
            &json!({"url": "http://example.com", "apiKey": SECRET_PLACEHOLDER}),
            &boxed(),
            &json!({"url": "http://example.com"}),
        )
        .unwrap_err();
        assert!(matches!(error, InstanceError::ServerConfig(_)), "{error}");
        assert!(error.to_string().contains("/apiKey"), "{error}");
    }

    #[test]
    fn the_staging_import_root_id_is_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let error = parse_host_config(&json!({
            "importRoots": [{"id": "staging", "path": path, "mode": "copy", "readOnly": false}]
        }))
        .unwrap_err();
        assert!(matches!(error, InstanceError::HostConfig(_)), "{error}");
        assert!(error.to_string().contains("staging"), "{error}");

        parse_host_config(&json!({
            "importRoots": [{"id": "downloads", "path": path, "mode": "copy", "readOnly": false}]
        }))
        .unwrap();
    }

    #[test]
    fn import_roots_must_be_absolute_and_exist() {
        let error = parse_host_config(&json!({
            "importRoots": [{"id": "a", "path": "relative", "mode": "move", "readOnly": false}]
        }))
        .unwrap_err();
        assert!(matches!(error, InstanceError::HostConfig(_)), "{error}");

        let error = parse_host_config(&json!({
            "importRoots": [{"id": "a", "path": "/nope/nope", "mode": "move"}]
        }))
        .unwrap_err();
        assert!(matches!(error, InstanceError::HostConfig(_)), "{error}");
    }

    #[test]
    fn import_root_ids_must_be_unique() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let error = parse_host_config(&json!({
            "importRoots": [
                {"id": "a", "path": path, "mode": "move"},
                {"id": "a", "path": path, "mode": "copy"}
            ]
        }))
        .unwrap_err();
        assert!(matches!(error, InstanceError::HostConfig(_)), "{error}");
    }

    #[test]
    fn host_configuration_defaults_are_filled_in() {
        let (config, json) = parse_host_config(&Value::Null).unwrap();
        assert_eq!(config, InstanceHostConfig::default());
        assert_eq!(config.max_concurrent, 2);
        assert_eq!(config.max_concurrent_searches, 2);
        assert_eq!(config.searches_per_hour, 120);
        assert!(json.contains("\"maxConcurrent\":2"));
    }

    /// The manifest the sealing tests validate against.
    fn manifest() -> Manifest {
        manifest_with(schema())
    }

    /// The manifest the sealing tests validate against, over one schema.
    fn manifest_with(schema: Value) -> Manifest {
        let json = json!({
            "manifestVersion": 1,
            "id": "com.example.plugin",
            "name": "Example",
            "version": "0.1.0",
            "description": "An example plugin.",
            "license": "MIT",
            "homepage": "https://example.com",
            "api": "hearthsound:plugin@0.4.0",
            "provides": ["catalog"],
            "permissions": {},
            "config": {"server": {"schema": schema, "uiSchema": {}}, "user": null},
            "resources": {"memoryMb": 128, "callTimeoutMs": 30000, "maxInstances": 2}
        });
        let settings = std::sync::Arc::new(
            hearthsound_config::SettingsStore::resolve(
                &hearthsound_config::SettingsRegistry::builtin(),
                &hearthsound_config::SettingSources::default(),
            )
            .unwrap(),
        );
        Manifest::parse(json.to_string().as_bytes(), &settings).unwrap()
    }

    /// A secret box over a throwaway key.
    fn boxed() -> SecretBox {
        SecretKey::generate().secret_box()
    }
}
