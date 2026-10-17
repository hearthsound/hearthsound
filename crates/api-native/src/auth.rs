//! Who is calling the native API.
//!
//! A request authenticates in one of two ways:
//!
//! - `Authorization: Bearer hs_...` with an API key, the way programmatic
//!   clients call; the same key works as `apiKey=` on `/rest`.
//! - The `hs_session` cookie that `POST /api/v1/auth/login` sets, the way the
//!   web client calls. Because a browser attaches the cookie to any request,
//!   an unsafe method (anything but `GET`, `HEAD` and `OPTIONS`) authenticated
//!   by it must also carry the session's CSRF token in `X-Hearthsound-CSRF`.
//!
//! A bearer key wins when both arrive, and needs no CSRF header, because a
//! browser never attaches one on its own. Failed bearer keys count against the
//! client address's login budget, like failed passwords.

use std::net::{IpAddr, SocketAddr};

use axum::extract::FromRequestParts;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method};
use hearthsound_auth::{AuthenticationError, Credential, Principal, ResolveError, TrustedProxies};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::problem::{self, Problem};
use crate::state::NativeState;

/// The cookie that carries a web session token.
pub const SESSION_COOKIE: &str = "hs_session";

/// The header an unsafe cookie-authenticated request echoes the CSRF token in.
pub const CSRF_HEADER: &str = "x-hearthsound-csrf";

/// The header a proxy appends the address it received a request from.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// The session a request authenticated with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// The session row's id.
    pub id: Uuid,
    /// The token the cookie carried.
    pub token: String,
    /// The token unsafe requests echo in `X-Hearthsound-CSRF`.
    pub csrf_token: String,
    /// When the session expires, in unix milliseconds.
    pub expires_at: i64,
}

/// An authenticated native API request.
///
/// A handler takes it as an argument; the extraction rejects the request with
/// a problem document before the handler runs when the credentials are
/// missing, wrong, or (for a cookie) lack the CSRF header.
#[derive(Debug, Clone)]
pub struct Authed {
    /// Who is calling.
    pub principal: Principal,
    /// The session, when the request authenticated with the cookie.
    pub session: Option<SessionInfo>,
}

impl Authed {
    /// Returns the caller's account id.
    pub fn user_id(&self) -> Uuid {
        self.principal.user_id.uuid()
    }
}

/// An authenticated request by an administrator.
///
/// Rejects any other caller with a `forbidden` problem.
#[derive(Debug, Clone)]
pub struct AdminAuthed(pub Authed);

impl FromRequestParts<NativeState> for Authed {
    type Rejection = Problem;

    /// Resolves the bearer key or the session cookie.
    async fn from_request_parts(parts: &mut Parts, state: &NativeState) -> Result<Authed, Problem> {
        let ip = client_ip(parts, &state.services().trusted_proxies);
        if let Some(value) = parts.headers.get(AUTHORIZATION) {
            let key = value
                .to_str()
                .ok()
                .and_then(|text| text.strip_prefix("Bearer "))
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .ok_or_else(|| {
                    Problem::unauthenticated("The Authorization header must be `Bearer hs_...`.")
                })?;
            return match state.authenticator().resolve_bearer(ip, key).await {
                Ok(principal) => Ok(Authed {
                    principal,
                    session: None,
                }),
                Err(ResolveError::Rejected(AuthenticationError::TooManyAttempts)) => {
                    Err(Problem::rate_limited())
                }
                Err(ResolveError::Rejected(_)) => {
                    Err(Problem::unauthenticated("The API key is not valid."))
                }
                Err(ResolveError::Internal(error)) => {
                    Err(problem::internal("resolving a bearer key", &error))
                }
            };
        }

        let Some(token) = session_token(&parts.headers) else {
            return Err(Problem::unauthenticated(
                "Sign in, or send an API key as `Authorization: Bearer`.",
            ));
        };
        let record = match state.sessions().resolve(&token).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Err(Problem::unauthenticated(
                    "The session has expired or ended; sign in again.",
                ));
            }
            Err(error) => return Err(problem::internal("resolving a session", &error)),
        };
        if !is_safe(&parts.method) {
            let echoed = parts
                .headers
                .get(CSRF_HEADER)
                .map(HeaderValue::as_bytes)
                .unwrap_or_default();
            if !bool::from(echoed.ct_eq(record.csrf_token.as_bytes())) {
                return Err(Problem::csrf_failed());
            }
        }
        Ok(Authed {
            principal: Principal::from_user(&record.user, Credential::Session { id: record.id }),
            session: Some(SessionInfo {
                id: record.id,
                token,
                csrf_token: record.csrf_token,
                expires_at: record.expires_at,
            }),
        })
    }
}

impl FromRequestParts<NativeState> for AdminAuthed {
    type Rejection = Problem;

    /// Authenticates the request and requires an administrator.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &NativeState,
    ) -> Result<AdminAuthed, Problem> {
        let authed = Authed::from_request_parts(parts, state).await?;
        if !authed.principal.is_admin {
            return Err(Problem::forbidden(
                "Only an administrator may use this resource.",
            ));
        }
        Ok(AdminAuthed(authed))
    }
}

/// Returns whether a method cannot change state, and so needs no CSRF token.
fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Reads the session token from the request's cookies.
///
/// # Parameters
///
/// - `headers`: the request headers.
///
/// # Returns
///
/// The token, or `None` when no non-empty `hs_session` cookie arrived.
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, value)| *name == SESSION_COOKIE && !value.is_empty())
        .map(|(_, value)| value.to_owned())
}

/// Builds the `Set-Cookie` value that stores a session token.
///
/// The cookie is `HttpOnly` (scripts cannot read it), `Secure` (sent only over
/// HTTPS, which the reverse proxy in front of the server terminates),
/// `SameSite=Lax`, scoped to the whole site, and lives as long as the session.
///
/// # Parameters
///
/// - `token`: the session token.
/// - `max_age_secs`: the cookie's lifetime in seconds.
///
/// # Returns
///
/// The header value.
pub fn session_cookie(token: &str, max_age_secs: i64) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age_secs}; HttpOnly; Secure; SameSite=Lax"
    ))
    .ok()
}

/// Builds the `Set-Cookie` value that removes the session cookie.
pub fn cleared_session_cookie() -> HeaderValue {
    HeaderValue::from_static("hs_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// Returns the address a request came from.
///
/// The peer address comes from the connection information axum was built with,
/// and the forwarding header is trusted only when that peer is a trusted proxy.
///
/// # Parameters
///
/// - `parts`: the request head.
/// - `trusted`: the trusted proxies.
///
/// # Returns
///
/// The client address.
pub fn client_ip(parts: &Parts, trusted: &TrustedProxies) -> IpAddr {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let forwarded_for = parts
        .headers
        .get(FORWARDED_FOR)
        .and_then(|value| value.to_str().ok());
    trusted.client_ip(peer, forwarded_for)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_token_is_read_from_any_cookie_header() {
        let mut headers = HeaderMap::new();
        headers.append(COOKIE, HeaderValue::from_static("theme=dark"));
        headers.append(
            COOKIE,
            HeaderValue::from_static("a=1; hs_session=abc123; b=2"),
        );
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));

        let mut empty = HeaderMap::new();
        empty.append(COOKIE, HeaderValue::from_static("hs_session="));
        assert_eq!(session_token(&empty), None);
    }

    #[test]
    fn the_session_cookie_is_http_only_secure_and_lax() {
        let cookie = session_cookie("abc", 60).unwrap();
        let text = cookie.to_str().unwrap();
        assert!(text.starts_with("hs_session=abc;"));
        for attribute in ["Path=/", "Max-Age=60", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(text.contains(attribute), "{text} has {attribute}");
        }
        assert!(
            cleared_session_cookie()
                .to_str()
                .unwrap()
                .contains("Max-Age=0")
        );
    }
}
