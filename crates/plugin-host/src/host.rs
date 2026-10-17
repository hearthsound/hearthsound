//! The plugin host: installing, loading, and running plugins.
//!
//! Installation is deliberately a directory rename: the archive is unpacked
//! into a staging directory next to the final location and moved into place
//! only once every file is written, so a crash cannot leave a half-installed
//! plugin behind. Loading prefers the precompiled `plugin.cwasm` written at
//! install time and falls back to recompiling `plugin.wasm` when the artifact
//! is unusable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_config::SettingsStore;
use hearthsound_core::Layer;
use hearthsound_core::events::EventBus;
use hearthsound_core::providers::{
    AcquisitionProvider, ArtworkProvider, CatalogProvider, MetadataProvider,
};
use hearthsound_db::Database;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use wasmtime::Engine;
use wasmtime::component::{Component, HasSelf, Linker};

use crate::acquisition::WasmAcquisitionProvider;
use crate::archive::PluginArchive;
use crate::artwork::WasmArtworkProvider;
use crate::bindings::{
    self, AcquisitionProviderPre, ArtworkProviderPre, CatalogProviderPre, MetadataProviderPre,
    imports,
};
use crate::catalog::WasmCatalogProvider;
use crate::engine::PluginEngine;
use crate::error::{HostError, InstallError};
use crate::files::FileAccess;
use crate::http::{HttpClient, HttpGrant};
use crate::instance::InstanceConfig;
use crate::kv::KvStore;
use crate::log::LogSink;
use crate::manifest::{Manifest, ManifestError, SUPPORTED_API};
use crate::metadata::WasmMetadataProvider;
use crate::pool::{self, PoolConfig, StorePool};
use crate::repo::InstanceDisabler;
use crate::state::InstanceState;
use crate::validate;

/// An installed plugin, ready to be instantiated.
#[derive(Debug)]
pub struct InstalledPlugin {
    manifest: Manifest,
    manifest_json: String,
    dir: PathBuf,
    sha256: [u8; 32],
    component: Component,
}

impl InstalledPlugin {
    /// Returns the plugin's manifest.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Returns the plugin's manifest as the JSON text the install wrote.
    ///
    /// This is the document that is stored in `plugins.manifest`, not a
    /// re-serialization of the parsed manifest, so nothing the parser accepted
    /// is lost or reformatted on the way into the database.
    pub fn manifest_json(&self) -> &str {
        &self.manifest_json
    }

    /// Returns the directory the plugin is installed in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Returns the SHA-256 of the archive the plugin was installed from.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Returns the compiled component.
    pub fn component(&self) -> &Component {
        &self.component
    }
}

/// Owns the wasmtime engine, the host linker, and the plugin directory.
pub struct PluginHost {
    engine: PluginEngine,
    linker: Linker<InstanceState>,
    plugins_dir: PathBuf,
    settings: Arc<SettingsStore>,
    database: Arc<Database>,
    events: Arc<EventBus>,
}

impl PluginHost {
    /// Creates a host rooted at `plugins_dir`.
    ///
    /// The linker is built once here: the WASI preview 2 interfaces and the
    /// catalog provider world, both backed by [`InstanceState`].
    ///
    /// # Parameters
    ///
    /// - `plugins_dir`: the directory plugin installs live under. It is created
    ///   when a plugin is installed, not here.
    /// - `settings`: the resolved settings, used to validate manifests.
    /// - `database`: the open database, used for the per-instance key-value
    ///   store and for loading instances.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Engine`] when the wasmtime engine or the linker
    /// cannot be built.
    pub fn new(
        plugins_dir: PathBuf,
        settings: Arc<SettingsStore>,
        database: Arc<Database>,
    ) -> Result<PluginHost, HostError> {
        let engine = PluginEngine::new()?;

        let mut linker = Linker::new(engine.engine());
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(HostError::Engine)?;
        imports::HostImports::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(HostError::Engine)?;

        Ok(PluginHost {
            engine,
            linker,
            plugins_dir,
            settings,
            database,
            events: Arc::new(EventBus::new()),
        })
    }

    /// Returns the directory plugin installs live under.
    pub fn plugins_dir(&self) -> &Path {
        &self.plugins_dir
    }

    /// Returns the bus the host publishes plugin status events on.
    ///
    /// The host publishes a `plugin.status` event whenever it disables an
    /// instance itself, and a composition root that wants those events on the
    /// server's bus hands its own bus to [`PluginHost::with_event_bus`].
    pub fn events(&self) -> &Arc<EventBus> {
        &self.events
    }

    /// Publishes plugin status events on `events` instead of a private bus.
    ///
    /// # Parameters
    ///
    /// - `events`: the bus the server serves its clients from.
    pub fn with_event_bus(mut self, events: Arc<EventBus>) -> PluginHost {
        self.events = events;
        self
    }

    /// Installs the archive at `archive`.
    ///
    /// # Parameters
    ///
    /// - `archive`: the `.hsplugin` file to install.
    ///
    /// # Returns
    ///
    /// The installed plugin.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError`] when the archive cannot be read, its manifest
    /// is invalid, its component fails to compile or breaks the import and
    /// export rules, it does not type-check against the host's world, or the
    /// same version is already installed.
    pub async fn install(&self, archive: &Path) -> Result<InstalledPlugin, InstallError> {
        let archive = archive.to_path_buf();
        let plugins_dir = self.plugins_dir.clone();
        let settings = Arc::clone(&self.settings);
        let engine = self.engine.engine().clone();
        let linker = self.linker.clone();
        let failure_path = plugins_dir.clone();

        tokio::task::spawn_blocking(move || {
            install_blocking(&engine, &linker, &plugins_dir, &settings, &archive)
        })
        .await
        .map_err(|error| InstallError::Io {
            path: failure_path,
            source: std::io::Error::other(error),
        })?
    }

    /// Removes an installed plugin's directory, with every installed version.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::OutsidePluginsDir`] when the path resolves outside
    /// the plugins directory and [`InstallError::Io`] when it cannot be removed.
    /// A directory that is already absent is not an error.
    pub async fn remove_plugin(&self, id: &str) -> Result<(), InstallError> {
        self.remove_within(vec![id.to_owned()]).await
    }

    /// Removes one installed version of a plugin.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    /// - `version`: the version directory to remove.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::OutsidePluginsDir`] when the path resolves outside
    /// the plugins directory and [`InstallError::Io`] when it cannot be removed.
    /// A directory that is already absent is not an error.
    pub async fn remove_version(&self, id: &str, version: &str) -> Result<(), InstallError> {
        self.remove_within(vec![id.to_owned(), version.to_owned()])
            .await
    }

    /// Removes a path inside the plugins directory on a blocking thread.
    async fn remove_within(&self, parts: Vec<String>) -> Result<(), InstallError> {
        let plugins_dir = self.plugins_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<(), InstallError> {
            let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
            match removal_target(&plugins_dir, &parts)? {
                Some(target) => {
                    std::fs::remove_dir_all(&target).map_err(|source| InstallError::Io {
                        path: target,
                        source,
                    })
                }
                None => Ok(()),
            }
        })
        .await
        .map_err(|error| InstallError::Io {
            path: self.plugins_dir.clone(),
            source: std::io::Error::other(error),
        })?
    }

    /// Loads an already installed plugin.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    /// - `version`: the plugin version.
    ///
    /// # Returns
    ///
    /// The loaded plugin.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::Io`] when the plugin directory cannot be read,
    /// [`InstallError::Manifest`] when the installed manifest is invalid, and
    /// [`InstallError`] when the component cannot be loaded or revalidated.
    pub async fn load(
        &self,
        id: &str,
        version: &semver::Version,
    ) -> Result<InstalledPlugin, InstallError> {
        let dir = self.plugins_dir.join(id).join(version.to_string());
        let settings = Arc::clone(&self.settings);
        let engine = self.engine.engine().clone();
        let linker = self.linker.clone();
        let failure_path = dir.clone();

        tokio::task::spawn_blocking(move || load_blocking(&engine, &linker, &settings, dir))
            .await
            .map_err(|error| InstallError::Io {
                path: failure_path,
                source: std::io::Error::other(error),
            })?
    }

    /// Wraps an installed plugin as a catalog provider.
    ///
    /// The returned provider owns a pool of guest stores for one instance; each
    /// call borrows a store, runs under the instance's deadline, and is returned
    /// to the pool when the guest answers.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the installed plugin to drive.
    /// - `instance`: the instance's configuration.
    ///
    /// # Returns
    ///
    /// The provider, as the core trait object the rest of the server uses.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Incompatible`] when the component cannot be prepared
    /// for instantiation against this host, and [`HostError::Http`] when the
    /// instance's HTTP client cannot be built.
    pub fn catalog_provider(
        &self,
        plugin: &InstalledPlugin,
        instance: InstanceConfig,
    ) -> Result<Arc<dyn CatalogProvider>, HostError> {
        let pool = self.store_pool::<bindings::CatalogProvider>(plugin, &instance)?;
        Ok(Arc::new(WasmCatalogProvider::new(
            pool,
            plugin.manifest().id.clone(),
            instance.slug,
        )))
    }

    /// Wraps an installed plugin as a metadata provider.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the installed plugin to drive.
    /// - `instance`: the instance's configuration.
    ///
    /// # Returns
    ///
    /// The provider, as the core trait object the rest of the server uses.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`PluginHost::catalog_provider`].
    pub fn metadata_provider(
        &self,
        plugin: &InstalledPlugin,
        instance: InstanceConfig,
    ) -> Result<Arc<dyn MetadataProvider>, HostError> {
        let pool = self.store_pool::<bindings::MetadataProvider>(plugin, &instance)?;
        Ok(Arc::new(WasmMetadataProvider::new(
            pool,
            plugin.manifest().id.clone(),
            instance.slug,
        )))
    }

    /// Wraps an installed plugin as an artwork provider.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the installed plugin to drive.
    /// - `instance`: the instance's configuration.
    ///
    /// # Returns
    ///
    /// The provider, as the core trait object the rest of the server uses.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`PluginHost::catalog_provider`].
    pub fn artwork_provider(
        &self,
        plugin: &InstalledPlugin,
        instance: InstanceConfig,
    ) -> Result<Arc<dyn ArtworkProvider>, HostError> {
        let pool = self.store_pool::<bindings::ArtworkProvider>(plugin, &instance)?;
        Ok(Arc::new(WasmArtworkProvider::new(
            pool,
            plugin.manifest().id.clone(),
            instance.slug,
        )))
    }

    /// Wraps an installed plugin as an acquisition provider.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the installed plugin to drive.
    /// - `instance`: the instance's configuration.
    ///
    /// # Returns
    ///
    /// The provider, as the core trait object the rest of the server uses.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`PluginHost::catalog_provider`].
    pub fn acquisition_provider(
        &self,
        plugin: &InstalledPlugin,
        instance: InstanceConfig,
    ) -> Result<Arc<dyn AcquisitionProvider>, HostError> {
        let pool = self.store_pool::<bindings::AcquisitionProvider>(plugin, &instance)?;
        Ok(Arc::new(WasmAcquisitionProvider::new(
            pool,
            plugin.manifest().id.clone(),
            instance.slug,
        )))
    }

    /// Builds the store pool of one instance for one provider world.
    ///
    /// Every world shares this setup, so an instance's log sink, HTTP client,
    /// key-value store and file grant are built the same way no matter which
    /// layer drives it.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the installed plugin to drive.
    /// - `instance`: the instance's configuration.
    ///
    /// # Errors
    ///
    /// Returns [`HostError::Http`] when the instance's HTTP client cannot be
    /// built and [`HostError::Incompatible`] when the component cannot be
    /// prepared for the world.
    fn store_pool<W: pool::World>(
        &self,
        plugin: &InstalledPlugin,
        instance: &InstanceConfig,
    ) -> Result<StorePool<W>, HostError> {
        let manifest = plugin.manifest();
        let log = Arc::new(LogSink::new(manifest.id.clone(), instance.slug.clone()));

        let http = instance_http_client(manifest, instance)?;
        let kv = Arc::new(KvStore::from_permission(
            self.database.writer().clone(),
            instance.id,
            instance.permissions.kv.as_ref(),
        ));

        let files = Arc::new(FileAccess::new(
            &instance.host_config,
            instance.permissions.files.as_ref(),
        ));

        StorePool::new(
            self.engine.engine(),
            &self.linker,
            plugin.component(),
            PoolConfig {
                plugin: manifest.id.clone(),
                slug: instance.slug.clone(),
                server_config: Arc::from(instance.server_config.as_str()),
                log,
                http,
                kv,
                files,
                memory_mb: manifest.resources.memory_mb,
                max_instances: manifest.resources.max_instances,
                call_timeout_ms: manifest.resources.call_timeout_ms,
                auto_disable: InstanceDisabler::new(
                    &self.database,
                    Arc::clone(&self.events),
                    instance.id,
                    manifest.id.clone(),
                    instance.slug.to_string(),
                ),
            },
        )
        .map_err(|error| HostError::Incompatible(format!("{error:#}")))
    }
}

/// Builds the outbound HTTP client of one instance.
///
/// The client applies the instance's approved allowlist and its resolved
/// `$config:` hosts, so everything that reaches the network on an instance's
/// behalf shares one grant: the `host-http` import and the host's own artwork
/// fetches.
///
/// # Parameters
///
/// - `manifest`: the plugin's manifest, used for the User-Agent.
/// - `instance`: the instance's configuration, with secrets decrypted.
///
/// # Errors
///
/// Returns [`HostError::Http`] when the client cannot be built.
pub(crate) fn instance_http_client(
    manifest: &Manifest,
    instance: &InstanceConfig,
) -> Result<Arc<HttpClient>, HostError> {
    let grant = HttpGrant::new(instance.permissions.http.as_ref(), &instance.server_config);
    HttpClient::new(
        manifest.id.clone(),
        &manifest.version,
        instance.slug.clone(),
        grant,
    )
    .map(Arc::new)
    .map_err(|error| HostError::Http(error.to_string()))
}

/// Installs an archive without touching the async runtime.
fn install_blocking(
    engine: &Engine,
    linker: &Linker<InstanceState>,
    plugins_dir: &Path,
    settings: &SettingsStore,
    archive_path: &Path,
) -> Result<InstalledPlugin, InstallError> {
    let archive = PluginArchive::read(archive_path)?;
    let manifest = Manifest::parse(archive.manifest(), settings)?;

    if manifest.api != SUPPORTED_API {
        return Err(InstallError::UnsupportedApi {
            api: manifest.api.clone(),
        });
    }
    let version = plugin_version(&manifest)?;
    let target = plugins_dir.join(&manifest.id).join(version.to_string());
    if target.exists() {
        return Err(InstallError::AlreadyInstalled {
            id: manifest.id.clone(),
            version: version.to_string(),
        });
    }

    let component = Component::new(engine, archive.wasm()).map_err(InstallError::Compile)?;
    validate::validate(&component, &manifest)?;
    ensure_instantiable(linker, &component, &manifest)?;

    let staging = plugins_dir.join(format!(".staging-{}", Uuid::now_v7()));
    std::fs::create_dir_all(&staging).map_err(|source| InstallError::Io {
        path: staging.clone(),
        source,
    })?;

    if let Err(error) = stage(&staging, &archive, &component) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let parent = plugins_dir.join(&manifest.id);
    if let Err(source) = std::fs::create_dir_all(&parent) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(InstallError::Io {
            path: parent,
            source,
        });
    }

    if let Err(source) = std::fs::rename(&staging, &target) {
        let _ = std::fs::remove_dir_all(&staging);
        if target.exists() {
            return Err(InstallError::AlreadyInstalled {
                id: manifest.id.clone(),
                version: version.to_string(),
            });
        }
        return Err(InstallError::Io {
            path: target,
            source,
        });
    }

    Ok(InstalledPlugin {
        manifest,
        // The bytes parsed as JSON, so this lossy conversion is exact.
        manifest_json: String::from_utf8_lossy(archive.manifest()).into_owned(),
        dir: target,
        sha256: archive.sha256(),
        component,
    })
}

/// Writes the plugin's files into the staging directory.
fn stage(
    staging: &Path,
    archive: &PluginArchive,
    component: &Component,
) -> Result<(), InstallError> {
    let write = |name: &str, bytes: &[u8]| {
        let path = staging.join(name);
        std::fs::write(&path, bytes).map_err(|source| InstallError::Io { path, source })
    };

    write("manifest.json", archive.manifest())?;
    write("plugin.wasm", archive.wasm())?;
    let cwasm = component.serialize().map_err(InstallError::Compile)?;
    write("plugin.cwasm", &cwasm)?;
    if let Some(readme) = archive.readme() {
        write("README.md", readme)?;
    }
    if let Some(license) = archive.license() {
        write("LICENSE", license)?;
    }
    if let Some(icon) = archive.icon() {
        write("icon.png", icon)?;
    }
    Ok(())
}

/// Resolves `plugins_dir/<parts>` for removal.
///
/// Returns `None` when the path does not exist. Refuses any part that is not a
/// single normal path component, and any path whose canonical form is not
/// strictly inside the canonical plugins directory, so a symbolic link cannot
/// steer a removal elsewhere.
fn removal_target(plugins_dir: &Path, parts: &[&str]) -> Result<Option<PathBuf>, InstallError> {
    let outside = || InstallError::OutsidePluginsDir(parts.iter().collect::<PathBuf>());
    for part in parts {
        let components: Vec<_> = Path::new(part).components().collect();
        if !matches!(components.as_slice(), [std::path::Component::Normal(_)]) {
            return Err(outside());
        }
    }

    let root = match plugins_dir.canonicalize() {
        Ok(root) => root,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InstallError::Io {
                path: plugins_dir.to_path_buf(),
                source,
            });
        }
    };
    let joined = plugins_dir.join(parts.iter().collect::<PathBuf>());
    let target = match joined.canonicalize() {
        Ok(target) => target,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(InstallError::Io {
                path: joined,
                source,
            });
        }
    };
    if target == root || !target.starts_with(&root) {
        return Err(outside());
    }
    Ok(Some(target))
}

/// Loads an installed plugin from its directory.
fn load_blocking(
    engine: &Engine,
    linker: &Linker<InstanceState>,
    settings: &SettingsStore,
    dir: PathBuf,
) -> Result<InstalledPlugin, InstallError> {
    let manifest_bytes =
        std::fs::read(dir.join("manifest.json")).map_err(|source| InstallError::Io {
            path: dir.join("manifest.json"),
            source,
        })?;
    let manifest = Manifest::parse(&manifest_bytes, settings)?;

    let component = load_component(engine, &dir.join("plugin.cwasm"), &dir.join("plugin.wasm"))?;
    validate::validate(&component, &manifest)?;
    ensure_instantiable(linker, &component, &manifest)?;

    let sha256 = std::fs::read(dir.join("plugin.wasm"))
        .map(|bytes| {
            let mut sha256 = [0u8; 32];
            sha256.copy_from_slice(Sha256::digest(&bytes).as_slice());
            sha256
        })
        .unwrap_or([0u8; 32]);

    Ok(InstalledPlugin {
        manifest,
        // The bytes parsed as JSON, so this lossy conversion is exact.
        manifest_json: String::from_utf8_lossy(&manifest_bytes).into_owned(),
        dir,
        sha256,
        component,
    })
}

/// Checks that the host can instantiate the component.
///
/// This is the strongest validation available without running the guest: the
/// linker resolves every import and each world's types must match exactly. Every
/// layer the manifest provides must be instantiable.
fn ensure_instantiable(
    linker: &Linker<InstanceState>,
    component: &Component,
    manifest: &Manifest,
) -> Result<(), InstallError> {
    for layer in manifest.provides() {
        let pre = linker
            .instantiate_pre(component)
            .map_err(|error| InstallError::Incompatible(format!("{error:#}")))?;
        let incompatible =
            |error: wasmtime::Error| InstallError::Incompatible(format!("{error:#}"));
        match layer {
            Layer::Catalog => {
                CatalogProviderPre::new(pre).map_err(incompatible)?;
            }
            Layer::Metadata => {
                MetadataProviderPre::new(pre).map_err(incompatible)?;
            }
            Layer::Acquisition => {
                AcquisitionProviderPre::new(pre).map_err(incompatible)?;
            }
            Layer::Artwork => {
                ArtworkProviderPre::new(pre).map_err(incompatible)?;
            }
            other => return Err(InstallError::UnsupportedCapability(*other)),
        }
    }
    Ok(())
}

/// Loads a precompiled component, recompiling when the artifact is unusable.
///
/// # Parameters
///
/// - `engine`: the engine to load into.
/// - `cwasm`: the precompiled artifact.
/// - `wasm`: the component bytes, used when the artifact cannot be loaded.
///
/// # Errors
///
/// Returns [`InstallError::Io`] when the component bytes cannot be read and
/// [`InstallError::Compile`] when the component does not compile.
#[allow(unsafe_code)]
fn load_component(engine: &Engine, cwasm: &Path, wasm: &Path) -> Result<Component, InstallError> {
    // SAFETY: `cwasm` is `plugin.cwasm`, a file this host wrote with
    // `Component::serialize` into its own plugin data directory, and wasmtime
    // rejects an artifact produced by a different version or configuration of
    // the engine. A user who can write that file can already replace
    // `plugin.wasm`, so this adds no trust beyond the plugin directory itself.
    if let Ok(component) = unsafe { Component::deserialize_file(engine, cwasm) } {
        return Ok(component);
    }

    let bytes = std::fs::read(wasm).map_err(|source| InstallError::Io {
        path: wasm.to_path_buf(),
        source,
    })?;
    let component = Component::new(engine, bytes).map_err(InstallError::Compile)?;
    if let Ok(serialized) = component.serialize() {
        let _ = std::fs::write(cwasm, serialized);
    }
    Ok(component)
}

/// Parses the manifest's version, which [`Manifest::parse`] already checked.
fn plugin_version(manifest: &Manifest) -> Result<semver::Version, InstallError> {
    semver::Version::parse(&manifest.version).map_err(|error| {
        InstallError::Manifest(ManifestError::Invalid {
            field: "version".to_owned(),
            value: manifest.version.clone(),
            reason: error.to_string(),
        })
    })
}
