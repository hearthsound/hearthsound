//! Loading the bootstrap configuration.
//!
//! Bootstrap configuration describes where the process stores its data, what it
//! listens on, and how it logs, so it is read before the database is opened. The
//! values come from an optional TOML file and from `HEARTHSOUND__SECTION__KEY`
//! environment variables, merged by the `config` crate: the environment wins
//! over the file, and a built-in default fills in whatever neither supplies.
//!
//! Data paths may contain the literal `<data>` placeholder, which is replaced
//! with the resolved data directory, so a file can stay portable:
//!
//! ```no_run
//! use std::collections::BTreeMap;
//! use std::path::Path;
//! use hearthsound_config::bootstrap::BootstrapConfig;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let config = BootstrapConfig::load(Some(Path::new("/etc/hearthsound/hearthsound.toml")), &BTreeMap::new())?;
//! assert!(config.paths.storage.starts_with(&config.paths.data));
//! # Ok(())
//! # }
//! ```
//!
//! The `[settings]` table of the same file is not interpreted here: it is
//! returned as the config-file layer of the runtime settings resolved by
//! [`crate::SettingsStore`], and republished by
//! [`BootstrapConfig::setting_sources`].

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use config::{Config, Environment, File, FileFormat};
use serde::Deserialize;

use crate::{SettingSources, split_list};

/// The root of the environment variable namespace used by the loader.
///
/// Bootstrap keys are read as `<root>__SECTION__KEY`; runtime settings use the
/// same namespace, and are left alone by this module because they are not part
/// of the raw bootstrap structs.
const ENV_ROOT: &str = "HEARTHSOUND";

/// The name of the file key holding the runtime settings table.
const SETTINGS_TABLE: &str = "settings";

/// The placeholder in path values replaced with the data directory.
const DATA_PLACEHOLDER: &str = "<data>";

/// The bind address used when neither the file nor the environment sets one.
const DEFAULT_BIND: &str = "0.0.0.0";

/// The TCP port used when neither the file nor the environment sets one.
const DEFAULT_PORT: u16 = 4327;

/// The log level used when neither the file nor the environment sets one.
const DEFAULT_LOG_LEVEL: LogLevel = LogLevel::Info;

/// The log format used when neither the file nor the environment sets one.
const DEFAULT_LOG_FORMAT: LogFormat = LogFormat::Text;

/// The level every other crate logs at when nothing sets one.
///
/// Third-party crates - a tag reader, a filesystem watcher, a codec - log
/// warnings about files the server reads by the thousand, which drowns the
/// server's own records at `info`; only their errors are worth a log line by
/// default, and `log.dependencies` raises it while diagnosing one of them.
const DEFAULT_DEPENDENCY_LOG_LEVEL: LogLevel = LogLevel::Error;

/// The name of the managed storage directory inside the data directory.
const STORAGE_DIR: &str = "storage";

/// The name of the SQLite database file inside the data directory.
const DATABASE_FILE: &str = "hearthsound.db";

/// The name of the secret key file inside the data directory.
const KEY_FILE: &str = "secret.key";

/// The name of the installed plugin directory inside the data directory.
const PLUGINS_DIR: &str = "plugins";

/// The name of the artwork cache directory inside the data directory.
const ARTWORK_DIR: &str = "artwork";

/// The name of the database backup directory inside the data directory.
const BACKUPS_DIR: &str = "backups";

/// The name of the permanent managed library directory inside storage.
const LIBRARY_DIR: &str = "library";

/// The name of the evictable managed cache directory inside storage.
const CACHE_DIR: &str = "cache";

/// The name of the acquisition staging directory inside storage.
const STAGING_DIR: &str = "staging";

/// The name of the upload staging directory inside storage.
const UPLOADS_STAGING_DIR: &str = "uploads-staging";

/// The typed bootstrap configuration of one server process.
///
/// Every path is absolute in the recommended deployments but is used exactly as
/// configured here; only the [`DATA_PLACEHOLDER`] substitution is applied to it.
#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapConfig {
    /// Address, port, and public URL of the HTTP server.
    pub server: ServerConfig,
    /// The data and storage trees and the directories derived from them.
    pub paths: PathsConfig,
    /// Location of the SQLite database.
    pub database: DatabaseConfig,
    /// Log output settings.
    pub log: LogConfig,
    /// Secret key material settings.
    pub secrets: SecretsConfig,
    /// Bundled web client settings.
    pub web: WebConfig,
    /// The config file's `[settings]` table, as the file layer of the runtime
    /// settings.
    pub settings: BTreeMap<String, toml::Value>,
}

impl BootstrapConfig {
    /// Loads and validates the bootstrap configuration.
    ///
    /// The environment wins over the file, which wins over the built-in
    /// defaults. Variables in the same namespace that belong to runtime settings
    /// (for example `HEARTHSOUND__STREAM__PENDING__MODE`) are ignored here.
    ///
    /// # Parameters
    ///
    /// - `file`: the TOML configuration file. A missing file is not an error,
    ///   so callers can pass the conventional path unconditionally.
    /// - `env`: the environment to read, keyed by full variable name. Pass the
    ///   map from [`process_env`] for the real process environment.
    ///
    /// # Returns
    ///
    /// The resolved bootstrap configuration, with `<data>` in path values
    /// replaced by the resolved data directory.
    ///
    /// # Errors
    ///
    /// Returns [`BootstrapError::Load`] when the file cannot be read, parsed, or
    /// merged, [`BootstrapError::Invalid`] when a value is present but not
    /// usable, and [`BootstrapError::NoDataDirectory`] when neither
    /// `XDG_DATA_HOME` nor `HOME` is set and the data directory is not
    /// configured.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use hearthsound_config::bootstrap::BootstrapConfig;
    ///
    /// let mut env = BTreeMap::new();
    /// env.insert("HOME".to_owned(), "/home/ada".to_owned());
    /// let config = BootstrapConfig::load(None, &env).unwrap();
    /// assert_eq!(config.paths.data.to_string_lossy(), "/home/ada/.local/share/hearthsound");
    /// assert_eq!(config.server.port, 4327);
    /// ```
    pub fn load(
        file: Option<&Path>,
        env: &BTreeMap<String, String>,
    ) -> Result<BootstrapConfig, BootstrapError> {
        let mut builder = Config::builder();
        if let Some(path) = file {
            builder = builder.add_source(File::from(path).format(FileFormat::Toml).required(false));
        }
        let injected: config::Map<String, String> = env
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        builder = builder.add_source(
            Environment::with_prefix(ENV_ROOT)
                .prefix_separator("__")
                .separator("__")
                .try_parsing(true)
                .ignore_empty(true)
                .source(Some(injected)),
        );
        let merged = builder.build()?;
        let settings = read_settings_table(&merged)?;
        let raw: RawBootstrap = merged.try_deserialize()?;
        Self::assemble(raw, settings, env)
    }

    /// Loads the bootstrap configuration from the real process environment.
    ///
    /// # Parameters
    ///
    /// - `file`: the TOML configuration file; a missing file is not an error.
    ///
    /// # Returns
    ///
    /// The resolved bootstrap configuration. The data directory falls back to
    /// the process's `XDG_DATA_HOME` or `HOME`.
    ///
    /// # Errors
    ///
    /// Same as [`BootstrapConfig::load`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::Path;
    /// use hearthsound_config::bootstrap::BootstrapConfig;
    ///
    /// let config = BootstrapConfig::load_process_env(Some(Path::new("hearthsound.toml"))).unwrap();
    /// println!("database: {}", config.database.path.display());
    /// ```
    pub fn load_process_env(file: Option<&Path>) -> Result<BootstrapConfig, BootstrapError> {
        Self::load(file, &process_env())
    }

    /// Returns the runtime setting sources for this configuration.
    ///
    /// # Parameters
    ///
    /// - `env`: the same environment map that was passed to
    ///   [`BootstrapConfig::load`].
    ///
    /// # Returns
    ///
    /// Sources whose file layer is this configuration's `[settings]` table and
    /// whose environment layer is `env`; keys from other namespaces are ignored
    /// during resolution.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use hearthsound_config::bootstrap::BootstrapConfig;
    ///
    /// let mut env = BTreeMap::new();
    /// env.insert("HOME".to_owned(), "/home/ada".to_owned());
    /// env.insert("HEARTHSOUND__STREAM__PENDING__MODE".to_owned(), "hold".to_owned());
    /// let config = BootstrapConfig::load(None, &env).unwrap();
    /// let sources = config.setting_sources(&env);
    /// assert!(sources.file.is_empty());
    /// assert!(sources.env.contains_key("HEARTHSOUND__STREAM__PENDING__MODE"));
    /// ```
    pub fn setting_sources(&self, env: &BTreeMap<String, String>) -> SettingSources {
        SettingSources::new(env.clone(), self.settings.clone())
    }

    /// Assembles the typed configuration from the raw values and defaults.
    ///
    /// Resolves the data directory first, because every other path may be
    /// written in terms of it.
    fn assemble(
        raw: RawBootstrap,
        settings: BTreeMap<String, toml::Value>,
        env: &BTreeMap<String, String>,
    ) -> Result<BootstrapConfig, BootstrapError> {
        let port = raw.server.port.unwrap_or(DEFAULT_PORT);
        if port == 0 {
            return Err(BootstrapError::Invalid {
                key: "server.port".to_owned(),
                reason: "must not be zero".to_owned(),
            });
        }

        let data = resolve_data_dir(raw.paths.data.as_deref(), env)?;
        let storage = resolve_path(
            raw.paths.storage.as_deref(),
            &format!("{DATA_PLACEHOLDER}/{STORAGE_DIR}"),
            &data,
        );
        let database = resolve_path(
            raw.database.path.as_deref(),
            &format!("{DATA_PLACEHOLDER}/{DATABASE_FILE}"),
            &data,
        );
        let key_file = resolve_path(
            raw.secrets.key_file.as_deref(),
            &format!("{DATA_PLACEHOLDER}/{KEY_FILE}"),
            &data,
        );

        Ok(BootstrapConfig {
            server: ServerConfig {
                bind: non_empty(raw.server.bind).unwrap_or_else(|| DEFAULT_BIND.to_owned()),
                port,
                base_url: raw.server.base_url.unwrap_or_default(),
                trusted_proxies: raw.server.trusted_proxies.unwrap_or_default().0,
                cors_origins: raw.server.cors_origins.unwrap_or_default().0,
            },
            paths: PathsConfig {
                plugins: data.join(PLUGINS_DIR),
                artwork: data.join(ARTWORK_DIR),
                backups: data.join(BACKUPS_DIR),
                library: storage.join(LIBRARY_DIR),
                cache: storage.join(CACHE_DIR),
                staging: storage.join(STAGING_DIR),
                uploads_staging: storage.join(UPLOADS_STAGING_DIR),
                data,
                storage,
            },
            database: DatabaseConfig { path: database },
            log: LogConfig {
                level: raw.log.level.unwrap_or(DEFAULT_LOG_LEVEL),
                format: raw.log.format.unwrap_or(DEFAULT_LOG_FORMAT),
                dependencies: raw.log.dependencies.unwrap_or(DEFAULT_DEPENDENCY_LOG_LEVEL),
            },
            secrets: SecretsConfig { key_file },
            web: WebConfig {
                root: non_empty(raw.web.root).map(PathBuf::from),
            },
            settings,
        })
    }
}

/// Address, port, and public URL of the HTTP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Address the HTTP server listens on.
    pub bind: String,
    /// TCP port the HTTP server listens on.
    pub port: u16,
    /// Public base URL used to build absolute links; empty derives it from the
    /// request.
    pub base_url: String,
    /// Proxy IPs and CIDRs whose `X-Forwarded-*` headers are trusted.
    pub trusted_proxies: Vec<String>,
    /// Allowed CORS origins as `scheme://host[:port]` or `null`, or `*` for any
    /// origin; empty denies cross-origin requests.
    pub cors_origins: Vec<String>,
}

/// The data and storage trees and the directories derived from them.
///
/// `data` and `storage` are configurable; every other directory is derived, so
/// they must not be overridden individually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsConfig {
    /// Private state: the database, plugin installs, the secret key, artwork,
    /// and backups.
    pub data: PathBuf,
    /// Managed audio tree holding `library/`, `cache/`, `staging/`, and
    /// `uploads-staging/`.
    pub storage: PathBuf,
    /// Installed plugin versions, one directory per plugin and version.
    pub plugins: PathBuf,
    /// Derived artwork cache.
    pub artwork: PathBuf,
    /// Database backups, including the pre-migration snapshot.
    pub backups: PathBuf,
    /// Managed permanent audio files.
    pub library: PathBuf,
    /// Managed evictable audio files.
    pub cache: PathBuf,
    /// Per-acquisition download workspaces.
    pub staging: PathBuf,
    /// Landing area for user uploads before they are ingested.
    pub uploads_staging: PathBuf,
}

/// Location of the SQLite database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseConfig {
    /// The SQLite database file.
    pub path: PathBuf,
}

/// Log output settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogConfig {
    /// Verbosity of Hearthsound's own log output (`log.level`).
    pub level: LogLevel,
    /// Encoding of the log output.
    pub format: LogFormat,
    /// Verbosity of every other crate's log output (`log.dependencies`).
    ///
    /// A third-party crate is any crate whose target does not begin with
    /// `hearthsound`, so this is what turns a tag reader's per-file warnings off
    /// while keeping the server's own records at `log.level`.
    pub dependencies: LogLevel,
}

/// Secret key material settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsConfig {
    /// File holding the 32-byte AES-256-GCM key.
    pub key_file: PathBuf,
}

/// Bundled web client settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// Directory served instead of the embedded web client, when set.
    pub root: Option<PathBuf>,
}

/// The verbosity the process logs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Everything, including per-subsystem detail.
    Trace,
    /// Development detail.
    Debug,
    /// Normal operation.
    Info,
    /// Degraded conditions.
    Warn,
    /// Failed operations.
    Error,
}

impl LogLevel {
    /// Returns the lowercase name of the level, as used by the configuration
    /// value and by log filters.
    pub const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The encoding of the log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable text.
    Text,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    /// Returns the lowercase name of the format, as used by the configuration
    /// value.
    pub const fn as_str(self) -> &'static str {
        match self {
            LogFormat::Text => "text",
            LogFormat::Json => "json",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An error from loading or validating the bootstrap configuration.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// The configuration sources could not be read, parsed, or merged.
    #[error("bootstrap configuration could not be loaded")]
    Load(#[from] config::ConfigError),
    /// No data directory could be determined.
    #[error("no data directory: set paths.data, XDG_DATA_HOME, or HOME")]
    NoDataDirectory,
    /// A value was present but not usable.
    #[error("bootstrap key `{key}` is invalid: {reason}")]
    Invalid {
        /// The dotted bootstrap key, for example `paths.data`.
        key: String,
        /// Why the value was rejected.
        reason: String,
    },
}

/// Reads the whole process environment.
///
/// Variables whose name or value is not valid UTF-8 are skipped: they cannot be
/// part of a textual configuration namespace.
///
/// # Returns
///
/// The environment keyed by variable name.
///
/// # Examples
///
/// ```
/// use hearthsound_config::bootstrap::process_env;
///
/// let env = process_env();
/// assert!(!env.keys().any(|name| name.is_empty()));
/// ```
pub fn process_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// Resolves the data directory from the raw value or the platform default.
///
/// # Parameters
///
/// - `raw`: the configured `paths.data` value, if any.
/// - `env`: the environment, read for `XDG_DATA_HOME` and `HOME`.
///
/// # Errors
///
/// Returns [`BootstrapError::NoDataDirectory`] when `raw` is unset or empty and
/// neither environment variable provides a home directory.
fn resolve_data_dir(
    raw: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf, BootstrapError> {
    if let Some(text) = raw.filter(|text| !text.is_empty()) {
        return Ok(PathBuf::from(text));
    }
    if let Some(dir) = env.get("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir).join("hearthsound"));
    }
    match env.get("HOME").filter(|value| !value.is_empty()) {
        Some(home) => Ok(PathBuf::from(home).join(".local/share/hearthsound")),
        None => Err(BootstrapError::NoDataDirectory),
    }
}

/// Resolves a path value by substituting the data directory placeholder.
///
/// # Parameters
///
/// - `raw`: the configured value, if any; an empty value is treated as unset.
/// - `fallback`: the value to use when `raw` is unset, which may itself contain
///   the placeholder.
/// - `data`: the resolved data directory.
///
/// # Returns
///
/// The path with every `<data>` occurrence replaced.
fn resolve_path(raw: Option<&str>, fallback: &str, data: &Path) -> PathBuf {
    let text = raw.filter(|text| !text.is_empty()).unwrap_or(fallback);
    PathBuf::from(text.replace(DATA_PLACEHOLDER, &data.to_string_lossy()))
}

/// Returns the trimmed value when it is not empty.
fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|text| !text.trim().is_empty())
}

/// Extracts the config file's `[settings]` table.
///
/// # Parameters
///
/// - `config`: the merged configuration.
///
/// # Returns
///
/// The table as TOML values, keyed by dotted setting key, or an empty map when
/// the file has no `[settings]` table.
///
/// # Errors
///
/// Returns [`BootstrapError::Invalid`] when a value cannot be represented as a
/// TOML value, and [`BootstrapError::Load`] for any other lookup failure.
fn read_settings_table(config: &Config) -> Result<BTreeMap<String, toml::Value>, BootstrapError> {
    match config.get_table(SETTINGS_TABLE) {
        Ok(table) => table
            .into_iter()
            .map(|(key, value)| {
                value
                    .try_deserialize::<toml::Value>()
                    .map(|value| (key.clone(), value))
                    .map_err(|error| BootstrapError::Invalid {
                        key: format!("{SETTINGS_TABLE}.{key}"),
                        reason: error.to_string(),
                    })
            })
            .collect(),
        Err(config::ConfigError::NotFound(_)) => Ok(BTreeMap::new()),
        Err(error) => Err(BootstrapError::Load(error)),
    }
}

/// The raw bootstrap values as read from the sources, before defaults.
#[derive(Debug, Default, Deserialize)]
struct RawBootstrap {
    /// Values of the `server` table.
    #[serde(default)]
    server: RawServer,
    /// Values of the `paths` table.
    #[serde(default)]
    paths: RawPaths,
    /// Values of the `database` table.
    #[serde(default)]
    database: RawDatabase,
    /// Values of the `log` table.
    #[serde(default)]
    log: RawLog,
    /// Values of the `secrets` table.
    #[serde(default)]
    secrets: RawSecrets,
    /// Values of the `web` table.
    #[serde(default)]
    web: RawWeb,
}

/// Raw values of the `server` table.
#[derive(Debug, Default, Deserialize)]
struct RawServer {
    /// `server.bind`.
    bind: Option<String>,
    /// `server.port`.
    port: Option<u16>,
    /// `server.base_url`.
    base_url: Option<String>,
    /// `server.trusted_proxies`.
    trusted_proxies: Option<StringList>,
    /// `server.cors_origins`.
    cors_origins: Option<StringList>,
}

/// Raw values of the `paths` table.
#[derive(Debug, Default, Deserialize)]
struct RawPaths {
    /// `paths.data`.
    data: Option<String>,
    /// `paths.storage`.
    storage: Option<String>,
}

/// Raw values of the `database` table.
#[derive(Debug, Default, Deserialize)]
struct RawDatabase {
    /// `database.path`.
    path: Option<String>,
}

/// Raw values of the `log` table.
#[derive(Debug, Default, Deserialize)]
struct RawLog {
    /// `log.level`.
    level: Option<LogLevel>,
    /// `log.format`.
    format: Option<LogFormat>,
    /// `log.dependencies`.
    dependencies: Option<LogLevel>,
}

/// Raw values of the `secrets` table.
#[derive(Debug, Default, Deserialize)]
struct RawSecrets {
    /// `secrets.key_file`.
    key_file: Option<String>,
}

/// Raw values of the `web` table.
#[derive(Debug, Default, Deserialize)]
struct RawWeb {
    /// `web.root`.
    root: Option<String>,
}

/// A list of strings accepted either as an array or as one comma-separated
/// value.
///
/// TOML files spell lists as arrays; environment variables can only hold one
/// string, so `a,b` is accepted for them.
#[derive(Debug, Clone, Default)]
struct StringList(Vec<String>);

impl<'de> Deserialize<'de> for StringList {
    fn deserialize<D>(deserializer: D) -> Result<StringList, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Accepts a sequence of strings or a single comma-separated string.
        struct ListVisitor;

        impl<'de> serde::de::Visitor<'de> for ListVisitor {
            type Value = StringList;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a list of strings or a comma-separated string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<StringList, E> {
                Ok(StringList(split_list(value)))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<StringList, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<String>()? {
                    values.push(value);
                }
                Ok(StringList(values))
            }
        }

        deserializer.deserialize_any(ListVisitor)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// Writes a TOML file into a temporary directory and returns its path
    /// together with the directory that owns it.
    fn toml_file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hearthsound.toml");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
        (dir, path)
    }

    /// Builds an environment map from `(name, value)` pairs.
    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn defaults_come_from_the_xdg_data_directory() {
        let env = env_of(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/ada")]);
        let config = BootstrapConfig::load(None, &env).unwrap();

        assert_eq!(config.paths.data, PathBuf::from("/xdg/hearthsound"));
        assert_eq!(
            config.paths.storage,
            PathBuf::from("/xdg/hearthsound/storage")
        );
        assert_eq!(
            config.database.path,
            PathBuf::from("/xdg/hearthsound/hearthsound.db")
        );
        assert_eq!(
            config.secrets.key_file,
            PathBuf::from("/xdg/hearthsound/secret.key")
        );
        assert_eq!(
            config.paths.plugins,
            PathBuf::from("/xdg/hearthsound/plugins")
        );
        assert_eq!(
            config.paths.artwork,
            PathBuf::from("/xdg/hearthsound/artwork")
        );
        assert_eq!(
            config.paths.backups,
            PathBuf::from("/xdg/hearthsound/backups")
        );
        assert_eq!(
            config.paths.library,
            PathBuf::from("/xdg/hearthsound/storage/library")
        );
        assert_eq!(
            config.paths.cache,
            PathBuf::from("/xdg/hearthsound/storage/cache")
        );
        assert_eq!(
            config.paths.staging,
            PathBuf::from("/xdg/hearthsound/storage/staging")
        );
        assert_eq!(
            config.paths.uploads_staging,
            PathBuf::from("/xdg/hearthsound/storage/uploads-staging")
        );

        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 4327);
        assert_eq!(config.server.base_url, "");
        assert!(config.server.trusted_proxies.is_empty());
        assert!(config.server.cors_origins.is_empty());
        assert_eq!(config.log.level, LogLevel::Info);
        assert_eq!(config.log.format, LogFormat::Text);
        assert_eq!(config.log.dependencies, LogLevel::Error);
        assert_eq!(config.web.root, None);
        assert!(config.settings.is_empty());
    }

    #[test]
    fn home_is_used_when_the_xdg_directory_is_unset() {
        let env = env_of(&[("HOME", "/home/ada")]);
        let config = BootstrapConfig::load(None, &env).unwrap();
        assert_eq!(
            config.paths.data,
            PathBuf::from("/home/ada/.local/share/hearthsound")
        );
    }

    #[test]
    fn missing_data_directory_is_reported() {
        let error = BootstrapConfig::load(None, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, BootstrapError::NoDataDirectory));
    }

    #[test]
    fn file_values_override_defaults_and_environment_overrides_the_file() {
        let (_dir, path) = toml_file(
            r#"
[server]
bind = "127.0.0.1"
port = 5000
base_url = "https://music.example.com"
trusted_proxies = ["10.0.0.0/8"]
cors_origins = []

[paths]
data = "/srv/hearthsound"
storage = "<data>/media"

[database]
path = "<data>/db/hearthsound.db"

[log]
level = "debug"
format = "json"
dependencies = "warn"

[secrets]
key_file = "/run/secrets/hearthsound.key"

[web]
root = "/srv/web"
"#,
        );
        let env = env_of(&[
            ("HEARTHSOUND__SERVER__PORT", "5001"),
            ("HEARTHSOUND__PATHS__STORAGE", "/mnt/audio"),
        ]);
        let config = BootstrapConfig::load(Some(&path), &env).unwrap();

        assert_eq!(config.server.bind, "127.0.0.1");
        assert_eq!(config.server.port, 5001);
        assert_eq!(config.server.base_url, "https://music.example.com");
        assert_eq!(config.server.trusted_proxies, ["10.0.0.0/8"]);
        assert!(config.server.cors_origins.is_empty());
        assert_eq!(config.paths.data, PathBuf::from("/srv/hearthsound"));
        assert_eq!(config.paths.storage, PathBuf::from("/mnt/audio"));
        assert_eq!(
            config.database.path,
            PathBuf::from("/srv/hearthsound/db/hearthsound.db")
        );
        assert_eq!(config.log.level, LogLevel::Debug);
        assert_eq!(config.log.format, LogFormat::Json);
        assert_eq!(config.log.dependencies, LogLevel::Warn);
        assert_eq!(
            config.secrets.key_file,
            PathBuf::from("/run/secrets/hearthsound.key")
        );
        assert_eq!(config.web.root, Some(PathBuf::from("/srv/web")));

        let (_unused, file_only) = toml_file(
            r#"
[paths]
data = "/srv/hearthsound"
storage = "<data>/media"
"#,
        );
        let config = BootstrapConfig::load(Some(&file_only), &BTreeMap::new()).unwrap();
        assert_eq!(
            config.paths.storage,
            PathBuf::from("/srv/hearthsound/media")
        );
    }

    #[test]
    fn comma_separated_environment_lists_are_split() {
        let env = env_of(&[
            ("HOME", "/home/ada"),
            (
                "HEARTHSOUND__SERVER__CORS_ORIGINS",
                "https://a.example, https://b.example",
            ),
            (
                "HEARTHSOUND__SERVER__TRUSTED_PROXIES",
                "10.0.0.0/8,172.16.0.0/12",
            ),
        ]);
        let config = BootstrapConfig::load(None, &env).unwrap();
        assert_eq!(
            config.server.cors_origins,
            ["https://a.example", "https://b.example"]
        );
        assert_eq!(
            config.server.trusted_proxies,
            ["10.0.0.0/8", "172.16.0.0/12"]
        );
    }

    #[test]
    fn settings_table_is_returned_as_the_file_layer() {
        let (_dir, path) = toml_file(
            r#"
[paths]
data = "/srv/hearthsound"

[settings]
"stream.pending.mode" = "hold"
"plugins.auto_update" = false
"storage.cache.max_bytes" = 21474836480
"sleep.gap_ms" = 1.5
"placeholder" = "{x}"
"#,
        );
        let config = BootstrapConfig::load(Some(&path), &BTreeMap::new()).unwrap();

        assert_eq!(
            config.settings.get("stream.pending.mode"),
            Some(&toml::Value::String("hold".to_owned()))
        );
        assert_eq!(
            config.settings.get("plugins.auto_update"),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            config.settings.get("storage.cache.max_bytes"),
            Some(&toml::Value::Integer(21_474_836_480))
        );
        assert_eq!(
            config.settings.get("sleep.gap_ms"),
            Some(&toml::Value::Float(1.5))
        );
        assert_eq!(
            config.settings.get("placeholder"),
            Some(&toml::Value::String("{x}".to_owned()))
        );
    }

    #[test]
    fn runtime_setting_environment_variables_do_not_break_loading() {
        let (_dir, path) = toml_file(
            r#"
[paths]
data = "/srv/hearthsound"

[settings]
"plugins.auto_update" = false
"#,
        );
        let env = env_of(&[
            ("HEARTHSOUND__STREAM__PENDING__MODE", "hold"),
            ("HEARTHSOUND__ACQUISITION__MAX_CONCURRENT", "2"),
            ("HEARTHSOUND__SEARCH__CATALOG__ENABLED", "false"),
            ("HEARTHSOUND__SECRET_KEY", "not-a-key"),
        ]);
        let config = BootstrapConfig::load(Some(&path), &env).unwrap();

        assert_eq!(
            config.settings.get("plugins.auto_update"),
            Some(&toml::Value::Boolean(false))
        );
        assert!(
            !config.settings.contains_key("stream.pending.mode"),
            "runtime settings must come from the file layer only"
        );

        let sources = config.setting_sources(&env);
        assert_eq!(
            sources
                .env
                .get("HEARTHSOUND__STREAM__PENDING__MODE")
                .map(String::as_str),
            Some("hold")
        );
        assert_eq!(sources.file.len(), 1);
    }

    #[test]
    fn an_absent_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let env = env_of(&[("HOME", "/home/ada")]);
        let config = BootstrapConfig::load(Some(&path), &env).unwrap();
        assert_eq!(
            config.paths.data,
            PathBuf::from("/home/ada/.local/share/hearthsound")
        );
    }

    #[test]
    fn an_invalid_file_is_reported() {
        let (_dir, path) = toml_file("[server]\nport = \"http\"\n");
        let error = BootstrapConfig::load(Some(&path), &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, BootstrapError::Load(_)));
    }

    #[test]
    fn a_zero_port_is_rejected() {
        let env = env_of(&[("HOME", "/home/ada"), ("HEARTHSOUND__SERVER__PORT", "0")]);
        let error = BootstrapConfig::load(None, &env).unwrap_err();
        match error {
            BootstrapError::Invalid { key, reason } => {
                assert_eq!(key, "server.port");
                assert_eq!(reason, "must not be zero");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn an_unknown_log_format_is_rejected() {
        let (_dir, path) = toml_file("[log]\nformat = \"xml\"\n");
        let error = BootstrapConfig::load(Some(&path), &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, BootstrapError::Load(_)));
    }

    #[test]
    fn an_empty_value_falls_back_to_its_default() {
        let env = env_of(&[
            ("HOME", "/home/ada"),
            ("HEARTHSOUND__SERVER__BIND", ""),
            ("HEARTHSOUND__PATHS__DATA", ""),
        ]);
        let config = BootstrapConfig::load(None, &env).unwrap();
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(
            config.paths.data,
            PathBuf::from("/home/ada/.local/share/hearthsound")
        );
    }

    #[test]
    fn process_env_reads_the_environment() {
        // The test binary always has a PATH; the map is keyed by full names.
        let env = process_env();
        assert!(env.contains_key("PATH") || env.is_empty());
    }
}
