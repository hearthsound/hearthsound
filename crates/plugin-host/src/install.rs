//! Registering an installed plugin in the database, with its requested
//! permissions labelled by risk.
//!
//! Installing an archive puts files on disk and then records the plugin in a
//! `plugins` row: the manifest's permission set becomes the grant, and the
//! plugin is enabled. The permissions the manifest asks for are described with
//! the risk labels of `03-plugin-system.md` section 10 so an administrator can
//! see what the install grants.
//!
//! An upgrade is compared against the installed grant. A version that adds a
//! permission the installed version does not hold is staged for review; any
//! other version switches in at once. The installed plugin's own files are the
//! source of truth for the manifest:
//! [`registration`](PluginRepository::register) stores the `manifest.json` text
//! the install wrote, so the row can never describe a different manifest than
//! the component beside it.

use hearthsound_core::Layer;
use uuid::Uuid;

use crate::host::InstalledPlugin;
use crate::manifest::Manifest;
use crate::repo::{PluginRepository, PluginRow, PluginSource, PluginUpgrade};
use crate::repo::{RepoError, now_millis};

/// The `http.maxBodyBytes` above which a request is labelled a raised quota.
const DEFAULT_BODY_BYTES: u64 = 10 * 1024 * 1024;

/// The `kv.maxBytes` above which a store is labelled a raised quota.
const DEFAULT_KV_BYTES: u64 = 1024 * 1024;

/// The `resources` a plugin gets without asking for anything.
const DEFAULT_MEMORY_MB: u32 = 128;
const DEFAULT_CALL_TIMEOUT_MS: u32 = 30_000;
const DEFAULT_MAX_INSTANCES: u8 = 2;

/// How much an administrator has to think about one requested permission.
///
/// The labels are the ones `03-plugin-system.md` section 10 assigns: a public
/// host is low risk, an administrator-chosen host (`$config:`) or an import root
/// is medium, and process execution is high. Raised quotas are low, because they
/// widen resource use rather than reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Safe to grant without much thought.
    Low,
    /// Worth reading before granting.
    Medium,
    /// Grants real power over the host.
    High,
}

impl Risk {
    /// Returns the lowercase label used in the admin UI and the CLI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Risk::Low => "low",
            Risk::Medium => "medium",
            Risk::High => "high",
        }
    }
}

impl std::fmt::Display for Risk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One permission a plugin asks for, as shown to the administrator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedPermission {
    /// The manifest field the permission comes from, for example `http.hosts`.
    pub scope: &'static str,
    /// What is requested, in a form an administrator can read.
    pub detail: String,
    /// How much thought the grant deserves.
    pub risk: Risk,
}

/// The result of recording an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRegistration {
    /// The plugin id.
    pub id: String,
    /// The installed version.
    pub version: String,
    /// The API version the component targets.
    pub api: String,
    /// Where the plugin came from.
    pub source: PluginSource,
    /// The permissions the manifest requests, with their risk labels, in the
    /// order they should be displayed.
    pub requested: Vec<RequestedPermission>,
    /// The layers the plugin provides.
    pub provides: Vec<Layer>,
}

/// Describes the permissions a manifest requests, with risk labels.
///
/// Hosts are grouped by risk: every public host of the allowlist becomes one
/// entry and every `$config:` host another, so a display can render the
/// distinction between "reaches a fixed host" and "reaches whatever host the
/// administrator configured".
///
/// # Parameters
///
/// - `manifest`: the manifest whose `permissions` are described.
///
/// # Returns
///
/// The requested permissions, in display order.
pub fn requested_permissions(manifest: &Manifest) -> Vec<RequestedPermission> {
    let permissions = &manifest.permissions;
    let mut requested = Vec::new();

    if let Some(http) = &permissions.http {
        let (configured, public): (Vec<&String>, Vec<&String>) = http
            .hosts
            .iter()
            .partition(|host| host.starts_with("$config:"));
        if !public.is_empty() {
            requested.push(RequestedPermission {
                scope: "http.hosts",
                detail: join(public),
                risk: Risk::Low,
            });
        }
        if !configured.is_empty() {
            requested.push(RequestedPermission {
                scope: "http.hosts",
                detail: join(configured),
                risk: Risk::Medium,
            });
        }
        if let Some(max_body_bytes) = http.max_body_bytes
            && max_body_bytes > DEFAULT_BODY_BYTES
        {
            requested.push(RequestedPermission {
                scope: "http.maxBodyBytes",
                detail: format!("{max_body_bytes} bytes"),
                risk: Risk::Low,
            });
        }
    }

    if let Some(kv) = &permissions.kv
        && let Some(max_bytes) = kv.max_bytes
        && max_bytes > DEFAULT_KV_BYTES
    {
        requested.push(RequestedPermission {
            scope: "kv.maxBytes",
            detail: format!("{max_bytes} bytes"),
            risk: Risk::Low,
        });
    }

    if permissions
        .files
        .and_then(|files| files.import_roots)
        .unwrap_or(false)
    {
        requested.push(RequestedPermission {
            scope: "files.importRoots",
            detail: "import roots granted on this instance".to_owned(),
            risk: Risk::Medium,
        });
    }

    if let Some(process) = &permissions.process
        && !process.tools.is_empty()
    {
        requested.push(RequestedPermission {
            scope: "process.tools",
            detail: join(process.tools.iter()),
            risk: Risk::High,
        });
    }

    let resources = &manifest.resources;
    let raised = resources.memory_mb > DEFAULT_MEMORY_MB
        || resources.call_timeout_ms > DEFAULT_CALL_TIMEOUT_MS
        || resources.max_instances > DEFAULT_MAX_INSTANCES;
    if raised {
        requested.push(RequestedPermission {
            scope: "resources",
            detail: format!(
                "{} MiB, {} ms per call, {} instances",
                resources.memory_mb, resources.call_timeout_ms, resources.max_instances
            ),
            risk: Risk::Low,
        });
    }

    requested
}

/// Joins values for display.
fn join<'a>(values: impl IntoIterator<Item = &'a String>) -> String {
    values
        .into_iter()
        .map(String::as_str)
        .collect::<Vec<&str>>()
        .join(", ")
}

impl PluginRepository {
    /// Records an installed plugin, granting what its manifest requests.
    ///
    /// The row's `approved_permissions` is the manifest's requested permission
    /// set and `enabled` is `1`: the caller is the consenting party, and the
    /// host still checks the granted set inside every import. A second
    /// registration of the same plugin id replaces the row.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin an install produced.
    /// - `source`: where the archive came from.
    ///
    /// # Returns
    ///
    /// The registration, including the requested permissions with their risk
    /// labels, for display.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails and
    /// [`RepoError::Serialize`] when the manifest's permissions cannot be
    /// serialized.
    pub async fn register(
        &self,
        plugin: &InstalledPlugin,
        source: PluginSource,
    ) -> Result<PluginRegistration, RepoError> {
        self.record_install(plugin, source, None).await
    }

    /// Writes the row for an installed plugin.
    ///
    /// The row grants the manifest's permission set and is enabled.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the plugin an install produced.
    /// - `source`: where the archive came from.
    /// - `repository_id`: the repository the archive was fetched from, if any.
    ///
    /// # Returns
    ///
    /// The registration, for display.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails and
    /// [`RepoError::Serialize`] when the permissions cannot be serialized.
    pub(crate) async fn record_install(
        &self,
        plugin: &InstalledPlugin,
        source: PluginSource,
        repository_id: Option<Uuid>,
    ) -> Result<PluginRegistration, RepoError> {
        let manifest = plugin.manifest();

        let row = PluginRow {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            api: manifest.api.clone(),
            manifest: plugin.manifest_json().to_owned(),
            sha256: plugin.sha256().to_vec(),
            source: source.as_str().to_owned(),
            repository_id,
            approved_permissions: Some(serde_json::to_string(&manifest.permissions)?),
            enabled: true,
            installed_at: now_millis(),
        };
        self.upsert_plugin(&row).await?;

        Ok(registration(plugin, source))
    }

    /// Switches the installed row to a new version without staging it.
    ///
    /// Used when the new version adds no permission the installed grant does
    /// not already hold. The grant becomes the new manifest's permissions and
    /// the enabled flag carries over.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the new version's install.
    /// - `existing`: the installed row being replaced.
    /// - `source`: where the archive came from.
    /// - `repository_id`: the repository the archive was fetched from, if any.
    ///
    /// # Returns
    ///
    /// The row as written.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails and
    /// [`RepoError::Serialize`] when the permissions cannot be serialized.
    pub(crate) async fn switch_version(
        &self,
        plugin: &InstalledPlugin,
        existing: &PluginRow,
        source: PluginSource,
        repository_id: Option<Uuid>,
    ) -> Result<PluginRow, RepoError> {
        let manifest = plugin.manifest();
        let row = PluginRow {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            api: manifest.api.clone(),
            manifest: plugin.manifest_json().to_owned(),
            sha256: plugin.sha256().to_vec(),
            source: source.as_str().to_owned(),
            repository_id,
            approved_permissions: Some(serde_json::to_string(&manifest.permissions)?),
            enabled: existing.enabled,
            installed_at: now_millis(),
        };
        self.upsert_plugin(&row).await?;
        Ok(row)
    }

    /// Stages a new version of an installed plugin for review.
    ///
    /// The installed row is untouched; the staged version waits in
    /// `plugin_upgrades` until it is applied or declined.
    ///
    /// # Parameters
    ///
    /// - `plugin`: the new version's install.
    /// - `source`: where the archive came from.
    /// - `repository_id`: the repository the archive was fetched from, if any.
    ///
    /// # Returns
    ///
    /// The upgrade this one replaced, when one was already staged.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub(crate) async fn stage_upgrade(
        &self,
        plugin: &InstalledPlugin,
        source: PluginSource,
        repository_id: Option<Uuid>,
    ) -> Result<Option<PluginUpgrade>, RepoError> {
        let manifest = plugin.manifest();
        let upgrade = PluginUpgrade {
            plugin_id: manifest.id.clone(),
            version: manifest.version.clone(),
            api: manifest.api.clone(),
            manifest: plugin.manifest_json().to_owned(),
            sha256: plugin.sha256().to_vec(),
            source: source.as_str().to_owned(),
            repository_id,
            staged_at: now_millis(),
        };
        self.insert_upgrade(&upgrade).await
    }
}

/// Builds the registration an install reports, from its manifest.
///
/// # Parameters
///
/// - `plugin`: the plugin the install produced.
/// - `source`: where the archive came from.
///
/// # Returns
///
/// The registration, for display.
pub(crate) fn registration(plugin: &InstalledPlugin, source: PluginSource) -> PluginRegistration {
    let manifest = plugin.manifest();
    PluginRegistration {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        api: manifest.api.clone(),
        source,
        requested: requested_permissions(manifest),
        provides: manifest.provides().to_vec(),
    }
}

/// Reports whether `new` requests a permission that `installed` does not.
///
/// Permissions are compared by scope and detail; the risk label is derived
/// from them and does not change what is granted.
///
/// # Parameters
///
/// - `new`: the manifest of a staged upgrade.
/// - `installed`: the manifest of the version in service.
///
/// # Returns
///
/// `true` when `new` asks for anything `installed` does not.
pub(crate) fn requests_new_permissions(new: &Manifest, installed: &Manifest) -> bool {
    let held = requested_permissions(installed);
    requested_permissions(new).iter().any(|wanted| {
        !held
            .iter()
            .any(|have| have.scope == wanted.scope && have.detail == wanted.detail)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Permissions;

    /// Builds a manifest from a permissions object.
    fn manifest(permissions: &str) -> Manifest {
        let json = format!(
            r#"{{
              "manifestVersion": 1,
              "id": "com.example.plugin",
              "name": "Example",
              "version": "0.1.0",
              "description": "An example plugin.",
              "license": "MIT",
              "homepage": "https://example.com",
              "api": "hearthsound:plugin@0.2.0",
              "provides": ["catalog"],
              "permissions": {permissions},
              "config": {{"server": null, "user": null}},
              "resources": {{"memoryMb": 128, "callTimeoutMs": 30000, "maxInstances": 2}}
            }}"#
        );
        let settings = std::sync::Arc::new(
            hearthsound_config::SettingsStore::resolve(
                &hearthsound_config::SettingsRegistry::builtin(),
                &hearthsound_config::SettingSources::default(),
            )
            .unwrap(),
        );
        Manifest::parse(json.as_bytes(), &settings).unwrap()
    }

    #[test]
    fn labels_every_requested_permission_by_risk() {
        let manifest = manifest(
            r#"{
              "http": {"hosts": ["api.example.com", "$config:url"], "maxBodyBytes": 20971520},
              "kv": {"maxBytes": 2097152},
              "files": {"importRoots": true}
            }"#,
        );

        let requested = requested_permissions(&manifest);
        let described: Vec<(&str, Risk)> = requested
            .iter()
            .map(|permission| (permission.scope, permission.risk))
            .collect();
        assert_eq!(
            described,
            [
                ("http.hosts", Risk::Low),
                ("http.hosts", Risk::Medium),
                ("http.maxBodyBytes", Risk::Low),
                ("kv.maxBytes", Risk::Low),
                ("files.importRoots", Risk::Medium),
            ]
        );
        assert_eq!(requested[0].detail, "api.example.com");
        assert_eq!(requested[1].detail, "$config:url");
    }

    #[test]
    fn a_plugin_asking_for_nothing_requests_nothing() {
        let manifest = manifest("{}");
        assert_eq!(manifest.permissions, Permissions::default());
        assert!(requested_permissions(&manifest).is_empty());
    }
}
