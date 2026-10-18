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

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hearthsound_auth::{NewUser, User};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::{ClientAddress, request_is_https};
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::routes::session::{LoginResponse, start_session};
use crate::state::NativeState;

/// The username the setup failures are charged against in the login rate
/// limit.
///
/// The token is compared against a secret, so it is guessed like a password;
/// counting the attempts against one key stops a token from being walked
/// through the endpoint no matter which address the attempts come from, and
/// contributes to the address's budget as every login failure does.
const SETUP_USERNAME: &str = "setup";

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

/// Creates the first administrator from the setup link.
///
/// # Errors
///
/// Answers `conflict` when an account already exists, `invalid-request` when
/// the username or the password is empty, `rate-limited` once the setup
/// failures spent the budget of the client address or of the `setup` key,
/// `unauthenticated` when the token is not the one the server logged, and
/// `internal` when the account cannot be created.
#[utoipa::path(
    post,
    path = "/api/v1/setup",
    tag = "auth",
    request_body = SetupRequest,
    responses(
        (status = 201, description = "The administrator exists and is signed in", body = LoginResponse),
        (status = 400, description = "The username or the password is empty", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "The setup link is not valid", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The server already has an account", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "Too many failed setup attempts", body = Problem, content_type = "application/problem+json"),
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

    let limiter = state.authenticator().limiter();
    if limiter.check(ip, SETUP_USERNAME).is_err() {
        return Err(Problem::rate_limited());
    }
    if !state.setup().consume(&request.token).await {
        limiter.record_failure(ip, SETUP_USERNAME);
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
    let user = create_administrator(&state, &request, display_name).await?;
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
    }
}
