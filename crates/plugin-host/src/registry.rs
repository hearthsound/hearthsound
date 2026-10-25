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
use hearthsound_core::providers::{
    AccountLinkProvider, AcquisitionProvider, ArtworkProvider, CatalogProvider, DiscoveryProvider,
    MetadataProvider, ScrobbleProvider, VerificationProvider,
};
use hearthsound_core::{Layer, ProviderError, Slug};

use crate::host::{InstalledPlugin, PluginHost, instance_http_client};
use crate::http::{HttpClient, HttpError, HttpRequest};
use crate::instance::{InstanceConfig, InstanceHostConfig};
use crate::instances::open_server_config;
use crate::manifest::{ImageCachePolicy, Manifest, Permissions};
use crate::repo::{InstanceRow, PluginRepository, PluginRow, RepoError, now_millis};

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
    http: Arc<HttpClient>,
    catalog: Option<Arc<dyn CatalogProvider>>,
    metadata: Option<Arc<dyn MetadataProvider>>,
    acquisition: Option<Arc<dyn AcquisitionProvider>>,
    artwork: Option<Arc<dyn ArtworkProvider>>,
    verification: Option<Arc<dyn VerificationProvider>>,
    scrobble: Option<Arc<dyn ScrobbleProvider>>,
    discovery: Option<Arc<dyn DiscoveryProvider>>,
    account_link: Option<Arc<dyn AccountLinkProvider>>,
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

    /// Returns the artwork provider of this instance, when it provides artwork.
    pub fn artwork(&self) -> Option<&Arc<dyn ArtworkProvider>> {
        self.artwork.as_ref()
    }

    /// Returns the verification provider of this instance, when it provides
    /// verification.
    pub fn verification(&self) -> Option<&Arc<dyn VerificationProvider>> {
        self.verification.as_ref()
    }

    /// Returns the scrobble provider of this instance, when it provides
    /// scrobbling.
    pub fn scrobble(&self) -> Option<&Arc<dyn ScrobbleProvider>> {
        self.scrobble.as_ref()
    }

    /// Returns the discovery provider of this instance, when it provides
    /// discovery.
    pub fn discovery(&self) -> Option<&Arc<dyn DiscoveryProvider>> {
        self.discovery.as_ref()
    }

    /// Returns the account-link provider of this instance, when its component
    /// exports `account-link`.
    ///
    /// Account linking is a capability rather than a layer, so an instance
    /// assigned only to `scrobble` or `discovery` can still link accounts.
    pub fn account_link(&self) -> Option<&Arc<dyn AccountLinkProvider>> {
        self.account_link.as_ref()
    }

    /// Fetches one image under the instance's HTTP permissions.
    ///
    /// This is the host asking, not the plugin: the server proxies cover art a
    /// catalog plugin named, and it does so through the same client, and
    /// therefore the same allowlist, address policy, redirect re-checks,
    /// timeout, body cap and rate limit, that the plugin itself would get from
    /// [`host-http`](crate::http). A URL the instance could not reach itself is
    /// denied here too, with the same message. The request uses the default
    /// timeout; a caller that needs a different budget has to go through the
    /// plugin.
    ///
    /// The response is returned as it arrived, including a status the caller may
    /// want to treat as an error. Whether it may be stored is the caller's
    /// decision, made from [`RegistryInstance::image_cache_policy`].
    ///
    /// # Parameters
    ///
    /// - `url`: the absolute image URL.
    ///
    /// # Returns
    ///
    /// The status, the `Content-Type` when the server sent one, the body, and
    /// the instance's image cache policy.
    ///
    /// # Errors
    ///
    /// Returns [`FetchError::Http`] with [`HttpError::Denied`](crate::http::HttpError::Denied)
    /// when the URL is outside the instance's grant,
    /// [`HttpError::BodyTooLarge`](crate::http::HttpError::BodyTooLarge) when
    /// the body exceeds the instance's cap, and
    /// [`HttpError::Timeout`](crate::http::HttpError::Timeout) or
    /// [`HttpError::Network`](crate::http::HttpError::Network) when the request
    /// does not complete.
    pub async fn fetch_image(&self, url: &str) -> Result<FetchedImage, FetchError> {
        let request = HttpRequest {
            method: "GET".to_owned(),
            url: url.to_owned(),
            headers: Vec::new(),
            body: None,
            timeout_ms: None,
        };
        let response = self.http.send(&request, None).await?;
        Ok(FetchedImage {
            status: response.status,
            content_type: header(&response.headers, "content-type"),
            bytes: response.body,
            cache_policy: self.image_cache_policy(),
        })
    }
}

/// One image fetched on an instance's behalf.
///
/// The bytes are the server's copy of the image, not the plugin's: nothing is
/// stored until the caller decides the instance's policy allows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedImage {
    /// The HTTP status code.
    pub status: u16,
    /// The response's `Content-Type`, when it sent one.
    pub content_type: Option<String>,
    /// The response body.
    pub bytes: Vec<u8>,
    /// How the issuing instance's manifest says the image may be stored.
    pub cache_policy: ImageCachePolicy,
}

/// Why an image could not be fetched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The registry serves no instance with that slug.
    #[error("no provider instance with slug `{0}`")]
    UnknownInstance(String),
    /// The request was denied or did not complete.
    #[error(transparent)]
    Http(#[from] HttpError),
}

/// Returns the first value of one response header, matched case-insensitively.
fn header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(field, _)| field.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
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
                    self.artwork.is_some(),
                    self.verification.is_some(),
                    self.scrobble.is_some(),
                    self.discovery.is_some(),
                    self.account_link.is_some(),
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
    /// How many stored secrets the rebuild sealed because they were still in
    /// the clear.
    pub resealed: usize,
}

/// The outcome of one provider instance health check.
///
/// The object is what the instance's row stores, so a later read reports the
/// check as it was taken.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthOutcome {
    /// The instance the check ran against.
    pub slug: String,
    /// Whether the instance answered that it can serve requests.
    pub ok: bool,
    /// What the probe reported: the plugin's own status line when the instance
    /// is healthy, and the failure when it is not.
    pub detail: String,
    /// When the probe ran, in unix milliseconds.
    pub checked_at: i64,
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
    /// Whether the entries may not match the plugin tables, because a rebuild
    /// failed after a write committed.
    stale: std::sync::atomic::AtomicBool,
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
            stale: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// Returns whether the entries may not match the plugin tables.
    ///
    /// The flag is cleared by a successful [`ProviderRegistry::rebuild_or_mark_stale`]
    /// and set when one fails: a write that committed and a rebuild that failed
    /// leave the server serving the previous entries, and the reload worker
    /// retries while the flag is set instead of waiting for the next admin
    /// write.
    pub fn is_stale(&self) -> bool {
        self.stale.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Rebuilds, remembering a failure so the registry is retried.
    ///
    /// The write that preceded the rebuild has already committed, so a failed
    /// rebuild is not the caller's data problem: it means the running server
    /// could not apply the change yet. The previous entries stay in place and
    /// the flag makes the reload worker try again within one poll interval.
    ///
    /// # Returns
    ///
    /// What the rebuild produced.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError`] when the plugin tables cannot be read, with the
    /// registry left marked stale.
    pub async fn rebuild_or_mark_stale(&self) -> Result<RegistryReport, RepoError> {
        match self.rebuild().await {
            Ok(report) => {
                self.stale.store(false, std::sync::atomic::Ordering::SeqCst);
                Ok(report)
            }
            Err(error) => {
                self.stale.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(error)
            }
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

        // A plugin that starts marking a field `writeOnly` - or one that already
        // did after an instance was configured - leaves the row holding the
        // value in the clear. Sealing it here means a plugin upgrade needs no
        // operator step, and it happens before any entry is built, so the
        // configuration the plugin is handed is unchanged.
        let resealed = self.reseal_plaintext_secrets(&instances, &plugins).await;

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
        report.resealed = resealed;
        drop(state);

        self.components
            .lock()
            .unwrap_or_else(poisoned)
            .retain(|key, _| used.contains(key));

        Ok(report)
    }

    /// Seals every write-only field a stored configuration still holds in the
    /// clear.
    ///
    /// One call per rebuild, over every instance with a stored configuration. A
    /// row whose plugin manifest cannot be read, whose configuration cannot be
    /// parsed, or whose write fails is logged and left as it is: a rebuild must
    /// never fail because one instance cannot be upgraded, which is the same
    /// policy a skipped instance follows.
    ///
    /// # Parameters
    ///
    /// - `instances`: the instance rows of this rebuild.
    /// - `plugins`: the plugin rows of this rebuild.
    ///
    /// # Returns
    ///
    /// How many fields were sealed across every instance.
    async fn reseal_plaintext_secrets(
        &self,
        instances: &HashMap<uuid::Uuid, InstanceRow>,
        plugins: &HashMap<String, PluginRow>,
    ) -> usize {
        let mut resealed = 0usize;
        for row in instances.values() {
            let Some(stored) = row.server_config.as_deref() else {
                continue;
            };
            let Some(plugin) = plugins.get(&row.plugin_id) else {
                continue;
            };
            let manifest = match Manifest::parse(plugin.manifest.as_bytes(), self.host.settings()) {
                Ok(manifest) => manifest,
                Err(error) => {
                    tracing::warn!(
                        target: "hearthsound::plugin",
                        plugin = %plugin.id,
                        instance = %row.slug,
                        error = %error,
                        "cannot re-seal a stored secret: the manifest cannot be read",
                    );
                    continue;
                }
            };
            let Some(section) = manifest.config.server.as_ref() else {
                continue;
            };
            let mut config: serde_json::Value = match serde_json::from_str(stored) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!(
                        target: "hearthsound::plugin",
                        plugin = %plugin.id,
                        instance = %row.slug,
                        error = %error,
                        "cannot re-seal a stored secret: the configuration cannot be read",
                    );
                    continue;
                }
            };
            let sealed = match crate::instances::seal_plaintext_write_only(
                &section.schema,
                &mut config,
                &self.secrets,
            ) {
                Ok(sealed) => sealed,
                Err(error) => {
                    tracing::warn!(
                        target: "hearthsound::plugin",
                        plugin = %plugin.id,
                        instance = %row.slug,
                        error = %error,
                        "cannot re-seal a stored secret",
                    );
                    continue;
                }
            };
            if sealed == 0 {
                continue;
            }
            let text = match serde_json::to_string(&config) {
                Ok(text) => text,
                Err(error) => {
                    tracing::warn!(
                        target: "hearthsound::plugin",
                        plugin = %plugin.id,
                        instance = %row.slug,
                        error = %error,
                        "cannot re-seal a stored secret: the configuration cannot be stored",
                    );
                    continue;
                }
            };
            match self
                .repository
                .set_instance_server_config(row.id, &text, now_millis())
                .await
            {
                Ok(()) => {
                    resealed += sealed;
                    tracing::info!(
                        target: "hearthsound::plugin",
                        plugin = %plugin.id,
                        instance = %row.slug,
                        fields = sealed,
                        "sealed stored secrets a plugin left in the clear",
                    );
                }
                Err(error) => tracing::warn!(
                    target: "hearthsound::plugin",
                    plugin = %plugin.id,
                    instance = %row.slug,
                    error = %error,
                    "cannot re-seal a stored secret: the row cannot be written",
                ),
            }
        }
        resealed
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

    /// Returns every instance the registry serves, ordered by slug.
    ///
    /// This is the set of enabled instances an administrator assigned to at
    /// least one layer; an instance that is not in service is absent, because
    /// nothing can be called through it.
    ///
    /// # Returns
    ///
    /// A snapshot of the entries, ordered by slug, so a caller that lists them
    /// is deterministic and a later rebuild does not change the vector.
    pub fn instances(&self) -> Vec<Arc<RegistryInstance>> {
        let state = self.read();
        let mut entries: Vec<Arc<RegistryInstance>> = state.by_slug.values().cloned().collect();
        entries.sort_by(|left, right| left.slug.as_str().cmp(right.slug.as_str()));
        entries
    }

    /// Fetches one image under the permissions of the instance with `slug`.
    ///
    /// The fetch is the host's, so it also applies to instances that are not
    /// catalog providers; the caller picks the instance whose grant should
    /// cover the URL, which is the instance that named it.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance whose HTTP permissions apply.
    /// - `url`: the absolute image URL.
    ///
    /// # Returns
    ///
    /// The status, content type, bytes and the instance's image cache policy.
    ///
    /// # Errors
    ///
    /// Returns [`FetchError::UnknownInstance`] when the registry serves no
    /// instance with the slug, and otherwise the errors of
    /// [`RegistryInstance::fetch_image`].
    pub async fn fetch_image(&self, slug: &str, url: &str) -> Result<FetchedImage, FetchError> {
        let instance = self
            .instance(slug)
            .ok_or_else(|| FetchError::UnknownInstance(slug.to_owned()))?;
        instance.fetch_image(url).await
    }

    /// Runs one instance's `lifecycle.health` probe.
    ///
    /// The probe runs through the first provider the instance holds, in layer
    /// order: catalog, metadata, acquisition, artwork, verification, scrobble,
    /// discovery, then account link. Every one of them is the plugin's own
    /// `lifecycle.health`, so one probe answers for the instance, and an
    /// instance in service always holds at least one provider, because the
    /// registry only serves instances an administrator assigned to a layer.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    ///
    /// # Returns
    ///
    /// `None` when the registry serves no instance with the slug - an unknown
    /// instance, or one that is disabled, unassigned or whose plugin could not
    /// be loaded - and otherwise what the probe reported.
    pub async fn health(&self, slug: &str) -> Option<Result<String, ProviderError>> {
        let instance = self.instance(slug)?;
        if let Some(provider) = instance.catalog() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.metadata() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.acquisition() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.artwork() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.verification() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.scrobble() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.discovery() {
            return Some(provider.health().await);
        }
        if let Some(provider) = instance.account_link() {
            return Some(provider.health().await);
        }
        // No provider is held, so nothing can serve a request for this
        // instance; that is what `unavailable` means.
        Some(Err(ProviderError::Unavailable))
    }

    /// Runs one instance's health probe and records the outcome on its row.
    ///
    /// This is the one path both the manual `GET
    /// /api/v1/admin/providers/{slug}/health` route and the periodic health job
    /// take, so a scheduled check publishes exactly what pressing Check Health
    /// publishes.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    ///
    /// # Returns
    ///
    /// `None` when the registry serves no instance with the slug - an unknown
    /// instance, or one that is disabled, unassigned or whose plugin could not
    /// be loaded - and otherwise the outcome that was recorded.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError`] when the outcome cannot be stored.
    pub async fn check_health(&self, slug: &str) -> Result<Option<HealthOutcome>, RepoError> {
        let Some(instance) = self.instance(slug) else {
            return Ok(None);
        };
        let (ok, detail) = match self.health(slug).await {
            Some(Ok(detail)) => (true, detail),
            Some(Err(error)) => (false, error.to_string()),
            None => return Ok(None),
        };
        let outcome = HealthOutcome {
            slug: slug.to_owned(),
            ok,
            detail,
            checked_at: now_millis(),
        };
        let text = serde_json::to_string(&outcome)?;
        self.repository
            .set_instance_health(instance.id(), Some(&text))
            .await?;
        Ok(Some(outcome))
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
        let http = instance_http_client(manifest, &instance).map_err(|error| error.to_string())?;

        let mut catalog = None;
        let mut metadata = None;
        let mut acquisition = None;
        let mut artwork = None;
        let mut verification = None;
        let mut scrobble = None;
        let mut discovery = None;
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
                Layer::Artwork => {
                    artwork = Some(
                        self.host
                            .artwork_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                Layer::Verification => {
                    verification = Some(
                        self.host
                            .verification_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                Layer::Scrobble => {
                    scrobble = Some(
                        self.host
                            .scrobble_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                Layer::Discovery => {
                    discovery = Some(
                        self.host
                            .discovery_provider(&component, instance.clone())
                            .map_err(|error| error.to_string())?,
                    );
                }
                other => {
                    return Err(format!("this server cannot host `{other}` plugins yet"));
                }
            }
        }

        let account_link =
            if crate::validate::exports_account_link(component.component(), component.api()) {
                Some(
                    self.host
                        .account_link_provider(&component, instance.clone())
                        .map_err(|error| error.to_string())?,
                )
            } else {
                None
            };

        Ok(RegistryInstance {
            id: row.id,
            slug: instance.slug.clone(),
            plugin_id: plugin.id.clone(),
            plugin_version: plugin.version.clone(),
            manifest: manifest.clone(),
            host_config,
            http,
            catalog,
            metadata,
            acquisition,
            artwork,
            verification,
            scrobble,
            discovery,
            account_link,
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

/// Reads the permissions granted to a plugin.
///
/// The granted set is what the instance's imports check against, so it is the
/// row's value and not the manifest's. A row without a grant, which only a
/// hand-edited database can hold, reaches nothing.
fn approved_permissions(plugin: &PluginRow) -> Result<Permissions, String> {
    match plugin.approved_permissions.as_deref() {
        Some(json) => serde_json::from_str(json)
            .map_err(|error| format!("stored permissions of `{}`: {error}", plugin.id)),
        None => Ok(Permissions::default()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hearthsound_config::secret::SecretKey;
    use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
    use hearthsound_db::{Database, MIGRATOR};

    use super::*;

    /// Builds a registry over a migrated temporary database.
    async fn registry() -> (Arc<Database>, ProviderRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearthsound.db");
        let database = Database::open(&path).await.unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        let database = Arc::new(database);
        let settings = Arc::new(
            SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default())
                .unwrap(),
        );
        let host = Arc::new(
            PluginHost::new(dir.path().join("plugins"), settings, Arc::clone(&database)).unwrap(),
        );
        let registry = ProviderRegistry::new(
            host,
            PluginRepository::new(&database),
            SecretKey::generate().secret_box(),
        );
        // The fixture directory must outlive the call.
        std::mem::forget(dir);
        (database, registry)
    }

    #[tokio::test]
    async fn a_failed_rebuild_leaves_the_registry_marked_stale() {
        let (database, registry) = registry().await;

        // A registry is stale until it has been built once.
        assert!(registry.is_stale());
        assert!(registry.rebuild_or_mark_stale().await.is_ok());
        assert!(!registry.is_stale());

        // A rebuild that cannot read the tables is reported and leaves the
        // previous entries in place, with the flag set so the caller retries.
        sqlx::query("DROP TABLE provider_instances")
            .execute(database.writer())
            .await
            .unwrap();
        assert!(registry.rebuild_or_mark_stale().await.is_err());
        assert!(registry.is_stale());
    }
}
