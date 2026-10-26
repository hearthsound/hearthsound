//! `/api/v1/setup`: the one-time link that creates the first administrator.
//!
//! A server whose users table is empty has no way in: every administrative
//! route needs an account, and creating one needs an administrator. The
//! serving process therefore logs a URL that carries a random token
//! ([`hearthsound_auth::SetupGate`]), and these two routes are the only ones
//! that answer without a credential.
//!
//! `GET /api/v1/setup` reports whether the setup is still open.
//! `POST /api/v1/setup` consumes the token once and creates the administrator,
//! answering exactly what a sign-in answers, so the newly created account is
//! signed in immediately.
//! `POST /api/v1/setup/check` validates a link without spending it, so the
//! wizard can tell a stale link from a live one before it draws its form.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hearthsound_auth::{AuthError, NewUser, User, validate_password, validate_username};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{ClientAddress, request_is_https};
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::routes::session::{LoginResponse, start_session};
use crate::state::NativeState;

/// Whether the server still needs its first administrator.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    /// `true` while the users table is empty.
    pub required: bool,
}

/// The one-time link and the administrator it creates.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupRequest {
    /// The token from the setup URL's query string.
    pub token: String,
    /// The administrator's sign-in name.
    pub username: String,
    /// The name to display; the username is used when it is absent.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The account password.
    pub password: String,
}

/// The link a caller wants checked before the wizard shows its form.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupCheck {
    /// The token the setup link carried.
    pub token: String,
}

/// Reports whether the server still needs its first administrator.
///
/// # Errors
///
/// Answers `internal` when the users table cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/setup",
    tag = "auth",
    responses(
        (status = 200, description = "Whether the setup is still open", body = SetupStatus),
    )
)]
pub async fn setup_status(State(state): State<NativeState>) -> Result<Json<SetupStatus>, Problem> {
    Ok(Json(SetupStatus {
        required: users_are_missing(&state).await?,
    }))
}

/// Reports whether a setup link is still the live one.
///
/// The check never spends the token, so the wizard can ask before it shows the
/// form. It is charged to the calling address exactly like a setup attempt, so
/// a token cannot be walked through it.
///
/// # Errors
///
/// Answers `conflict` when an account already exists, `rate-limited` once the
/// client address spent its budget, `invalid-setup-token` when the token is not
/// the one the server logged, and `internal` when the users table cannot be
/// read.
#[utoipa::path(
    post,
    path = "/api/v1/setup/check",
    tag = "auth",
    request_body = SetupCheck,
    responses(
        (status = 204, description = "The link is live"),
        (status = 403, description = "The link is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The server already has an account", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many failed attempts from this address", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn check_setup_token(
    State(state): State<NativeState>,
    ClientAddress(ip): ClientAddress,
    JsonBody(request): JsonBody<SetupCheck>,
) -> Result<StatusCode, Problem> {
    if !users_are_missing(&state).await? {
        return Err(Problem::conflict("Setup is already complete."));
    }
    // Only the address is charged, as `POST /api/v1/setup` charges it: a
    // per-token key would let a stranger's guesses spend the budget the
    // operator needs to get into a fresh server.
    let limiter = state.authenticator().limiter();
    if let Err(limited) = limiter.check_address(ip) {
        return Err(Problem::rate_limited(limited.retry_after));
    }
    if !state.setup().matches(&request.token).await {
        limiter.record_address_failure(ip);
        return Err(Problem::invalid_setup_token());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Creates the first administrator from the setup link.
///
/// The link is spent when the administrator is created. A failure that leaves
/// no account restores it, so the wizard's retry works; a failure after the
/// account exists needs no link, because the account can sign in. The username
/// and the password rules are checked before the spend, so a typo costs the
/// wizard nothing.
///
/// # Errors
///
/// Answers `conflict` when an account already exists, `invalid-request` when
/// the username or the password is empty or breaks its rule (a password is at
/// least 8 characters long), `rate-limited` once the client address spent its
/// failure budget, `unauthenticated` when the token is not the one the server
/// logged, and `internal` when the account cannot be created.
#[utoipa::path(
    post,
    path = "/api/v1/setup",
    tag = "auth",
    request_body = SetupRequest,
    responses(
        (status = 201, description = "The administrator exists and is signed in", body = LoginResponse),
        (status = 400, description = "The username or the password is empty, or it breaks its rule (a password is at least 8 characters long)", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "The setup link is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The server already has an account", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many failed setup attempts from this address", body = Problem, content_type = "application/problem+json"),
    )
)]
pub async fn complete_setup(
    State(state): State<NativeState>,
    ClientAddress(ip): ClientAddress,
    parts: Parts,
    JsonBody(request): JsonBody<SetupRequest>,
) -> Result<Response, Problem> {
    if !users_are_missing(&state).await? {
        return Err(Problem::conflict("Setup is already complete."));
    }
    if request.username.trim().is_empty() || request.password.is_empty() {
        return Err(Problem::invalid_request(
            "The setup needs a username and a password.",
        ));
    }
    // The name and the password rules are checked before the one-time link is
    // spent, so a typo costs the wizard nothing.
    if let Err(AuthError::InvalidUsername { rule, .. }) = validate_username(request.username.trim())
    {
        return Err(Problem::invalid_request(format!(
            "The username is not allowed: {rule}."
        )));
    }
    // The empty password is refused above, so only the length rule is left.
    if let Err(AuthError::InvalidPassword { rule }) = validate_password(&request.password) {
        return Err(Problem::invalid_request(format!(
            "The password is not allowed: {rule}."
        )));
    }

    // Only the address is charged: the token is 52 random base32 characters, so
    // walking it is pointless, and a shared key would let any stranger spend the
    // budget the operator needs to get into a fresh server.
    let limiter = state.authenticator().limiter();
    if let Err(limited) = limiter.check_address(ip) {
        return Err(Problem::rate_limited(limited.retry_after));
    }
    if !state.setup().consume(&request.token).await {
        limiter.record_address_failure(ip);
        return Err(Problem::unauthenticated("The setup link is not valid."));
    }

    // The token is spent, so this is the only request that got here; the check
    // is repeated because the command line can create an account while the
    // request is in flight.
    if !users_are_missing(&state).await? {
        return Err(Problem::conflict("Setup is already complete."));
    }

    let display_name = request
        .display_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&request.username);
    let user = match create_administrator(&state, &request, display_name).await {
        Ok(user) => user,
        Err(problem) => {
            // No account exists, so the link is the only way in and the
            // operator's retry has to work without restarting the server. The
            // token is never logged.
            if state.setup().restore(&request.token).await {
                tracing::warn!(
                    "the first administrator could not be created; the setup link is usable again"
                );
            }
            return Err(problem);
        }
    };
    tracing::info!(user = %user.id, "created the first administrator");

    let secure = request_is_https(
        &parts,
        &state.services().trusted_proxies,
        state.services().base_url_is_https,
    );
    let user_agent = parts
        .headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok());
    // The account exists now, so a failure here needs no link: signing in with
    // the password the wizard chose is the recovery.
    let (body, cookie) = start_session(&state, &user, user_agent, secure).await?;
    Ok((StatusCode::CREATED, [(SET_COOKIE, cookie)], Json(body)).into_response())
}

/// Reports whether the users table is empty.
///
/// # Errors
///
/// Answers `internal` when the table cannot be read.
async fn users_are_missing(state: &NativeState) -> Result<bool, Problem> {
    let users = state
        .users()
        .list()
        .await
        .map_err(|error| problem::internal("reading the accounts", &error))?;
    Ok(users.is_empty())
}

/// Creates the administrator the setup request describes.
///
/// The account is an administrator with the server's default permissions,
/// which is what the wizard goes on to change.
///
/// # Errors
///
/// Answers `invalid-request` for an empty field, `conflict` when another
/// account took the username in the meantime, and `internal` when the insert
/// fails.
async fn create_administrator(
    state: &NativeState,
    request: &SetupRequest,
    display_name: &str,
) -> Result<User, Problem> {
    let new = NewUser::new(
        request.username.trim(),
        display_name,
        request.password.as_str(),
        true,
    );
    let settings = state.settings().current();
    match state.users().create(&settings, &new).await {
        Ok(user) => Ok(user),
        Err(error @ hearthsound_auth::AuthError::EmptyField { .. }) => Err(problem::internal(
            "creating the first administrator",
            &error,
        )),
        Err(hearthsound_auth::AuthError::UsernameTaken { .. }) => {
            Err(Problem::conflict("That username is already taken."))
        }
        Err(error) => Err(problem::internal(
            "creating the first administrator",
            &error,
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_document_describes_the_setup_routes() {
        let document = crate::openapi();
        assert!(
            document.paths.paths.contains_key("/api/v1/setup"),
            "the setup path is in the document"
        );
        let post = document.paths.paths["/api/v1/setup"]
            .post
            .as_ref()
            .expect("the setup route is registered");
        assert_eq!(post.tags, Some(vec!["auth".to_owned()]));
        assert!(post.security.is_none(), "the setup route is public");

        let check = document.paths.paths["/api/v1/setup/check"]
            .post
            .as_ref()
            .expect("the setup check route is registered");
        assert_eq!(check.tags, Some(vec!["auth".to_owned()]));
        assert!(check.security.is_none(), "the setup check route is public");
    }
}
