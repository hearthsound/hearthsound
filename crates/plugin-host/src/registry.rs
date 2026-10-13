//! The provider registry: which instances serve which layer, in which order.
//!
//! The rest of the server never asks the database who can serve a layer. It
//! asks the registry, which holds one entry per configured provider instance
//! and, per layer, the enabled instances in the order the administrator chose,
//! each with the trait object its layer is driven through.
//!
//! Rebuilding is the only mutation. The registry is built from the plugin
//! tables, so a change an administrator makes from another process - the CLI -
//! becomes visible when the server notices `PRAGMA data_version` move and calls
//! [`ProviderRegistry::rebuild`]. Compiled components are cached per plugin and
//! version, so a rebuild that changes nothing but a configuration reuses what is
//! already in memory, and one component is shared by every instance of a plugin.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use hearthsound_config::secret::SecretBox;
use hearthsound_core::providers::{AcquisitionProvider, CatalogProvider, MetadataProvider};
use hearthsound_core::{Layer, Slug};

use crate::host::{InstalledPlugin, PluginHost};
use crate::instance::{InstanceConfig, InstanceHostConfig};
use crate::instances::open_server_config;
use crate::manifest::{ImageCachePolicy, Manifest, Permissions};
use crate::repo::{InstanceRow, PluginRepository, PluginRow, RepoError};

/// One configured instance, as the layers see it.
///
/// The entry carries everything a caller needs to know about the instance and
/// the trait objects it can be driven through. The server-scoped configuration
/// is held in plaintext, because that is what the plugin is handed, so it is
/// deliberately neither a public field nor part of [`std::fmt::Debug`].
pub struct RegistryInstance {
    id: uuid::Uuid,
    slug: Slug,
    plugin_id: String,
    plugin_version: String,
    manifest: Manifest,
    host_config: InstanceHostConfig,
    catalog: Option<Arc<dyn CatalogProvider>>,
    metadata: Option<Arc<dyn MetadataProvider>>,
    acquisition: Option<Arc<dyn AcquisitionProvider>>,
}

impl RegistryInstance {
    /// Returns the instance id, the key its key-value rows and events use.
    pub fn id(&self) -> uuid::Uuid {
        self.id
    }

    /// Returns the instance slug.
    pub fn slug(&self) -> &Slug {
        &self.slug
    }

    /// Returns the plugin the instance is configured from.
    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    /// Returns the installed version of the plugin.
    pub fn plugin_version(&self) -> &str {
        &self.plugin_version
    }

    /// Returns the plugin's manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Returns the host-level settings the administrator chose.
    pub fn host_config(&self) -> &InstanceHostConfig {
        &self.host_config
    }

    /// Returns how the host treats cover images this instance returns.
    ///
    /// A plugin that declares no catalog section - one that is not a catalog
    /// provider - keeps the default policy.
    pub fn image_cache_policy(&self) -> ImageCachePolicy {
        self.manifest
            .catalog
            .map(|catalog| catalog.image_cache_policy)
            .unwrap_or_default()
    }

    /// Returns the catalog provider of this instance, when it provides catalog.
    pub fn catalog(&self) -> Option<&Arc<dyn CatalogProvider>> {
        self.catalog.as_ref()
    }

    /// Returns the metadata provider of this instance, when it provides
    /// metadata.
    pub fn metadata(&self) -> Option<&Arc<dyn MetadataProvider>> {
        self.metadata.as_ref()
    }

    /// Returns the acquisition provider of this instance, when it provides
    /// acquisition.
    pub fn acquisition(&self) -> Option<&Arc<dyn AcquisitionProvider>> {
        self.acquisition.as_ref()
    }
}

impl std::fmt::Debug for RegistryInstance {
    /// Reports the entry without the instance's configuration, which is the
    /// plugin's plaintext configuration.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryInstance")
            .field("id", &self.id)
            .field("slug", &self.slug)
            .field("plugin_id", &self.plugin_id)
            .field("plugin_version", &self.plugin_version)
            .field("host_config", &self.host_config)
            .field("server_config", &"<redacted>")
            .field(
                "providers",
                &(
                    self.catalog.is_some(),
                    self.metadata.is_some(),
                    self.acquisition.is_some(),
                ),
            )
            .finish()
    }
}

/// One instance a rebuild could not put into the registry.
///
/// A skipped instance stays in the database and keeps its configuration; it is
/// simply not served, and the next rebuild tries it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedInstance {
    /// The instance slug.
    pub slug: String,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// Why the instance could not be built.
    pub reason: String,
}

/// What one rebuild produced.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RegistryReport {
    /// How many instances the registry now serves.
    pub instances: usize,
    /// The instances that could not be built, with the reason.
    pub skipped: Vec<SkippedInstance>,
}

/// The instances of every layer, with their provider trait objects.
///
/// A registry is shared by every caller that resolves a provider, so it is
/// mutated through interior locks: a rebuild replaces the entries and every
/// reader sees the new set. Entries are handed out as `Arc`s, so a caller that
/// is mid-call keeps the provider it started with.
pub struct ProviderRegistry {
    host: Arc<PluginHost>,
    repository: PluginRepository,
    secrets: SecretBox,
    components: Mutex<HashMap<(String, String), Arc<InstalledPlugin>>>,
    state: RwLock<RegistryState>,
}

/// The entries a registry currently holds.
#[derive(Default)]
struct RegistryState {
    layers: BTreeMap<Layer, Vec<Arc<RegistryInstance>>>,
    by_slug: HashMap<String, Arc<RegistryInstance>>,
}

impl ProviderRegistry {
    /// Builds an empty registry over the plugin tables.
    ///
    /// The registry is empty until [`ProviderRegistry::rebuild`] runs, so a
    /// server can construct it before the database holds anything.
    ///
    /// # Parameters
    ///
    /// - `host`: the plugin host the components are loaded from.
    /// - `repository`: the plugin tables the registry is built from.
    /// - `secrets`: the box instance configurations are opened with.
    pub fn new(
        host: Arc<PluginHost>,
        repository: PluginRepository,
        secrets: SecretBox,
    ) -> ProviderRegistry {
        ProviderRegistry {
            host,
            repository,
            secrets,
            components: Mutex::new(HashMap::new()),
            state: RwLock::new(RegistryState::default()),
        }
    }

    /// Rebuilds every layer from the plugin tables.
    ///
    /// The layers are rebuilt from `layer_assignments`, so only instances an
    /// administrator assigned take part. An instance is included when it is
    /// enabled and its plugin is enabled; every other instance is left out, and
    /// an instance whose component cannot be loaded or whose configuration
    /// cannot be read is skipped and reported rather than failing the rebuild.
    ///
    /// # Returns
    ///
    /// What the rebuild produced, including every instance it skipped and why.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError`] when the plugin tables cannot be read. A failure
    /// leaves the previous entries in place.
    pub async fn rebuild(&self) -> Result<RegistryReport, RepoError> {
        let assignments = self.repository.assignments().await?;
        let instances: HashMap<uuid::Uuid, InstanceRow> = self
            .repository
            .instances()
            .await?
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        let plugins: HashMap<String, PluginRow> = self
            .repository
            .plugins()
            .await?
            .into_iter()
            .map(|row| (row.id.clone(), row))
            .collect();

        let mut order: BTreeMap<Layer, Vec<uuid::Uuid>> = BTreeMap::new();
        for assignment in assignments {
            order
                .entry(assignment.layer)
                .or_default()
                .push(assignment.instance_id);
        }

        let mut report = RegistryReport::default();
        let mut built: HashMap<uuid::Uuid, Arc<RegistryInstance>> = HashMap::new();
        let mut used: HashSet<(String, String)> = HashSet::new();

        for ids in order.values() {
            for id in ids {
                if built.contains_key(id) {
                    continue;
                }
                let Some(row) = instances.get(id) else {
                    report.skipped.push(SkippedInstance {
                        slug: id.to_string(),
                        plugin_id: String::new(),
                        reason: "the instance is assigned to a layer but does not exist".to_owned(),
                    });
                    continue;
                };
                let Some(plugin) = plugins.get(&row.plugin_id) else {
                    report.skipped.push(SkippedInstance {
                        slug: row.slug.clone(),
                        plugin_id: row.plugin_id.clone(),
                        reason: "the instance's plugin is not installed".to_owned(),
                    });
                    continue;
                };
                if !row.enabled || !plugin.enabled {
                    continue;
                }
                let layers = assigned_layers(&order, row.id);
                match self.build(row, plugin, &layers, &mut used).await {
                    Ok(instance) => {
                        built.insert(*id, Arc::new(instance));
                    }
                    Err(reason) => report.skipped.push(SkippedInstance {
                        slug: row.slug.clone(),
                        plugin_id: row.plugin_id.clone(),
                        reason,
                    }),
                }
            }
        }

        let mut state = self.state.write().unwrap_or_else(poisoned);
        state.layers = BTreeMap::new();
        state.by_slug = HashMap::new();
        for (layer, ids) in &order {
            let entries: Vec<Arc<RegistryInstance>> =
                ids.iter().filter_map(|id| built.get(id).cloned()).collect();
            for entry in &entries {
                state
                    .by_slug
                    .insert(entry.slug.as_str().to_owned(), Arc::clone(entry));
            }
            state.layers.insert(*layer, entries);
        }
        report.instances = state.by_slug.len();
        drop(state);

        self.components
            .lock()
            .unwrap_or_else(poisoned)
            .retain(|key, _| used.contains(key));

        Ok(report)
    }

    /// Returns the instances of one layer, in the order the layer tries them.
    ///
    /// The vector is a snapshot: a rebuild after the call does not change it,
    /// which is what lets a caller walk the layer while a rebuild happens.
    ///
    /// # Parameters
    ///
    /// - `layer`: the layer to read.
    pub fn layers(&self, layer: Layer) -> Vec<Arc<RegistryInstance>> {
        self.read().layers.get(&layer).cloned().unwrap_or_default()
    }

    /// Returns the instance with `slug`, when the registry serves it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    pub fn instance(&self, slug: &str) -> Option<Arc<RegistryInstance>> {
        self.read().by_slug.get(slug).cloned()
    }

    /// Builds one instance's entry.
    async fn build(
        &self,
        row: &InstanceRow,
        plugin: &PluginRow,
        layers: &[Layer],
        used: &mut HashSet<(String, String)>,
    ) -> Result<RegistryInstance, String> {
        let component = self.component(plugin, used).await?;
        let manifest = component.manifest();
        let slug = Slug::new(&row.slug).map_err(|error| error.to_string())?;
        let host_config: InstanceHostConfig = match row.host_config.as_deref() {
            Some(text) => serde_json::from_str(text)
                .map_err(|error| format!("host configuration: {error}"))?,
            None => InstanceHostConfig::default(),
        };
        let server_config = open_server_config(row.server_config.as_deref(), &self.secrets)
            .map_err(|error| error.to_string())?;
        let permissions = approved_permissions(plugin)?;

        let instance = InstanceConfig::new(slug)
            .with_id(row.id)
            .with_server_config(server_config)
            .with_permissions(permissions)
            .with_host_config(host_config.clone());

        let mut catalog = None;
        let mut metadata = None;
        let mut acquisition = None;
        for layer in layers {
            match layer {
                Layer::Catalog => {
                    catalog = Some(
                        self.host
                            .catalog_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                Layer::Metadata => {
                    metadata = Some(
                        self.host
                            .metadata_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                Layer::Acquisition => {
                    acquisition = Some(
                        self.host
                            .acquisition_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                other => {
                    return Err(format!("this server cannot host `{other}` plugins yet"));
                }
            }
        }

        Ok(RegistryInstance {
            id: row.id,
            slug: instance.slug.clone(),
            plugin_id: plugin.id.clone(),
            plugin_version: plugin.version.clone(),
            manifest: manifest.clone(),
            host_config,
            catalog,
            metadata,
            acquisition,
        })
    }

    /// Returns the compiled component of a plugin, loading it once.
    async fn component(
        &self,
        plugin: &PluginRow,
        used: &mut HashSet<(String, String)>,
    ) -> Result<Arc<InstalledPlugin>, String> {
        let key = (plugin.id.clone(), plugin.version.clone());
        used.insert(key.clone());
        if let Some(component) = self
            .components
            .lock()
            .unwrap_or_else(poisoned)
            .get(&key)
            .cloned()
        {
            return Ok(component);
        }

        let version = semver::Version::parse(&plugin.version)
            .map_err(|_| format!("`{}` is not a SemVer version", plugin.version))?;
        let component = self
            .host
            .load(&plugin.id, &version)
            .await
            .map_err(|error| format!("loading `{}`: {error}", plugin.id))?;

        let component = Arc::new(component);
        self.components
            .lock()
            .unwrap_or_else(poisoned)
            .insert(key, Arc::clone(&component));
        Ok(component)
    }

    /// Reads the current entries.
    fn read(&self) -> RwLockReadGuard<'_, RegistryState> {
        self.state.read().unwrap_or_else(poisoned)
    }
}

/// Extracts a read guard from a lock whose poisoning is not fatal.
///
/// A poisoned lock means another thread panicked while holding it; the entries
/// are still structurally sound, and refusing to answer would turn one panic
/// into a permanently unusable registry.
fn poisoned<T>(poisoned: std::sync::PoisonError<T>) -> T {
    poisoned.into_inner()
}

/// The layers an instance is assigned to, in layer order.
fn assigned_layers(order: &BTreeMap<Layer, Vec<uuid::Uuid>>, id: uuid::Uuid) -> Vec<Layer> {
    let mut layers: Vec<Layer> = order
        .iter()
        .filter(|(_, ids)| ids.contains(&id))
        .map(|(layer, _)| *layer)
        .collect();
    layers.sort();
    layers
}

/// Reads the permissions an administrator approved for a plugin.
///
/// The approved set is what the instance's imports check against, so it is the
/// row's value and not the manifest's: a plugin that was installed before the
/// administrator reviewed its permissions keeps the approved set.
fn approved_permissions(plugin: &PluginRow) -> Result<Permissions, String> {
    match plugin.approved_permissions.as_deref() {
        Some(json) => serde_json::from_str(json)
            .map_err(|error| format!("stored permissions of `{}`: {error}", plugin.id)),
        None => Ok(Permissions::default()),
    }
}
