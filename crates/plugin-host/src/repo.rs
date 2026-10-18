//! The plugin-side repositories: installed plugins, provider instances, layer
//! assignments, and the auto-disable write.
//!
//! These are the tables `hearthsound-plugin-host` owns. Every write goes through
//! the single writer pool and every read through the reader pool, and the
//! statements are compile-checked against the schema.
//!
//! The repository is deliberately thin: it moves rows and nothing else. What an
//! instance's configuration must look like, and what a layer assignment must
//! satisfy, is decided one layer up in [`crate::instances`].

use std::str::FromStr;

use crate::manifest::Permissions;
use hearthsound_core::events::{Audience, EventBus, PluginStatus, ServerEvent};
use hearthsound_core::{Layer, new_uuid_v7};
use hearthsound_db::Database;
use sqlx::SqlitePool;
use uuid::Uuid;

/// Where an installed plugin came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    /// The official plugin index.
    Official,
    /// A third-party repository the administrator added.
    Repository,
    /// An uploaded `.hsplugin` file.
    Sideload,
    /// Shipped with the server.
    Bundled,
}

impl PluginSource {
    /// Every source, in the order of the data model's constraint.
    pub const ALL: [PluginSource; 4] = [
        PluginSource::Official,
        PluginSource::Repository,
        PluginSource::Sideload,
        PluginSource::Bundled,
    ];

    /// Returns the literal stored in `plugins.source`.
    pub const fn as_str(self) -> &'static str {
        match self {
            PluginSource::Official => "official",
            PluginSource::Repository => "repository",
            PluginSource::Sideload => "sideload",
            PluginSource::Bundled => "bundled",
        }
    }
}

impl std::fmt::Display for PluginSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`PluginSource`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown plugin source `{0}`")]
pub struct UnknownPluginSource(pub String);

impl FromStr for PluginSource {
    type Err = UnknownPluginSource;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PluginSource::ALL
            .iter()
            .copied()
            .find(|source| source.as_str() == s)
            .ok_or_else(|| UnknownPluginSource(s.to_owned()))
    }
}

/// One row of the `plugins` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    /// The plugin id from the manifest.
    pub id: String,
    /// The installed version.
    pub version: String,
    /// The API version the component targets.
    pub api: String,
    /// The manifest as installed, JSON text.
    pub manifest: String,
    /// The SHA-256 of the archive the plugin was installed from.
    pub sha256: Vec<u8>,
    /// Where the plugin came from.
    pub source: String,
    /// The repository the plugin was installed from, when it came from one.
    pub repository_id: Option<Uuid>,
    /// The permissions the administrator approved, as JSON text.
    pub approved_permissions: Option<String>,
    /// Whether the plugin is active.
    pub enabled: bool,
    /// When the plugin was installed, in unix milliseconds.
    pub installed_at: i64,
}

/// The lifecycle state of an installed plugin, as administrators see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginState {
    /// Installed but not yet approved: its permissions are not granted and it
    /// does not load.
    PendingApproval,
    /// Approved and switched off.
    Disabled,
    /// Approved and active.
    Enabled,
    /// Active, with a newer version staged until its new permissions are
    /// approved.
    UpgradePendingApproval,
}

impl PluginState {
    /// The wire literal for this state.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending-approval",
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::UpgradePendingApproval => "upgrade-pending-approval",
        }
    }
}

impl PluginRow {
    /// The lifecycle state of this row.
    ///
    /// `upgrade_pending` says whether an upgrade is staged for this plugin. The
    /// caller loads it, because the plugins table does not join the upgrade
    /// table.
    pub fn state(&self, upgrade_pending: bool) -> PluginState {
        if self.approved_permissions.is_none() {
            PluginState::PendingApproval
        } else if upgrade_pending {
            PluginState::UpgradePendingApproval
        } else if self.enabled {
            PluginState::Enabled
        } else {
            PluginState::Disabled
        }
    }
}

/// One staged upgrade of an installed plugin, held until an administrator
/// approves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUpgrade {
    /// The plugin id, shared with the installed row.
    pub plugin_id: String,
    /// The staged version.
    pub version: String,
    /// The API version the staged component targets.
    pub api: String,
    /// The staged manifest, JSON text.
    pub manifest: String,
    /// The SHA-256 of the staged archive.
    pub sha256: Vec<u8>,
    /// Where the staged plugin came from.
    pub source: String,
    /// The repository the staged plugin was fetched from, if any.
    pub repository_id: Option<Uuid>,
    /// When the upgrade was staged, in unix milliseconds.
    pub staged_at: i64,
}

/// One row of the `provider_instances` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceRow {
    /// The instance id.
    pub id: Uuid,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// The instance slug.
    pub slug: String,
    /// The name shown in the admin UI.
    pub display_name: String,
    /// The server-scoped configuration at rest, JSON text with secrets sealed.
    pub server_config: Option<String>,
    /// The host-level settings, JSON text.
    pub host_config: Option<String>,
    /// Whether the instance is active.
    pub enabled: bool,
    /// The last health check result, JSON text.
    pub health: Option<String>,
    /// When the instance was created, in unix milliseconds.
    pub created_at: i64,
    /// When the instance was last changed, in unix milliseconds.
    pub updated_at: i64,
}

/// One ordered layer assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// The layer the instance is assigned to.
    pub layer: Layer,
    /// The position within the layer, first is primary.
    pub position: i64,
    /// The assigned instance.
    pub instance_id: Uuid,
}

/// An error from the plugin repositories.
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// The database reported an error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A stored layer name is not a known layer.
    #[error("stored layer `{0}` is not a known layer")]
    UnknownLayer(String),
    /// A value that must be stored as JSON text could not be serialized.
    #[error("a value could not be stored as JSON: {0}")]
    Serialize(#[from] serde_json::Error),
    /// A value in the database is malformed.
    #[error("{column} holds a value that cannot be read: {reason}")]
    Malformed {
        /// The column that could not be read.
        column: &'static str,
        /// Why the value could not be read.
        reason: String,
    },
}

/// The repositories of the plugin tables.
#[derive(Clone)]
pub struct PluginRepository {
    writer: SqlitePool,
    readers: SqlitePool,
}

impl PluginRepository {
    /// Creates the repositories over an open database.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database. Writes use its writer pool and reads its
    ///   reader pool.
    pub fn new(database: &Database) -> PluginRepository {
        PluginRepository {
            writer: database.writer().clone(),
            readers: database.readers().clone(),
        }
    }

    /// Inserts or replaces one plugin row.
    ///
    /// # Parameters
    ///
    /// - `row`: the plugin to store.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn upsert_plugin(&self, row: &PluginRow) -> Result<(), RepoError> {
        sqlx::query!(
            "INSERT INTO plugins
               (id, version, api, manifest, sha256, source, repository_id,
                approved_permissions, enabled, installed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               version = excluded.version,
               api = excluded.api,
               manifest = excluded.manifest,
               sha256 = excluded.sha256,
               source = excluded.source,
               repository_id = excluded.repository_id,
               approved_permissions = excluded.approved_permissions,
               enabled = excluded.enabled,
               installed_at = excluded.installed_at",
            row.id,
            row.version,
            row.api,
            row.manifest,
            row.sha256,
            row.source,
            row.repository_id
                .as_ref()
                .map(|id| id.as_bytes().as_slice()),
            row.approved_permissions,
            row.enabled,
            row.installed_at,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Reads one plugin.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the row cannot be read.
    pub async fn plugin(&self, id: &str) -> Result<Option<PluginRow>, RepoError> {
        let row = sqlx::query!(
            "SELECT id, version, api, manifest, sha256, source, repository_id, approved_permissions, enabled, installed_at
             FROM plugins WHERE id = ?",
            id,
        )
        .fetch_optional(&self.readers)
        .await?;

        row.map(|row| -> Result<PluginRow, RepoError> {
            Ok(PluginRow {
                id: row.id,
                version: row.version,
                api: row.api,
                manifest: row.manifest,
                sha256: row.sha256,
                source: row.source,
                repository_id: optional_uuid("plugins.repository_id", row.repository_id)?,
                approved_permissions: row.approved_permissions,
                enabled: row.enabled != 0,
                installed_at: row.installed_at,
            })
        })
        .transpose()
    }

    /// Reads every plugin, ordered by id.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the rows cannot be read.
    pub async fn plugins(&self) -> Result<Vec<PluginRow>, RepoError> {
        let rows = sqlx::query!(
            "SELECT id, version, api, manifest, sha256, source, repository_id, approved_permissions, enabled, installed_at
             FROM plugins ORDER BY id",
        )
        .fetch_all(&self.readers)
        .await?;

        rows.into_iter()
            .map(|row| -> Result<PluginRow, RepoError> {
                Ok(PluginRow {
                    id: row.id,
                    version: row.version,
                    api: row.api,
                    manifest: row.manifest,
                    sha256: row.sha256,
                    source: row.source,
                    repository_id: optional_uuid("plugins.repository_id", row.repository_id)?,
                    approved_permissions: row.approved_permissions,
                    enabled: row.enabled != 0,
                    installed_at: row.installed_at,
                })
            })
            .collect()
    }

    /// Enables or disables one plugin.
    ///
    /// Disabling a plugin takes its instances out of the registry without
    /// touching their own enabled flags, so re-enabling the plugin brings them
    /// back.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    /// - `enabled`: whether the plugin is active.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<(), RepoError> {
        sqlx::query!("UPDATE plugins SET enabled = ? WHERE id = ?", enabled, id)
            .execute(&self.writer)
            .await?;
        Ok(())
    }

    /// Approves a pending plugin.
    ///
    /// The approved permissions become the ones the stored manifest requests,
    /// and the plugin is enabled.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Returns
    ///
    /// The updated row, or `None` when no such plugin is installed.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError`] when the stored manifest cannot be parsed or the
    /// write fails.
    pub async fn approve(&self, id: &str) -> Result<Option<PluginRow>, RepoError> {
        let Some(row) = self.plugin(id).await? else {
            return Ok(None);
        };
        let stored: StoredPermissions = serde_json::from_str(&row.manifest)?;
        let approved = serde_json::to_string(&stored.permissions)?;
        sqlx::query!(
            "UPDATE plugins SET approved_permissions = ?, enabled = 1 WHERE id = ?",
            approved,
            id,
        )
        .execute(&self.writer)
        .await?;
        self.plugin(id).await
    }

    /// Deletes one plugin row.
    ///
    /// The plugin's instances, layer assignments, key-value rows and staged
    /// upgrade are removed with it by cascade.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Returns
    ///
    /// Whether a row was removed.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn delete(&self, id: &str) -> Result<bool, RepoError> {
        let result = sqlx::query!("DELETE FROM plugins WHERE id = ?", id)
            .execute(&self.writer)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Reads the upgrade staged for one plugin.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Returns
    ///
    /// The staged upgrade, or `None` when nothing is staged.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the row cannot be read.
    pub async fn upgrade(&self, id: &str) -> Result<Option<PluginUpgrade>, RepoError> {
        select_upgrade(&self.readers, id).await
    }

    /// Stages an upgrade for an installed plugin, replacing any staged one.
    ///
    /// # Parameters
    ///
    /// - `upgrade`: the staged version. Its plugin must already be installed.
    ///
    /// # Returns
    ///
    /// The upgrade this one replaced, when there was one. The caller prunes
    /// the files that replaced upgrade staged.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn insert_upgrade(
        &self,
        upgrade: &PluginUpgrade,
    ) -> Result<Option<PluginUpgrade>, RepoError> {
        let mut transaction = self.writer.begin().await?;
        let replaced = select_upgrade(&mut *transaction, &upgrade.plugin_id).await?;
        sqlx::query!(
            "INSERT INTO plugin_upgrades
                (plugin_id, version, api, manifest, sha256, source, repository_id, staged_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(plugin_id) DO UPDATE SET
                version = excluded.version,
                api = excluded.api,
                manifest = excluded.manifest,
                sha256 = excluded.sha256,
                source = excluded.source,
                repository_id = excluded.repository_id,
                staged_at = excluded.staged_at",
            upgrade.plugin_id,
            upgrade.version,
            upgrade.api,
            upgrade.manifest,
            upgrade.sha256,
            upgrade.source,
            upgrade
                .repository_id
                .as_ref()
                .map(|uuid| uuid.as_bytes().as_slice()),
            upgrade.staged_at,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(replaced)
    }

    /// Applies the staged upgrade to the installed row.
    ///
    /// The row takes the staged version, manifest, digest and source, and its
    /// approved permissions become the staged manifest's requested set. The
    /// enabled flag is left as it was. The staged row is removed in the same
    /// transaction.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Returns
    ///
    /// The updated plugin row, or `None` when no upgrade is staged.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError`] when the staged manifest cannot be parsed or the
    /// write fails.
    pub async fn apply_upgrade(&self, id: &str) -> Result<Option<PluginRow>, RepoError> {
        let mut transaction = self.writer.begin().await?;
        let Some(upgrade) = select_upgrade(&mut *transaction, id).await? else {
            return Ok(None);
        };
        let stored: StoredPermissions = serde_json::from_str(&upgrade.manifest)?;
        let approved = serde_json::to_string(&stored.permissions)?;
        sqlx::query!(
            "UPDATE plugins SET version = ?, api = ?, manifest = ?, sha256 = ?, source = ?,
                repository_id = ?, approved_permissions = ?
             WHERE id = ?",
            upgrade.version,
            upgrade.api,
            upgrade.manifest,
            upgrade.sha256,
            upgrade.source,
            upgrade
                .repository_id
                .as_ref()
                .map(|uuid| uuid.as_bytes().as_slice()),
            approved,
            id,
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query!("DELETE FROM plugin_upgrades WHERE plugin_id = ?", id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        self.plugin(id).await
    }

    /// Discards the staged upgrade, keeping the installed version in service.
    ///
    /// # Parameters
    ///
    /// - `id`: the plugin id.
    ///
    /// # Returns
    ///
    /// The discarded upgrade, or `None` when nothing was staged.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn decline_upgrade(&self, id: &str) -> Result<Option<PluginUpgrade>, RepoError> {
        let mut transaction = self.writer.begin().await?;
        let removed = select_upgrade(&mut *transaction, id).await?;
        if removed.is_some() {
            sqlx::query!("DELETE FROM plugin_upgrades WHERE plugin_id = ?", id)
                .execute(&mut *transaction)
                .await?;
        }
        transaction.commit().await?;
        Ok(removed)
    }

    /// Inserts one provider instance.
    ///
    /// # Parameters
    ///
    /// - `row`: the instance to store.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails, including when the
    /// slug is already taken or the plugin does not exist.
    pub async fn insert_instance(&self, row: &InstanceRow) -> Result<(), RepoError> {
        sqlx::query!(
            "INSERT INTO provider_instances
               (id, plugin_id, slug, display_name, server_config, host_config,
                enabled, health, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            row.id.as_bytes().as_slice(),
            row.plugin_id,
            row.slug,
            row.display_name,
            row.server_config,
            row.host_config,
            row.enabled,
            row.health,
            row.created_at,
            row.updated_at,
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Reads one instance by id.
    ///
    /// # Parameters
    ///
    /// - `id`: the instance id.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the row cannot be read and
    /// [`RepoError::Malformed`] when the stored id is not 16 bytes.
    pub async fn instance(&self, id: Uuid) -> Result<Option<InstanceRow>, RepoError> {
        let row = sqlx::query!(
            "SELECT id, plugin_id, slug, display_name, server_config, host_config,
                    enabled, health, created_at, updated_at
             FROM provider_instances WHERE id = ?",
            id.as_bytes().as_slice(),
        )
        .fetch_optional(&self.readers)
        .await?;

        row.map(|row| {
            Ok(InstanceRow {
                id: instance_id(&row.id)?,
                plugin_id: row.plugin_id,
                slug: row.slug,
                display_name: row.display_name,
                server_config: row.server_config,
                host_config: row.host_config,
                enabled: row.enabled != 0,
                health: row.health,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .transpose()
    }

    /// Reads one instance by slug.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the row cannot be read and
    /// [`RepoError::Malformed`] when the stored id is not 16 bytes.
    pub async fn instance_by_slug(&self, slug: &str) -> Result<Option<InstanceRow>, RepoError> {
        let row = sqlx::query!(
            "SELECT id, plugin_id, slug, display_name, server_config, host_config,
                    enabled, health, created_at, updated_at
             FROM provider_instances WHERE slug = ?",
            slug,
        )
        .fetch_optional(&self.readers)
        .await?;

        row.map(|row| {
            Ok(InstanceRow {
                id: instance_id(&row.id)?,
                plugin_id: row.plugin_id,
                slug: row.slug,
                display_name: row.display_name,
                server_config: row.server_config,
                host_config: row.host_config,
                enabled: row.enabled != 0,
                health: row.health,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
        })
        .transpose()
    }

    /// Reads every instance, ordered by slug.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the rows cannot be read and
    /// [`RepoError::Malformed`] when a stored id is not 16 bytes.
    pub async fn instances(&self) -> Result<Vec<InstanceRow>, RepoError> {
        let rows = sqlx::query!(
            "SELECT id, plugin_id, slug, display_name, server_config, host_config,
                    enabled, health, created_at, updated_at
             FROM provider_instances ORDER BY slug",
        )
        .fetch_all(&self.readers)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(InstanceRow {
                    id: instance_id(&row.id)?,
                    plugin_id: row.plugin_id,
                    slug: row.slug,
                    display_name: row.display_name,
                    server_config: row.server_config,
                    host_config: row.host_config,
                    enabled: row.enabled != 0,
                    health: row.health,
                    created_at: row.created_at,
                    updated_at: row.updated_at,
                })
            })
            .collect()
    }

    /// Replaces one instance's stored configuration.
    ///
    /// # Parameters
    ///
    /// - `id`: the instance id.
    /// - `display_name`: the name shown in the admin UI.
    /// - `server_config`: the sealed server-scoped configuration, JSON text.
    /// - `host_config`: the host-level settings, JSON text.
    /// - `updated_at`: the change time in unix milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn update_instance(
        &self,
        id: Uuid,
        display_name: &str,
        server_config: &str,
        host_config: &str,
        updated_at: i64,
    ) -> Result<(), RepoError> {
        sqlx::query!(
            "UPDATE provider_instances
             SET display_name = ?, server_config = ?, host_config = ?, updated_at = ?
             WHERE id = ?",
            display_name,
            server_config,
            host_config,
            updated_at,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Enables or disables one instance.
    ///
    /// # Parameters
    ///
    /// - `id`: the instance id.
    /// - `enabled`: whether the instance is active.
    /// - `updated_at`: the change time in unix milliseconds.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn set_instance_enabled(
        &self,
        id: Uuid,
        enabled: bool,
        updated_at: i64,
    ) -> Result<(), RepoError> {
        sqlx::query!(
            "UPDATE provider_instances SET enabled = ?, updated_at = ? WHERE id = ?",
            enabled,
            updated_at,
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Deletes one instance and everything that hangs off it.
    ///
    /// The instance's `plugin_kv` rows and its layer assignments go with it,
    /// because both foreign keys cascade.
    ///
    /// # Parameters
    ///
    /// - `id`: the instance id.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn delete_instance(&self, id: Uuid) -> Result<(), RepoError> {
        sqlx::query!(
            "DELETE FROM provider_instances WHERE id = ?",
            id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await?;
        Ok(())
    }

    /// Reads every layer assignment, ordered by layer and position.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::UnknownLayer`] when a stored layer is not known,
    /// [`RepoError::Malformed`] when a stored id is not 16 bytes, and
    /// [`RepoError::Database`] when the rows cannot be read.
    pub async fn assignments(&self) -> Result<Vec<Assignment>, RepoError> {
        let rows = sqlx::query!(
            "SELECT layer, position, instance_id FROM layer_assignments
             ORDER BY layer, position",
        )
        .fetch_all(&self.readers)
        .await?;

        let mut assignments = Vec::with_capacity(rows.len());
        for row in rows {
            let layer = Layer::from_str(&row.layer)
                .map_err(|_| RepoError::UnknownLayer(row.layer.clone()))?;
            assignments.push(Assignment {
                layer,
                position: row.position,
                instance_id: instance_id(&row.instance_id)?,
            });
        }
        Ok(assignments)
    }

    /// Replaces one layer's ordered instances.
    ///
    /// The whole layer is rewritten in one transaction, so a failed write leaves
    /// the previous order in place.
    ///
    /// # Parameters
    ///
    /// - `layer`: the layer to configure.
    /// - `instances`: the instance ids in the order they should be tried.
    ///
    /// # Errors
    ///
    /// Returns [`RepoError::Database`] when the write fails.
    pub async fn set_layer_assignments(
        &self,
        layer: Layer,
        instances: &[Uuid],
    ) -> Result<(), RepoError> {
        let mut transaction = self.writer.begin().await?;
        sqlx::query!(
            "DELETE FROM layer_assignments WHERE layer = ?",
            layer.as_str()
        )
        .execute(&mut *transaction)
        .await?;
        for (position, instance) in instances.iter().enumerate() {
            sqlx::query!(
                "INSERT INTO layer_assignments (layer, position, instance_id) VALUES (?, ?, ?)",
                layer.as_str(),
                position as i64,
                instance.as_bytes().as_slice(),
            )
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// Reads an instance id from its stored bytes.
fn instance_id(bytes: &[u8]) -> Result<Uuid, RepoError> {
    Uuid::from_slice(bytes).map_err(|error| RepoError::Malformed {
        column: "provider_instances.id",
        reason: error.to_string(),
    })
}

/// Reads an optional repository id from its stored bytes.
fn optional_uuid(column: &'static str, bytes: Option<Vec<u8>>) -> Result<Option<Uuid>, RepoError> {
    bytes
        .map(|bytes| {
            Uuid::from_slice(&bytes).map_err(|error| RepoError::Malformed {
                column,
                reason: error.to_string(),
            })
        })
        .transpose()
}

/// Reads the upgrade staged for one plugin through any executor.
///
/// Returns `None` when nothing is staged.
async fn select_upgrade<'c, E>(executor: E, id: &str) -> Result<Option<PluginUpgrade>, RepoError>
where
    E: sqlx::Executor<'c, Database = sqlx::Sqlite>,
{
    let row = sqlx::query!(
        "SELECT plugin_id, version, api, manifest, sha256, source, repository_id, staged_at
         FROM plugin_upgrades WHERE plugin_id = ?",
        id,
    )
    .fetch_optional(executor)
    .await?;

    row.map(|row| -> Result<PluginUpgrade, RepoError> {
        Ok(PluginUpgrade {
            plugin_id: row.plugin_id,
            version: row.version,
            api: row.api,
            manifest: row.manifest,
            sha256: row.sha256,
            source: row.source,
            repository_id: optional_uuid("plugin_upgrades.repository_id", row.repository_id)?,
            staged_at: row.staged_at,
        })
    })
    .transpose()
}

/// The part of a stored manifest that approval reads.
#[derive(serde::Deserialize)]
struct StoredPermissions {
    /// The permissions the manifest requests.
    #[serde(default)]
    permissions: Permissions,
}

/// The write side of auto-disabling: what the host does when an instance traps
/// often enough to be switched off.
///
/// The instance is disabled in the database so the next start does not bring it
/// back, and a `plugin.status` event tells every connected client without
/// polling.
#[derive(Clone)]
pub(crate) struct InstanceDisabler {
    writer: SqlitePool,
    events: std::sync::Arc<EventBus>,
    instance_id: Uuid,
    plugin: String,
    slug: String,
}

impl InstanceDisabler {
    /// Builds the disabler of one instance.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `events`: the bus the status event is published on.
    /// - `instance_id`: the instance id to disable.
    /// - `plugin`: the plugin id, for the event payload.
    /// - `slug`: the instance slug, for the event payload.
    pub(crate) fn new(
        database: &Database,
        events: std::sync::Arc<EventBus>,
        instance_id: Uuid,
        plugin: String,
        slug: String,
    ) -> InstanceDisabler {
        InstanceDisabler {
            writer: database.writer().clone(),
            events,
            instance_id,
            plugin,
            slug,
        }
    }

    /// Disables the instance and publishes the status event.
    ///
    /// A write that fails is logged rather than propagated: the caller is a
    /// trap path that must not turn a database hiccup into a second failure, and
    /// the in-memory trap counter has already disabled the instance for this
    /// run.
    ///
    /// # Parameters
    ///
    /// - `detail`: the reason, carried by the event.
    pub(crate) async fn disable(&self, detail: &str) {
        let now = now_millis();
        let result = sqlx::query!(
            "UPDATE provider_instances SET enabled = 0, updated_at = ? WHERE id = ? AND enabled != 0",
            now,
            self.instance_id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await;

        if let Err(error) = result {
            tracing::error!(
                target: "hearthsound::plugin",
                plugin = %self.plugin,
                instance = %self.slug,
                error = %error,
                "failed to disable an instance that kept trapping",
            );
        }

        self.events.publish_for(
            Audience::Admins,
            ServerEvent::PluginStatus(PluginStatus {
                plugin_id: self.plugin.clone(),
                slug: Some(self.slug.clone()),
                state: "disabled".to_owned(),
                detail: Some(detail.to_owned()),
            }),
        );
    }
}

/// Returns a fresh instance id for a row a caller is about to insert.
pub(crate) fn new_instance_id() -> Uuid {
    new_uuid_v7()
}

/// Returns the current time in unix milliseconds UTC.
pub(crate) fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    (nanos / 1_000_000) as i64
}
