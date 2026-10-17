//! Response shapes several resources share.

use hearthsound_auth::{User, UserPermissions};
use hearthsound_config::{SettingDef, SettingKind, SettingValue};
use hearthsound_core::{ClientId, EntityId, IdPrefix};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::problem::Problem;

/// What an account may do besides streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsView {
    /// May trigger acquisitions (`can_acquire`).
    pub acquire: bool,
    /// May upload files (`can_upload`).
    pub upload: bool,
    /// May download files (`can_download`).
    pub download: bool,
}

impl From<UserPermissions> for PermissionsView {
    fn from(permissions: UserPermissions) -> PermissionsView {
        PermissionsView {
            acquire: permissions.acquire,
            upload: permissions.upload,
            download: permissions.download,
        }
    }
}

impl From<PermissionsView> for UserPermissions {
    fn from(view: PermissionsView) -> UserPermissions {
        UserPermissions {
            acquire: view.acquire,
            upload: view.upload,
            download: view.download,
        }
    }
}

/// An account, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserView {
    /// The account's client id (`us_...`).
    pub id: String,
    /// The sign-in name.
    pub username: String,
    /// The name clients show.
    pub display_name: String,
    /// Whether the account administers the server.
    pub is_admin: bool,
    /// What the account may do besides streaming.
    pub permissions: PermissionsView,
    /// When the account was created, in unix milliseconds.
    pub created_at: i64,
    /// When the account last changed, in unix milliseconds.
    pub updated_at: i64,
    /// When the account was disabled, in unix milliseconds; absent while it is
    /// active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<i64>,
}

impl From<&User> for UserView {
    fn from(user: &User) -> UserView {
        UserView {
            id: EntityId::from_uuid(IdPrefix::User, user.id).to_string(),
            username: user.username.clone(),
            display_name: user.display_name.clone(),
            is_admin: user.is_admin,
            permissions: user.permissions().into(),
            created_at: user.created_at,
            updated_at: user.updated_at,
            disabled_at: user.disabled_at,
        }
    }
}

/// The current time in unix milliseconds.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Reads a client id from a path and checks its kind.
///
/// # Parameters
///
/// - `value`: the path segment.
/// - `prefix`: the kind the resource names, for example [`IdPrefix::User`].
///
/// # Returns
///
/// The row id.
///
/// # Errors
///
/// Answers `not-found` when the segment is not a client id of that kind:
/// nothing can live at such a path.
pub fn parse_id(value: &str, prefix: IdPrefix) -> Result<Uuid, Problem> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == prefix => Ok(id.uuid()),
        _ => Err(Problem::not_found(format!(
            "`{value}` is not a `{prefix}_` id."
        ))),
    }
}

/// Formats a row id as the client id of its kind.
pub fn client_id(prefix: IdPrefix, id: Uuid) -> String {
    EntityId::from_uuid(prefix, id).to_string()
}

/// Reads a JSON value as the value of one setting.
///
/// The JSON type has to match the setting's kind (a boolean, an integer, a
/// number, a string, or a list of strings), and the value has to pass the
/// setting's own validation (its range or its allowed literals).
///
/// # Parameters
///
/// - `def`: the setting.
/// - `json`: the value a request sent.
///
/// # Returns
///
/// The setting value.
///
/// # Errors
///
/// Answers `invalid-request` naming the key when the value does not fit.
pub fn setting_value(def: &SettingDef, json: &serde_json::Value) -> Result<SettingValue, Problem> {
    let invalid = |reason: &str| Problem::invalid_request(format!("`{}`: {reason}", def.key));
    let value = match def.kind {
        SettingKind::Bool => json
            .as_bool()
            .map(SettingValue::Bool)
            .ok_or_else(|| invalid("expected a boolean"))?,
        SettingKind::Integer { .. } => json
            .as_i64()
            .map(SettingValue::Integer)
            .ok_or_else(|| invalid("expected an integer"))?,
        SettingKind::Float { .. } => json
            .as_f64()
            .map(SettingValue::Float)
            .ok_or_else(|| invalid("expected a number"))?,
        SettingKind::String { .. } => json
            .as_str()
            .map(|text| SettingValue::String(text.to_owned()))
            .ok_or_else(|| invalid("expected a string"))?,
        SettingKind::StringList { .. } => {
            let items = json
                .as_array()
                .ok_or_else(|| invalid("expected a list of strings"))?;
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(
                    item.as_str()
                        .ok_or_else(|| invalid("expected a list of strings"))?
                        .to_owned(),
                );
            }
            SettingValue::StringList(values)
        }
    };
    def.validate(&value)
        .map_err(|error| Problem::invalid_request(error.to_string()))?;
    Ok(value)
}

/// Writes a setting value as JSON.
pub fn setting_json(value: &SettingValue) -> serde_json::Value {
    serde_json::from_str(&value.to_json()).unwrap_or(serde_json::Value::Null)
}
