//! The cross-origin policy built from `server.cors_origins`.
//!
//! A browser only lets a page read a response from another origin when the
//! response names that origin, or any origin, in `Access-Control-Allow-Origin`.
//! Clients that run in a browser engine need this: the Feishin desktop app, for
//! one, loads its interface from a `file://` page and calls `/rest` from there.
//!
//! The policy is denied by default: with an empty `server.cors_origins` no CORS
//! header is ever sent. Otherwise the list names the origins allowed, each as
//! `scheme://host[:port]` (or the literal `null` of an opaque origin), and the
//! single value `*` allows every origin. Any origin is safe for `/rest` because
//! its credentials travel in the request parameters, never in cookies, and the
//! policy never allows credentials, so a page from an allowed origin learns
//! nothing it could not already send itself.
//!
//! The layer wraps the whole application, so every response carries the header,
//! including Subsonic errors and paths no route matched, and it answers a
//! preflight `OPTIONS` request itself before routing.

use axum::http::{HeaderValue, Method};
use tower_http::cors::{AllowOrigin, Any, CorsLayer};

use crate::error::ServerError;

/// The value of `server.cors_origins` that allows every origin.
const ANY_ORIGIN: &str = "*";

/// Builds the CORS layer for the configured origins.
///
/// # Parameters
///
/// - `origins`: the values of `server.cors_origins`.
///
/// # Returns
///
/// `None` when the list is empty, which sends no CORS header at all, and the
/// layer otherwise. A list holding `*` allows every origin, whatever else it
/// holds.
///
/// # Errors
///
/// Returns [`ServerError::CorsOrigin`] for a value that is not `*`, `null`,
/// or a bare `scheme://host[:port]` origin. A trailing slash or a path is
/// refused rather than ignored, because a browser never sends one and such an
/// entry would silently match nothing.
pub(crate) fn layer(origins: &[String]) -> Result<Option<CorsLayer>, ServerError> {
    if origins.is_empty() {
        return Ok(None);
    }
    let allow = if origins.iter().any(|origin| origin == ANY_ORIGIN) {
        AllowOrigin::from(Any)
    } else {
        let values = origins
            .iter()
            .map(|origin| parse_origin(origin))
            .collect::<Result<Vec<_>, _>>()?;
        AllowOrigin::list(values)
    };
    Ok(Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::GET, Method::HEAD, Method::POST])
            .allow_headers(Any),
    ))
}

/// Parses one configured origin into the header value a browser compares.
///
/// # Parameters
///
/// - `origin`: one entry of `server.cors_origins` other than `*`.
///
/// # Returns
///
/// The header value.
///
/// # Errors
///
/// Returns [`ServerError::CorsOrigin`] when the value is not `null` and not a
/// `scheme://host[:port]` origin without a path.
fn parse_origin(origin: &str) -> Result<HeaderValue, ServerError> {
    let invalid = || ServerError::CorsOrigin {
        origin: origin.to_owned(),
    };
    if origin != "null" {
        let (scheme, authority) = origin.split_once("://").ok_or_else(invalid)?;
        if scheme.is_empty() || authority.is_empty() || authority.contains('/') {
            return Err(invalid());
        }
    }
    HeaderValue::from_str(origin).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origins(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn an_empty_list_sends_no_cors_headers() {
        assert!(layer(&[]).unwrap().is_none());
    }

    #[test]
    fn a_value_that_is_not_an_origin_is_refused() {
        for value in [
            "music.example",
            "https://music.example/",
            "https://music.example/app",
            "://music.example",
            "https://",
        ] {
            assert!(
                matches!(
                    layer(&origins(&[value])),
                    Err(ServerError::CorsOrigin { origin }) if origin == value
                ),
                "{value} is refused"
            );
        }
    }
}
