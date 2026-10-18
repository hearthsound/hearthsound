//! The administrative lifecycle of installed plugins.
//!
//! [`PluginAdmin`] is the one place where an installed plugin changes state:
//! an archive is installed and recorded as pending approval, an approver
//! grants its requested permissions, an administrator switches it on or off,
//! and removal deletes its row and its files. Every change publishes a
//! `plugin.status` event to administrator connections. Repository writes do
//! not publish, so the event always describes a change that was made.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use hearthsound_core::events::{Audience, EventBus, PluginStatus, ServerEvent};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::InstallError;
use crate::host::{PluginHost, parse_version};
use crate::install::{PluginRegistration, registration, requests_new_permissions};
use crate::manifest::{Manifest, ManifestError};
use crate::repo::{PluginRepository, PluginRow, PluginSource, PluginState, RepoError};

/// An error from an administrative plugin operation.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// No installed plugin has this id.
    #[error("no plugin `{0}` is installed")]
    NotFound(String),
    /// The plugin is waiting for approval, so it cannot be switched.
    #[error("plugin `{0}` is pending approval")]
    PendingApproval(String),
    /// The plugin is not waiting for approval, so there is nothing to approve.
    #[error("plugin `{id}` is {state} and has nothing to approve")]
    NothingToApprove {
        /// The plugin id.
        id: String,
        /// The state the plugin is in.
        state: &'static str,
    },
    /// The plugin has no staged upgrade to decline.
    #[error("plugin `{0}` has no staged upgrade")]
    NoUpgradeStaged(String),
    /// Installing or removing files failed.
    #[error(transparent)]
    Install(#[from] InstallError),
    /// The plugin database could not be read or written.
    #[error(transparent)]
    Repo(#[from] RepoError),
}

/// Installs, approves, switches and removes plugins, publishing their state.
pub struct PluginAdmin {
    host: Arc<PluginHost>,
    repository: PluginRepository,
    events: Option<Arc<EventBus>>,
}

impl PluginAdmin {
    /// Creates the admin service.
    ///
    /// # Parameters
    ///
    /// - `host`: the plugin host that owns the plugins directory.
    /// - `repository`: the plugin rows in the database.
    /// - `events`: the bus that `plugin.status` events are published on, or
    ///   `None` when no event stream runs.
    pub fn new(
        host: Arc<PluginHost>,
        repository: PluginRepository,
        events: Option<Arc<EventBus>>,
    ) -> Self {
        Self {
            host,
            repository,
            events,
        }
    }

    /// Installs an archive and records it.
    ///
    /// The archive is checked against `expected_sha256` first, when given, then
    /// installed, then recorded. A plugin that is not recorded yet is recorded
    /// as pending approval unless `approved` is set. A newer version of a
    /// recorded plugin is switched in at once when it requests no permission
    /// the installed version lacks; the old files are then removed. A newer
    /// version that requests more is staged as an upgrade instead: the
    /// installed version keeps serving until [`PluginAdmin::approve`] applies
    /// it or [`PluginAdmin::decline_upgrade`] discards it. Installing the
    /// recorded version again is refused, and so is an older version.
    ///
    /// # Parameters
    ///
    /// - `archive`: the `.hsplugin` file to install.
    /// - `expected_sha256`: the digest the archive must have, when it is known.
    /// - `source`: where the archive came from.
    /// - `repository_id`: the repository the archive was fetched from, if any.
    /// - `approved`: whether a plugin that is not recorded yet is approved on
    ///   install. Upgrades never grant approval.
    ///
    /// # Errors
    ///
    /// Returns [`InstallError::IntegrityFailed`] when the digest differs,
    /// [`InstallError::AlreadyInstalled`] when the recorded version is
    /// installed again, [`InstallError::Downgrade`] when an older version is
    /// installed, and the host's install errors otherwise.
    pub async fn install_archive(
        &self,
        archive: &Path,
        expected_sha256: Option<[u8; 32]>,
        source: PluginSource,
        repository_id: Option<Uuid>,
        approved: bool,
    ) -> Result<PluginRegistration, InstallError> {
        if let Some(expected) = expected_sha256 {
            let bytes = tokio::fs::read(archive)
                .await
                .map_err(|source| InstallError::Io {
                    path: archive.to_path_buf(),
                    source,
                })?;
            let actual: [u8; 32] = Sha256::digest(&bytes).into();
            if actual != expected {
                return Err(InstallError::IntegrityFailed {
                    expected: to_hex(&expected),
                    actual: to_hex(&actual),
                });
            }
        }

        let installed = self.host.install(archive).await?;
        let manifest = installed.manifest();
        let Some(existing) = self.repository.plugin(&manifest.id).await? else {
            let registration = self
                .repository
                .record_install(&installed, source, repository_id, approved)
                .await?;
            let state = if approved {
                PluginState::Enabled
            } else {
                PluginState::PendingApproval
            };
            self.publish(&registration.id, state.as_str());
            return Ok(registration);
        };

        let installed_version = parse_version(&existing.version)?;
        let new_version = parse_version(&manifest.version)?;
        if new_version <= installed_version {
            self.host
                .remove_version(&manifest.id, &manifest.version)
                .await?;
            return Err(if new_version == installed_version {
                InstallError::AlreadyInstalled {
                    id: manifest.id.clone(),
                    version: existing.version,
                }
            } else {
                InstallError::Downgrade {
                    id: manifest.id.clone(),
                    installed: existing.version,
                    requested: manifest.version.clone(),
                }
            });
        }

        let installed_manifest: Manifest = serde_json::from_str(&existing.manifest)
            .map_err(|error| InstallError::Manifest(ManifestError::Parse(error)))?;
        let serving = existing.approved_permissions.is_some();
        if serving && requests_new_permissions(manifest, &installed_manifest) {
            let replaced = self
                .repository
                .stage_upgrade(&installed, source, repository_id)
                .await?;
            if let Some(replaced) = replaced.filter(|old| old.version != manifest.version) {
                self.host
                    .remove_version(&manifest.id, &replaced.version)
                    .await?;
            }
            let staged = registration(&installed, source);
            self.publish(&staged.id, PluginState::UpgradePendingApproval.as_str());
            return Ok(staged);
        }

        let staged = self.repository.upgrade(&manifest.id).await?;
        let superseded = match staged {
            Some(staged) if parse_version(&staged.version)? <= new_version => Some(staged),
            _ => None,
        };
        let switched = self
            .repository
            .switch_version(&installed, &existing, source, repository_id)
            .await?;
        if let Some(superseded) = superseded {
            self.repository.decline_upgrade(&manifest.id).await?;
            self.host
                .remove_version(&manifest.id, &superseded.version)
                .await?;
        }
        self.host
            .remove_version(&manifest.id, &existing.version)
            .await?;
        self.publish(&switched.id, switched.state(false).as_str());
        Ok(registration(&installed, source))
    }

    /// Approves a plugin's requested permissions, or applies its staged upgrade.
    ///
    /// A plugin waiting for its first approval is enabled once approved. A
    /// plugin with a staged upgrade has the upgrade applied: its row switches to
    /// the new version and the files of the old version are removed.
    ///
    /// # Errors
    ///
    /// Returns [`AdminError::NotFound`] for an unknown id and
    /// [`AdminError::NothingToApprove`] when the plugin has neither a first
    /// approval pending nor a staged upgrade.
    pub async fn approve(&self, id: &str) -> Result<PluginRow, AdminError> {
        let row = self.require(id).await?;
        let upgrade_staged = self.repository.upgrade(id).await?.is_some();
        let state = row.state(upgrade_staged);
        match state {
            PluginState::PendingApproval => {
                let approved = self
                    .repository
                    .approve(id)
                    .await?
                    .ok_or_else(|| AdminError::NotFound(id.to_owned()))?;
                self.publish(id, PluginState::Enabled.as_str());
                Ok(approved)
            }
            PluginState::UpgradePendingApproval => {
                let old_version = row.version;
                let applied = self
                    .repository
                    .apply_upgrade(id)
                    .await?
                    .ok_or_else(|| AdminError::NotFound(id.to_owned()))?;
                self.host.remove_version(id, &old_version).await?;
                self.publish(id, applied.state(false).as_str());
                Ok(applied)
            }
            PluginState::Enabled | PluginState::Disabled => Err(AdminError::NothingToApprove {
                id: id.to_owned(),
                state: state.as_str(),
            }),
        }
    }

    /// Discards a staged upgrade and keeps the installed version in service.
    ///
    /// # Returns
    ///
    /// The plugin row, which no longer has a staged upgrade.
    ///
    /// # Errors
    ///
    /// Returns [`AdminError::NotFound`] for an unknown id and
    /// [`AdminError::NoUpgradeStaged`] when no upgrade is waiting.
    pub async fn decline_upgrade(&self, id: &str) -> Result<PluginRow, AdminError> {
        self.require(id).await?;
        let declined = self
            .repository
            .decline_upgrade(id)
            .await?
            .ok_or_else(|| AdminError::NoUpgradeStaged(id.to_owned()))?;
        self.host.remove_version(id, &declined.version).await?;
        let row = self.require(id).await?;
        self.publish(id, row.state(false).as_str());
        Ok(row)
    }

    /// Switches an approved plugin on or off.
    ///
    /// # Errors
    ///
    /// Returns [`AdminError::NotFound`] for an unknown id and
    /// [`AdminError::PendingApproval`] while the plugin awaits approval, in
    /// either direction.
    pub async fn set_enabled(&self, id: &str, enabled: bool) -> Result<(), AdminError> {
        let row = self.require(id).await?;
        if row.state(false) == PluginState::PendingApproval {
            return Err(AdminError::PendingApproval(id.to_owned()));
        }
        self.repository.set_plugin_enabled(id, enabled).await?;
        let state = if enabled {
            PluginState::Enabled
        } else {
            PluginState::Disabled
        };
        self.publish(id, state.as_str());
        Ok(())
    }

    /// Removes a plugin: its row, its instances, layer assignments and key-value
    /// data (by cascade), and its files.
    ///
    /// The row goes first, so a failure while deleting files leaves no row
    /// pointing at a plugin that is gone.
    ///
    /// # Returns
    ///
    /// The row that was removed, so the caller can report it.
    ///
    /// # Errors
    ///
    /// Returns [`AdminError::NotFound`] for an unknown id and the file removal
    /// error otherwise.
    pub async fn remove(&self, id: &str) -> Result<PluginRow, AdminError> {
        let row = self.require(id).await?;
        self.repository.delete(id).await?;
        self.host.remove_plugin(id).await?;
        self.publish(id, "removed");
        Ok(row)
    }

    /// Loads a plugin row or reports it as not found.
    async fn require(&self, id: &str) -> Result<PluginRow, AdminError> {
        self.repository
            .plugin(id)
            .await?
            .ok_or_else(|| AdminError::NotFound(id.to_owned()))
    }

    /// Publishes a `plugin.status` event to administrator connections.
    fn publish(&self, plugin_id: &str, state: &str) {
        if let Some(events) = &self.events {
            events.publish_for(
                Audience::Admins,
                ServerEvent::PluginStatus(PluginStatus {
                    plugin_id: plugin_id.to_owned(),
                    slug: None,
                    state: state.to_owned(),
                    detail: None,
                }),
            );
        }
    }
}

/// Renders bytes as lowercase hexadecimal.
fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
