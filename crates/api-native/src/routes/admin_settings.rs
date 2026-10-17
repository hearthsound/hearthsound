//! `/api/v1/admin/settings`: the server-wide settings.
//!
//! The listing shows every registered setting with its effective value, the
//! built-in default, and the layer that supplied the value. A change is stored
//! in the database layer. A key the configuration file or an environment
//! variable pins cannot be changed here: the request is refused with a
//! `conflict` problem that names every such key, and nothing in the batch is
//! written.
use axum::Json;
use axum::extract::State;
use hearthsound_config::settings::{SettingEntry, SettingsError};
use hearthsound_config::{SettingKind, SettingSource, SettingsRegistry};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::AdminAuthed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{setting_json, setting_value};

/// One registered server setting.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServerSetting {
    /// The dotted setting key.
    pub key: String,
    /// The effective value.
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
    /// The built-in default.
    #[schema(value_type = Object)]
    pub default: serde_json::Value,
    /// The layer that supplied the value: `environment`, `file`, `database`,
    /// or `default`.
    pub source: String,
    /// Whether the configuration file pins the key.
    pub pinned: bool,
    /// Whether an environment variable pins the key.
    pub environment_pinned: bool,
    /// The value's shape: `bool`, `integer`, `float`, `string`, or
    /// `stringList`.
    pub kind: String,
    /// Whether users may override the setting for themselves.
    pub user_overridable: bool,
    /// What the setting does.
    pub description: String,
}

/// Every registered server setting, in registry order.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ServerSettings {
    /// One entry per registered setting.
    pub settings: Vec<ServerSetting>,
}

/// A batch of server setting changes.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ServerSettingsPatch {
    /// The new value per key; `null` removes the stored value so the layers
    /// below apply again.
    #[schema(value_type = Object)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

/// Lists every registered server setting.
///
/// # Errors
///
/// Answers `forbidden` for a caller who is not an administrator.
#[utoipa::path(
    get,
    path = "/api/v1/admin/settings",
    tag = "admin",
    responses(
        (status = 200, description = "Every registered server setting", body = ServerSettings),
        (status = 403, description = "The caller is not an administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_settings(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<ServerSettings>, Problem> {
    Ok(Json(listing(&state)))
}

/// Changes server settings; `null` removes a stored value.
///
/// Every change is checked before any is written. A key the configuration
/// file or an environment variable pins is refused with a `conflict` problem
/// naming all such keys; an unknown key or a value that does not fit is an
/// `invalid-request` problem. Either way nothing changes.
///
/// # Errors
///
/// Answers `forbidden` for a non-administrator, `invalid-request` for an
/// unknown key or invalid value, and `conflict` for pinned keys.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/settings",
    tag = "admin",
    request_body = ServerSettingsPatch,
    responses(
        (status = 200, description = "Every registered server setting after the change", body = ServerSettings),
        (status = 400, description = "An unknown or invalid setting", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The caller is not an administrator", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A setting is pinned by the configuration file or the environment", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_settings(
    AdminAuthed(authed): AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(patch): JsonBody<ServerSettingsPatch>,
) -> Result<Json<ServerSettings>, Problem> {
    let settings = state.settings();
    let mut changes = Vec::with_capacity(patch.settings.len());
    let entries = settings.list();
    let mut pinned = Vec::new();
    for (key, value) in &patch.settings {
        let def = settings
            .registry()
            .def(key)
            .ok_or_else(|| Problem::invalid_request(format!("`{key}` is not a setting.")))?;
        let entry_pinned = entries.iter().any(|entry| {
            entry.key == key.as_str() && (entry.pinned || settings.is_environment_override(key))
        });
        if entry_pinned {
            pinned.push(key.as_str());
            continue;
        }
        let value = if value.is_null() {
            None
        } else {
            Some(setting_value(def, value)?)
        };
        changes.push((key.clone(), value));
    }
    if !pinned.is_empty() {
        return Err(Problem::conflict(format!(
            "Pinned by the configuration file or the environment, so they cannot be changed here: {}.",
            pinned.join(", ")
        )));
    }
    let keys: Vec<String> = changes.iter().map(|(key, _)| key.clone()).collect();
    settings
        .apply(&changes, Some(authed.user_id()))
        .await
        .map_err(|error| match error {
            SettingsError::Pinned { key } | SettingsError::EnvironmentOverride { key } => {
                Problem::conflict(format!("`{key}` is pinned by the configuration."))
            }
            SettingsError::UnknownKey { key } => {
                Problem::invalid_request(format!("`{key}` is not a setting."))
            }
            SettingsError::Invalid(error) => Problem::invalid_request(error.to_string()),
            SettingsError::Database(error) => problem::internal("writing server settings", &error),
        })?;
    tracing::info!(user = %authed.user_id(), ?keys, "changed server settings");
    Ok(Json(listing(&state)))
}

/// Builds the listing from the current snapshot and the registry.
///
/// # Parameters
///
/// - `state`: the shared services.
fn listing(state: &NativeState) -> ServerSettings {
    let settings = state.settings();
    let registry = settings.registry();
    let settings = settings
        .list()
        .into_iter()
        .map(|entry| server_setting(state, &entry, registry))
        .collect();
    ServerSettings { settings }
}

/// Converts one registry entry to its view.
///
/// # Parameters
///
/// - `state`: the shared services, which report environment pins.
/// - `entry`: the entry from [`hearthsound_config::Settings::list`].
/// - `registry`: the registry, which supplies the kind.
fn server_setting(
    state: &NativeState,
    entry: &SettingEntry,
    registry: &SettingsRegistry,
) -> ServerSetting {
    let kind = registry
        .def(entry.key)
        .map(|def| kind_name(&def.kind))
        .unwrap_or("unknown");
    ServerSetting {
        key: entry.key.to_owned(),
        value: setting_json(&entry.value),
        default: setting_json(&entry.default),
        source: source_name(entry.source).to_owned(),
        pinned: entry.pinned,
        environment_pinned: state.settings().is_environment_override(entry.key),
        kind: kind.to_owned(),
        user_overridable: entry.user_overridable,
        description: entry.description.to_owned(),
    }
}

/// Names a setting's source the way the API reports it.
fn source_name(source: SettingSource) -> &'static str {
    match source {
        SettingSource::Environment => "environment",
        SettingSource::File => "file",
        SettingSource::Database => "database",
        SettingSource::Default => "default",
    }
}

/// Names a setting's kind the way the API reports it.
fn kind_name(kind: &SettingKind) -> &'static str {
    match kind {
        SettingKind::Bool => "bool",
        SettingKind::Integer { .. } => "integer",
        SettingKind::Float { .. } => "float",
        SettingKind::String { .. } => "string",
        SettingKind::StringList { .. } => "stringList",
    }
}
