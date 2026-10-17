//! Response shapes several resources share.

use hearthsound_auth::{User, UserPermissions};
use hearthsound_core::{EntityId, IdPrefix};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
