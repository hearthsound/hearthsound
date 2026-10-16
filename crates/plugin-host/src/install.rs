//! Registering an installed plugin in the database, with its requested
//! permissions labelled by risk.
//!
//! Installing an archive only puts files on disk. Recording the install, and
//! deciding what the administrator has approved, is this module's job: the
//! plugin's manifest is unpacked into a `plugins` row, and the permissions the
//! manifest asks for are described with the risk labels of
//! `03-plugin-system.md` section 10 so an approver can see what they are
//! granting before the plugin is enabled.
//!
//! The installed plugin's own files are the source of truth for the manifest:
//! [`registration`](PluginRepository::register_installed) stores the
//! `manifest.json` text the install wrote, so the row can never describe a
//! different manifest than the component beside it.

use hearthsound_core::Layer;

use crate::host::InstalledPlugin;
use crate::manifest::Manifest;
use crate::repo::{PluginRepository, PluginRow, PluginSource};
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
    /// Records an installed plugin and enables it.
    ///
    /// The row's `approved_permissions` is the manifest's requested permission
    /// set and `enabled` is `1`: the command-line administrator who ran the
    /// install is the consenting party, and the host still checks the approved
    /// set inside every import. A second registration of the same plugin id
    /// replaces the row, which is how an upgrade is recorded.
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
    pub async fn register_installed(
        &self,
        plugin: &InstalledPlugin,
        source: PluginSource,
    ) -> Result<PluginRegistration, RepoError> {
        let manifest = plugin.manifest();
        let approved_permissions = serde_json::to_string(&manifest.permissions)?;

        let row = PluginRow {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            api: manifest.api.clone(),
            manifest: plugin.manifest_json().to_owned(),
            sha256: plugin.sha256().to_vec(),
            source: source.as_str().to_owned(),
            approved_permissions: Some(approved_permissions),
            enabled: true,
            installed_at: now_millis(),
        };
        self.upsert_plugin(&row).await?;

        Ok(PluginRegistration {
            id: manifest.id.clone(),
            version: manifest.version.clone(),
            api: manifest.api.clone(),
            source,
            requested: requested_permissions(manifest),
            provides: manifest.provides().to_vec(),
        })
    }
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
