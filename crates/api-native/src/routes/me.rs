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
use hearthsound_auth::{AuthError, validate_password, verify_password};
use hearthsound_core::IdPrefix;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{Authed, ClientAddress};
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

/// Maps a credential write failure to its problem.
///
/// The name rules are the crate's, so every surface - this route, the CLI and any
/// later caller - refuses the same names with the same sentence.
fn credential_problem(context: &str, error: hearthsound_auth::AuthError) -> Problem {
    match error {
        hearthsound_auth::AuthError::EmptyField { .. } => {
            Problem::invalid_request("A credential name must not be empty.")
        }
        hearthsound_auth::AuthError::InvalidCredentialName { rule } => {
            Problem::invalid_request(format!("The credential name is not allowed: {rule}."))
        }
        error => problem::internal(context, &error),
    }
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
        (status = 400, description = "The name is empty or longer than 64 characters", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_api_key(
    authed: Authed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewCredential>,
) -> Result<(StatusCode, Json<CreatedApiKey>), Problem> {
    let name = hearthsound_auth::validate_credential_name(&request.name)
        .map_err(|error| credential_problem("validating an API key name", error))?;
    let generated = state
        .api_keys()
        .generate(authed.user_id(), name)
        .await
        .map_err(|error| credential_problem("creating an API key", error))?;
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
        (status = 400, description = "The name is empty or longer than 64 characters", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_app_password(
    authed: Authed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewCredential>,
) -> Result<(StatusCode, Json<CreatedAppPassword>), Problem> {
    let name = hearthsound_auth::validate_credential_name(&request.name)
        .map_err(|error| credential_problem("validating an app password name", error))?;
    let generated = state
        .app_passwords()
        .generate(authed.user_id(), name)
        .await
        .map_err(|error| credential_problem("creating an app password", error))?;
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

/// The caller's password change.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PasswordChange {
    /// The account's current password.
    pub current_password: String,
    /// The password to store; the rules are the ones every password write keeps.
    pub new_password: String,
}

/// Changes the caller's own password.
///
/// The current password has to be presented, so a stolen session cannot lock the
/// owner out. A successful change ends every *other* session of the account and
/// revokes every API key and app password of it; the session that made the change
/// - present only for a cookie-authenticated call - keeps working. A change made
/// with an API key keeps no session, so the key itself is revoked too: an
/// API-key client has to create a new key afterwards.
///
/// The attempt is rate limited against the caller's address only. Charging the
/// username as well would let a stolen session spend the owner's sign-in budget
/// and keep them out of the login form.
///
/// # Errors
///
/// Answers `invalid-request` when the new password breaks the length rule,
/// `rate-limited` when the address has spent its failure budget (with
/// `Retry-After`), `unauthenticated` when the caller has no credential or the
/// account is gone or disabled, `wrong-password` when the current password does
/// not match, and `internal` when the write fails.
#[utoipa::path(
    post,
    path = "/api/v1/me/password",
    tag = "me",
    request_body = PasswordChange,
    responses(
        (status = 204, description = "The password changed; every other credential of the account is revoked"),
        (status = 400, description = "The new password is shorter than 8 characters", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The current password does not match", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many attempts from this address", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn change_password(
    authed: Authed,
    ClientAddress(ip): ClientAddress,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<PasswordChange>,
) -> Result<StatusCode, Problem> {
    if let Err(error) = validate_password(&request.new_password) {
        return Err(match error {
            AuthError::InvalidPassword { rule } => {
                Problem::invalid_request(format!("The password is not allowed: {rule}."))
            }
            error => problem::internal("validating a password", &error),
        });
    }
    let limiter = state.authenticator().limiter();
    if let Err(limited) = limiter.check_address(ip) {
        return Err(Problem::rate_limited(limited.retry_after));
    }
    let user_id = authed.user_id();
    let user = match state.users().find_by_id(user_id).await {
        Ok(Some(user)) if !user.is_disabled() => user,
        Ok(_) => {
            return Err(Problem::unauthenticated(
                "The account this credential belongs to is no longer usable.",
            ));
        }
        Err(error) => return Err(problem::internal("reading an account", &error)),
    };
    let matches = verify_password(&request.current_password, user.password_hash())
        .await
        .map_err(|error| problem::internal("verifying a password", &error))?;
    if !matches {
        limiter.record_address_failure(ip);
        return Err(Problem::wrong_password());
    }
    let revoked = state
        .users()
        .set_password(
            user_id,
            &request.new_password,
            authed.session.as_ref().map(|session| session.id),
        )
        .await
        .map_err(|error| problem::internal("changing a password", &error))?;
    tracing::info!(
        user = %user_id,
        sessions = revoked.sessions,
        api_keys = revoked.api_keys,
        app_passwords = revoked.app_passwords,
        "changed a password and revoked credentials"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// One setting a user may override, with its effective value for the caller.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersonalSetting {
    /// The dotted setting key.
    pub key: String,
    /// The value in effect for the caller.
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
    /// Where the value comes from: `user` (the caller's override) or `server`.
    pub source: String,
    /// The server-wide value, which applies when the caller has no override.
    #[schema(value_type = Object)]
    pub server_value: serde_json::Value,
    /// What the setting does.
    pub description: String,
}

/// The settings a user may override.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PersonalSettings {
    /// One entry per overridable setting, by key.
    pub settings: Vec<PersonalSetting>,
}

/// A batch of personal setting changes.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PersonalSettingsPatch {
    /// The new value per key; `null` removes the override.
    #[schema(value_type = Object)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

/// Lists the settings the caller may override, with the caller's values.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when a value
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/me/settings",
    tag = "me",
    responses(
        (status = 200, description = "The caller's overridable settings", body = PersonalSettings),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn get_settings(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<PersonalSettings>, Problem> {
    personal_settings(&state, authed.user_id()).await.map(Json)
}

/// Changes the caller's overrides; `null` removes one.
///
/// Every change is checked before any is written, so a batch with one bad
/// entry changes nothing.
///
/// # Errors
///
/// Answers `invalid-request` for an unknown key, a key users may not
/// override, or a value that does not fit the setting.
#[utoipa::path(
    patch,
    path = "/api/v1/me/settings",
    tag = "me",
    request_body = PersonalSettingsPatch,
    responses(
        (status = 200, description = "The caller's overridable settings after the change", body = PersonalSettings),
        (status = 400, description = "An unknown, fixed, or invalid setting", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_settings(
    authed: Authed,
    State(state): State<NativeState>,
    JsonBody(patch): JsonBody<PersonalSettingsPatch>,
) -> Result<Json<PersonalSettings>, Problem> {
    let registry = state.settings().registry();
    let mut changes = Vec::with_capacity(patch.settings.len());
    for (key, value) in &patch.settings {
        let def = registry
            .def(key)
            .ok_or_else(|| Problem::invalid_request(format!("`{key}` is not a setting.")))?;
        if !def.user_overridable {
            return Err(Problem::invalid_request(format!(
                "`{key}` is a server setting; users cannot override it."
            )));
        }
        let value = if value.is_null() {
            None
        } else {
            Some(crate::views::setting_value(def, value)?)
        };
        changes.push((key.clone(), value));
    }
    let user = authed.user_id();
    for (key, value) in changes {
        let written = match value {
            Some(value) => state.user_settings().set(user, &key, value).await,
            None => state.user_settings().unset(user, &key).await,
        };
        written.map_err(|error| problem::internal("writing a personal setting", &error))?;
    }
    personal_settings(&state, user).await.map(Json)
}

/// Reads the overridable settings with one user's values.
///
/// # Errors
///
/// Answers `internal` when a value cannot be read.
async fn personal_settings(
    state: &NativeState,
    user: uuid::Uuid,
) -> Result<PersonalSettings, Problem> {
    let snapshot = state.settings().current();
    let overrides = state
        .user_settings()
        .list(user)
        .await
        .map_err(|error| problem::internal("reading personal settings", &error))?;
    let mut settings: Vec<PersonalSetting> = state
        .settings()
        .registry()
        .iter()
        .filter(|def| def.user_overridable)
        .map(|def| {
            let server_value = snapshot
                .value(def.key)
                .map(crate::views::setting_json)
                .unwrap_or_else(|| crate::views::setting_json(&def.default));
            let own = overrides.iter().find(|entry| entry.key == def.key);
            PersonalSetting {
                key: def.key.to_owned(),
                value: own
                    .map(|entry| crate::views::setting_json(&entry.value))
                    .unwrap_or_else(|| server_value.clone()),
                source: if own.is_some() { "user" } else { "server" }.to_owned(),
                server_value,
                description: def.description.to_owned(),
            }
        })
        .collect();
    settings.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(PersonalSettings { settings })
}
