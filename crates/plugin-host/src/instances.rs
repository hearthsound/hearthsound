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
//! the only place plaintext is produced is the JSON handed to the plugin.

use std::collections::HashSet;
use std::sync::Arc;

use data_encoding::BASE64;
use hearthsound_config::secret::{SecretBox, SecretError};
use hearthsound_core::{InvalidSlug, Layer, ProviderError, Slug};
use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::{HostError, InstallError};
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
        let prepared = self.prepare(spec, &plugin).await?;

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
    /// same plugin and the same schema. Because a read returns
    /// [`SECRET_PLACEHOLDER`] in place of a secret, an update that carries the
    /// placeholder back would store the placeholder as the secret; a caller
    /// that did not change a secret has to send its plaintext again.
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

        let plugin = self.load_plugin(&row.plugin_id).await?;
        let prepared = self.prepare(spec, &plugin).await?;

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
    async fn prepare(
        &self,
        spec: &InstanceSpec,
        plugin: &InstalledPlugin,
    ) -> Result<Prepared, InstanceError> {
        let manifest = plugin.manifest();
        let (server_config, plain) =
            seal_server_config(manifest, &spec.server_config, &self.secrets)?;
        let (host_config, host_config_json) = parse_host_config(&spec.host_config)?;

        let layer = hosted_layer(manifest)
            .ok_or_else(|| InstanceError::NoHostedLayer(manifest.id.clone()))?;
        let temporary = InstanceConfig::new(Slug::new(&spec.slug)?)
            .with_id(new_instance_id())
            .with_server_config(plain)
            .with_permissions(manifest.permissions.clone())
            .with_host_config(host_config.clone());
        self.validate_config(plugin, &temporary, layer).await?;

        Ok(Prepared {
            server_config,
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
/// must be unique, because the plugin names a root by id.
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

/// Validates, defaults, and seals one instance's server-scoped configuration.
///
/// # Returns
///
/// The configuration to store, with write-only fields sealed, and the same
/// configuration with those fields in plaintext, for `lifecycle.validate-config`.
fn seal_server_config(
    manifest: &Manifest,
    input: &Value,
    secrets: &SecretBox,
) -> Result<(String, String), InstanceError> {
    let Some(section) = manifest.config.server.as_ref() else {
        if input.as_object().is_some_and(Map::is_empty) {
            return Ok(("{}".to_owned(), "{}".to_owned()));
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

    let mut config = input.clone();
    apply_defaults(schema, &mut config);

    let validator = jsonschema::draft7::new(schema).map_err(|error| {
        InstanceError::ServerConfig(format!("the plugin's schema does not compile: {error}"))
    })?;
    let problems: Vec<String> = validator
        .iter_errors(&config)
        .map(|error| error.to_string())
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
        &mut |value| match seal_value(value, secrets) {
            Ok(sealed) => *value = sealed,
            Err(error) => failure = Some(error),
        },
    );
    if let Some(error) = failure {
        return Err(error);
    }

    let stored = serde_json::to_string(&stored).map_err(RepoError::from)?;
    Ok((stored, plain))
}

/// Seals one value into the stored representation of a secret.
fn seal_value(value: &Value, secrets: &SecretBox) -> Result<Value, InstanceError> {
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
fn open_value(value: &Value, secrets: &SecretBox) -> Result<Value, InstanceError> {
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
fn apply_defaults(schema: &Value, config: &mut Value) {
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
/// property of the object, of a nested object, or of an array element.
fn visit_write_only(schema: &Value, value: &mut Value, replace: &mut impl FnMut(&mut Value)) {
    if let Some(object) = value.as_object_mut() {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return;
        };
        for (name, property) in properties {
            let Some(field) = object.get_mut(name) else {
                continue;
            };
            if property.get("writeOnly").and_then(Value::as_bool) == Some(true) {
                replace(field);
            } else {
                visit_write_only(property, field, replace);
            }
        }
        return;
    }
    if let Some(items) = value.as_array_mut()
        && let Some(item_schema) = schema.get("items")
    {
        for item in items {
            visit_write_only(item_schema, item, replace);
        }
    }
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
            &mut |value| match seal_value(value, &secrets) {
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
        let error = seal_server_config(&manifest(), &json!({"url": 7}), &boxed()).unwrap_err();
        assert!(matches!(error, InstanceError::Schema(_)), "{error}");
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
        let json = json!({
            "manifestVersion": 1,
            "id": "com.example.plugin",
            "name": "Example",
            "version": "0.1.0",
            "description": "An example plugin.",
            "license": "MIT",
            "homepage": "https://example.com",
            "api": "hearthsound:plugin@0.2.0",
            "provides": ["catalog"],
            "permissions": {},
            "config": {"server": {"schema": schema(), "uiSchema": {}}, "user": null},
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
