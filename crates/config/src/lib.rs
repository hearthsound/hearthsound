//! The settings registry and resolution.
//!
//! Settings have four layers, resolved with a fixed precedence: environment
//! variables, then values from the configuration file, then values from the
//! database, then the built-in default. This crate owns the registry of known
//! settings, the typed keys used to read them, and the resolution rules; the
//! [`bootstrap`] module loads the bootstrap keys and hands the file's
//! `[settings]` table over as the file layer, [`settings`] owns the database
//! layer and the change notification, and [`secret`] owns the AES-256-GCM key
//! material.
#![forbid(unsafe_code)]

pub mod bootstrap;
pub mod secret;
pub mod settings;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

/// The prefix every settings environment variable carries.
const ENV_PREFIX: &str = "HEARTHSOUND__";

/// The literal value of a setting.
///
/// Values are untyped at rest; a [`SettingKind`] says which variant is
/// acceptable for a given setting.
///
/// The JSON representation derived here is the one stored in the `settings`
/// table: a boolean, a number, a string, or an array of strings.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(untagged)]
pub enum SettingValue {
    /// A boolean value.
    Bool(bool),
    /// A 64-bit signed integer value.
    Integer(i64),
    /// A floating point value.
    Float(f64),
    /// A string value.
    String(String),
    /// A list of strings, such as a set of permission names.
    StringList(Vec<String>),
}

impl SettingValue {
    /// Parses an environment variable value.
    ///
    /// `true` and `false` become [`SettingValue::Bool`], anything that parses as
    /// a 64-bit signed integer becomes [`SettingValue::Integer`], anything that
    /// parses as a finite floating point number becomes [`SettingValue::Float`],
    /// and everything else stays a raw [`SettingValue::String`]. The result is
    /// validated against the setting's kind afterwards, which is also where a
    /// comma-separated value becomes a [`SettingValue::StringList`].
    ///
    /// # Parameters
    ///
    /// - `raw`: the raw environment variable value.
    ///
    /// # Returns
    ///
    /// The parsed value.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::SettingValue;
    ///
    /// assert_eq!(SettingValue::parse_env("true"), SettingValue::Bool(true));
    /// assert_eq!(SettingValue::parse_env("320"), SettingValue::Integer(320));
    /// assert_eq!(SettingValue::parse_env("0.8"), SettingValue::Float(0.8));
    /// assert_eq!(SettingValue::parse_env("pad"), SettingValue::String("pad".to_owned()));
    /// assert_eq!(SettingValue::parse_env("nan"), SettingValue::String("nan".to_owned()));
    /// ```
    pub fn parse_env(raw: &str) -> SettingValue {
        match raw {
            "true" => SettingValue::Bool(true),
            "false" => SettingValue::Bool(false),
            _ => match raw.parse::<i64>() {
                Ok(number) => SettingValue::Integer(number),
                Err(_) => match raw.parse::<f64>() {
                    Ok(number) if number.is_finite() => SettingValue::Float(number),
                    _ => SettingValue::String(raw.to_owned()),
                },
            },
        }
    }
    /// Renders the value as the JSON text stored in the `settings` table.
    ///
    /// # Returns
    ///
    /// A JSON document: `true`, a number, a string, or an array of strings. The
    /// representation round-trips through [`SettingValue::from_json`].
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::SettingValue;
    ///
    /// assert_eq!(SettingValue::Bool(false).to_json(), "false");
    /// assert_eq!(SettingValue::Float(0.8).to_json(), "0.8");
    /// assert_eq!(
    ///     SettingValue::StringList(vec!["acquire".to_owned()]).to_json(),
    ///     r#"["acquire"]"#
    /// );
    /// ```
    pub fn to_json(&self) -> String {
        match self {
            SettingValue::Bool(value) => value.to_string(),
            SettingValue::Integer(value) => value.to_string(),
            SettingValue::Float(value) => serde_json::Value::from(*value).to_string(),
            SettingValue::String(value) => serde_json::Value::String(value.clone()).to_string(),
            SettingValue::StringList(values) => serde_json::Value::from(values.clone()).to_string(),
        }
    }

    /// Parses a value from the JSON text stored in the `settings` table.
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key, used in the error.
    /// - `text`: the stored JSON document.
    ///
    /// # Returns
    ///
    /// The parsed value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidValue`] with the database origin when the
    /// text is not valid JSON, or holds something other than a boolean, number,
    /// string, or array of strings.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::SettingValue;
    ///
    /// let value = SettingValue::from_json("subsonic.token_auth", "true").unwrap();
    /// assert_eq!(value, SettingValue::Bool(true));
    /// assert!(SettingValue::from_json("subsonic.token_auth", "{}").is_err());
    /// ```
    pub fn from_json(key: &str, text: &str) -> Result<SettingValue, ConfigError> {
        serde_json::from_str(text).map_err(|error| ConfigError::InvalidValue {
            key: key.to_owned(),
            origin: SettingSource::Database,
            value: text.to_owned(),
            reason: format!("expected a boolean, number, string, or list of strings: {error}"),
        })
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Bool(value) => write!(f, "{value}"),
            SettingValue::Integer(value) => write!(f, "{value}"),
            SettingValue::Float(value) => write!(f, "{value}"),
            SettingValue::String(value) => f.write_str(value),
            SettingValue::StringList(values) => f.write_str(&values.join(", ")),
        }
    }
}

/// The type a setting must have, and any constraint on its value.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    /// A floating point number within an inclusive range.
    Float {
        /// The smallest accepted value.
        min: f64,
        /// The largest accepted value.
        max: f64,
    },
    /// A string, optionally restricted to a fixed set of values.
    String {
        /// The accepted values; an empty slice accepts any string.
        allowed: &'static [&'static str],
    },
    /// A list of distinct strings, each of which must be in the fixed set when
    /// one is given.
    StringList {
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
            SettingKind::Float { .. } => "expected a number",
            SettingKind::String { .. } => "expected a string",
            SettingKind::StringList { .. } => "expected a list of strings",
        }
    }
}

/// The definition of one known setting.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingDef {
    /// The dotted setting key, for example `plugins.max_memory_mb`.
    pub key: &'static str,
    /// The type and constraints of the value.
    pub kind: SettingKind,
    /// The value used when neither the environment, the file, nor the database
    /// supplies one.
    pub default: SettingValue,
    /// A one-line human readable description.
    pub description: &'static str,
    /// Whether a user may override this setting for themselves.
    ///
    /// A user override lives in the `user_settings` table, shadows the server
    /// value for that user only, and is read by the API layer rather than by
    /// [`SettingsStore`].
    pub user_overridable: bool,
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

impl SettingType for f64 {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::Float(value) => Some(*value),
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

impl SettingType for Vec<String> {
    fn from_value(value: &SettingValue) -> Option<Self> {
        match value {
            SettingValue::StringList(values) => Some(values.clone()),
            _ => None,
        }
    }
}

/// The typed keys for the built-in settings.
///
/// Every key registered by [`SettingsRegistry::builtin`] has a constant here;
/// read a value with [`SettingsStore::get`], which checks the type at compile
/// time.
pub mod keys {
    use super::SettingKey;

    /// Import path template for acquired and uploaded files.
    pub const LIBRARY_NAMING_TEMPLATE: SettingKey<String> =
        SettingKey::new("library.naming_template");
    /// Filename of the cover saved beside an imported file.
    pub const LIBRARY_COVER_FILENAME: SettingKey<String> =
        SettingKey::new("library.cover_filename");
    /// Whether to embed cover art into tags on import.
    pub const LIBRARY_TAGS_EMBED_COVER: SettingKey<bool> =
        SettingKey::new("library.tags.embed_cover");
    /// Whether saving a remote item acquires it.
    pub const LIBRARY_ACQUIRE_ON_SAVE: SettingKey<bool> =
        SettingKey::new("library.acquire_on_save");
    /// Whether adding a remote item to a permanent playlist acquires it.
    pub const LIBRARY_ACQUIRE_ON_PLAYLIST_ADD: SettingKey<bool> =
        SettingKey::new("library.acquire_on_playlist_add");

    /// Upper bound on the total size of the cache directory, in bytes.
    pub const STORAGE_CACHE_MAX_BYTES: SettingKey<i64> = SettingKey::new("storage.cache.max_bytes");
    /// Free-space floor below which cache eviction runs, in bytes.
    pub const STORAGE_CACHE_MIN_FREE_BYTES: SettingKey<i64> =
        SettingKey::new("storage.cache.min_free_bytes");
    /// Cache eviction order: `lru`, `lfu`, or `fifo`.
    pub const STORAGE_CACHE_EVICTION: SettingKey<String> =
        SettingKey::new("storage.cache.eviction");
    /// Age in days above which a cached file is evicted; 0 disables the limit.
    pub const STORAGE_CACHE_MAX_AGE_DAYS: SettingKey<i64> =
        SettingKey::new("storage.cache.max_age_days");
    /// What happens to a file when its last pin is removed: `demote` or
    /// `delete`.
    pub const STORAGE_UNPIN_ACTION: SettingKey<String> = SettingKey::new("storage.unpin_action");
    /// Play count after which a track is pinned automatically; 0 disables it.
    pub const STORAGE_AUTOPIN_PLAY_COUNT: SettingKey<i64> =
        SettingKey::new("storage.autopin.play_count");

    /// Server-wide acquisition quality profile.
    pub const ACQUISITION_QUALITY_PROFILE: SettingKey<String> =
        SettingKey::new("acquisition.quality_profile");
    /// Minimum score for a candidate to be acceptable.
    pub const ACQUISITION_MIN_MATCH_SCORE: SettingKey<f64> =
        SettingKey::new("acquisition.min_match_score");
    /// ETA ceiling in seconds for interactive candidate selection.
    pub const ACQUISITION_INTERACTIVE_ETA_TARGET_SECS: SettingKey<i64> =
        SettingKey::new("acquisition.interactive_eta_target_secs");
    /// No-progress window in seconds before a download is considered stalled.
    pub const ACQUISITION_STALL_TIMEOUT_SECS: SettingKey<i64> =
        SettingKey::new("acquisition.stall_timeout_secs");
    /// Seconds an interactive transfer may sit queued.
    pub const ACQUISITION_QUEUE_TIMEOUT_SECS_INTERACTIVE: SettingKey<i64> =
        SettingKey::new("acquisition.queue_timeout_secs.interactive");
    /// Seconds a background transfer may sit queued.
    pub const ACQUISITION_QUEUE_TIMEOUT_SECS_BACKGROUND: SettingKey<i64> =
        SettingKey::new("acquisition.queue_timeout_secs.background");
    /// Candidates tried before moving to the next instance.
    pub const ACQUISITION_MAX_CANDIDATES: SettingKey<i64> =
        SettingKey::new("acquisition.max_candidates");
    /// Global acquisition slots.
    pub const ACQUISITION_MAX_CONCURRENT: SettingKey<i64> =
        SettingKey::new("acquisition.max_concurrent");
    /// Cooldown in hours before an unavailable track is retried.
    pub const ACQUISITION_UNAVAILABLE_COOLDOWN_HOURS: SettingKey<i64> =
        SettingKey::new("acquisition.unavailable_cooldown_hours");
    /// Whether acquired files are verified by fingerprint.
    pub const ACQUISITION_VERIFY_FINGERPRINT: SettingKey<bool> =
        SettingKey::new("acquisition.verify.fingerprint");

    /// Whether upcoming queue items are prefetched.
    pub const PREFETCH_ENABLED: SettingKey<bool> = SettingKey::new("prefetch.enabled");
    /// Number of upcoming queue items prefetched.
    pub const PREFETCH_LOOKAHEAD: SettingKey<i64> = SettingKey::new("prefetch.lookahead");

    /// Cap on concurrent ffmpeg transcode processes.
    pub const STREAM_MAX_CONCURRENT_TRANSCODES: SettingKey<i64> =
        SettingKey::new("stream.max_concurrent_transcodes");
    /// Behavior of a stream for a track that is still being acquired: `pad`,
    /// `hold`, or `fail`.
    pub const STREAM_PENDING_MODE: SettingKey<String> = SettingKey::new("stream.pending.mode");
    /// Transcode format used for a pending stream: `mp3`, `opus`, or `aac`.
    pub const STREAM_PENDING_FORMAT: SettingKey<String> = SettingKey::new("stream.pending.format");
    /// Bitrate in kbit/s for the pending transcode.
    pub const STREAM_PENDING_BITRATE_KBPS: SettingKey<i64> =
        SettingKey::new("stream.pending.bitrate_kbps");
    /// Seconds a pending stream is held before it ends without a file.
    pub const STREAM_PENDING_MAX_WAIT_SECS: SettingKey<i64> =
        SettingKey::new("stream.pending.max_wait_secs");
    /// Seconds a hold-mode stream waits before it serves normally.
    pub const STREAM_HOLD_TIMEOUT_SECS: SettingKey<i64> =
        SettingKey::new("stream.hold.timeout_secs");

    /// Whether catalog-layer results are included in searches.
    pub const SEARCH_CATALOG_ENABLED: SettingKey<bool> = SettingKey::new("search.catalog.enabled");
    /// Catalog fan-out deadline in milliseconds.
    pub const SEARCH_CATALOG_TIMEOUT_MS: SettingKey<i64> =
        SettingKey::new("search.catalog.timeout_ms");
    /// Maximum number of catalog results per search.
    pub const SEARCH_CATALOG_MAX_RESULTS: SettingKey<i64> =
        SettingKey::new("search.catalog.max_results");

    /// Whether the account password is accepted on `/rest`.
    pub const SUBSONIC_ALLOW_ACCOUNT_PASSWORD: SettingKey<bool> =
        SettingKey::new("subsonic.allow_account_password");
    /// Whether legacy `t`/`s` token authentication is accepted.
    pub const SUBSONIC_TOKEN_AUTH: SettingKey<bool> = SettingKey::new("subsonic.token_auth");
    /// Default browse scope for users without an override: `library` or
    /// `server`.
    pub const SUBSONIC_DEFAULT_SCOPE: SettingKey<String> =
        SettingKey::new("subsonic.default_scope");
    /// Whether a rating of 1 means a dislike.
    pub const SUBSONIC_RATING_ONE_IS_DISLIKE: SettingKey<bool> =
        SettingKey::new("subsonic.rating_one_is_dislike");
    /// Suffix appended to catalog entity titles.
    pub const SUBSONIC_CATALOG_TITLE_SUFFIX: SettingKey<String> =
        SettingKey::new("subsonic.catalog_title_suffix");

    /// Content of the server browse view: `all`, `permanent_only`, or
    /// `disabled`.
    pub const PRIVACY_SERVER_BROWSE: SettingKey<String> = SettingKey::new("privacy.server_browse");
    /// Whether a user's now-playing activity is visible to others.
    pub const PRIVACY_NOW_PLAYING_VISIBLE: SettingKey<bool> =
        SettingKey::new("privacy.now_playing_visible");

    /// Hours between plugin update checks; 0 disables them.
    pub const PLUGINS_UPDATE_CHECK_HOURS: SettingKey<i64> =
        SettingKey::new("plugins.update_check_hours");
    /// Whether plugin updates are installed automatically.
    pub const PLUGINS_AUTO_UPDATE: SettingKey<bool> = SettingKey::new("plugins.auto_update");
    /// Whether a plugin may be installed by uploading a `.hsplugin` file.
    pub const PLUGINS_ALLOW_SIDELOAD: SettingKey<bool> = SettingKey::new("plugins.allow_sideload");
    /// Whether third-party plugin repositories may be added.
    pub const PLUGINS_ALLOW_THIRD_PARTY_REPOSITORIES: SettingKey<bool> =
        SettingKey::new("plugins.allow_third_party_repositories");
    /// The ceiling an instance may request for `resources.memoryMb`.
    pub const PLUGINS_MAX_MEMORY_MB: SettingKey<u32> = SettingKey::new("plugins.max_memory_mb");

    /// Whether user uploads are allowed.
    pub const UPLOADS_ENABLED: SettingKey<bool> = SettingKey::new("uploads.enabled");
    /// Maximum size in bytes of a single uploaded file.
    pub const UPLOADS_MAX_FILE_BYTES: SettingKey<i64> = SettingKey::new("uploads.max_file_bytes");

    /// Permissions granted to a newly created user.
    pub const USERS_DEFAULT_PERMISSIONS: SettingKey<Vec<String>> =
        SettingKey::new("users.default_permissions");

    /// Lifetime in days of a web session cookie.
    pub const AUTH_SESSION_TTL_DAYS: SettingKey<i64> = SettingKey::new("auth.session_ttl_days");
    /// Login attempts per minute per IP and username.
    pub const AUTH_LOGIN_RATE_LIMIT_PER_MINUTE: SettingKey<i64> =
        SettingKey::new("auth.login_rate_limit_per_minute");
}

/// The default import path template.
const DEFAULT_NAMING_TEMPLATE: &str =
    "{album_artist}/{album}[ ({year})]/{disc_prefix}{track:02} - {title}.{ext}";

/// Bytes in a gibibyte, used to spell out size defaults.
const GIB: i64 = 1024 * 1024 * 1024;

/// The quality profiles an operator can select.
const QUALITY_PROFILES: &[&str] = &["lossless-preferred", "compact"];

/// The cache eviction orders an operator can select.
const EVICTION_ORDERS: &[&str] = &["lru", "lfu", "fifo"];

/// The actions taken when a track's last pin is removed.
const UNPIN_ACTIONS: &[&str] = &["demote", "delete"];

/// The behaviors of a stream for a track that is still being acquired.
const PENDING_MODES: &[&str] = &["pad", "hold", "fail"];

/// The transcode formats an operator can select for pending streams.
const TRANSCODE_FORMATS: &[&str] = &["mp3", "opus", "aac"];

/// The browse scopes a user can select.
const BROWSE_SCOPES: &[&str] = &["library", "server"];

/// The contents of the server browse view.
const SERVER_BROWSE_VIEWS: &[&str] = &["all", "permanent_only", "disabled"];

/// The permission names a user can hold.
const PERMISSION_NAMES: &[&str] = &["acquire", "upload", "download"];

/// Builds a definition for a server-wide setting.
///
/// # Parameters
///
/// - `key`: the typed key, whose name becomes the definition's key.
/// - `kind`: the accepted type and constraints.
/// - `default`: the built-in default.
/// - `description`: the one-line description.
///
/// # Returns
///
/// The definition, not user overridable.
fn def<T>(
    key: SettingKey<T>,
    kind: SettingKind,
    default: SettingValue,
    description: &'static str,
) -> SettingDef {
    SettingDef {
        key: key.name(),
        kind,
        default,
        description,
        user_overridable: false,
    }
}

/// Builds a definition for a setting a user may override for themselves.
///
/// # Parameters
///
/// Same as [`def`].
///
/// # Returns
///
/// The definition, marked user overridable.
fn user_def<T>(
    key: SettingKey<T>,
    kind: SettingKind,
    default: SettingValue,
    description: &'static str,
) -> SettingDef {
    SettingDef {
        user_overridable: true,
        ..def(key, kind, default, description)
    }
}

/// The registry of known settings.
#[derive(Debug, Clone)]
pub struct SettingsRegistry {
    defs: Vec<SettingDef>,
}

impl SettingsRegistry {
    /// Builds the registry of every setting the server knows.
    ///
    /// The registry carries the built-in defaults and the constraint of each
    /// setting. Keys of features that have not landed yet (fast start, scheduled
    /// pre-acquisition, upgrades, radio) and the templated `tools.<name>.*` keys
    /// are not registered.
    ///
    /// # Returns
    ///
    /// A registry containing the built-in settings.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::{SettingsRegistry, keys};
    ///
    /// let registry = SettingsRegistry::builtin();
    /// let def = registry.def(keys::STREAM_PENDING_MODE.name()).unwrap();
    /// assert!(def.user_overridable == false);
    /// assert_eq!(registry.def("acquisition.fast_start.enabled"), None);
    /// ```
    pub fn builtin() -> SettingsRegistry {
        SettingsRegistry {
            defs: vec![
                def(
                    keys::LIBRARY_NAMING_TEMPLATE,
                    SettingKind::String { allowed: &[] },
                    SettingValue::String(DEFAULT_NAMING_TEMPLATE.to_owned()),
                    "Import path template; `[...]` is omitted when a token inside it is empty",
                ),
                def(
                    keys::LIBRARY_COVER_FILENAME,
                    SettingKind::String { allowed: &[] },
                    SettingValue::String("cover.jpg".to_owned()),
                    "Filename of the cover saved beside an imported file",
                ),
                def(
                    keys::LIBRARY_TAGS_EMBED_COVER,
                    SettingKind::Bool,
                    SettingValue::Bool(false),
                    "Whether to embed cover art into tags on import",
                ),
                def(
                    keys::LIBRARY_ACQUIRE_ON_SAVE,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether saving a remote item acquires it",
                ),
                def(
                    keys::LIBRARY_ACQUIRE_ON_PLAYLIST_ADD,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether adding a remote item to a permanent playlist acquires it",
                ),
                def(
                    keys::STORAGE_CACHE_MAX_BYTES,
                    SettingKind::Integer {
                        min: 0,
                        max: i64::MAX,
                    },
                    SettingValue::Integer(20 * GIB),
                    "Upper bound on the total size of the cache directory, in bytes",
                ),
                def(
                    keys::STORAGE_CACHE_MIN_FREE_BYTES,
                    SettingKind::Integer {
                        min: 0,
                        max: i64::MAX,
                    },
                    SettingValue::Integer(5 * GIB),
                    "Free-space floor below which cache eviction runs, in bytes",
                ),
                def(
                    keys::STORAGE_CACHE_EVICTION,
                    SettingKind::String {
                        allowed: EVICTION_ORDERS,
                    },
                    SettingValue::String("lru".to_owned()),
                    "Cache eviction order",
                ),
                def(
                    keys::STORAGE_CACHE_MAX_AGE_DAYS,
                    SettingKind::Integer {
                        min: 0,
                        max: 36_500,
                    },
                    SettingValue::Integer(0),
                    "Age in days above which a cached file is evicted; 0 disables the limit",
                ),
                def(
                    keys::STORAGE_UNPIN_ACTION,
                    SettingKind::String {
                        allowed: UNPIN_ACTIONS,
                    },
                    SettingValue::String("demote".to_owned()),
                    "What happens to a file when its last pin is removed",
                ),
                def(
                    keys::STORAGE_AUTOPIN_PLAY_COUNT,
                    SettingKind::Integer {
                        min: 0,
                        max: 1_000_000,
                    },
                    SettingValue::Integer(0),
                    "Play count after which a track is pinned automatically; 0 disables it",
                ),
                def(
                    keys::ACQUISITION_QUALITY_PROFILE,
                    SettingKind::String {
                        allowed: QUALITY_PROFILES,
                    },
                    SettingValue::String("lossless-preferred".to_owned()),
                    "Server-wide acquisition quality profile",
                ),
                def(
                    keys::ACQUISITION_MIN_MATCH_SCORE,
                    SettingKind::Float { min: 0.0, max: 1.0 },
                    SettingValue::Float(0.80),
                    "Minimum score for a candidate to be acceptable",
                ),
                def(
                    keys::ACQUISITION_INTERACTIVE_ETA_TARGET_SECS,
                    SettingKind::Integer {
                        min: 1,
                        max: 86_400,
                    },
                    SettingValue::Integer(60),
                    "ETA ceiling in seconds for interactive candidate selection",
                ),
                def(
                    keys::ACQUISITION_STALL_TIMEOUT_SECS,
                    SettingKind::Integer { min: 1, max: 3_600 },
                    SettingValue::Integer(45),
                    "No-progress window in seconds before a download is considered stalled",
                ),
                def(
                    keys::ACQUISITION_QUEUE_TIMEOUT_SECS_INTERACTIVE,
                    SettingKind::Integer {
                        min: 1,
                        max: 86_400,
                    },
                    SettingValue::Integer(90),
                    "Seconds an interactive transfer may sit queued",
                ),
                def(
                    keys::ACQUISITION_QUEUE_TIMEOUT_SECS_BACKGROUND,
                    SettingKind::Integer {
                        min: 1,
                        max: 604_800,
                    },
                    SettingValue::Integer(1_800),
                    "Seconds a background transfer may sit queued",
                ),
                def(
                    keys::ACQUISITION_MAX_CANDIDATES,
                    SettingKind::Integer { min: 1, max: 100 },
                    SettingValue::Integer(5),
                    "Candidates tried before moving to the next instance",
                ),
                def(
                    keys::ACQUISITION_MAX_CONCURRENT,
                    SettingKind::Integer { min: 1, max: 256 },
                    SettingValue::Integer(4),
                    "Global acquisition slots",
                ),
                def(
                    keys::ACQUISITION_UNAVAILABLE_COOLDOWN_HOURS,
                    SettingKind::Integer { min: 0, max: 8_760 },
                    SettingValue::Integer(6),
                    "Cooldown in hours before an unavailable track is retried",
                ),
                def(
                    keys::ACQUISITION_VERIFY_FINGERPRINT,
                    SettingKind::Bool,
                    SettingValue::Bool(false),
                    "Whether acquired files are verified by fingerprint",
                ),
                def(
                    keys::PREFETCH_ENABLED,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether upcoming queue items are prefetched",
                ),
                def(
                    keys::PREFETCH_LOOKAHEAD,
                    SettingKind::Integer { min: 0, max: 100 },
                    SettingValue::Integer(3),
                    "Number of upcoming queue items prefetched",
                ),
                def(
                    keys::STREAM_MAX_CONCURRENT_TRANSCODES,
                    SettingKind::Integer { min: 1, max: 1_024 },
                    SettingValue::Integer(8),
                    "Cap on concurrent ffmpeg transcode processes",
                ),
                def(
                    keys::STREAM_PENDING_MODE,
                    SettingKind::String {
                        allowed: PENDING_MODES,
                    },
                    SettingValue::String("pad".to_owned()),
                    "Behavior of a stream for a track that is still being acquired",
                ),
                def(
                    keys::STREAM_PENDING_FORMAT,
                    SettingKind::String {
                        allowed: TRANSCODE_FORMATS,
                    },
                    SettingValue::String("mp3".to_owned()),
                    "Transcode format used for a pending stream",
                ),
                def(
                    keys::STREAM_PENDING_BITRATE_KBPS,
                    SettingKind::Integer { min: 32, max: 512 },
                    SettingValue::Integer(320),
                    "Bitrate in kbit/s for the pending transcode",
                ),
                def(
                    keys::STREAM_PENDING_MAX_WAIT_SECS,
                    SettingKind::Integer { min: 1, max: 3_600 },
                    SettingValue::Integer(180),
                    "Seconds a pending stream is held before it ends without a file",
                ),
                def(
                    keys::STREAM_HOLD_TIMEOUT_SECS,
                    SettingKind::Integer { min: 1, max: 3_600 },
                    SettingValue::Integer(60),
                    "Seconds a hold-mode stream waits before it serves normally",
                ),
                def(
                    keys::SEARCH_CATALOG_ENABLED,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether catalog-layer results are included in searches",
                ),
                def(
                    keys::SEARCH_CATALOG_TIMEOUT_MS,
                    SettingKind::Integer {
                        min: 1,
                        max: 60_000,
                    },
                    SettingValue::Integer(2_500),
                    "Catalog fan-out deadline in milliseconds",
                ),
                def(
                    keys::SEARCH_CATALOG_MAX_RESULTS,
                    SettingKind::Integer { min: 0, max: 500 },
                    SettingValue::Integer(20),
                    "Maximum number of catalog results per search",
                ),
                def(
                    keys::SUBSONIC_ALLOW_ACCOUNT_PASSWORD,
                    SettingKind::Bool,
                    SettingValue::Bool(false),
                    "Whether the account password is accepted on /rest",
                ),
                def(
                    keys::SUBSONIC_TOKEN_AUTH,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether legacy t/s token authentication is accepted",
                ),
                user_def(
                    keys::SUBSONIC_DEFAULT_SCOPE,
                    SettingKind::String {
                        allowed: BROWSE_SCOPES,
                    },
                    SettingValue::String("library".to_owned()),
                    "Default browse scope for users without an override",
                ),
                def(
                    keys::SUBSONIC_RATING_ONE_IS_DISLIKE,
                    SettingKind::Bool,
                    SettingValue::Bool(false),
                    "Whether a rating of 1 means a dislike",
                ),
                def(
                    keys::SUBSONIC_CATALOG_TITLE_SUFFIX,
                    SettingKind::String { allowed: &[] },
                    SettingValue::String(String::new()),
                    "Suffix appended to catalog entity titles",
                ),
                def(
                    keys::PRIVACY_SERVER_BROWSE,
                    SettingKind::String {
                        allowed: SERVER_BROWSE_VIEWS,
                    },
                    SettingValue::String("permanent_only".to_owned()),
                    "Content of the server browse view",
                ),
                user_def(
                    keys::PRIVACY_NOW_PLAYING_VISIBLE,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether a user's now-playing activity is visible to others",
                ),
                def(
                    keys::PLUGINS_UPDATE_CHECK_HOURS,
                    SettingKind::Integer { min: 0, max: 8_760 },
                    SettingValue::Integer(24),
                    "Hours between plugin update checks; 0 disables them",
                ),
                def(
                    keys::PLUGINS_AUTO_UPDATE,
                    SettingKind::Bool,
                    SettingValue::Bool(false),
                    "Whether plugin updates are installed automatically",
                ),
                def(
                    keys::PLUGINS_ALLOW_SIDELOAD,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether a plugin may be installed by uploading a .hsplugin file",
                ),
                def(
                    keys::PLUGINS_ALLOW_THIRD_PARTY_REPOSITORIES,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether third-party plugin repositories may be added",
                ),
                def(
                    keys::PLUGINS_MAX_MEMORY_MB,
                    SettingKind::Integer { min: 1, max: 4096 },
                    SettingValue::Integer(1024),
                    "Ceiling an instance may request for resources.memoryMb",
                ),
                def(
                    keys::UPLOADS_ENABLED,
                    SettingKind::Bool,
                    SettingValue::Bool(true),
                    "Whether user uploads are allowed",
                ),
                def(
                    keys::UPLOADS_MAX_FILE_BYTES,
                    SettingKind::Integer {
                        min: 0,
                        max: i64::MAX,
                    },
                    SettingValue::Integer(2 * GIB),
                    "Maximum size in bytes of a single uploaded file",
                ),
                def(
                    keys::USERS_DEFAULT_PERMISSIONS,
                    SettingKind::StringList {
                        allowed: PERMISSION_NAMES,
                    },
                    SettingValue::StringList(
                        PERMISSION_NAMES
                            .iter()
                            .map(|name| (*name).to_owned())
                            .collect(),
                    ),
                    "Permissions granted to a newly created user",
                ),
                def(
                    keys::AUTH_SESSION_TTL_DAYS,
                    SettingKind::Integer { min: 1, max: 3_650 },
                    SettingValue::Integer(30),
                    "Lifetime in days of a web session cookie",
                ),
                def(
                    keys::AUTH_LOGIN_RATE_LIMIT_PER_MINUTE,
                    SettingKind::Integer {
                        min: 1,
                        max: 10_000,
                    },
                    SettingValue::Integer(10),
                    "Login attempts per minute per IP and username",
                ),
            ],
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
    /// The `settings` table of the database.
    Database,
    /// The built-in default.
    Default,
}

impl fmt::Display for SettingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingSource::Environment => f.write_str("the environment"),
            SettingSource::File => f.write_str("the config file"),
            SettingSource::Database => f.write_str("the database"),
            SettingSource::Default => f.write_str("the built-in default"),
        }
    }
}

/// The raw sources resolution reads from.
///
/// The precedence is environment, then file, then database, then built-in
/// default.
#[derive(Debug, Clone, Default)]
pub struct SettingSources {
    /// Environment variables, keyed by their full name.
    pub env: BTreeMap<String, String>,
    /// Configuration file values, keyed by dotted setting key.
    pub file: BTreeMap<String, toml::Value>,
    /// Database values, keyed by dotted setting key.
    pub database: BTreeMap<String, SettingValue>,
}

impl SettingSources {
    /// Builds sources from an already read environment and file.
    ///
    /// # Parameters
    ///
    /// - `env`: environment variables keyed by their full name. Names outside
    ///   the `HEARTHSOUND__` namespace are ignored during resolution.
    /// - `file`: configuration file values keyed by dotted setting key, as
    ///   produced by [`bootstrap::BootstrapConfig::settings`].
    ///
    /// # Returns
    ///
    /// The sources, with an empty database layer: the [`settings::Settings`]
    /// service reads that layer from the `settings` table.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    /// use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore, keys};
    ///
    /// let mut file = BTreeMap::new();
    /// file.insert("plugins.max_memory_mb".to_owned(), toml::Value::Integer(512));
    /// let sources = SettingSources::new(BTreeMap::new(), file);
    /// let store = SettingsStore::resolve(&SettingsRegistry::builtin(), &sources).unwrap();
    /// assert_eq!(store.get(keys::PLUGINS_MAX_MEMORY_MB).unwrap(), 512);
    /// ```
    pub fn new(
        env: BTreeMap<String, String>,
        file: BTreeMap<String, toml::Value>,
    ) -> SettingSources {
        SettingSources {
            env,
            file,
            database: BTreeMap::new(),
        }
    }

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
            database: BTreeMap::new(),
        }
    }
}

/// A resolved set of settings.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsStore {
    values: BTreeMap<&'static str, StoredSetting>,
}

#[derive(Debug, Clone, PartialEq)]
struct StoredSetting {
    value: SettingValue,
    source: SettingSource,
}

impl SettingsStore {
    /// Resolves every registered setting from the given sources.
    ///
    /// The precedence is environment, then file, then default. Environment
    /// values are parsed leniently and then validated; file values must already
    /// be a boolean, integer, float, string, or array of strings.
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
    /// assert_eq!(store.get(keys::STREAM_PENDING_MODE).unwrap(), "pad");
    /// assert_eq!(store.get(keys::ACQUISITION_MIN_MATCH_SCORE).unwrap(), 0.80);
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
                (value_from_env(def, raw), SettingSource::Environment)
            } else if let Some(raw) = sources.file.get(def.key) {
                (value_from_toml(def.key, raw)?, SettingSource::File)
            } else if let Some(value) = sources.database.get(def.key) {
                (value.clone(), SettingSource::Database)
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

    /// Returns the effective value of a setting.
    ///
    /// This is the untyped view used by admin listings; feature code reads typed
    /// values through [`SettingsStore::get`].
    ///
    /// # Parameters
    ///
    /// - `key`: the dotted setting key.
    ///
    /// # Returns
    ///
    /// The resolved value, or `None` when the key is not in this store.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore, SettingValue, keys};
    ///
    /// let store = SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap();
    /// assert_eq!(
    ///     store.value(keys::STREAM_PENDING_MODE.name()),
    ///     Some(&SettingValue::String("pad".to_owned()))
    /// );
    /// ```
    pub fn value(&self, key: &str) -> Option<&SettingValue> {
        self.values.get(key).map(|stored| &stored.value)
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
///
/// # Parameters
///
/// - `key`: the dotted setting key, used in the error.
/// - `value`: the parsed TOML value.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidValue`] when the value is not a boolean,
/// integer, float, string, or array of strings.
fn value_from_toml(key: &str, value: &toml::Value) -> Result<SettingValue, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidValue {
        key: key.to_owned(),
        origin: SettingSource::File,
        value: value.to_string(),
        reason: reason.to_owned(),
    };

    match value {
        toml::Value::Boolean(value) => Ok(SettingValue::Bool(*value)),
        toml::Value::Integer(value) => Ok(SettingValue::Integer(*value)),
        toml::Value::Float(value) => Ok(SettingValue::Float(*value)),
        toml::Value::String(value) => Ok(SettingValue::String(value.clone())),
        toml::Value::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    toml::Value::String(text) => values.push(text.clone()),
                    other => {
                        return Err(invalid(&format!(
                            "expected a list of strings, found {other}"
                        )));
                    }
                }
            }
            Ok(SettingValue::StringList(values))
        }
        other => Err(invalid(&format!(
            "expected a boolean, integer, float, string, or list of strings, found {other}"
        ))),
    }
}

/// Converts an environment value into a [`SettingValue`] for a given setting.
///
/// [`SettingValue::parse_env`] does not know the target type, so `1` becomes an
/// integer even for a float setting and `a,b` stays one string. Both are widened
/// here: an integer is accepted as a float, and a string split on commas becomes
/// a list.
///
/// # Parameters
///
/// - `def`: the definition of the setting the value is for.
/// - `raw`: the raw environment variable value.
///
/// # Returns
///
/// The parsed value, which still has to pass [`validate`].
fn value_from_env(def: &SettingDef, raw: &str) -> SettingValue {
    match (def.kind, SettingValue::parse_env(raw)) {
        (SettingKind::Float { .. }, SettingValue::Integer(number)) => {
            SettingValue::Float(number as f64)
        }
        (SettingKind::StringList { .. }, SettingValue::String(text)) => {
            SettingValue::StringList(split_list(&text))
        }
        (_, parsed) => parsed,
    }
}

/// Splits a comma-separated list, trimming whitespace and dropping empty
/// entries, so `a, b,` yields `["a", "b"]`.
pub(crate) fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Checks a value against its setting's kind.
///
/// # Parameters
///
/// - `def`: the definition to check against.
/// - `value`: the value to check.
/// - `source`: where the value came from, used in the error.
///
/// # Errors
///
/// Returns [`ConfigError::InvalidValue`] when the type does not match, a number
/// is outside its range, a string is not in its fixed set, or a list repeats a
/// value or holds one outside its fixed set.
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
        (SettingKind::Float { min, max }, SettingValue::Float(number))
            if number >= min && number <= max =>
        {
            Ok(())
        }
        (SettingKind::Float { min, max }, SettingValue::Float(_)) => {
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
        (SettingKind::StringList { allowed }, SettingValue::StringList(values)) => {
            validate_list(allowed, values).map_err(invalid)
        }
        (kind, _) => Err(invalid(kind.expected().to_owned())),
    }
}

/// Checks that every list entry is distinct and inside the fixed set.
///
/// # Parameters
///
/// - `allowed`: the accepted values; an empty slice accepts any string.
/// - `values`: the list to check.
///
/// # Returns
///
/// `Ok(())` when the list is acceptable.
///
/// # Errors
///
/// Returns the reason as a string when an entry is not allowed or repeats.
fn validate_list(allowed: &[&str], values: &[String]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !allowed.is_empty() && !allowed.contains(&value.as_str()) {
            return Err(format!("must only contain: {}", allowed.join(", ")));
        }
        if !seen.insert(value.as_str()) {
            return Err(format!("must not repeat `{value}`"));
        }
    }
    Ok(())
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

    #[test]
    fn every_registered_default_validates_and_has_its_kind() {
        let registry = SettingsRegistry::builtin();
        let mut kinds_seen = BTreeSet::new();
        for def in registry.iter() {
            validate(def, &def.default, SettingSource::Default)
                .unwrap_or_else(|error| panic!("default of `{}`: {error}", def.key));
            let readable = match def.kind {
                SettingKind::Bool => bool::from_value(&def.default).is_some(),
                SettingKind::Integer { .. } => i64::from_value(&def.default).is_some(),
                SettingKind::Float { .. } => f64::from_value(&def.default).is_some(),
                SettingKind::String { .. } => String::from_value(&def.default).is_some(),
                SettingKind::StringList { .. } => Vec::<String>::from_value(&def.default).is_some(),
            };
            assert!(readable, "default of `{}` is not readable", def.key);
            assert!(
                !def.description.is_empty(),
                "`{}` has no description",
                def.key
            );
            kinds_seen.insert(format!("{:?}", std::mem::discriminant(&def.kind)));
        }
        assert_eq!(kinds_seen.len(), 5, "every setting kind must be exercised");
    }

    #[test]
    fn keys_are_unique_and_dotted() {
        let registry = SettingsRegistry::builtin();
        let names: Vec<&str> = registry.iter().map(|def| def.key).collect();
        let unique: BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "duplicate setting key");
        for name in names {
            assert!(name.contains('.'), "`{name}` is not dotted");
            assert_eq!(name, name.to_lowercase(), "`{name}` must be lowercase");
        }
        assert!(
            unique.len() >= 47,
            "the runtime settings reference is incomplete"
        );
    }

    #[test]
    fn out_of_range_and_not_in_set_values_are_rejected() {
        let registry = SettingsRegistry::builtin();
        let mut checked = 0;
        for def in registry.iter() {
            let bad = match def.kind {
                SettingKind::Integer { min, max } => {
                    Some(SettingValue::Integer(if max < i64::MAX {
                        max + 1
                    } else {
                        min - 1
                    }))
                }
                SettingKind::Float { max, .. } => Some(SettingValue::Float(max + 1.0)),
                SettingKind::String { allowed } if !allowed.is_empty() => {
                    Some(SettingValue::String("not-in-set".to_owned()))
                }
                SettingKind::StringList { allowed } if !allowed.is_empty() => {
                    Some(SettingValue::StringList(vec!["not-in-set".to_owned()]))
                }
                SettingKind::Bool | SettingKind::String { .. } | SettingKind::StringList { .. } => {
                    None
                }
            };
            if let Some(value) = bad {
                checked += 1;
                assert!(
                    validate(def, &value, SettingSource::Default).is_err(),
                    "`{}` accepted {value}",
                    def.key
                );
            }
        }
        assert!(checked >= 24, "only {checked} settings were range checked");
    }

    #[test]
    fn values_of_the_wrong_type_are_rejected() {
        let registry = SettingsRegistry::builtin();
        for def in registry.iter() {
            let wrong = match def.kind {
                SettingKind::Bool => SettingValue::Integer(1),
                SettingKind::Integer { .. } => SettingValue::Bool(true),
                SettingKind::Float { .. } => SettingValue::String("1.0".to_owned()),
                SettingKind::String { .. } => SettingValue::Bool(true),
                SettingKind::StringList { .. } => SettingValue::String("acquire".to_owned()),
            };
            assert!(
                validate(def, &wrong, SettingSource::Default).is_err(),
                "`{}` accepted {wrong}",
                def.key
            );
        }
    }

    #[test]
    fn repeated_list_entries_are_rejected() {
        let registry = SettingsRegistry::builtin();
        let def = registry
            .def(keys::USERS_DEFAULT_PERMISSIONS.name())
            .unwrap();
        let repeated = SettingValue::StringList(vec!["upload".to_owned(), "upload".to_owned()]);
        let error = validate(def, &repeated, SettingSource::Default).unwrap_err();
        match error {
            ConfigError::InvalidValue { key, reason, .. } => {
                assert_eq!(key, "users.default_permissions");
                assert_eq!(reason, "must not repeat `upload`");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn unlanded_feature_keys_are_not_registered() {
        let registry = SettingsRegistry::builtin();
        for key in [
            "acquisition.fast_start.enabled",
            "acquisition.fast_start.instance",
            "preacquire.max_tracks_per_run",
            "upgrades.include_cached",
            "radio.remote_ratio",
            "tools.yt-dlp.path",
            "tools.yt-dlp.env_allowlist",
        ] {
            assert_eq!(registry.def(key), None, "`{key}` must not be registered");
        }
    }

    #[test]
    fn user_overridable_settings_are_marked() {
        let registry = SettingsRegistry::builtin();
        assert!(
            registry
                .def(keys::SUBSONIC_DEFAULT_SCOPE.name())
                .unwrap()
                .user_overridable
        );
        assert!(
            registry
                .def(keys::PRIVACY_NOW_PLAYING_VISIBLE.name())
                .unwrap()
                .user_overridable
        );
        for key in [
            keys::PLUGINS_MAX_MEMORY_MB.name(),
            keys::STREAM_PENDING_MODE.name(),
            keys::LIBRARY_COVER_FILENAME.name(),
        ] {
            assert!(
                !registry.def(key).unwrap().user_overridable,
                "`{key}` must not be user overridable"
            );
        }
    }

    #[test]
    fn new_kinds_resolve_from_the_environment_and_the_file() {
        let registry = SettingsRegistry::builtin();
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__USERS__DEFAULT_PERMISSIONS".to_owned(),
            "acquire, upload".to_owned(),
        );
        sources.env.insert(
            "HEARTHSOUND__ACQUISITION__MIN_MATCH_SCORE".to_owned(),
            "1".to_owned(),
        );
        sources.file.insert(
            "stream.pending.mode".to_owned(),
            toml::Value::String("fail".to_owned()),
        );

        let store = SettingsStore::resolve(&registry, &sources).unwrap();
        assert_eq!(
            store.get(keys::USERS_DEFAULT_PERMISSIONS).unwrap(),
            ["acquire", "upload"]
        );
        assert_eq!(
            store.source(keys::USERS_DEFAULT_PERMISSIONS.name()),
            Some(SettingSource::Environment)
        );
        assert_eq!(store.get(keys::ACQUISITION_MIN_MATCH_SCORE).unwrap(), 1.0);
        assert_eq!(store.get(keys::STREAM_PENDING_MODE).unwrap(), "fail");
        assert_eq!(
            store.source(keys::STREAM_PENDING_MODE.name()),
            Some(SettingSource::File)
        );

        let mut sources = SettingSources::default();
        sources.file.insert(
            "users.default_permissions".to_owned(),
            toml::Value::Array(vec![toml::Value::String("download".to_owned())]),
        );
        let store = SettingsStore::resolve(&registry, &sources).unwrap();
        assert_eq!(
            store.get(keys::USERS_DEFAULT_PERMISSIONS).unwrap(),
            ["download"]
        );
    }

    #[test]
    fn a_list_with_a_missing_element_is_rejected_from_the_file() {
        let registry = SettingsRegistry::builtin();
        let mut sources = SettingSources::default();
        sources.file.insert(
            "users.default_permissions".to_owned(),
            toml::Value::Array(vec![toml::Value::String("admin".to_owned())]),
        );
        let error = SettingsStore::resolve(&registry, &sources).unwrap_err();
        match error {
            ConfigError::InvalidValue {
                key,
                origin,
                reason,
                ..
            } => {
                assert_eq!(key, "users.default_permissions");
                assert_eq!(origin, SettingSource::File);
                assert_eq!(reason, "must only contain: acquire, upload, download");
            }
            other => panic!("unexpected error: {other}"),
        }
    }
}
