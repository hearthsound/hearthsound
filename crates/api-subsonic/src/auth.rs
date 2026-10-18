//! Who is calling: the address a request came from and its credentials.
//!
//! [`Call`] is the extractor a normal endpoint takes. It reads the request's
//! parameters, resolves the credential parameters into a
//! [`Principal`](hearthsound_auth::Principal), and answers a failed check with
//! a failed envelope at HTTP 200, which is what the base protocol mandates.
//! [`BinaryCall`] is the extractor the binary endpoints (`stream`, `download`,
//! `getCoverArt`) take: its rejection is an HTTP status code instead, because a
//! client that treats any 200 body as audio would otherwise play the error
//! document as noise.
//!
//! The address is the peer address from the connection, unless the peer is one
//! of `server.trusted_proxies` and sent an `X-Forwarded-For` header, in which
//! case the header names the client. An untrusted peer's forwarding header is
//! ignored, because a client can write any header it likes.
//!
//! A call that carries none of `u`, `p`, `t`, `s` and `apiKey` may instead
//! arrive with the `hs_session` cookie a web client signed in with. An
//! envelope call must then echo the session's CSRF token in
//! `X-Hearthsound-CSRF`; a binary call cannot send a header from a media
//! element and is accepted without one, unless a browser labels it
//! `Sec-Fetch-Site: cross-site`.

use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;

use axum::extract::connect_info::ConnectInfo;
use axum::extract::{FromRequest, Request};
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, Method, StatusCode};
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_auth::{
    AuthenticationError, CSRF_HEADER, Credential, Principal, TrustedProxies, session_token,
};
use subtle::ConstantTimeEq;

use crate::params::{Params, read_values};
use crate::response::{Encoded, Response, SubsonicError};
use crate::state::SubsonicState;

/// The header a proxy appends the address it received a request from.
const FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

/// The header a browser sets to tell the server where a request came from.
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");

/// Which surface a call belongs to, which decides what cookie authentication
/// must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    /// An endpoint that answers a Subsonic envelope; a cookie-authenticated
    /// call must echo the session's CSRF token.
    Envelope,
    /// A media endpoint that answers bytes; a media element cannot set a
    /// header, so no CSRF token is required.
    Binary,
}

/// A `/rest` call with its principal and parameters.
///
/// A handler takes this as its last argument; it dereferences to the call's
/// [`Params`], so an endpoint reads a parameter as `call.get("id")` and
/// authenticates nothing itself.
#[derive(Debug, Clone)]
pub struct Call {
    /// The shared services.
    state: SubsonicState,
    /// The authenticated principal.
    principal: Principal,
    /// The request's parameters.
    params: Params,
}

impl Call {
    /// Returns the shared services.
    pub fn state(&self) -> &SubsonicState {
        &self.state
    }

    /// Returns the authenticated principal.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the request's parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Renders a response in the encoding the client asked for.
    ///
    /// # Parameters
    ///
    /// - `response`: the response to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from the handler.
    pub fn encode(&self, response: Response) -> Encoded {
        self.params.encode(response)
    }

    /// Renders a response, decorating it with the `hearthsound` member when the
    /// request asked for the vendor extension.
    ///
    /// Without `hearthsound=1` this is exactly [`Call::encode`], so a standard
    /// response is byte-identical whether or not the decorator exists.
    ///
    /// # Parameters
    ///
    /// - `response`: the response to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from the handler.
    pub(crate) async fn respond(&self, response: Response) -> Encoded {
        match response {
            Response::Payload(mut payload) if self.params.vendor_enabled() => {
                let user = self.principal.user_id.uuid();
                match crate::vendor::decorate(&self.state, user, &mut payload).await {
                    Ok(()) => self.params.encode(Response::Payload(payload)),
                    Err(error) => self.params.encode(Response::failed(error)),
                }
            }
            response => self.params.encode(response),
        }
    }
}

impl Deref for Call {
    type Target = Params;

    /// Borrows the call's parameters, so a handler can read them directly.
    fn deref(&self) -> &Params {
        &self.params
    }
}

impl FromRequest<SubsonicState> for Call {
    type Rejection = HttpResponse;

    /// Parses a request's parameters and resolves its credentials.
    ///
    /// # Errors
    ///
    /// Returns a bare HTTP status code when the request's form body cannot be
    /// read, and a failed envelope at HTTP 200 with the credential error's code
    /// when the credentials are absent, contradictory, or wrong.
    async fn from_request(request: Request, state: &SubsonicState) -> Result<Call, HttpResponse> {
        let (parts, body) = request.into_parts();
        let params = match read_values(&parts, body).await {
            Ok(values) => Params::from_values(values),
            Err(rejection) => return Err(rejection.into_response()),
        };
        match authenticate(state, &parts, &params, CallKind::Envelope).await {
            Ok(principal) => Ok(Call {
                state: state.clone(),
                principal,
                params,
            }),
            Err(error) => Err(params.encode(Response::failed(error)).into_response()),
        }
    }
}

/// A `/rest` call whose body may be a JSON document.
///
/// The transcoding extension posts a client description as a JSON body, so this
/// extractor reads the query string for parameters and the body as a document,
/// and hands both to the handler. Everything else - the credentials, the
/// principal, the encoding of the answer - is what [`Call`] does.
#[derive(Debug, Clone)]
pub struct JsonCall {
    /// The call, with the parameters the query string carried.
    call: Call,
    /// The request's method, which decides whether a body could be read.
    method: axum::http::Method,
    /// The JSON document the body carried, when there was one.
    body: Option<serde_json::Value>,
}

impl JsonCall {
    /// Returns the shared services.
    pub fn state(&self) -> &SubsonicState {
        self.call.state()
    }

    /// Returns the authenticated principal.
    pub fn principal(&self) -> &Principal {
        self.call.principal()
    }

    /// Returns the request's parameters.
    pub fn params(&self) -> &Params {
        self.call.params()
    }

    /// Returns the request's method.
    pub fn method(&self) -> &axum::http::Method {
        &self.method
    }

    /// Deserializes the request's body.
    ///
    /// # Parameters
    ///
    /// - `member`: the body's name, for the error message.
    ///
    /// # Returns
    ///
    /// The body, deserialized into `T`.
    ///
    /// # Errors
    ///
    /// Answers Subsonic error 10 when the request carried no body and error 0
    /// when the body is not the document the extension defines.
    pub fn body<T: serde::de::DeserializeOwned>(
        &self,
        member: &'static str,
    ) -> Result<T, SubsonicError> {
        let Some(document) = &self.body else {
            return Err(SubsonicError::missing_parameter(member));
        };
        serde_json::from_value(document.clone()).map_err(|error| {
            tracing::debug!(%error, member, "a request body is not the document it must be");
            SubsonicError::new(
                crate::response::codes::GENERIC,
                format!("The request body is not a valid {member}."),
            )
        })
    }

    /// Renders a response in the encoding the client asked for.
    ///
    /// # Parameters
    ///
    /// - `response`: the response to render.
    ///
    /// # Returns
    ///
    /// The encoded response, ready to be returned from the handler.
    pub fn encode(&self, response: Response) -> Encoded {
        self.call.encode(response)
    }
}

impl Deref for JsonCall {
    type Target = Call;

    /// Borrows the call, so a handler reads its parameters as it does on
    /// [`Call`].
    fn deref(&self) -> &Call {
        &self.call
    }
}

impl FromRequest<SubsonicState> for JsonCall {
    type Rejection = HttpResponse;

    /// Parses a request's parameters and body, and resolves its credentials.
    ///
    /// # Errors
    ///
    /// Returns a bare HTTP status code when the body cannot be read, a failed
    /// envelope at HTTP 200 when the credentials are absent, contradictory, or
    /// wrong, and a failed envelope at HTTP 200 with error 0 when a body
    /// declared as JSON is not one.
    async fn from_request(
        request: Request,
        state: &SubsonicState,
    ) -> Result<JsonCall, HttpResponse> {
        let (parts, body) = request.into_parts();
        let (values, document) = match crate::params::read_json_body(&parts, body).await {
            Ok(read) => read,
            Err(rejection) => return Err(rejection.into_response()),
        };
        let params = Params::from_values(values);
        match authenticate(state, &parts, &params, CallKind::Envelope).await {
            Ok(principal) => Ok(JsonCall {
                call: Call {
                    state: state.clone(),
                    principal,
                    params,
                },
                method: parts.method.clone(),
                body: document,
            }),
            Err(error) => Err(params.encode(Response::failed(error)).into_response()),
        }
    }
}

/// A `/rest` call to a binary endpoint with its principal and parameters.
///
/// The three endpoints that answer with media bytes take this instead of
/// [`Call`], because their failures are HTTP status codes rather than
/// envelopes: a client that plays any 200 body would otherwise play an error
/// document as noise.
///
/// A binary handler answers a permission refusal with [`forbidden`], and its own
/// not-found and unavailable cases with the matching status code.
///
/// Unlike [`Call`], a binary call keeps the request's method and headers,
/// because what these endpoints answer depends on them: `stream` serves
/// `Range` requests and revalidates with the `If-*` headers, and `getCoverArt`
/// revalidates the same way. A handler reads them with
/// [`BinaryCall::method`] and [`BinaryCall::headers`].
#[derive(Debug, Clone)]
pub struct BinaryCall {
    /// The shared services.
    state: SubsonicState,
    /// The authenticated principal.
    principal: Principal,
    /// The request's parameters.
    params: Params,
    /// The request's method.
    method: Method,
    /// The request's headers.
    headers: HeaderMap,
}

impl BinaryCall {
    /// Returns the shared services.
    pub fn state(&self) -> &SubsonicState {
        &self.state
    }

    /// Returns the authenticated principal.
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Returns the request's parameters.
    pub fn params(&self) -> &Params {
        &self.params
    }

    /// Returns the request's method.
    ///
    /// A binary endpoint serves the same bytes for `GET` and `HEAD`; the method
    /// tells it whether the body is wanted, which is what `stream` uses to
    /// decide whether it starts a stream session.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the request's headers.
    ///
    /// The `Range` and `If-*` headers live here, so a binary endpoint reads its
    /// conditional requests from the request rather than from the parameters.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }
}

impl Deref for BinaryCall {
    type Target = Params;

    /// Borrows the call's parameters, so a handler can read them directly.
    fn deref(&self) -> &Params {
        &self.params
    }
}

impl FromRequest<SubsonicState> for BinaryCall {
    type Rejection = HttpResponse;

    /// Parses a request's parameters and resolves its credentials.
    ///
    /// # Errors
    ///
    /// Returns HTTP 400 or 413 when the request's form body cannot be read,
    /// HTTP 401 when the credentials are absent, contradictory, or wrong, and
    /// HTTP 500 when the check itself could not be completed. The body is always
    /// empty, for the reason [`BinaryCall`] exists.
    async fn from_request(
        request: Request,
        state: &SubsonicState,
    ) -> Result<BinaryCall, HttpResponse> {
        let (parts, body) = request.into_parts();
        let params = match read_values(&parts, body).await {
            Ok(values) => Params::from_values(values),
            Err(rejection) => return Err(rejection.into_response()),
        };
        match authenticate(state, &parts, &params, CallKind::Binary).await {
            Ok(principal) => Ok(BinaryCall {
                state: state.clone(),
                principal,
                params,
                method: parts.method.clone(),
                headers: parts.headers.clone(),
            }),
            Err(error) => Err(binary_rejection(&error)),
        }
    }
}

/// Answers that the principal may not have the bytes it asked for.
///
/// A binary endpoint calls this when the request is authenticated and refers to
/// an entity the principal's permissions do not cover; the alternative, a failed
/// envelope, is forbidden for these endpoints. An entity that does not exist is
/// HTTP 404 instead, and one that is not available yet is HTTP 503 with a
/// `Retry-After` header.
///
/// # Returns
///
/// HTTP 403 with an empty body.
pub fn forbidden() -> HttpResponse {
    StatusCode::FORBIDDEN.into_response()
}

/// Resolves the credentials of a request into a principal.
///
/// A call that carries any credential parameter authenticates through it and
/// ignores the session cookie. A call that carries none falls back to the
/// `hs_session` cookie; without a cookie the parameter path runs unchanged, so
/// a call with no credential at all is still told which parameter is missing.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `parts`: the request's head, which supplies the peer address, the
///   forwarding header, the cookie and the CSRF header.
/// - `params`: the request's parameters.
/// - `kind`: whether the endpoint answers an envelope or media bytes, which
///   decides what a cookie-authenticated call must carry.
///
/// # Errors
///
/// Returns the error to report when the credentials are absent, contradictory,
/// or wrong, and a generic error when the check itself could not be completed.
async fn authenticate(
    state: &SubsonicState,
    parts: &Parts,
    params: &Params,
    kind: CallKind,
) -> Result<Principal, crate::response::SubsonicError> {
    let credentials = params.credentials();
    let carries_parameters = credentials.username.is_some()
        || credentials.password.is_some()
        || credentials.token.is_some()
        || credentials.salt.is_some()
        || credentials.api_key.is_some();
    if !carries_parameters {
        let token = session_token(
            parts
                .headers
                .get_all(COOKIE)
                .iter()
                .filter_map(|value| value.to_str().ok()),
        );
        if let Some(token) = token {
            return resolve_session(state, parts, &token, kind).await;
        }
    }

    state
        .authenticate(client_ip(parts, state.trusted_proxies()), &credentials)
        .await
}

/// Resolves the `hs_session` cookie of a call into a principal.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `parts`: the request's head, which supplies the CSRF and `Sec-Fetch-Site`
///   headers.
/// - `token`: the token the cookie carried.
/// - `kind`: whether the call must carry the CSRF token.
///
/// # Returns
///
/// The principal, authenticated by the session.
///
/// # Errors
///
/// Returns Subsonic error 40 for an unknown or expired session, for an envelope
/// call whose CSRF header does not match the session's token, and for a binary
/// call a browser labelled cross-site; a generic error when the session could
/// not be resolved.
async fn resolve_session(
    state: &SubsonicState,
    parts: &Parts,
    token: &str,
    kind: CallKind,
) -> Result<Principal, crate::response::SubsonicError> {
    let record = match state.sessions().resolve(token).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            tracing::debug!("rejected an unknown or expired /rest session");
            return Err(crate::response::SubsonicError::authentication(
                &AuthenticationError::WrongCredentials,
            ));
        }
        Err(error) => {
            tracing::error!(%error, "could not resolve the session of a /rest call");
            return Err(crate::response::SubsonicError::generic());
        }
    };

    match kind {
        CallKind::Envelope => {
            let echoed = parts
                .headers
                .get(CSRF_HEADER)
                .map(|value| value.as_bytes())
                .unwrap_or_default();
            if !bool::from(echoed.ct_eq(record.csrf_token.as_bytes())) {
                return Err(crate::response::SubsonicError::new(
                    crate::response::codes::WRONG_CREDENTIALS,
                    "Cookie-authenticated calls must carry the X-Hearthsound-CSRF header.",
                ));
            }
        }
        CallKind::Binary => {
            let cross_site = parts
                .headers
                .get(SEC_FETCH_SITE)
                .and_then(|value| value.to_str().ok())
                == Some("cross-site");
            if cross_site {
                tracing::debug!("refused a cross-site media request authenticated by cookie");
                return Err(crate::response::SubsonicError::authentication(
                    &AuthenticationError::WrongCredentials,
                ));
            }
        }
    }

    Ok(Principal::from_user(
        &record.user,
        Credential::Session { id: record.id },
    ))
}

/// Returns the status a rejected binary call is answered with.
///
/// A credential rejection is HTTP 401; a generic error means the check itself
/// failed, which is HTTP 500. Neither carries a body.
fn binary_rejection(error: &crate::response::SubsonicError) -> HttpResponse {
    if error.code() == crate::response::codes::GENERIC {
        tracing::error!("a binary /rest call could not be authenticated");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tracing::debug!(
        code = error.code(),
        "rejected the credentials of a binary /rest call"
    );
    StatusCode::UNAUTHORIZED.into_response()
}

/// Returns the address a request came from.
///
/// The peer address comes from the connection information axum was built with,
/// and the forwarding header is trusted only when that peer is a trusted proxy.
fn client_ip(parts: &Parts, trusted: &TrustedProxies) -> IpAddr {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip());
    let forwarded_for = parts
        .headers
        .get(&FORWARDED_FOR)
        .and_then(|value| value.to_str().ok());
    trusted.client_ip(peer, forwarded_for)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use axum::body::Body;
    use axum::http::Request as HttpRequest;

    use super::*;

    /// Builds the request head of a request that carries the headers.
    fn parts(headers: &[(&str, &str)]) -> Parts {
        let mut builder = HttpRequest::builder().uri("/rest/ping");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).unwrap().into_parts().0
    }

    #[test]
    fn the_request_head_supplies_the_peer_and_the_forwarding_header() {
        let proxies = TrustedProxies::parse(["10.0.0.0/8"]).unwrap();
        let mut parts = parts(&[("x-forwarded-for", "198.51.100.4")]);
        assert_eq!(
            client_ip(&parts, &proxies),
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            "without connection information the header must not be believed"
        );

        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 5000))));
        assert_eq!(
            client_ip(&parts, &proxies),
            "198.51.100.4".parse::<IpAddr>().unwrap()
        );

        parts
            .extensions
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 9], 5000))));
        assert_eq!(
            client_ip(&parts, &proxies),
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
        );
    }
}
