//! Plugin manifest parsing and validation.
//!
//! A plugin archive carries a `manifest.json` describing the plugin: who wrote
//! it, which interface version it speaks, which layers it provides, what
//! resources it needs and what the host is allowed to do on its behalf. The
//! typed model here mirrors the manifest rules field for field, and
//! [`Manifest::parse`] applies the rules a JSON parser cannot express.

use std::str::FromStr;

use hearthsound_config::{ConfigError, SettingsStore, keys};
use hearthsound_core::Layer;
use serde::Deserialize;

/// The API version this server hosts.
pub const SUPPORTED_API: &str = "hearthsound:plugin@0.1.0";

/// The largest accepted `id` length.
const MAX_ID_LENGTH: usize = 128;

/// The largest accepted `permissions.http.maxBodyBytes`.
const MAX_BODY_BYTES: u64 = 67_108_864;

/// The largest accepted `resources.callTimeoutMs`.
const MAX_CALL_TIMEOUT_MS: u32 = 120_000;

/// The largest accepted `resources.maxInstances`.
const MAX_MAX_INSTANCES: u8 = 8;

/// A parsed plugin manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    /// The manifest format version; must be `1`.
    pub manifest_version: u32,
    /// The plugin id, a reverse-domain name.
    pub id: String,
    /// The human readable plugin name.
    pub name: String,
    /// The plugin version, as SemVer.
    pub version: String,
    /// The plugin authors.
    #[serde(default)]
    pub authors: Vec<String>,
    /// A one-line description of the plugin.
    pub description: String,
    /// The license the plugin is distributed under.
    pub license: String,
    /// The project's home page.
    pub homepage: String,
    /// The plugin API version the plugin was built against.
    pub api: String,
    /// The layers the plugin provides.
    pub provides: Vec<String>,
    /// What the plugin is allowed to do.
    pub permissions: Permissions,
    /// The configuration the plugin exposes to administrators and users.
    pub config: PluginConfig,
    /// The resources the plugin asks for.
    pub resources: Resources,
    /// Catalog-specific options; only valid when `catalog` is provided.
    #[serde(default)]
    pub catalog: Option<CatalogOptions>,
    /// URL patterns the plugin imports; only valid for `playlist-import`.
    #[serde(default)]
    pub url_patterns: Option<Vec<String>>,
    /// The parsed form of `provides`.
    #[serde(skip)]
    layers: Vec<Layer>,
}

impl Manifest {
    /// Parses and validates a manifest.
    ///
    /// # Parameters
    ///
    /// - `bytes`: the raw `manifest.json` contents.
    /// - `settings`: the resolved settings, used for rules that depend on
    ///   server configuration such as the memory ceiling.
    ///
    /// # Returns
    ///
    /// The parsed manifest.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestError::Parse`] when the bytes are not JSON, are not an
    /// object, or carry unknown properties, and [`ManifestError::Invalid`] when
    /// a value breaks a manifest rule.
    pub fn parse(bytes: &[u8], settings: &SettingsStore) -> Result<Manifest, ManifestError> {
        let mut manifest: Manifest = serde_json::from_slice(bytes).map_err(ManifestError::Parse)?;
        manifest.validate(settings)?;
        Ok(manifest)
    }

    /// Returns the layers the plugin provides.
    pub fn provides(&self) -> &[Layer] {
        &self.layers
    }

    /// Applies every semantic manifest rule.
    fn validate(&mut self, settings: &SettingsStore) -> Result<(), ManifestError> {
        if self.manifest_version != 1 {
            return Err(self.invalid(
                "manifestVersion",
                self.manifest_version.to_string(),
                "the only supported manifest version is 1",
            ));
        }
        if !valid_id(&self.id) {
            return Err(self.invalid(
                "id",
                self.id.clone(),
                "expected a reverse-domain id of at least two lowercase labels",
            ));
        }
        for (field, value) in [
            ("name", &self.name),
            ("description", &self.description),
            ("license", &self.license),
        ] {
            if value.trim().is_empty() {
                return Err(self.invalid(field, value.clone(), "must not be empty"));
            }
        }
        if !self.homepage.starts_with("https://") && !self.homepage.starts_with("http://") {
            return Err(self.invalid(
                "homepage",
                self.homepage.clone(),
                "must start with https:// or http://",
            ));
        }
        if !valid_api(&self.api) {
            return Err(self.invalid(
                "api",
                self.api.clone(),
                "expected an api of the form hearthsound:plugin@X.Y.Z",
            ));
        }
        if semver::Version::parse(&self.version).is_err() {
            return Err(self.invalid("version", self.version.clone(), "expected a SemVer version"));
        }

        if self.provides.is_empty() {
            return Err(self.invalid("provides", "[]".to_owned(), "must not be empty"));
        }
        let mut layers = Vec::with_capacity(self.provides.len());
        for name in &self.provides {
            let layer = Layer::from_str(name)
                .map_err(|_| self.invalid("provides", name.clone(), "is not a known layer"))?;
            if layers.contains(&layer) {
                return Err(self.invalid("provides", name.clone(), "is listed more than once"));
            }
            layers.push(layer);
        }

        let playlist_import = layers.contains(&Layer::PlaylistImport);
        match (&self.url_patterns, playlist_import) {
            (Some(_), false) => {
                return Err(self.invalid(
                    "urlPatterns",
                    "present".to_owned(),
                    "is only allowed when playlist-import is provided",
                ));
            }
            (None, true) => {
                return Err(self.invalid(
                    "urlPatterns",
                    "missing".to_owned(),
                    "is required when playlist-import is provided",
                ));
            }
            _ => {}
        }
        if self.catalog.is_some() && !layers.contains(&Layer::Catalog) {
            return Err(self.invalid(
                "catalog",
                "present".to_owned(),
                "is only allowed when catalog is provided",
            ));
        }

        self.validate_permissions()?;
        self.validate_config()?;
        self.validate_resources(settings)?;

        self.layers = layers;
        Ok(())
    }

    /// Validates the permissions block.
    fn validate_permissions(&self) -> Result<(), ManifestError> {
        if let Some(http) = &self.permissions.http {
            if http.hosts.is_empty() {
                return Err(self.invalid(
                    "permissions.http.hosts",
                    "[]".to_owned(),
                    "must not be empty",
                ));
            }
            for host in &http.hosts {
                if !valid_host(host) {
                    return Err(self.invalid(
                        "permissions.http.hosts",
                        host.clone(),
                        "is not a hostname, a wildcard hostname, or a $config: reference",
                    ));
                }
            }
            if let Some(rate_limit) = &http.rate_limit
                && rate_limit.per_second < 1
            {
                return Err(self.invalid(
                    "permissions.http.rateLimit.perSecond",
                    rate_limit.per_second.to_string(),
                    "must be at least 1",
                ));
            }
            if let Some(rate_limit) = &http.rate_limit
                && rate_limit.burst < 1
            {
                return Err(self.invalid(
                    "permissions.http.rateLimit.burst",
                    rate_limit.burst.to_string(),
                    "must be at least 1",
                ));
            }
            if let Some(max_body_bytes) = http.max_body_bytes
                && !(1..=MAX_BODY_BYTES).contains(&max_body_bytes)
            {
                return Err(self.invalid(
                    "permissions.http.maxBodyBytes",
                    max_body_bytes.to_string(),
                    "must be between 1 and 67108864",
                ));
            }
        }
        if let Some(max_bytes) = self.permissions.kv.as_ref().and_then(|kv| kv.max_bytes)
            && !(1..=MAX_BODY_BYTES).contains(&max_bytes)
        {
            return Err(self.invalid(
                "permissions.kv.maxBytes",
                max_bytes.to_string(),
                "must be between 1 and 67108864",
            ));
        }
        if let Some(process) = &self.permissions.process
            && !process.tools.is_empty()
        {
            return Err(self.invalid(
                "permissions.process.tools",
                format!("{:?}", process.tools),
                "host-process does not exist in this API version, so tools must be empty",
            ));
        }
        Ok(())
    }

    /// Validates that every configuration schema compiles.
    fn validate_config(&self) -> Result<(), ManifestError> {
        for (field, section) in [
            ("config.server", &self.config.server),
            ("config.user", &self.config.user),
        ] {
            let Some(section) = section else {
                continue;
            };
            if let Err(error) = jsonschema::draft7::new(&section.schema) {
                return Err(self.invalid(
                    format!("{field}.schema"),
                    "invalid".to_owned(),
                    error.to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Validates the resource requests against the server's settings.
    fn validate_resources(&self, settings: &SettingsStore) -> Result<(), ManifestError> {
        if self.resources.memory_mb < 1 {
            return Err(self.invalid(
                "resources.memoryMb",
                self.resources.memory_mb.to_string(),
                "must be at least 1",
            ));
        }
        let ceiling = settings
            .get(keys::PLUGINS_MAX_MEMORY_MB)
            .map_err(ManifestError::Settings)?;
        if self.resources.memory_mb > ceiling {
            return Err(self.invalid(
                "resources.memoryMb",
                self.resources.memory_mb.to_string(),
                format!("must not exceed the plugins.max_memory_mb setting of {ceiling}"),
            ));
        }
        if !(1..=MAX_CALL_TIMEOUT_MS).contains(&self.resources.call_timeout_ms) {
            return Err(self.invalid(
                "resources.callTimeoutMs",
                self.resources.call_timeout_ms.to_string(),
                "must be between 1 and 120000",
            ));
        }
        if !(1..=MAX_MAX_INSTANCES).contains(&self.resources.max_instances) {
            return Err(self.invalid(
                "resources.maxInstances",
                self.resources.max_instances.to_string(),
                "must be between 1 and 8",
            ));
        }
        Ok(())
    }

    /// Builds an [`ManifestError::Invalid`] for one of this manifest's fields.
    fn invalid(
        &self,
        field: impl Into<String>,
        value: impl Into<String>,
        reason: impl Into<String>,
    ) -> ManifestError {
        ManifestError::Invalid {
            field: field.into(),
            value: value.into(),
            reason: reason.into(),
        }
    }
}

/// What a plugin is allowed to do.
///
/// A `Default` value grants nothing: an instance whose plugin was approved for
/// no permission can reach nothing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Permissions {
    /// Outbound HTTP permission.
    #[serde(default)]
    pub http: Option<HttpPermission>,
    /// Key-value storage permission.
    #[serde(default)]
    pub kv: Option<KvPermission>,
    /// Filesystem permission.
    #[serde(default)]
    pub files: Option<FilesPermission>,
    /// Process execution permission.
    #[serde(default)]
    pub process: Option<ProcessPermission>,
}

/// The hosts a plugin may reach, and how hard it may hammer them.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpPermission {
    /// The approved hosts.
    pub hosts: Vec<String>,
    /// An optional request rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// An optional cap on request and response body size.
    #[serde(default)]
    pub max_body_bytes: Option<u64>,
}

/// A request rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimit {
    /// Sustained requests per second.
    pub per_second: u32,
    /// The burst size above the sustained rate.
    pub burst: u32,
}

/// The key-value storage quota.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KvPermission {
    /// The maximum number of bytes the plugin may store.
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

/// The filesystem permission.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesPermission {
    /// Whether the plugin may read import roots.
    #[serde(default)]
    pub import_roots: Option<bool>,
}

/// The process execution permission.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessPermission {
    /// The approved tools; must be empty in this API version.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// The configuration a plugin exposes.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginConfig {
    /// Server-scoped configuration, visible only to administrators.
    pub server: Option<ConfigSection>,
    /// User-scoped configuration.
    pub user: Option<ConfigSection>,
}

/// One configuration section: a JSON Schema and a UI schema.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConfigSection {
    /// The draft-07 JSON Schema describing the configuration object.
    pub schema: serde_json::Value,
    /// The UI schema describing how to render the form.
    pub ui_schema: serde_json::Value,
}

/// The resources a plugin asks for.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resources {
    /// The memory ceiling of one instance, in megabytes.
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,
    /// The deadline of one plugin call, in milliseconds.
    #[serde(default = "default_call_timeout_ms")]
    pub call_timeout_ms: u32,
    /// How many instances of the plugin may run at once.
    #[serde(default = "default_max_instances")]
    pub max_instances: u8,
}

/// The default `resources.memoryMb`.
fn default_memory_mb() -> u32 {
    128
}

/// The default `resources.callTimeoutMs`.
fn default_call_timeout_ms() -> u32 {
    30_000
}

/// The default `resources.maxInstances`.
fn default_max_instances() -> u8 {
    2
}

/// Catalog-specific manifest options.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogOptions {
    /// How the host caches the plugin's cover images.
    #[serde(default)]
    pub image_cache_policy: ImageCachePolicy,
}

/// How the host treats cover images a catalog plugin returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageCachePolicy {
    /// Cache images under the artwork cache.
    #[default]
    Cache,
    /// Do not store the images.
    NoStore,
}

/// An error from parsing or validating a manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The bytes are not a valid manifest document.
    #[error("manifest.json: {0}")]
    Parse(serde_json::Error),
    /// A manifest field broke a rule.
    #[error("manifest field `{field}` = {value}: {reason}")]
    Invalid {
        /// The field, named in JSON-pointer style.
        field: String,
        /// The rejected value.
        value: String,
        /// Why the value was rejected.
        reason: String,
    },
    /// A rule that depends on server settings could not be applied.
    #[error(transparent)]
    Settings(ConfigError),
}

/// Returns whether `id` is a valid reverse-domain plugin id.
fn valid_id(id: &str) -> bool {
    if id.is_empty() || id.len() > MAX_ID_LENGTH {
        return false;
    }
    let labels: Vec<&str> = id.split('.').collect();
    labels.len() >= 2 && labels.iter().all(|label| valid_label(label))
}

/// Returns whether `label` is a lowercase alphanumeric label with inner dashes.
fn valid_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    let mut previous_dash = false;
    for (index, byte) in label.bytes().enumerate() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() {
            previous_dash = false;
        } else if byte == b'-' {
            if index == 0 || previous_dash {
                return false;
            }
            previous_dash = true;
        } else {
            return false;
        }
    }
    !previous_dash
}

/// Returns whether `api` has the shape `hearthsound:plugin@X.Y.Z`.
fn valid_api(api: &str) -> bool {
    let Some(rest) = api.strip_prefix("hearthsound:plugin@") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Returns whether `host` is an approved host pattern.
fn valid_host(host: &str) -> bool {
    if let Some(field) = host.strip_prefix("$config:") {
        return valid_config_field(field);
    }
    let hostname = host.strip_prefix("*.").unwrap_or(host);
    valid_hostname(hostname)
}

/// Returns whether `name` is a lowercase dotted hostname.
fn valid_hostname(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(valid_label)
}

/// Returns whether `field` is a configuration field name.
fn valid_config_field(field: &str) -> bool {
    let mut bytes = field.bytes();
    match bytes.next() {
        Some(byte) if byte.is_ascii_alphabetic() => {}
        _ => return false,
    }
    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_config::{SettingSources, SettingsRegistry};

    const HELLO: &str =
        include_str!("../../test-support/fixtures/plugins/hello-catalog/manifest.json");

    fn store(sources: &SettingSources) -> SettingsStore {
        SettingsStore::resolve(&SettingsRegistry::builtin(), sources).unwrap()
    }

    fn hello_value() -> serde_json::Value {
        serde_json::from_str(HELLO).unwrap()
    }

    fn parse(
        value: &serde_json::Value,
        settings: &SettingsStore,
    ) -> Result<Manifest, ManifestError> {
        Manifest::parse(&serde_json::to_vec(value).unwrap(), settings)
    }

    fn rejected_field(value: &serde_json::Value, settings: &SettingsStore) -> String {
        match parse(value, settings).unwrap_err() {
            ManifestError::Invalid { field, .. } => field,
            other => panic!("expected an invalid-field error, got {other}"),
        }
    }

    #[test]
    fn parses_the_hello_fixture() {
        let settings = store(&SettingSources::default());
        let manifest = Manifest::parse(HELLO.as_bytes(), &settings).unwrap();
        assert_eq!(manifest.id, "com.example.hello-catalog");
        assert_eq!(manifest.provides(), [Layer::Catalog]);
        assert_eq!(manifest.resources.memory_mb, 64);
        assert_eq!(manifest.resources.call_timeout_ms, 5000);
        assert_eq!(manifest.resources.max_instances, 2);
    }

    #[test]
    fn rejects_too_many_instances() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["resources"]["maxInstances"] = serde_json::json!(9);
        assert_eq!(rejected_field(&value, &settings), "resources.maxInstances");
    }

    #[test]
    fn rejects_too_long_a_call_timeout() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["resources"]["callTimeoutMs"] = serde_json::json!(120_001);
        assert_eq!(rejected_field(&value, &settings), "resources.callTimeoutMs");
    }

    #[test]
    fn rejects_a_bad_id() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["id"] = serde_json::json!("Bad");
        assert_eq!(rejected_field(&value, &settings), "id");
    }

    #[test]
    fn rejects_an_unknown_top_level_key() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["extra"] = serde_json::json!(true);
        let error = parse(&value, &settings).unwrap_err();
        assert!(
            error.to_string().contains("extra"),
            "the error should name the unknown key: {error}"
        );
    }

    #[test]
    fn rejects_process_tools() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["permissions"] = serde_json::json!({ "process": { "tools": ["yt-dlp"] } });
        assert_eq!(
            rejected_field(&value, &settings),
            "permissions.process.tools"
        );
    }

    #[test]
    fn rejects_url_patterns_without_playlist_import() {
        let settings = store(&SettingSources::default());
        let mut value = hello_value();
        value["urlPatterns"] = serde_json::json!(["https://example.com/playlist"]);
        assert_eq!(rejected_field(&value, &settings), "urlPatterns");
    }

    #[test]
    fn rejects_memory_above_the_server_setting() {
        let mut sources = SettingSources::default();
        sources.env.insert(
            "HEARTHSOUND__PLUGINS__MAX_MEMORY_MB".to_owned(),
            "64".to_owned(),
        );
        let settings = store(&sources);

        let mut value = hello_value();
        value["resources"]["memoryMb"] = serde_json::json!(128);
        assert_eq!(rejected_field(&value, &settings), "resources.memoryMb");
    }
}
