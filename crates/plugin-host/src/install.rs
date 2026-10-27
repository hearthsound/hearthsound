//! Registering an installed plugin in the database, with its requested
//! permissions labelled by risk.
//!
//! Installing an archive puts files on disk and then records the plugin in a
//! `plugins` row: the manifest's permission set becomes the grant, and the
//! plugin is enabled. The permissions the manifest asks for are described with
//! the risk labels of `03-plugin-system.md` section 10 so an administrator can
//! see what the install grants.
//!
//! An upgrade is compared with the installed grant item by item. A version that
//! widens it, by adding a host, tool, import root, quota or resource limit, is
//! staged for review; any other version switches in at once. The installed
//! plugin's own files are the source of truth for the manifest:
//! [`registration`](PluginRepository::register) stores the `manifest.json` text
//! the install wrote, so the row can never describe a different manifest than
//! the component beside it.

use hearthsound_core::Layer;
use uuid::Uuid;

use crate::host::InstalledPlugin;
use crate::manifest::{Manifest, Permissions};
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
    /// The permissions a staged upgrade adds; empty unless the install was
    /// staged for review.
    pub new_permissions: Vec<RequestedPermission>,
    /// The layers the plugin provides.
    pub provides: Vec<Layer>,
}

/// Describes a permission set as an administrator would read it.
///
/// Hosts are grouped by risk: every public host of the allowlist becomes one
/// entry and every `$config:` host another, so a display can render the
/// distinction between "reaches a fixed host" and "reaches whatever host the
/// administrator configured". Resource limits live on the manifest rather than
/// in its permission set, so they are not described here; use
/// [`requested_permissions`] to describe a whole manifest.
///
/// # Parameters
///
/// - `permissions`: the manifest's `permissions` object.
///
/// # Returns
///
/// The requested permissions, in display order.
pub fn requested_permissions_of(permissions: &Permissions) -> Vec<RequestedPermission> {
    let mut requested = Vec::new();

    if let Some(http) = &permissions.http {
        let (configured, public): (Vec<&str>, Vec<&str>) = http
            .hosts
            .iter()
            .map(|entry| entry.host())
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
            detail: join(process.tools.iter().map(String::as_str)),
            risk: Risk::High,
        });
    }

    requested
}

/// Describes the permissions a manifest requests, with risk labels.
///
/// This is [`requested_permissions_of`] over `manifest.permissions`, followed
/// by the manifest's resource limits when any of them exceeds its default.
///
/// # Parameters
///
/// - `manifest`: the manifest whose permissions are described.
///
/// # Returns
///
/// The requested permissions, in display order.
pub fn requested_permissions(manifest: &Manifest) -> Vec<RequestedPermission> {
    let mut requested = requested_permissions_of(&manifest.permissions);

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
fn join<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values.into_iter().collect::<Vec<&str>>().join(", ")
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
        new_permissions: Vec::new(),
        provides: manifest.provides().to_vec(),
    }
}

/// Describes what a version adds to the permissions already granted.
///
/// Each permission is compared item by item, so a version that merely reorders
/// hosts or lowers a quota adds nothing. The rule is:
///
/// - hosts in `new` that `installed` does not list, public and `$config:`
///   groups separately;
/// - a `http.maxBodyBytes` or `kv.maxBytes` above the installed value, where an
///   absent value counts as its default;
/// - `files.importRoots` newly turned on;
/// - process tools that `installed` does not list;
/// - a resource limit above both the installed value and its default.
///
/// Only widening counts. Dropping a host, reordering hosts, lowering a quota
/// (the instance-wide `http.rateLimit` or a host's own one), turning an import
/// root off or removing a tool never stages a review; a host pattern that both
/// versions list is not widened whatever limit the new version gives it.
///
/// # Parameters
///
/// - `new`: the manifest of a version that would replace the installed one.
/// - `installed`: the manifest of the version in service.
///
/// # Returns
///
/// The widened permissions, in display order; empty when `installed` already
/// covers everything `new` asks for.
pub fn new_permissions(new: &Manifest, installed: &Manifest) -> Vec<RequestedPermission> {
    let wanted = &new.permissions;
    let held = &installed.permissions;
    let mut added = Vec::new();

    if let Some(http) = &wanted.http {
        let held_hosts: Vec<&str> = held
            .http
            .as_ref()
            .map(|held| held.hosts.iter().map(|entry| entry.host()).collect())
            .unwrap_or_default();
        let (configured, public): (Vec<&str>, Vec<&str>) = http
            .hosts
            .iter()
            .map(|entry| entry.host())
            .filter(|host| !held_hosts.contains(host))
            .partition(|host| host.starts_with("$config:"));
        if !public.is_empty() {
            added.push(RequestedPermission {
                scope: "http.hosts",
                detail: join(public),
                risk: Risk::Low,
            });
        }
        if !configured.is_empty() {
            added.push(RequestedPermission {
                scope: "http.hosts",
                detail: join(configured),
                risk: Risk::Medium,
            });
        }
    }

    let wanted_body = wanted
        .http
        .as_ref()
        .and_then(|http| http.max_body_bytes)
        .unwrap_or(DEFAULT_BODY_BYTES);
    let held_body = held
        .http
        .as_ref()
        .and_then(|http| http.max_body_bytes)
        .unwrap_or(DEFAULT_BODY_BYTES);
    if wanted_body > held_body {
        added.push(RequestedPermission {
            scope: "http.maxBodyBytes",
            detail: format!("{wanted_body} bytes"),
            risk: Risk::Low,
        });
    }

    let wanted_kv = wanted
        .kv
        .as_ref()
        .and_then(|kv| kv.max_bytes)
        .unwrap_or(DEFAULT_KV_BYTES);
    let held_kv = held
        .kv
        .as_ref()
        .and_then(|kv| kv.max_bytes)
        .unwrap_or(DEFAULT_KV_BYTES);
    if wanted_kv > held_kv {
        added.push(RequestedPermission {
            scope: "kv.maxBytes",
            detail: format!("{wanted_kv} bytes"),
            risk: Risk::Low,
        });
    }

    let granted_import_roots =
        wanted.files.as_ref().and_then(|files| files.import_roots) == Some(true);
    let held_import_roots = held.files.as_ref().and_then(|files| files.import_roots) == Some(true);
    if granted_import_roots && !held_import_roots {
        added.push(RequestedPermission {
            scope: "files.importRoots",
            detail: "import roots granted on this instance".to_owned(),
            risk: Risk::Medium,
        });
    }

    if let Some(process) = &wanted.process {
        let held_tools: &[String] = held
            .process
            .as_ref()
            .map(|held| held.tools.as_slice())
            .unwrap_or_default();
        let added_tools: Vec<&String> = process
            .tools
            .iter()
            .filter(|tool| !held_tools.contains(*tool))
            .collect();
        if !added_tools.is_empty() {
            added.push(RequestedPermission {
                scope: "process.tools",
                detail: join(added_tools.into_iter().map(String::as_str)),
                risk: Risk::High,
            });
        }
    }

    let resources = &new.resources;
    let raised = resources.memory_mb > installed.resources.memory_mb.max(DEFAULT_MEMORY_MB)
        || resources.call_timeout_ms
            > installed
                .resources
                .call_timeout_ms
                .max(DEFAULT_CALL_TIMEOUT_MS)
        || resources.max_instances > installed.resources.max_instances.max(DEFAULT_MAX_INSTANCES);
    if raised {
        added.push(RequestedPermission {
            scope: "resources",
            detail: format!(
                "{} MiB, {} ms per call, {} instances",
                resources.memory_mb, resources.call_timeout_ms, resources.max_instances
            ),
            risk: Risk::Low,
        });
    }

    added
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the manifest document for a permissions and resources object.
    fn manifest_json(permissions: &str, resources: &str) -> String {
        format!(
            r#"{{
              "manifestVersion": 1,
              "id": "com.example.plugin",
              "name": "Example",
              "version": "0.1.0",
              "description": "An example plugin.",
              "license": "MIT",
              "homepage": "https://example.com",
              "api": "hearthsound:plugin@0.4.0",
              "provides": ["catalog"],
              "permissions": {permissions},
              "config": {{"server": null, "user": null}},
              "resources": {resources}
            }}"#
        )
    }

    /// Parses a manifest from a permissions object and a resources object.
    fn manifest_with(permissions: &str, resources: &str) -> Manifest {
        let json = manifest_json(permissions, resources);
        let settings = std::sync::Arc::new(
            hearthsound_config::SettingsStore::resolve(
                &hearthsound_config::SettingsRegistry::builtin(),
                &hearthsound_config::SettingSources::default(),
            )
            .unwrap(),
        );
        Manifest::parse(json.as_bytes(), &settings).unwrap()
    }

    /// Builds a manifest from a permissions object, with the default resources.
    fn manifest(permissions: &str) -> Manifest {
        manifest_with(
            permissions,
            r#"{"memoryMb": 128, "callTimeoutMs": 30000, "maxInstances": 2}"#,
        )
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

    #[test]
    fn an_identical_manifest_widens_nothing() {
        let permissions = r#"{
          "http": {"hosts": ["api.example.com", "$config:url"], "maxBodyBytes": 20971520},
          "kv": {"maxBytes": 2097152},
          "files": {"importRoots": true},
          "process": {"tools": ["ffmpeg"]}
        }"#;
        let installed = manifest(permissions);
        let upgrade = manifest(permissions);
        assert!(new_permissions(&upgrade, &installed).is_empty());
    }

    #[test]
    fn dropping_or_reordering_hosts_widens_nothing() {
        let installed =
            manifest(r#"{"http": {"hosts": ["a.example.com", "b.example.com", "$config:url"]}}"#);
        let upgrade = manifest(r#"{"http": {"hosts": ["$config:url", "a.example.com"]}}"#);
        assert!(new_permissions(&upgrade, &installed).is_empty());
    }

    #[test]
    fn an_added_public_host_is_reported_alone() {
        let installed = manifest(r#"{"http": {"hosts": ["a.example.com"]}}"#);
        let upgrade = manifest(r#"{"http": {"hosts": ["b.example.com", "a.example.com"]}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "http.hosts",
                detail: "b.example.com".to_owned(),
                risk: Risk::Low,
            }]
        );
    }

    #[test]
    fn an_added_configured_host_is_medium() {
        let installed = manifest(r#"{"http": {"hosts": ["a.example.com"]}}"#);
        let upgrade = manifest(r#"{"http": {"hosts": ["a.example.com", "$config:other"]}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "http.hosts",
                detail: "$config:other".to_owned(),
                risk: Risk::Medium,
            }]
        );
    }

    #[test]
    fn raising_the_kv_quota_is_reported_and_lowering_it_is_not() {
        let installed = manifest(r#"{"kv": {"maxBytes": 2097152}}"#);
        let upgrade = manifest(r#"{"kv": {"maxBytes": 4194304}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "kv.maxBytes",
                detail: "4194304 bytes".to_owned(),
                risk: Risk::Low,
            }]
        );

        let installed = manifest(r#"{"kv": {"maxBytes": 4194304}}"#);
        let upgrade = manifest(r#"{"kv": {"maxBytes": 2097152}}"#);
        assert!(new_permissions(&upgrade, &installed).is_empty());
    }

    #[test]
    fn raising_the_body_quota_past_the_default_is_reported() {
        let installed = manifest(r#"{"http": {"hosts": ["api.example.com"]}}"#);
        let upgrade =
            manifest(r#"{"http": {"hosts": ["api.example.com"], "maxBodyBytes": 20971520}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "http.maxBodyBytes",
                detail: "20971520 bytes".to_owned(),
                risk: Risk::Low,
            }]
        );

        let installed =
            manifest(r#"{"http": {"hosts": ["api.example.com"], "maxBodyBytes": 1048576}}"#);
        let upgrade = manifest(r#"{"http": {"hosts": ["api.example.com"]}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "http.maxBodyBytes",
                detail: "10485760 bytes".to_owned(),
                risk: Risk::Low,
            }]
        );
    }

    #[test]
    fn granting_import_roots_is_medium_and_revoking_them_widens_nothing() {
        let installed = manifest("{}");
        let upgrade = manifest(r#"{"files": {"importRoots": true}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "files.importRoots",
                detail: "import roots granted on this instance".to_owned(),
                risk: Risk::Medium,
            }]
        );

        let installed = manifest(r#"{"files": {"importRoots": true}}"#);
        let upgrade = manifest(r#"{"files": {"importRoots": false}}"#);
        assert!(new_permissions(&upgrade, &installed).is_empty());
    }

    #[test]
    fn an_added_process_tool_is_high() {
        let installed = manifest(r#"{"process": {"tools": ["git"]}}"#);
        let upgrade = manifest(r#"{"process": {"tools": ["git", "ffmpeg"]}}"#);
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "process.tools",
                detail: "ffmpeg".to_owned(),
                risk: Risk::High,
            }]
        );
    }

    #[test]
    fn raising_resources_above_the_installed_value_and_the_default_is_reported() {
        let installed = manifest_with(
            "{}",
            r#"{"memoryMb": 64, "callTimeoutMs": 30000, "maxInstances": 2}"#,
        );
        let upgrade = manifest_with(
            "{}",
            r#"{"memoryMb": 512, "callTimeoutMs": 30000, "maxInstances": 2}"#,
        );
        assert_eq!(
            new_permissions(&upgrade, &installed),
            [RequestedPermission {
                scope: "resources",
                detail: "512 MiB, 30000 ms per call, 2 instances".to_owned(),
                risk: Risk::Low,
            }]
        );

        let upgrade = manifest_with(
            "{}",
            r#"{"memoryMb": 128, "callTimeoutMs": 30000, "maxInstances": 2}"#,
        );
        assert!(new_permissions(&upgrade, &installed).is_empty());
    }
}
