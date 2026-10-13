//! The settings registry and resolution.
//!
//! Settings have three layers, resolved with a fixed precedence: environment
//! variables, then values from the configuration file, then the built-in
//! default. This crate owns the registry of known settings, the typed keys used
//! to read them, and the resolution rules. Loading the TOML file itself is the
//! job of a later component: callers hand this crate an already parsed map of
//! file values.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

/// The prefix every settings environment variable carries.
const ENV_PREFIX: &str = "HEARTHSOUND__";

/// The literal value of a setting.
///
/// Values are untyped at rest; a [`SettingKind`] says which variant is
/// acceptable for a given setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    /// A boolean value.
    Bool(bool),
    /// A 64-bit signed integer value.
    Integer(i64),
    /// A string value.
    String(String),
}

impl SettingValue {
    /// Parses an environment variable value.
    ///
    /// `true` and `false` become [`SettingValue::Bool`], anything that parses as
    /// a 64-bit signed integer becomes [`SettingValue::Integer`], and everything
    /// else stays a raw [`SettingValue::String`]. The result is validated
    /// against the setting's kind afterwards.
    ///
    /// # Parameters
    ///
    /// - `raw`: the raw environment variable value.
    ///
    /// # Returns
    ///
    /// The parsed value.
    pub fn parse_env(raw: &str) -> SettingValue {
        match raw {
            "true" => SettingValue::Bool(true),
            "false" => SettingValue::Bool(false),
            _ => match raw.parse::<i64>() {
                Ok(number) => SettingValue::Integer(number),
                Err(_) => SettingValue::String(raw.to_owned()),
            },
        }
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Bool(value) => write!(f, "{value}"),
            SettingValue::Integer(value) => write!(f, "{value}"),
            SettingValue::String(value) => f.write_str(value),
        }
    }
}

/// The type a setting must have, and any constraint on its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// A boolean.
    Bool,
    /// An integer within an inclusive range.
    Integer {
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
    },
    /// A string, optionally restricted to a fixed set of values.
    String {
        /// The accepted values; an empty slice accepts any string.
        allowed: &'static [&'static str],
    },
}

impl SettingKind {
    /// Describes the kind in a phrase suitable for an error message.
    fn expected(self) -> &'static str {
        match self {
            SettingKind::Bool => "expected a boolean",
            SettingKind::Integer { .. } => "expected an integer",
            SettingKind::String { .. } => "expected a string",
        }
    }
}

/// The definition of one known setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingDef {
    /// The dotted setting key, for example `plugins.max_memory_mb`.
    pub key: &'static str,
    /// The type and constraints of the value.
    pub kind: SettingKind,
    /// The value used when neither the environment nor the file supplies one.
    pub default: SettingValue,
    /// A one-line human readable description.
    pub description: &'static str,
}

/// A typed handle to a setting.
///
/// The type parameter is the Rust type the setting is read as. It is part of the
/// key's type so a caller cannot read an integer setting as a string: the
/// compiler rejects the mismatch, and [`SettingsStore::get`] only has to handle
/// a registry that disagrees with the key.
pub struct SettingKey<T> {
    name: &'static str,
    marker: PhantomData<fn() -> T>,
}

impl<T> SettingKey<T> {
    /// Creates a key for the setting named `name`.
    ///
    /// # Parameters
    ///
    /// - `name`: the dotted setting key. It must match a registered setting.
    pub const fn new(name: &'static str) -> SettingKey<T> {
        SettingKey {
            name,
            marker: PhantomData,
        }
    }

    /// Returns the dotted setting key.
    pub const fn name(&self) -> &'static str {
        self.name
    }
}

impl<T> fmt::Debug for SettingKey<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingKey")
            .field("name", &self.name)
            .finish()
    }
}

impl<T> Clone for SettingKey<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SettingKey<T> {}

impl<T> PartialEq for SettingKey<T> {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl<T> Eq for SettingKey<T> {}

/// The Rust types a setting value can be read as.
pub trait SettingType: Sized {
    /// Converts a stored value into this type.
    ///
    /// # Parameters
    ///
    /// - `value`: the stored value.
    ///
    /// # Returns
    ///
    /// `Some` when the value is representable as this type, `None` otherwise.
    fn from_value(value: &SettingValue) -> Option<Self>;
}

impl SettingType for bool {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

impl SettingType for i64 {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::Integer(value) => Some(*value),
            _ => None,
        }
    }
}

impl SettingType for u32 {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
                Some(*value as u32)
            }
            _ => None,
        }
    }
}

impl SettingType for String {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::String(value) => Some(value.clone()),
            _ => None,
        }
    }
}

/// The typed keys for the built-in settings.
pub mod keys {
    use super::SettingKey;

    /// The ceiling an instance may request for `resources.memoryMb`.
    pub const PLUGINS_MAX_MEMORY_MB: SettingKey<u32> = SettingKey::new("plugins.max_memory_mb");
}

/// The registry of known settings.
#[derive(Debug, Clone)]
pub struct SettingsRegistry {
    defs: Vec<SettingDef>,
}

impl SettingsRegistry {
    /// Builds the registry of every setting the server knows.
    ///
    /// # Returns
    ///
    /// A registry containing the built-in settings.
    pub fn builtin() -> SettingsRegistry {
        SettingsRegistry {
            defs: vec![SettingDef {
                key: keys::PLUGINS_MAX_MEMORY_MB.name(),
                kind: SettingKind::Integer { min: 1, max: 4096 },
                default: SettingValue::Integer(1024),
                description: "Ceiling an instance may request for resources.memoryMb",
            }],
        }
    }

    /// Looks up a setting definition by key.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The definition, or `None` when the key is not registered.
    pub fn def(&self, key: &str) -> Option<&SettingDef> {
        self.defs.iter().find(|def| def.key == key)
    }

    /// Iterates over every registered setting.
    pub fn iter(&self) -> impl Iterator<Item = &SettingDef> {
        self.defs.iter()
    }
}

/// Where a resolved setting value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingSource {
    /// An `HEARTHSOUND__` environment variable.
    Environment,
    /// The configuration file's settings table.
    File,
    /// The built-in default.
    Default,
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingSource::Environment => f.write_str("the environment"),
            SettingSource::File => f.write_str("the config file"),
            SettingSource::Default => f.write_str("the built-in default"),
        }
    }
}

/// The raw sources resolution reads from.
#[derive(Debug, Clone, Default)]
pub struct SettingSources {
    /// Environment variables, keyed by their full name.
    pub env: BTreeMap<String, String>,
    /// Configuration file values, keyed by dotted setting key.
    pub file: BTreeMap<String, toml::Value>,
}

impl SettingSources {
    /// Collects the process environment.
    ///
    /// Only variables starting with `HEARTHSOUND__` are read; this is the only
    /// place the settings layer touches the process environment.
    ///
    /// # Returns
    ///
    /// Sources holding the matching environment variables and no file values.
    pub fn from_process_env() -> SettingSources {
        let env = std::env::vars()
            .filter(|(name, _)| name.starts_with(ENV_PREFIX))
            .collect();
        SettingSources {
            env,
            file: BTreeMap::new(),
        }
    }
}

/// A resolved set of settings.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    values: BTreeMap<&'static str, StoredSetting>,
}

#[derive(Debug, Clone)]
struct StoredSetting {
    value: SettingValue,
    source: SettingSource,
}

impl SettingsStore {
    /// Resolves every registered setting from the given sources.
    ///
    /// The precedence is environment, then file, then default. Environment
    /// values are parsed leniently and then validated; file values must already
    /// be a boolean, integer, or string.
    ///
    /// # Parameters
    ///
    /// - `registry`: the settings to resolve.
    /// - `sources`: the environment and file values.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnknownSetting`] when the file contains a key the
    /// registry does not know, and [`ConfigError::InvalidValue`] when a value is
    /// out of range, of the wrong type, or not in a fixed set. Unknown
    /// environment variables are ignored.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore, keys};
    ///
    /// let registry = SettingsRegistry::builtin();
    /// let sources = SettingSources::default();
    /// let store = SettingsStore::resolve(&registry, &sources).unwrap();
    /// assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 1024);
    /// ```
    pub fn resolve(
        registry: &SettingsRegistry,
        sources: &SettingSources,
    ) -> Result<SettingsStore, ConfigError> {
        for key in sources.file.keys() {
            if registry.def(key).is_none() {
                return Err(ConfigError::UnknownSetting { key: key.clone() });
            }
        }

        let mut values = BTreeMap::new();
        for def in registry.iter() {
            let (value, source) = if let Some(raw) = sources.env.get(&env_name_for(def.key)) {
                (SettingValue::parse_env(raw), SettingSource::Environment)
            } else if let Some(raw) = sources.file.get(def.key) {
                (value_from_toml(def.key, raw)?, SettingSource::File)
            } else {
                (def.default.clone(), SettingSource::Default)
            };
            validate(def, &value, source)?;
            values.insert(def.key, StoredSetting { value, source });
        }
        Ok(SettingsStore { values })
    }

    /// Builds a store holding only built-in defaults.
    ///
    /// # Parameters
    ///
    /// - `registry`: the settings to include.
    ///
    /// # Returns
    ///
    /// A store in which every registered setting reports
    /// [`SettingSource::Default`].
    pub fn defaults(registry: &SettingsRegistry) -> SettingsStore {
        let values = registry
            .iter()
            .map(|def| {
                (
                    def.key,
                    StoredSetting {
                        value: def.default.clone(),
                        source: SettingSource::Default,
                    },
                )
            })
            .collect();
        SettingsStore { values }
    }

    /// Reads a setting as its declared type.
    ///
    /// # Parameters
    ///
    /// - `key`: the typed key to read.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::UnregisteredKey`] when the key is absent from this
    /// store, and [`ConfigError::TypeMismatch`] when the stored value is not
    /// representable as `T`. Both indicate a bug in the caller rather than
    /// bad input.
    pub fn get<T: SettingType>(&self, key: SettingKey<T>) -> Result<T, ConfigError> {
        let name = key.name();
        let stored = self
            .values
            .get(name)
            .ok_or_else(|| ConfigError::UnregisteredKey {
                key: name.to_owned(),
            })?;
        T::from_value(&stored.value).ok_or_else(|| ConfigError::TypeMismatch {
            key: name.to_owned(),
        })
    }

    /// Reports where a setting's value came from.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The source, or `None` when the key is not in this store.
    pub fn source(&self, key: &str) -> Option<SettingSource> {
        self.values.get(key).map(|stored| stored.source)
    }
}

/// Returns the environment variable name for a setting key.
fn env_name_for(key: &str) -> String {
    let mut name = String::from(ENV_PREFIX);
    for ch in key.chars() {
        if ch == '.' {
            name.push_str("__");
        } else {
            name.push(ch.to_ascii_uppercase());
        }
    }
    name
}

/// Converts a file value into a [`SettingValue`].
fn value_from_toml(key: &str, value: &toml::Value) -> Result<SettingValue, ConfigError> {
    match value {
        toml::Value::Boolean(value) => Ok(SettingValue::Bool(*value)),
        toml::Value::Integer(value) => Ok(SettingValue::Integer(*value)),
        toml::Value::String(value) => Ok(SettingValue::String(value.clone())),
        other => Err(ConfigError::InvalidValue {
            key: key.to_owned(),
            origin: SettingSource::File,
            value: other.to_string(),
            reason: "expected a boolean, integer, or string".to_owned(),
        }),
    }
}

/// Checks a value against its setting's kind.
fn validate(
    def: &SettingDef,
    value: &SettingValue,
    source: SettingSource,
) -> Result<(), ConfigError> {
    let invalid = |reason: String| ConfigError::InvalidValue {
        key: def.key.to_owned(),
        origin: source,
        value: value.to_string(),
        reason,
    };

    match (&def.kind, value) {
        (SettingKind::Bool, SettingValue::Bool(_)) => Ok(()),
        (SettingKind::Integer { min, max }, SettingValue::Integer(number))
            if number >= min && number <= max =>
        {
            Ok(())
        }
        (SettingKind::Integer { min, max }, SettingValue::Integer(_)) => {
            Err(invalid(format!("must be between {min} and {max}")))
        }
        (SettingKind::String { allowed }, SettingValue::String(text))
            if allowed.is_empty() || allowed.contains(&text.as_str()) =>
        {
            Ok(())
        }
        (SettingKind::String { allowed }, SettingValue::String(_)) => {
            Err(invalid(format!("must be one of: {}", allowed.join(", "))))
        }
        (kind, _) => Err(invalid(kind.expected().to_owned())),
    }
}

/// An error from registering, resolving, or reading a setting.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// A value did not satisfy its setting's kind.
    #[error("setting `{key}` = {value} from {origin} is invalid: {reason}")]
    InvalidValue {
        /// The dotted setting key.
        key: String,
        /// Where the rejected value came from.
        origin: SettingSource,
        /// The rejected value, rendered as text.
        value: String,
        /// Why the value was rejected.
        reason: String,
    },
    /// The configuration file contained a key the registry does not know.
    #[error("unknown setting `{key}`")]
    UnknownSetting {
        /// The unrecognized key.
        key: String,
    },
    /// A key was read through a store that does not hold it.
    #[error("setting `{key}` is not registered")]
    UnregisteredKey {
        /// The key that was not found.
        key: String,
    },
    /// A key was read as a type the registry does not store.
    #[error("setting `{key}` does not have the requested type")]
    TypeMismatch {
        /// The key whose type did not match.
        key: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_value(value: toml::Value) -> SettingSources {
        let mut sources = SettingSources::default();
        sources
            .file
            .insert(keys::PLUGINS_MAX_MEMORY_MB.name().to_owned(), value);
        sources
    }

    #[test]
    fn environment_beats_file_beats_default() {
        let registry = SettingsRegistry::builtin();

        let mut sources = file_value(toml::Value::Integer(512));
        sources.env.insert(
            "HEARTHSOUND__PLUGINS__MAX_MEMORY_MB".to_owned(),
            "256".to_owned(),
        );
        let store = SettingsStore::resolve(&registry, &sources).unwrap();
        assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 256);
        assert_eq!(
            store.source(keys::PLUGINS_MAX_MEMORY_MB.name()),
            Some(SettingSource::Environment)
        );

        let store =
            SettingsStore::resolve(&registry, &file_value(toml::Value::Integer(512))).unwrap();
        assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 512);
        assert_eq!(
            store.source(keys::PLUGINS_MAX_MEMORY_MB.name()),
            Some(SettingSource::File)
        );

        let store = SettingsStore::resolve(&registry, &SettingSources::default()).unwrap();
        assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 1024);
        assert_eq!(
            store.source(keys::PLUGINS_MAX_MEMORY_MB.name()),
            Some(SettingSource::Default)
        );
    }

    #[test]
    fn non_numeric_environment_value_is_rejected() {
        let registry = SettingsRegistry::builtin();
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__PLUGINS__MAX_MEMORY_MB".to_owned(),
            "abc".to_owned(),
        );
        let error = SettingsStore::resolve(&registry, &sources).unwrap_err();
        match error {
            ConfigError::InvalidValue {
                key, origin, value, ..
            } => {
                assert_eq!(key, "plugins.max_memory_mb");
                assert_eq!(origin, SettingSource::Environment);
                assert_eq!(value, "abc");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn out_of_range_file_value_is_rejected() {
        let registry = SettingsRegistry::builtin();
        let error =
            SettingsStore::resolve(&registry, &file_value(toml::Value::Integer(0))).unwrap_err();
        match error {
            ConfigError::InvalidValue {
                key, origin, value, ..
            } => {
                assert_eq!(key, "plugins.max_memory_mb");
                assert_eq!(origin, SettingSource::File);
                assert_eq!(value, "0");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn unknown_file_key_is_rejected() {
        let registry = SettingsRegistry::builtin();
        let mut sources = SettingSources::default();
        sources
            .file
            .insert("plugins.nope".to_owned(), toml::Value::Integer(1));
        let error = SettingsStore::resolve(&registry, &sources).unwrap_err();
        assert_eq!(
            error,
            ConfigError::UnknownSetting {
                key: "plugins.nope".to_owned()
            }
        );
    }

    #[test]
    fn unknown_environment_variables_are_ignored() {
        let registry = SettingsRegistry::builtin();
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__PATHS__DATA".to_owned(),
            "/tmp/data".to_owned(),
        );
        let store = SettingsStore::resolve(&registry, &sources).unwrap();
        assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 1024);
    }

    #[test]
    fn unregistered_key_and_type_mismatch_are_reported() {
        let registry = SettingsRegistry::builtin();
        let store = SettingsStore::defaults(&registry);
        let missing = SettingKey::<u32>::new("plugins.missing");
        assert_eq!(
            store.get(missing).unwrap_err(),
            ConfigError::UnregisteredKey {
                key: "plugins.missing".to_owned()
            }
        );
        assert_eq!(
            store
                .get(SettingKey::<String>::new("plugins.max_memory_mb"))
                .unwrap_err(),
            ConfigError::TypeMismatch {
                key: "plugins.max_memory_mb".to_owned()
            }
        );
    }
}
