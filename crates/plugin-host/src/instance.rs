//! What one configured provider instance is.
//!
//! An instance is the unit an administrator configures: a plugin, the
//! server-scoped configuration handed to it, the permissions its plugin was
//! approved for, and the host-level settings the administrator chose (import
//! roots and concurrency). The host builds one [`HttpClient`](crate::http::HttpClient)
//! and one pool of guest stores per instance from this.
//!
//! This module deliberately holds only data. Loading instances from the
//! database, and turning them into provider trait objects, happens one layer up.

use std::path::PathBuf;

use hearthsound_core::{ImportMode, Slug};
use serde::{Deserialize, Serialize};

use crate::manifest::Permissions;

/// The defaults a host configuration falls back to.
const DEFAULT_MAX_CONCURRENT: u32 = 2;
const DEFAULT_SEARCHES_PER_HOUR: u32 = 120;

/// How one plugin instance is configured.
///
/// The slug and the server configuration are what the plugin sees; the
/// permissions are what the host passes to every import's permission check; the
/// host configuration carries the settings that are the administrator's, not
/// the plugin's.
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceConfig {
    /// The instance slug, unique among the server's provider instances.
    pub slug: Slug,
    /// The server-scoped configuration as JSON text, with secrets decrypted.
    /// An instance with no configuration uses `{}`.
    pub server_config: String,
    /// The permissions the plugin was approved for. Every host import checks
    /// against this set, not against what the plugin asked for.
    pub permissions: Permissions,
    /// The host-level settings of the instance.
    pub host_config: InstanceHostConfig,
}

impl InstanceConfig {
    /// Creates the configuration of an instance with no permissions, no host
    /// configuration, and an empty server configuration.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance slug.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::Slug;
    /// use hearthsound_plugin_host::InstanceConfig;
    ///
    /// let config = InstanceConfig::new(Slug::new("hello").unwrap());
    /// assert_eq!(config.server_config, "{}");
    /// assert!(config.permissions.http.is_none());
    /// ```
    pub fn new(slug: Slug) -> InstanceConfig {
        InstanceConfig {
            slug,
            server_config: "{}".to_owned(),
            permissions: Permissions::default(),
            host_config: InstanceHostConfig::default(),
        }
    }

    /// Sets the server-scoped configuration.
    ///
    /// # Parameters
    ///
    /// - `server_config`: the configuration as JSON text.
    pub fn with_server_config(mut self, server_config: impl Into<String>) -> InstanceConfig {
        self.server_config = server_config.into();
        self
    }

    /// Sets the approved permissions.
    ///
    /// # Parameters
    ///
    /// - `permissions`: the permissions the plugin was approved for.
    pub fn with_permissions(mut self, permissions: Permissions) -> InstanceConfig {
        self.permissions = permissions;
        self
    }

    /// Sets the host-level configuration.
    ///
    /// # Parameters
    ///
    /// - `host_config`: the import roots and concurrency settings.
    pub fn with_host_config(mut self, host_config: InstanceHostConfig) -> InstanceConfig {
        self.host_config = host_config;
        self
    }
}

/// The host-level settings of one instance.
///
/// These are the administrator's choices, not the plugin's: the plugin may only
/// name an import root by id, and it cannot change how many of its calls run at
/// once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstanceHostConfig {
    /// The directories the instance may read and write, each with an id the
    /// plugin names.
    #[serde(default)]
    pub import_roots: Vec<ImportRoot>,
    /// How many calls of the instance may run at once.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u32,
    /// How many acquisition searches of the instance may run at once.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_searches: u32,
    /// How many acquisition searches the instance may start per hour.
    #[serde(default = "default_searches_per_hour")]
    pub searches_per_hour: u32,
}

impl Default for InstanceHostConfig {
    fn default() -> InstanceHostConfig {
        InstanceHostConfig {
            import_roots: Vec::new(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_concurrent_searches: DEFAULT_MAX_CONCURRENT,
            searches_per_hour: DEFAULT_SEARCHES_PER_HOUR,
        }
    }
}

impl InstanceHostConfig {
    /// Returns the import root with `id`, when the instance grants one.
    ///
    /// # Parameters
    ///
    /// - `id`: the root id a plugin named.
    pub fn import_root(&self, id: &str) -> Option<&ImportRoot> {
        self.import_roots.iter().find(|root| root.id == id)
    }
}

/// One directory an acquisition instance may reach.
///
/// The administrator sets `path`; the plugin only ever names the root by `id`,
/// so it cannot widen its own reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportRoot {
    /// The id the plugin names this root by.
    pub id: String,
    /// The absolute host directory of the root.
    pub path: PathBuf,
    /// What happens to a file after it is imported out of this root.
    pub mode: ImportMode,
    /// Whether the root may only be read.
    #[serde(default)]
    pub read_only: bool,
}

/// The default concurrency of an instance.
fn default_max_concurrent() -> u32 {
    DEFAULT_MAX_CONCURRENT
}

/// The default search budget of an instance.
fn default_searches_per_hour() -> u32 {
    DEFAULT_SEARCHES_PER_HOUR
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_the_import_mode_to_the_administrator() {
        let config: InstanceHostConfig = serde_json::from_str(
            r#"{"importRoots":[{"id":"slskd","path":"/imports/slskd","mode":"move","readOnly":false}]}"#,
        )
        .unwrap();
        assert_eq!(config.max_concurrent, DEFAULT_MAX_CONCURRENT);
        assert_eq!(config.searches_per_hour, DEFAULT_SEARCHES_PER_HOUR);
        let root = config.import_root("slskd").unwrap();
        assert_eq!(root.path, PathBuf::from("/imports/slskd"));
        assert_eq!(root.mode, ImportMode::Move);
        assert!(!root.read_only);
    }
}
