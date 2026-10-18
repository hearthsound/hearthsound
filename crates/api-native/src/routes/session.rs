//! `/api/v1/auth`: signing in and out with a web session.
//!
//! `POST /api/v1/auth/login` checks the account password (rate limited per
//! address and username, Argon2id) and answers with the session cookie and the
//! CSRF token the client echoes on unsafe requests. `GET /api/v1/auth/session`
//! tells a client who it is and hands the CSRF token out again, so a reloaded
//! web page can pick its session back up. `POST /api/v1/auth/logout` ends the
//! session and clears the cookie.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{SET_COOKIE, USER_AGENT};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hearthsound_auth::{Credential, LoginError};
use hearthsound_config::keys;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{
    Authed, ClientAddress, cleared_session_cookie, request_is_https, session_cookie,
};
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{UserView, now_millis};

/// Seconds in one day, for `auth.session_ttl_days`.
const SECS_PER_DAY: u64 = 86_400;

/// The credentials of a sign-in.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    /// The account's sign-in name, compared case-insensitively.
    pub username: String,
    /// The account password.
    pub password: String,
}

/// A started session.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LoginResponse {
    /// The signed-in account.
    pub user: UserView,
    /// The token to send in `X-Hearthsound-CSRF` on unsafe requests.
    pub csrf_token: String,
    /// When the session expires, in unix milliseconds.
    pub expires_at: i64,
}

/// Who a request is.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SessionResponse {
    /// The account.
    pub user: UserView,
    /// How the request authenticated: `session` or `apiKey`.
    pub credential: String,
    /// The session's CSRF token, for a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf_token: Option<String>,
    /// When the session expires, in unix milliseconds, for a session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// Signs in with the account password and starts a web session.
///
/// # Errors
///
/// Answers `unauthenticated` for a wrong username or password (or a disabled
/// account), `rate-limited` once the address or the username spent its
/// failure budget, and `internal` when the session cannot be stored.
#[utoipa::path(
    post,
    path = "/api/v1/auth/login",
    tag = "auth",
    request_body = LoginRequest,
    responses(
        (status = 200, description = "Signed in; the session cookie is set", body = LoginResponse),
        (status = 401, description = "Wrong username or password", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many failed sign-ins", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn login(
    State(state): State<NativeState>,
    ClientAddress(ip): ClientAddress,
    parts: Parts,
    JsonBody(request): JsonBody<LoginRequest>,
) -> Result<Response, Problem> {
    let user = match state
        .authenticator()
        .login(ip, &request.username, &request.password)
        .await
    {
        Ok(user) => user,
        Err(LoginError::WrongCredentials) => {
            return Err(Problem::unauthenticated("Wrong username or password."));
        }
        Err(LoginError::RateLimited) => return Err(Problem::rate_limited()),
        Err(LoginError::Internal(error)) => return Err(problem::internal("signing in", &error)),
    };
    let secure = request_is_https(
        &parts,
        &state.services().trusted_proxies,
        state.services().base_url_is_https,
    );
    let days = state
        .settings()
        .current()
        .get(keys::AUTH_SESSION_TTL_DAYS)
        .map_err(|error| problem::internal("reading auth.session_ttl_days", &error))?;
    let ttl_secs = u64::try_from(days).unwrap_or(1).max(1) * SECS_PER_DAY;
    let user_agent = parts
        .headers
        .get(USER_AGENT)
        .and_then(|value| value.to_str().ok());
    let session = state
        .sessions()
        .create(user.id, user_agent, Duration::from_secs(ttl_secs))
        .await
        .map_err(|error| problem::internal("storing a session", &error))?;
    // Expired rows are never served; dropping them here keeps the table from
    // growing without a schedule of its own.
    if let Err(error) = state.sessions().purge_expired(now_millis()).await {
        tracing::warn!(%error, "could not purge expired sessions");
    }
    tracing::info!(user = %user.id, "signed in with a web session");

    let cookie = session_cookie(
        &session.token,
        i64::try_from(ttl_secs).unwrap_or(i64::MAX),
        secure,
    )
    .ok_or_else(Problem::internal)?;
    let body = LoginResponse {
        user: UserView::from(&user),
        csrf_token: session.csrf_token,
        expires_at: session.expires_at,
    };
    Ok(([(SET_COOKIE, cookie)], Json(body)).into_response())
}

/// Ends the request's web session and clears the cookie.
///
/// A request authenticated by an API key has no session to end and is answered
/// the same way; the key stays valid.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `csrf-failed` for a cookie
/// without the CSRF header, and `internal` when the session cannot be removed.
#[utoipa::path(
    post,
    path = "/api/v1/auth/logout",
    tag = "auth",
    responses(
        (status = 204, description = "Signed out; the session cookie is cleared"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn logout(
    authed: Authed,
    State(state): State<NativeState>,
    parts: Parts,
) -> Result<Response, Problem> {
    if let Some(session) = &authed.session {
        state
            .sessions()
            .revoke(&session.token)
            .await
            .map_err(|error| problem::internal("ending a session", &error))?;
        tracing::info!(user = %authed.user_id(), "signed out of a web session");
    }
    let secure = request_is_https(
        &parts,
        &state.services().trusted_proxies,
        state.services().base_url_is_https,
    );
    Ok((
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, cleared_session_cookie(secure))],
    )
        .into_response())
}

/// Answers who the request is, and the session's CSRF token.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when the
/// account cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/auth/session",
    tag = "auth",
    responses(
        (status = 200, description = "The authenticated account", body = SessionResponse),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn session(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<SessionResponse>, Problem> {
    let user = state
        .users()
        .find_by_id(authed.user_id())
        .await
        .map_err(|error| problem::internal("reading an account", &error))?
        .ok_or_else(|| Problem::unauthenticated("The account no longer exists."))?;
    let credential = match authed.principal.credential {
        Credential::Session { .. } => "session",
        Credential::ApiKey { .. } => "apiKey",
        Credential::AppPassword { .. } => "appPassword",
        Credential::AccountPassword => "accountPassword",
    };
    Ok(Json(SessionResponse {
        user: UserView::from(&user),
        credential: credential.to_owned(),
        csrf_token: authed
            .session
            .as_ref()
            .map(|session| session.csrf_token.clone()),
        expires_at: authed.session.as_ref().map(|session| session.expires_at),
    }))
}
