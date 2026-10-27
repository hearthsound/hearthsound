//! The mandatory plugins compiled into the server binary.
//!
//! `build.rs` reads `bundled-plugins.lock` and renders the archives the lock
//! marks `mandatory = true` into [`EMBEDDED_PLUGINS`], beside the id list
//! [`MANDATORY_PLUGIN_IDS`]. [`ensure_mandatory_plugins`] runs once at startup,
//! before the first provider registry rebuild: it installs a mandatory plugin the
//! server does not have yet (or a newer version of it), enables it, gives it an
//! instance when none exists, and appends that instance to the layers the plugin
//! provides. The protection the plugin ids carry lives in
//! [`hearthsound_plugin_host::ProtectedPlugins`], which the composition root
//! builds from the same list.
//!
//! A mandatory plugin whose archive was not embedded - a build made without
//! `cargo xtask fetch-plugins` - is reported once and skipped; the server starts
//! without it and keeps serving everything else.

include!(concat!(env!("OUT_DIR"), "/bundled.rs"));

use std::path::Path;
use std::sync::Arc;

use hearthsound_core::Layer;
use hearthsound_core::events::{Audience, EventBus, ServerEvent, SystemNotice};
use hearthsound_plugin_host::{
    AdminError, InstanceError, InstanceService, InstanceSpec, PluginAdmin, PluginRepository,
    PluginSource, RepoError,
};
use serde::Deserialize;
use serde_json::json;

/// The layers a mandatory plugin is attached to when it is installed.
///
/// Every catalog, metadata and artwork plugin gets one; the other layers are
/// configured by the administrator, because an acquisition or scrobble provider
/// needs credentials a default instance cannot supply.
const PREASSIGNED_LAYERS: [Layer; 3] = [Layer::Catalog, Layer::Metadata, Layer::Artwork];

/// An error from preparing the mandatory plugins.
#[derive(Debug, thiserror::Error)]
pub enum BundledError {
    /// The embedded archive could not be written for installation.
    #[error("writing {}: {source}", path.display())]
    Write {
        /// The file that could not be written.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The archive could not be removed after installation.
    #[error("removing {}: {source}", path.display())]
    Remove {
        /// The file that could not be removed.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// A stored version is not SemVer.
    #[error("required plugin `{plugin}` has stored version `{version}`, which is not SemVer")]
    Version {
        /// The plugin id.
        plugin: String,
        /// The stored version.
        version: String,
    },
    /// The plugin host refused the install.
    #[error(transparent)]
    Admin(#[from] AdminError),
    /// The archive could not be installed.
    #[error(transparent)]
    Install(#[from] hearthsound_plugin_host::InstallError),
    /// The instance could not be created or changed.
    #[error(transparent)]
    Instance(#[from] InstanceError),
    /// The plugin rows could not be read.
    #[error(transparent)]
    Repository(#[from] RepoError),
}

/// The parts of a stored manifest this module reads.
#[derive(Debug, Deserialize)]
struct StoredManifest {
    /// The plugin's human readable name, used as the default instance name.
    name: String,
    /// The layer names the plugin provides.
    #[serde(default)]
    provides: Vec<String>,
}

/// Installs, enables and configures every embedded mandatory plugin.
///
/// A failure on one plugin is logged, published to administrators as a
/// `system.notice` with code `mandatory-plugin`, and does not stop the others or
/// the startup.
///
/// # Parameters
///
/// - `admin`: the plugin admin service that installs archives.
/// - `instances`: the instance service, which also exposes the plugin rows.
/// - `events`: the bus a failure is published on.
/// - `data_dir`: the data directory a temporary copy of an embedded archive is
///   written to before installation.
///
/// # Returns
///
/// `Ok(())` once every embedded plugin was handled, whether or not all of them
/// succeeded; a plugin without an embedded archive is reported through the
/// build warning and skipped here.
///
/// # Errors
///
/// Returns [`BundledError`] only for a failure that makes the loop itself
/// impossible, which today means the plugin rows cannot be read at all.
pub async fn ensure_mandatory_plugins(
    admin: &PluginAdmin,
    instances: &InstanceService,
    events: &Arc<EventBus>,
    data_dir: &Path,
) -> Result<(), BundledError> {
    let repository = instances.repository();
    for plugin in EMBEDDED_PLUGINS {
        match ensure_one(admin, instances, repository, plugin, data_dir).await {
            Ok(()) => {}
            Err(error) => {
                tracing::error!(
                    plugin = plugin.id,
                    version = plugin.version,
                    error = %error,
                    "could not prepare a required plugin",
                );
                events.publish_for(
                    Audience::Admins,
                    ServerEvent::SystemNotice(SystemNotice {
                        level: "error".to_owned(),
                        message: format!(
                            "The required plugin {} could not be prepared: {error}",
                            plugin.id
                        ),
                        code: Some("mandatory-plugin".to_owned()),
                    }),
                );
            }
        }
    }
    Ok(())
}

/// Brings one embedded plugin to the state the server expects.
///
/// # Parameters
///
/// - `admin`: the plugin admin service.
/// - `instances`: the instance service.
/// - `repository`: the plugin rows.
/// - `plugin`: the embedded archive.
/// - `data_dir`: where the temporary archive copy goes.
async fn ensure_one(
    admin: &PluginAdmin,
    instances: &InstanceService,
    repository: &PluginRepository,
    plugin: &EmbeddedPlugin,
    data_dir: &Path,
) -> Result<(), BundledError> {
    let installed = repository.plugin(plugin.id).await?;
    let wanted = semver::Version::parse(plugin.version).map_err(|_| BundledError::Version {
        plugin: plugin.id.to_owned(),
        version: plugin.version.to_owned(),
    })?;
    let needs_install = match &installed {
        None => true,
        Some(row) => {
            let current =
                semver::Version::parse(&row.version).map_err(|_| BundledError::Version {
                    plugin: row.id.clone(),
                    version: row.version.clone(),
                })?;
            current < wanted
        }
    };
    if needs_install {
        install(admin, plugin, data_dir).await?;
    }

    let row = repository
        .plugin(plugin.id)
        .await?
        .ok_or_else(|| BundledError::Admin(AdminError::NotFound(plugin.id.to_owned())))?;
    if !row.enabled {
        admin.set_enabled(plugin.id, true).await?;
    }

    let manifest: StoredManifest = serde_json::from_str(&row.manifest)
        .map_err(|error| BundledError::Repository(RepoError::Serialize(error)))?;
    let mut views = instances.list().await?;
    let mut current = views
        .iter()
        .find(|view| view.plugin_id == plugin.id)
        .cloned();
    if current.is_none() {
        let created = instances
            .create(&InstanceSpec {
                slug: instance_slug(plugin.id),
                plugin_id: plugin.id.to_owned(),
                display_name: manifest.name.clone(),
                server_config: json!({}),
                host_config: json!(null),
            })
            .await?;
        views.push(created.clone());
        current = Some(created);
    }
    let Some(current) = current else {
        return Ok(());
    };
    if !current.enabled {
        instances.set_enabled(&current.slug, true).await?;
        if let Some(view) = views.iter_mut().find(|view| view.id == current.id) {
            view.enabled = true;
        }
    }

    let plugin_slugs: Vec<String> = views
        .iter()
        .filter(|view| view.plugin_id == plugin.id)
        .map(|view| view.slug.clone())
        .collect();
    let assignments = instances.assignments().await?;
    for layer in PREASSIGNED_LAYERS {
        if !manifest
            .provides
            .iter()
            .any(|provided| provided == layer.as_str())
        {
            continue;
        }
        let members = layer_members(&assignments, &views, layer);
        if members
            .iter()
            .any(|member| plugin_slugs.iter().any(|plugin_slug| plugin_slug == member))
        {
            continue;
        }
        let mut next = members;
        next.push(current.slug.clone());
        instances.set_layer(layer, &next).await?;
    }
    Ok(())
}

/// Returns the instance slugs of one layer, in layer order.
///
/// # Parameters
///
/// - `assignments`: every layer assignment.
/// - `views`: every instance, used to turn ids into slugs.
/// - `layer`: the layer to read.
fn layer_members(
    assignments: &[hearthsound_plugin_host::Assignment],
    views: &[hearthsound_plugin_host::instances::InstanceView],
    layer: Layer,
) -> Vec<String> {
    assignments
        .iter()
        .filter(|assignment| assignment.layer == layer)
        .filter_map(|assignment| {
            views
                .iter()
                .find(|view| view.id == assignment.instance_id)
                .map(|view| view.slug.clone())
        })
        .collect()
}

/// Writes an embedded archive to the data directory and installs it.
///
/// # Parameters
///
/// - `admin`: the plugin admin service.
/// - `plugin`: the embedded archive.
/// - `data_dir`: where the temporary copy goes.
///
/// # Errors
///
/// Returns [`BundledError::Write`] when the copy cannot be written,
/// [`BundledError::Admin`] when the install is refused, and
/// [`BundledError::Remove`] when the copy cannot be cleaned up afterwards. A
/// failed install leaves the copy removed as well, so a retry starts clean.
async fn install(
    admin: &PluginAdmin,
    plugin: &EmbeddedPlugin,
    data_dir: &Path,
) -> Result<(), BundledError> {
    let path = data_dir.join(format!("bundled-{}-{}.hsplugin", plugin.id, plugin.version));
    tokio::fs::write(&path, plugin.bytes)
        .await
        .map_err(|source| BundledError::Write {
            path: path.clone(),
            source,
        })?;
    let result = admin
        .install_archive(&path, Some(plugin.sha256), PluginSource::Bundled, None)
        .await;
    tokio::fs::remove_file(&path)
        .await
        .map_err(|source| BundledError::Remove {
            path: path.clone(),
            source,
        })?;
    match result {
        Ok(_) => Ok(()),
        Err(error) => Err(BundledError::Install(error)),
    }
}

/// Returns the default instance slug of a plugin.
///
/// The slug is the last segment of the plugin id, lowercased:
/// `app.hearthsound.musicbrainz` becomes `musicbrainz`, which satisfies the
/// instance slug rule.
///
/// # Parameters
///
/// - `plugin_id`: the reverse-DNS plugin id.
fn instance_slug(plugin_id: &str) -> String {
    plugin_id
        .rsplit('.')
        .next()
        .unwrap_or(plugin_id)
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instance_slug_is_the_last_id_segment() {
        assert_eq!(instance_slug("app.hearthsound.musicbrainz"), "musicbrainz");
        assert_eq!(instance_slug("MusicBrainz"), "musicbrainz");
        assert_eq!(instance_slug(""), "");
    }

    #[test]
    fn the_lock_file_marks_the_musicbrainz_plugin_mandatory() {
        assert!(
            MANDATORY_PLUGIN_IDS.contains(&"app.hearthsound.musicbrainz"),
            "{MANDATORY_PLUGIN_IDS:?}"
        );
    }
}
