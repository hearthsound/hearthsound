//! `/api/v1/admin/users`: accounts, their permissions and their quotas.
//!
//! An administrator creates accounts with an explicit permission set (or the
//! `users.default_permissions` one), sets daily acquisition quotas, changes a
//! password, and disables or re-enables an account; a disabled account keeps
//! its rows but cannot authenticate, and its web sessions end at once. An
//! administrator cannot take their own administrator flag away or disable
//! themselves, so a server never loses its last way in by accident.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_auth::{AuthError, NewUser, User, UserQuota, UserUpdate};
use hearthsound_core::IdPrefix;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::AdminAuthed;
use crate::extract::{JsonBody, QueryParams};
use crate::pagination::{Cursor, PageParams, page};
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{PermissionsView, UserView, parse_id};

/// A daily acquisition quota; an absent field is unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaView {
    /// How many acquisitions the account may start in 24 hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisitions_per_day: Option<u32>,
    /// How many bytes of acquired files the account may add in 24 hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition_bytes_per_day: Option<u64>,
}

impl From<UserQuota> for QuotaView {
    fn from(quota: UserQuota) -> QuotaView {
        QuotaView {
            acquisitions_per_day: quota.acquisitions_per_day,
            acquisition_bytes_per_day: quota.acquisition_bytes_per_day,
        }
    }
}

impl From<QuotaView> for UserQuota {
    fn from(view: QuotaView) -> UserQuota {
        UserQuota {
            acquisitions_per_day: view.acquisitions_per_day,
            acquisition_bytes_per_day: view.acquisition_bytes_per_day,
        }
    }
}

/// An account as an administrator sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AdminUser {
    /// The account.
    #[serde(flatten)]
    pub user: UserView,
    /// The account's daily acquisition quota.
    pub quota: QuotaView,
}

/// One page of accounts.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserPage {
    /// The accounts, by username.
    pub users: Vec<AdminUser>,
    /// The cursor of the next page; absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// A new account.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewUserRequest {
    /// The sign-in name, unique case-insensitively.
    pub username: String,
    /// The name clients show; the username when absent or empty.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The account password.
    pub password: String,
    /// Whether the account administers the server.
    #[serde(default)]
    pub is_admin: bool,
    /// The permissions; `users.default_permissions` when absent.
    #[serde(default)]
    pub permissions: Option<PermissionsView>,
    /// The daily acquisition quota; unlimited when absent.
    #[serde(default)]
    pub quota: Option<QuotaView>,
}

/// Changes to an account; an absent field is left as it is.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserPatch {
    /// The new display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The new password.
    #[serde(default)]
    pub password: Option<String>,
    /// Whether the account administers the server.
    #[serde(default)]
    pub is_admin: Option<bool>,
    /// The new permissions.
    #[serde(default)]
    pub permissions: Option<PermissionsView>,
    /// The new quota, which replaces the old one entirely.
    #[serde(default)]
    pub quota: Option<QuotaView>,
    /// `true` disables the account, `false` enables it again.
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// Maps an account write failure to its problem.
fn write_problem(context: &str, error: AuthError) -> Problem {
    match error {
        AuthError::EmptyField { field } => {
            Problem::invalid_request(format!("`{field}` must not be empty."))
        }
        AuthError::InvalidPassword { rule } => {
            Problem::invalid_request(format!("The password is not allowed: {rule}."))
        }
        AuthError::InvalidCredentialName { rule } => {
            Problem::invalid_request(format!("The credential name is not allowed: {rule}."))
        }
        AuthError::InvalidUsername { username, rule } => {
            Problem::invalid_request(format!("The username `{username}` is not allowed: {rule}."))
        }
        AuthError::UsernameTaken { username } => {
            Problem::conflict(format!("The username `{username}` is taken."))
        }
        AuthError::LastAdministrator => {
            Problem::conflict("The last active administrator cannot be disabled or demoted.")
        }
        AuthError::UnknownUser { .. } => Problem::not_found("No such account."),
        AuthError::QuotaOutOfRange { .. } => {
            Problem::invalid_request("A quota value is larger than the server can store.")
        }
        error => problem::internal(context, &error),
    }
}

/// Deletes an account.
///
/// The account's sessions, keys, playlists, feedback, ratings and quota go with
/// it through the foreign keys; a folder it owned becomes generic and the files
/// it uploaded keep their rows without an uploader. The files on disk are not
/// touched.
///
/// # Errors
///
/// Answers `not-found` for an unknown account, and `conflict` when the caller
/// would delete their own account.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/users/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The account's client id (`us_...`)")),
    responses(
        (status = 204, description = "The account is gone"),
        (status = 404, description = "No such account", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The deletion would lock the server out", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_user(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::User)?;
    existing(&state, id).await?;
    if id == admin.user_id() {
        return Err(Problem::conflict(
            "Administrators cannot delete their own account.",
        ));
    }
    let deleted = state
        .users()
        .delete(id)
        .await
        .map_err(|error| problem::internal("deleting an account", &error))?;
    if !deleted {
        return Err(Problem::not_found("No such account."));
    }
    tracing::info!(admin = %admin.user_id(), user = %id, "deleted an account");
    Ok(StatusCode::NO_CONTENT)
}

/// Builds the administrator's view of one account.
async fn admin_user(state: &NativeState, user: &User) -> Result<AdminUser, Problem> {
    let quota = state
        .quotas()
        .get(user.id)
        .await
        .map_err(|error| problem::internal("reading a quota", &error))?;
    Ok(AdminUser {
        user: UserView::from(user),
        quota: quota.into(),
    })
}

/// Reads one account or answers `not-found`.
async fn existing(state: &NativeState, id: Uuid) -> Result<User, Problem> {
    state
        .users()
        .find_by_id(id)
        .await
        .map_err(|error| problem::internal("reading an account", &error))?
        .ok_or_else(|| Problem::not_found("No such account."))
}

/// Lists the accounts, a page at a time, by username.
///
/// # Errors
///
/// Answers `forbidden` for a caller who is not an administrator and
/// `invalid-request` for a cursor this server did not issue.
#[utoipa::path(
    get,
    path = "/api/v1/admin/users",
    tag = "admin",
    params(PageParams),
    responses(
        (status = 200, description = "One page of accounts", body = UserPage),
        (status = 403, description = "Not an administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_users(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    QueryParams(params): QueryParams<PageParams>,
) -> Result<Json<UserPage>, Problem> {
    let mut users = state
        .users()
        .list()
        .await
        .map_err(|error| problem::internal("listing accounts", &error))?;
    let position = |user: &User| {
        Cursor::new(
            user.username.to_lowercase(),
            crate::views::client_id(IdPrefix::User, user.id),
        )
    };
    users.sort_by_key(|user| {
        let cursor = position(user);
        (cursor.key, cursor.id)
    });
    let (users, next_cursor) = page(users, &params, position)?;
    let mut views = Vec::with_capacity(users.len());
    for user in &users {
        views.push(admin_user(&state, user).await?);
    }
    Ok(Json(UserPage {
        users: views,
        next_cursor,
    }))
}

/// Creates an account.
///
/// # Errors
///
/// Answers `invalid-request` for an empty username or password, `conflict`
/// for a taken username, and `forbidden` for a caller who is not an
/// administrator.
#[utoipa::path(
    post,
    path = "/api/v1/admin/users",
    tag = "admin",
    request_body = NewUserRequest,
    responses(
        (status = 201, description = "The new account", body = AdminUser),
        (status = 400, description = "An empty username or password", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The username is taken", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_user(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewUserRequest>,
) -> Result<(StatusCode, Json<AdminUser>), Problem> {
    let display_name = request
        .display_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| request.username.clone());
    let mut new = NewUser::new(
        request.username.trim(),
        display_name,
        request.password.clone(),
        request.is_admin,
    );
    if let Some(permissions) = request.permissions {
        new = new.with_permissions(permissions.into());
    }
    let user = state
        .users()
        .create(&state.settings().current(), &new)
        .await
        .map_err(|error| write_problem("creating an account", error))?;
    if let Some(quota) = request.quota {
        state
            .quotas()
            .set(user.id, &quota.into())
            .await
            .map_err(|error| write_problem("setting a quota", error))?;
    }
    tracing::info!(admin = %admin.user_id(), user = %user.id, "created an account");
    Ok((StatusCode::CREATED, Json(admin_user(&state, &user).await?)))
}

/// Reads one account.
///
/// # Errors
///
/// Answers `not-found` for an unknown account.
#[utoipa::path(
    get,
    path = "/api/v1/admin/users/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The account's client id (`us_...`)")),
    responses(
        (status = 200, description = "The account", body = AdminUser),
        (status = 404, description = "No such account", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn get_user(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<Json<AdminUser>, Problem> {
    let user = existing(&state, parse_id(&id, IdPrefix::User)?).await?;
    Ok(Json(admin_user(&state, &user).await?))
}

/// Changes an account.
///
/// A new password revokes the account's credentials: every session except the
/// caller's own when an administrator changes their own password, plus every
/// API key and app password of the account.
///
/// # Errors
///
/// Answers `not-found` for an unknown account, `invalid-request` for an empty
/// password or one shorter than 8 characters, and `conflict` when the change
/// would leave no active administrator, or when administrators would remove
/// their own administrator flag or disable themselves.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/users/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The account's client id (`us_...`)")),
    request_body = UserPatch,
    responses(
        (status = 200, description = "The account after the change", body = AdminUser),
        (status = 400, description = "The password is empty or shorter than 8 characters", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such account", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The change would leave no active administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_user(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    JsonBody(patch): JsonBody<UserPatch>,
) -> Result<Json<AdminUser>, Problem> {
    let id = parse_id(&id, IdPrefix::User)?;
    existing(&state, id).await?;
    if id == admin.user_id() {
        if patch.is_admin == Some(false) {
            return Err(Problem::conflict(
                "Administrators cannot remove their own administrator flag.",
            ));
        }
        if patch.disabled == Some(true) {
            return Err(Problem::conflict(
                "Administrators cannot disable their own account.",
            ));
        }
    }
    let mut update = UserUpdate::default();
    if let Some(name) = &patch.display_name {
        update = update.with_display_name(name.clone());
    }
    if let Some(is_admin) = patch.is_admin {
        update = update.with_is_admin(is_admin);
    }
    if let Some(permissions) = patch.permissions {
        update = update.with_permissions(permissions.into());
    }
    let users = state.users();
    users
        .update(id, &update)
        .await
        .map_err(|error| write_problem("changing an account", error))?;
    if let Some(password) = &patch.password {
        // An administrator changing their own password through this route is a
        // self-change in every meaningful sense, so the session that asked for
        // it is kept and the page they are using keeps working. Resetting
        // somebody else's password keeps nothing.
        let keep = if id == admin.user_id() {
            admin.session.as_ref().map(|session| session.id)
        } else {
            None
        };
        let revoked = users
            .set_password(id, password, keep)
            .await
            .map_err(|error| write_problem("changing a password", error))?;
        tracing::info!(
            admin = %admin.user_id(),
            user = %id,
            sessions = revoked.sessions,
            api_keys = revoked.api_keys,
            app_passwords = revoked.app_passwords,
            "changed a password and revoked credentials"
        );
    }
    if let Some(quota) = patch.quota {
        state
            .quotas()
            .set(id, &quota.into())
            .await
            .map_err(|error| write_problem("setting a quota", error))?;
    }
    match patch.disabled {
        Some(true) => {
            users
                .disable(id)
                .await
                .map_err(|error| write_problem("disabling an account", error))?;
            state
                .sessions()
                .revoke_all(id)
                .await
                .map_err(|error| problem::internal("ending sessions", &error))?;
        }
        Some(false) => users
            .enable(id)
            .await
            .map_err(|error| write_problem("enabling an account", error))?,
        None => {}
    }
    tracing::info!(admin = %admin.user_id(), user = %id, "changed an account");
    let user = existing(&state, id).await?;
    Ok(Json(admin_user(&state, &user).await?))
}
