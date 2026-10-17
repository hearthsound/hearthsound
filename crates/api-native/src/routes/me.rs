//! `/api/v1/me`: the caller's own credentials.
//!
//! An API key (`hs_...`) authenticates both surfaces: `apiKey=` on `/rest` and
//! `Authorization: Bearer` here. An app password serves the legacy Subsonic
//! sign-ins (`u`+`p` and `u`+`t`+`s`). Either secret is answered exactly once,
//! when it is created; a listing shows only names, prefixes and dates, and a
//! revoked credential stops working at once.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_core::IdPrefix;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{client_id, parse_id};

/// The name a new credential is listed under.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewCredential {
    /// What the credential is for, for example the client it is given to.
    pub name: String,
}

/// An API key as a listing shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyView {
    /// The key's client id (`ak_...`).
    pub id: String,
    /// What the key is for.
    pub name: String,
    /// The key's leading characters, which identify it.
    pub prefix: String,
    /// When the key was created, in unix milliseconds.
    pub created_at: i64,
    /// When the key last authenticated, in unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

/// A key that was just created, with its secret.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedApiKey {
    /// The key as a listing shows it.
    #[serde(flatten)]
    pub key: ApiKeyView,
    /// The full key; it is shown this once and never again.
    pub secret: String,
}

/// An app password as a listing shows it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppPasswordView {
    /// The app password's client id (`ap_...`).
    pub id: String,
    /// What the app password is for.
    pub name: String,
    /// When it was created, in unix milliseconds.
    pub created_at: i64,
    /// When it last authenticated, in unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
}

/// An app password that was just created, with its secret.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreatedAppPassword {
    /// The app password as a listing shows it.
    #[serde(flatten)]
    pub password: AppPasswordView,
    /// The password to enter in the client; it is shown this once.
    pub secret: String,
}

/// Checks the name of a new credential.
fn checked_name(name: &str) -> Result<&str, Problem> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Problem::invalid_request("A credential needs a name."));
    }
    Ok(name)
}

/// Lists the caller's active API keys.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when the keys
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/me/api-keys",
    tag = "me",
    responses(
        (status = 200, description = "The caller's active API keys, newest first", body = [ApiKeyView]),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_api_keys(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<Vec<ApiKeyView>>, Problem> {
    let keys = state
        .api_keys()
        .list_for_user(authed.user_id())
        .await
        .map_err(|error| problem::internal("listing API keys", &error))?;
    Ok(Json(
        keys.into_iter()
            .filter(|key| key.revoked_at.is_none())
            .map(|key| ApiKeyView {
                id: client_id(IdPrefix::ApiKey, key.id),
                name: key.name,
                prefix: key.key_prefix,
                created_at: key.created_at,
                last_used_at: key.last_used_at,
            })
            .collect(),
    ))
}

/// Creates an API key for the caller and answers its secret once.
///
/// # Errors
///
/// Answers `invalid-request` for an empty name and `internal` when the key
/// cannot be stored.
#[utoipa::path(
    post,
    path = "/api/v1/me/api-keys",
    tag = "me",
    request_body = NewCredential,
    responses(
        (status = 201, description = "The new key, with its secret", body = CreatedApiKey),
        (status = 400, description = "No name", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_api_key(
    authed: Authed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewCredential>,
) -> Result<(StatusCode, Json<CreatedApiKey>), Problem> {
    let name = checked_name(&request.name)?;
    let generated = state
        .api_keys()
        .generate(authed.user_id(), name)
        .await
        .map_err(|error| problem::internal("creating an API key", &error))?;
    tracing::info!(user = %authed.user_id(), "created an API key");
    Ok((
        StatusCode::CREATED,
        Json(CreatedApiKey {
            key: ApiKeyView {
                id: client_id(IdPrefix::ApiKey, generated.id),
                name: name.to_owned(),
                prefix: generated.key_prefix,
                created_at: crate::views::now_millis(),
                last_used_at: None,
            },
            secret: generated.plaintext,
        }),
    ))
}

/// Revokes one of the caller's API keys.
///
/// # Errors
///
/// Answers `not-found` for a key that is not the caller's or is already
/// revoked, and `internal` when the key cannot be updated.
#[utoipa::path(
    delete,
    path = "/api/v1/me/api-keys/{id}",
    tag = "me",
    params(("id" = String, Path, description = "The key's client id (`ak_...`)")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, description = "No such key of the caller's", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn revoke_api_key(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::ApiKey)?;
    let owned = state
        .api_keys()
        .list_for_user(authed.user_id())
        .await
        .map_err(|error| problem::internal("listing API keys", &error))?
        .into_iter()
        .any(|key| key.id == id && key.revoked_at.is_none());
    if !owned {
        return Err(Problem::not_found("The caller has no such active API key."));
    }
    state
        .api_keys()
        .revoke(id)
        .await
        .map_err(|error| problem::internal("revoking an API key", &error))?;
    tracing::info!(user = %authed.user_id(), "revoked an API key");
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the caller's active app passwords.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when the app
/// passwords cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/me/app-passwords",
    tag = "me",
    responses(
        (status = 200, description = "The caller's active app passwords, newest first", body = [AppPasswordView]),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_app_passwords(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<Vec<AppPasswordView>>, Problem> {
    let passwords = state
        .app_passwords()
        .list_for_user(authed.user_id())
        .await
        .map_err(|error| problem::internal("listing app passwords", &error))?;
    Ok(Json(
        passwords
            .into_iter()
            .filter(|password| password.revoked_at.is_none())
            .map(|password| AppPasswordView {
                id: client_id(IdPrefix::AppPassword, password.id),
                name: password.name,
                created_at: password.created_at,
                last_used_at: password.last_used_at,
            })
            .collect(),
    ))
}

/// Creates an app password for the caller and answers it once.
///
/// # Errors
///
/// Answers `invalid-request` for an empty name and `internal` when the app
/// password cannot be stored.
#[utoipa::path(
    post,
    path = "/api/v1/me/app-passwords",
    tag = "me",
    request_body = NewCredential,
    responses(
        (status = 201, description = "The new app password, with its secret", body = CreatedAppPassword),
        (status = 400, description = "No name", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_app_password(
    authed: Authed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewCredential>,
) -> Result<(StatusCode, Json<CreatedAppPassword>), Problem> {
    let name = checked_name(&request.name)?;
    let generated = state
        .app_passwords()
        .generate(authed.user_id(), name)
        .await
        .map_err(|error| problem::internal("creating an app password", &error))?;
    tracing::info!(user = %authed.user_id(), "created an app password");
    Ok((
        StatusCode::CREATED,
        Json(CreatedAppPassword {
            password: AppPasswordView {
                id: client_id(IdPrefix::AppPassword, generated.id),
                name: name.to_owned(),
                created_at: crate::views::now_millis(),
                last_used_at: None,
            },
            secret: generated.plaintext,
        }),
    ))
}

/// Revokes one of the caller's app passwords.
///
/// # Errors
///
/// Answers `not-found` for an app password that is not the caller's or is
/// already revoked, and `internal` when it cannot be updated.
#[utoipa::path(
    delete,
    path = "/api/v1/me/app-passwords/{id}",
    tag = "me",
    params(("id" = String, Path, description = "The app password's client id (`ap_...`)")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 404, description = "No such app password of the caller's", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn revoke_app_password(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::AppPassword)?;
    let owned = state
        .app_passwords()
        .list_for_user(authed.user_id())
        .await
        .map_err(|error| problem::internal("listing app passwords", &error))?
        .into_iter()
        .any(|password| password.id == id && password.revoked_at.is_none());
    if !owned {
        return Err(Problem::not_found(
            "The caller has no such active app password.",
        ));
    }
    state
        .app_passwords()
        .revoke(id)
        .await
        .map_err(|error| problem::internal("revoking an app password", &error))?;
    tracing::info!(user = %authed.user_id(), "revoked an app password");
    Ok(StatusCode::NO_CONTENT)
}
