//! Per-user provider configuration.
//!
//! A listener links their account on a scrobble or discovery service, and the
//! plugin is handed that listener's configuration on every call it makes for
//! them. This module is the write and read side of that link: one row per
//! (instance, user) pair in `provider_user_configs`, holding the account name
//! the service reported and the configuration the plugin needs, sealed as one
//! envelope with the server's [`SecretBox`].
//!
//! A sealed envelope is stored as `{"$secret": {"ciphertext": <base64>,
//! "nonce": <base64>}}`, the ciphertext being the AES-256-GCM seal of the
//! envelope's JSON text, exactly like a plugin instance's write-only fields.
//! Nothing outside [`UserConfigService`] ever sees a plaintext token, and
//! [`UserConfig::context`] is the only place that turns a row into the
//! [`UserContext`] a plugin receives.
//!
//! A listing hands the configuration back with `[sealed]` in place of every
//! write-only field ([`redact_user_config`]), and a write that echoes the
//! placeholder keeps the stored secret ([`merge_user_config`]).

use hearthsound_config::secret::{SecretBox, SecretError};
use hearthsound_core::providers::UserContext;
use hearthsound_db::Database;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::instances::{
    InstanceError, SECRET_PLACEHOLDER, apply_defaults, open_value, seal_value, visit_write_only,
};
use crate::repo::{RepoError, now_millis};

/// The key a sealed envelope is stored under.
const SEALED_KEY: &str = "$secret";

/// The key of a sealed envelope's ciphertext.
const CIPHERTEXT_KEY: &str = "ciphertext";

/// The key of a sealed envelope's nonce.
const NONCE_KEY: &str = "nonce";

/// The envelope member holding the account name the service reported.
const ACCOUNT_NAME_KEY: &str = "accountName";

/// The envelope member holding the plugin's user configuration.
const CONFIG_KEY: &str = "config";

/// One listener's link to one provider instance.
///
/// The configuration is the JSON object the plugin receives, opened from the
/// sealed envelope the database stores.
#[derive(Debug, Clone, PartialEq)]
pub struct UserConfig {
    /// The account name the service reported, when it named one.
    pub account_name: Option<String>,
    /// The plugin's user configuration, in plaintext.
    pub config: Value,
    /// When the listener linked the account, in unix milliseconds.
    pub linked_at: i64,
}

impl UserConfig {
    /// Returns the context a plugin call is made with.
    ///
    /// The user id is the listener's UUID in lowercase hyphenated form and the
    /// configuration is the inner object of the stored envelope, serialized as
    /// JSON.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener's id.
    ///
    /// # Returns
    ///
    /// The user context to hand the plugin.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_plugin_host::UserConfig;
    /// use uuid::Uuid;
    ///
    /// let config = UserConfig {
    ///     account_name: Some("listener".to_owned()),
    ///     config: serde_json::json!({"token": "s"}),
    ///     linked_at: 0,
    /// };
    /// let context = config.context(Uuid::nil());
    /// assert_eq!(context.user_id, "00000000-0000-0000-0000-000000000000");
    /// assert_eq!(context.config.as_deref(), Some("{\"token\":\"s\"}"));
    /// ```
    pub fn context(&self, user_id: Uuid) -> UserContext {
        UserContext {
            user_id: user_id.to_string(),
            config: Some(self.config.to_string()),
        }
    }
}

/// One listener linked to an instance, with their configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkedUser {
    /// The listener's id.
    pub user_id: Uuid,
    /// The listener's link to the instance.
    pub link: UserConfig,
}

/// An error from reading or writing a per-user configuration.
#[derive(Debug, thiserror::Error)]
pub enum UserConfigError {
    /// The database reported an error.
    #[error(transparent)]
    Repository(#[from] RepoError),
    /// The stored envelope or the configuration is not the JSON this module
    /// wrote.
    #[error("user configuration: {0}")]
    Config(String),
    /// The configuration does not match the plugin's user schema.
    #[error("the user configuration does not match the plugin's schema: {}", .0.join("; "))]
    Schema(Vec<String>),
    /// A secret could not be sealed or opened.
    #[error(transparent)]
    Secret(#[from] SecretError),
}

/// Reads and writes the per-user configuration of provider instances.
///
/// The service borrows the database's pools, so it is cheap to clone and shares
/// the server's writer with everything else.
#[derive(Clone)]
pub struct UserConfigService {
    /// The writer pool every mutation goes through.
    writer: sqlx::SqlitePool,
    /// The reader pool every query goes through.
    readers: sqlx::SqlitePool,
    /// The box envelopes are sealed and opened with.
    secrets: SecretBox,
}

impl UserConfigService {
    /// Builds the service over a database and the server's secret key.
    ///
    /// # Parameters
    ///
    /// - `db`: the open database; its writer and reader pools are borrowed, not
    ///   taken.
    /// - `secrets`: the box user configurations are sealed with.
    ///
    /// # Returns
    ///
    /// The service.
    pub fn new(db: &Database, secrets: SecretBox) -> UserConfigService {
        UserConfigService {
            writer: db.writer().clone(),
            readers: db.readers().clone(),
            secrets,
        }
    }

    /// Returns one listener's link to an instance.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance.
    /// - `user_id`: the listener.
    ///
    /// # Returns
    ///
    /// The link, or `None` when the listener has not linked the instance.
    ///
    /// # Errors
    ///
    /// Returns [`UserConfigError::Repository`] when the row cannot be read and
    /// [`UserConfigError::Config`] or [`UserConfigError::Secret`] when the
    /// stored envelope cannot be opened.
    pub async fn get(
        &self,
        instance_id: Uuid,
        user_id: Uuid,
    ) -> Result<Option<UserConfig>, UserConfigError> {
        let row = sqlx::query!(
            "SELECT config, linked_at FROM provider_user_configs
             WHERE instance_id = ? AND user_id = ?",
            instance_id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
        )
        .fetch_optional(&self.readers)
        .await
        .map_err(RepoError::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let config = match row.config {
            Some(stored) => Some(decode_envelope(&stored, &self.secrets)?),
            None => None,
        };
        Ok(config.map(|config| UserConfig {
            linked_at: row.linked_at,
            ..config
        }))
    }

    /// Stores one listener's link to an instance, replacing an existing one.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance.
    /// - `user_id`: the listener.
    /// - `account_name`: the name the service reported, when it named one.
    /// - `config`: the plugin's user configuration, as an object.
    ///
    /// # Returns
    ///
    /// The stored link.
    ///
    /// # Errors
    ///
    /// Returns [`UserConfigError::Repository`] when the write fails and
    /// [`UserConfigError::Secret`] when the configuration cannot be sealed.
    pub async fn put(
        &self,
        instance_id: Uuid,
        user_id: Uuid,
        account_name: Option<String>,
        config: &Value,
    ) -> Result<UserConfig, UserConfigError> {
        let linked_at = now_millis();
        let envelope = encode_envelope(account_name.as_deref(), config, &self.secrets)?;
        sqlx::query!(
            "INSERT INTO provider_user_configs (instance_id, user_id, config, linked_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(instance_id, user_id) DO UPDATE SET
                config = excluded.config,
                linked_at = excluded.linked_at",
            instance_id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
            envelope,
            linked_at,
        )
        .execute(&self.writer)
        .await
        .map_err(RepoError::from)?;
        Ok(UserConfig {
            account_name,
            config: config.clone(),
            linked_at,
        })
    }

    /// Deletes one listener's link to an instance.
    ///
    /// Only the link row is removed here. The scrobble outbox refuses to send
    /// anything for a listener who has no link, so its own rows are the outbox's
    /// to purge when it learns the link is gone.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance.
    /// - `user_id`: the listener.
    ///
    /// # Returns
    ///
    /// `true` when a link existed and was removed, `false` when there was none.
    ///
    /// # Errors
    ///
    /// Returns [`UserConfigError::Repository`] when the delete fails.
    pub async fn delete(&self, instance_id: Uuid, user_id: Uuid) -> Result<bool, UserConfigError> {
        let outcome = sqlx::query!(
            "DELETE FROM provider_user_configs WHERE instance_id = ? AND user_id = ?",
            instance_id.as_bytes().as_slice(),
            user_id.as_bytes().as_slice(),
        )
        .execute(&self.writer)
        .await
        .map_err(RepoError::from)?;
        Ok(outcome.rows_affected() > 0)
    }

    /// Returns every listener linked to an instance.
    ///
    /// The rows are ordered by user id, so a caller that walks them is
    /// deterministic.
    ///
    /// # Parameters
    ///
    /// - `instance_id`: the provider instance.
    ///
    /// # Returns
    ///
    /// One entry per linked listener.
    ///
    /// # Errors
    ///
    /// Returns [`UserConfigError::Repository`] when the rows cannot be read and
    /// [`UserConfigError::Config`] or [`UserConfigError::Secret`] when a stored
    /// envelope cannot be opened.
    pub async fn linked_users(
        &self,
        instance_id: Uuid,
    ) -> Result<Vec<LinkedUser>, UserConfigError> {
        let rows = sqlx::query!(
            "SELECT user_id, config, linked_at FROM provider_user_configs
             WHERE instance_id = ? ORDER BY user_id",
            instance_id.as_bytes().as_slice(),
        )
        .fetch_all(&self.readers)
        .await
        .map_err(RepoError::from)?;
        let mut users = Vec::with_capacity(rows.len());
        for row in rows {
            let user_id = Uuid::from_slice(&row.user_id).map_err(|error| RepoError::Malformed {
                column: "provider_user_configs.user_id",
                reason: error.to_string(),
            })?;
            let Some(stored) = row.config else {
                continue;
            };
            let link = UserConfig {
                linked_at: row.linked_at,
                ..decode_envelope(&stored, &self.secrets)?
            };
            users.push(LinkedUser { user_id, link });
        }
        Ok(users)
    }
}

/// Checks a listener's input against a plugin's user schema and fills in
/// defaults.
///
/// # Parameters
///
/// - `schema`: the manifest's `config.user.schema`, a draft-07 JSON Schema.
/// - `input`: the configuration the listener sent.
///
/// # Returns
///
/// The configuration with the schema's defaults applied, ready to be handed to
/// the plugin.
///
/// # Errors
///
/// Returns [`UserConfigError::Config`] when the input is not an object or the
/// schema does not compile, and [`UserConfigError::Schema`] with one message per
/// violation when the input does not match the schema.
pub fn validate_user_config(schema: &Value, input: &Value) -> Result<Value, UserConfigError> {
    if !input.is_object() {
        return Err(UserConfigError::Config(
            "the configuration must be a JSON object".to_owned(),
        ));
    }
    let mut config = input.clone();
    apply_defaults(schema, &mut config);

    let validator = jsonschema::draft7::new(schema).map_err(|error| {
        UserConfigError::Config(format!("the plugin's schema does not compile: {error}"))
    })?;
    let problems: Vec<String> = validator
        .iter_errors(&config)
        .map(|error| error.to_string())
        .collect();
    if !problems.is_empty() {
        return Err(UserConfigError::Schema(problems));
    }
    Ok(config)
}

/// Replaces every write-only field of a user configuration with
/// [`SECRET_PLACEHOLDER`], so a listing can be rendered without holding a
/// secret.
///
/// A field the configuration does not carry is not invented, and a field the
/// schema does not mark `writeOnly` is handed back as it is.
///
/// # Parameters
///
/// - `schema`: the manifest's `config.user.schema`, a draft-07 JSON Schema.
/// - `config`: the stored configuration.
///
/// # Returns
///
/// The configuration, with every write-only field masked.
pub fn redact_user_config(schema: &Value, config: &Value) -> Value {
    let mut masked = config.clone();
    visit_write_only(schema, &mut masked, &mut |_path, value| {
        *value = Value::String(SECRET_PLACEHOLDER.to_owned());
    });
    masked
}

/// Puts the stored value back under every write-only field the caller echoed as
/// [`SECRET_PLACEHOLDER`].
///
/// A field the caller left out is not touched: the schema's defaults are
/// applied later, exactly as for a first write.
///
/// # Parameters
///
/// - `schema`: the manifest's `config.user.schema`, a draft-07 JSON Schema.
/// - `input`: the configuration the listener sent.
/// - `previous`: the configuration that is stored, when the service is linked.
///
/// # Returns
///
/// The configuration to validate and store.
///
/// # Errors
///
/// Returns [`UserConfigError::Config`] when a field carries the placeholder
/// while no stored configuration holds a value for it.
pub fn merge_user_config(
    schema: &Value,
    input: &Value,
    previous: Option<&Value>,
) -> Result<Value, UserConfigError> {
    let mut merged = input.clone();
    let mut failure: Option<UserConfigError> = None;
    visit_write_only(schema, &mut merged, &mut |path, value| {
        if value.as_str() != Some(SECRET_PLACEHOLDER) {
            return;
        }
        match previous.and_then(|stored| stored.pointer(path)) {
            Some(stored) if !stored.is_null() => *value = stored.clone(),
            _ => {
                failure.get_or_insert_with(|| {
                    UserConfigError::Config(format!(
                        "`{path}` is the sealed placeholder, but no configuration is stored for this service"
                    ))
                });
            }
        }
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(merged),
    }
}

/// Seals the account name and the configuration into the stored envelope.
///
/// # Parameters
///
/// - `account_name`: the account name the service reported.
/// - `config`: the plugin's user configuration.
/// - `secrets`: the box the envelope is sealed with.
///
/// # Returns
///
/// The sealed envelope's JSON text, ready for the `config` column.
///
/// # Errors
///
/// Returns [`UserConfigError::Secret`] when the envelope cannot be sealed.
fn encode_envelope(
    account_name: Option<&str>,
    config: &Value,
    secrets: &SecretBox,
) -> Result<String, UserConfigError> {
    let envelope = json!({ACCOUNT_NAME_KEY: account_name, CONFIG_KEY: config});
    let sealed = seal_value(&envelope, secrets).map_err(map_instance_error)?;
    serde_json::to_string(&sealed)
        .map_err(|error| UserConfigError::Config(format!("sealing the envelope: {error}")))
}

/// Opens a stored envelope into the account name and the configuration.
///
/// # Parameters
///
/// - `stored`: the `config` column's text.
/// - `secrets`: the box the envelope was sealed with.
///
/// # Returns
///
/// The account name and configuration the envelope holds; `linked_at` is left
/// at zero for the caller to fill in from the row.
///
/// # Errors
///
/// Returns [`UserConfigError::Config`] when the text is not a sealed envelope
/// this module wrote and [`UserConfigError::Secret`] when it does not
/// authenticate.
fn decode_envelope(stored: &str, secrets: &SecretBox) -> Result<UserConfig, UserConfigError> {
    let sealed: Value = serde_json::from_str(stored).map_err(|error| {
        UserConfigError::Config(format!("the stored envelope is not JSON: {error}"))
    })?;
    if sealed.get(SEALED_KEY).is_none()
        || sealed
            .get(SEALED_KEY)
            .and_then(|inner| inner.get(CIPHERTEXT_KEY))
            .and_then(Value::as_str)
            .is_none()
        || sealed
            .get(SEALED_KEY)
            .and_then(|inner| inner.get(NONCE_KEY))
            .and_then(Value::as_str)
            .is_none()
    {
        return Err(UserConfigError::Config(
            "the stored configuration is not a sealed envelope".to_owned(),
        ));
    }
    let opened = open_value(&sealed, secrets).map_err(map_instance_error)?;
    let account_name = opened
        .get(ACCOUNT_NAME_KEY)
        .and_then(Value::as_str)
        .map(str::to_owned);
    let config = opened
        .get(CONFIG_KEY)
        .filter(|config| config.is_object())
        .cloned()
        .ok_or_else(|| {
            UserConfigError::Config("the stored envelope holds no configuration object".to_owned())
        })?;
    Ok(UserConfig {
        account_name,
        config,
        linked_at: 0,
    })
}

/// Maps an instance error from the sealing helpers onto this module's error.
///
/// # Parameters
///
/// - `error`: what the sealing helper reported.
fn map_instance_error(error: InstanceError) -> UserConfigError {
    match error {
        InstanceError::Secret(error) => UserConfigError::Secret(error),
        other => UserConfigError::Config(other.to_string()),
    }
}
