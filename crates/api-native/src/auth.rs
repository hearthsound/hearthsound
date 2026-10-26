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

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::extract::OptionalFromRequestParts;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderValue, Method};
use hearthsound_auth::{
    AuthError, AuthenticationError, CSRF_HEADER, Credential, Principal, ResolveError,
    SESSION_COOKIE, TrustedProxies, session_token,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::problem::{self, Problem};
use crate::state::NativeState;

/// The header a proxy appends the address it received a request from.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// The header a proxy appends the scheme it received a request over.
const FORWARDED_PROTO: &str = "x-forwarded-proto";

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
                Err(ResolveError::Rejected(AuthenticationError::TooManyAttempts {
                    retry_after,
                })) => Err(Problem::rate_limited(retry_after)),
                Err(ResolveError::Rejected(_)) => {
                    Err(Problem::unauthenticated("The API key is not valid."))
                }
                Err(ResolveError::Internal(error)) => {
                    Err(problem::internal("resolving a bearer key", &error))
                }
            };
        }

        let Some(token) = session_token(
            parts
                .headers
                .get_all(COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok()),
        ) else {
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

impl OptionalFromRequestParts<NativeState> for Authed {
    type Rejection = Infallible;

    /// Resolves the credential like the strict extractor, and answers `None`
    /// instead of a rejection when there is none.
    ///
    /// This is for browser-facing routes that must answer a page instead of a
    /// problem document; every other route keeps the strict [`Authed`].
    async fn from_request_parts(
        parts: &mut Parts,
        state: &NativeState,
    ) -> Result<Option<Authed>, Infallible> {
        Ok(
            <Authed as FromRequestParts<NativeState>>::from_request_parts(parts, state)
                .await
                .ok(),
        )
    }
}

impl FromRequestParts<NativeState> for AdminAuthed {
    type Rejection = Problem;

    /// Authenticates the request and requires an administrator.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &NativeState,
    ) -> Result<AdminAuthed, Problem> {
        let authed =
            <Authed as FromRequestParts<NativeState>>::from_request_parts(parts, state).await?;
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

/// Re-reads the account and the credential behind a principal.
///
/// A principal is a snapshot: it carries the role and permission flags the
/// account had when the request resolved it. A long-lived connection - the
/// event stream - has to look again, because a logout, a revocation, a disable,
/// a deletion or a role change writes rows and leaves the snapshot saying what
/// used to be true.
///
/// # Parameters
///
/// - `state`: the native state, which holds the accounts, sessions, keys and app
///   passwords.
/// - `principal`: the caller as the connection resolved it.
///
/// # Returns
///
/// The principal rebuilt from the current account, so a role or permission
/// change applies at once, or `None` when the account or the credential that
/// proved it no longer authenticates.
///
/// # Errors
///
/// Returns [`AuthError`] when a read fails; a caller that cannot tell whether
/// the credential is still valid must fail closed.
pub(crate) async fn reauthorize(
    state: &NativeState,
    principal: &Principal,
) -> Result<Option<Principal>, AuthError> {
    let user_id = principal.user_id.uuid();
    let Some(user) = state.users().find_by_id(user_id).await? else {
        return Ok(None);
    };
    if user.is_disabled() {
        return Ok(None);
    }
    let credential = principal.credential;
    let active = match credential {
        Credential::Session { id } => state.sessions().is_active(id, user_id).await?,
        Credential::ApiKey { id } => state.api_keys().is_active(id, user_id).await?,
        Credential::AppPassword { id } => state.app_passwords().is_active(id, user_id).await?,
        // A `/rest` password request proves the password on every call and
        // never opens a stream, so there is no stored credential to re-check.
        Credential::AccountPassword => true,
    };
    if !active {
        return Ok(None);
    }
    Ok(Some(Principal::from_user(&user, credential)))
}

/// The address a request came from, as the login rate limit sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientAddress(pub IpAddr);

impl FromRequestParts<NativeState> for ClientAddress {
    type Rejection = std::convert::Infallible;

    /// Resolves the client address through the trusted proxies.
    async fn from_request_parts(
        parts: &mut Parts,
        state: &NativeState,
    ) -> Result<ClientAddress, std::convert::Infallible> {
        Ok(ClientAddress(client_ip(
            parts,
            &state.services().trusted_proxies,
        )))
    }
}

/// Builds the `Set-Cookie` value that stores a session token.
///
/// The cookie is `HttpOnly` (scripts cannot read it), `SameSite=Lax`, scoped to
/// the whole site, and lives as long as the session. The `Secure` attribute is
/// added only when the request arrived over HTTPS: a browser drops a `Secure`
/// cookie it received over plain HTTP, so a LAN deployment without a
/// certificate could otherwise never sign in.
///
/// # Parameters
///
/// - `token`: the session token.
/// - `max_age_secs`: the cookie's lifetime in seconds.
/// - `secure`: whether the request arrived over HTTPS
///   ([`request_is_https`]).
///
/// # Returns
///
/// The header value.
pub fn session_cookie(token: &str, max_age_secs: i64, secure: bool) -> Option<HeaderValue> {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age_secs}; HttpOnly{secure}; SameSite=Lax"
    ))
    .ok()
}

/// Builds the `Set-Cookie` value that removes the session cookie.
///
/// # Parameters
///
/// - `secure`: whether the request arrived over HTTPS; the attribute has to
///   match what the stored cookie carried, or a browser keeps the stale one.
///
/// # Returns
///
/// The header value.
pub fn cleared_session_cookie(secure: bool) -> HeaderValue {
    if secure {
        HeaderValue::from_static("hs_session=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
    } else {
        HeaderValue::from_static("hs_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
    }
}

/// Reports whether a request arrived over HTTPS.
///
/// # Parameters
///
/// - `parts`: the request head, which supplies the peer address and the
///   forwarding header.
/// - `trusted`: the proxies whose forwarding headers are believed.
/// - `base_url_is_https`: whether `server.base_url` names an `https://` address,
///   computed once at startup.
///
/// # Returns
///
/// `true` when `server.base_url` is an `https://` URL, or when the peer is a
/// trusted proxy whose first `X-Forwarded-Proto` value is `https`. An untrusted
/// peer's forwarding header is ignored, because a client can write any header
/// it likes.
pub fn request_is_https(parts: &Parts, trusted: &TrustedProxies, base_url_is_https: bool) -> bool {
    if base_url_is_https {
        return true;
    }
    let Some(peer) = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
    else {
        return false;
    };
    if !trusted.trusts(peer) {
        return false;
    }
    parts
        .headers
        .get(FORWARDED_PROTO)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"))
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
    fn the_session_cookie_is_http_only_and_lax_and_secure_only_over_https() {
        let secure = session_cookie("abc", 60, true).unwrap();
        let text = secure.to_str().unwrap();
        assert!(text.starts_with("hs_session=abc;"));
        for attribute in ["Path=/", "Max-Age=60", "HttpOnly", "Secure", "SameSite=Lax"] {
            assert!(text.contains(attribute), "{text} has {attribute}");
        }

        let plain = session_cookie("abc", 60, false).unwrap();
        let text = plain.to_str().unwrap();
        assert!(!text.contains("Secure"), "{text} must not be Secure");
        for attribute in ["Path=/", "Max-Age=60", "HttpOnly", "SameSite=Lax"] {
            assert!(text.contains(attribute), "{text} has {attribute}");
        }

        assert!(
            cleared_session_cookie(true)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
        let cleared = cleared_session_cookie(false);
        let text = cleared.to_str().unwrap();
        assert!(text.contains("Max-Age=0"), "{text} clears the cookie");
        assert!(!text.contains("Secure"), "{text} must not be Secure");
    }

    #[test]
    fn a_request_is_https_only_over_https_or_from_a_trusted_proxy() {
        let trusted = TrustedProxies::parse(["127.0.0.1/32", "::1"]).unwrap();
        let mut parts = parts_with_peer(None, &[]);
        assert!(!request_is_https(&parts, &trusted, false));

        parts = parts_with_peer(Some([127, 0, 0, 1]), &[("x-forwarded-proto", "https")]);
        assert!(request_is_https(&parts, &trusted, false));

        parts = parts_with_peer(Some([127, 0, 0, 1]), &[("x-forwarded-proto", "HTTPS")]);
        assert!(request_is_https(&parts, &trusted, false));

        parts = parts_with_peer(
            Some([127, 0, 0, 1]),
            &[("x-forwarded-proto", "https, http")],
        );
        assert!(request_is_https(&parts, &trusted, false));

        parts = parts_with_peer(Some([127, 0, 0, 1]), &[("x-forwarded-proto", "http")]);
        assert!(!request_is_https(&parts, &trusted, false));

        parts = parts_with_peer(Some([203, 0, 113, 9]), &[("x-forwarded-proto", "https")]);
        assert!(
            !request_is_https(&parts, &trusted, false),
            "an untrusted peer's header must not be believed"
        );

        let plain = parts_with_peer(None, &[]);
        assert!(request_is_https(&plain, &trusted, true));
    }

    /// Builds the request head of a request from a peer with some headers.
    fn parts_with_peer(peer: Option<[u8; 4]>, headers: &[(&str, &str)]) -> Parts {
        let mut builder = axum::http::Request::builder().uri("/api/v1/system/info");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut parts = builder
            .body(axum::body::Body::empty())
            .unwrap()
            .into_parts()
            .0;
        if let Some(peer) = peer {
            parts
                .extensions
                .insert(ConnectInfo(SocketAddr::from((peer, 5000))));
        }
        parts
    }
}
