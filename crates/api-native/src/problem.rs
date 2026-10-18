//! RFC 9457 problem responses.
//!
//! Every error of the native API is an `application/problem+json` document
//! with a stable, machine-readable `code` member that clients branch on; the
//! `title` is for people and may change. The `type` member is
//! `https://hearthsound.app/problems/<code>`.
//!
//! | Code | Status | Meaning |
//! |---|---|---|
//! | `unauthenticated` | 401 | no credential, or one that does not resolve |
//! | `forbidden` | 403 | the caller is not an administrator |
//! | `csrf-failed` | 403 | a cookie-authenticated unsafe request without the matching `X-Hearthsound-CSRF` header |
//! | `permission-denied` | 403 | the caller lacks the permission the action needs |
//! | `quota-exceeded` | 403 | the caller's daily acquisition quota is spent |
//! | `sideload-disabled` | 403 | plugin sideloading is switched off |
//! | `repositories-disabled` | 403 | third-party plugin repositories are switched off |
//! | `not-found` | 404 | the resource does not exist |
//! | `invalid-request` | 400 | the request is malformed or names something invalid |
//! | `conflict` | 409 | the request contradicts the current state |
//! | `too-large` | 413 | an upload exceeds `uploads.max_file_bytes` |
//! | `unsupported-media` | 415 | an upload is not audio the library indexes |
//! | `invalid-plugin` | 422 | the archive is not a plugin this server can host |
//! | `invalid-provider` | 422 | the provider instance cannot be configured this way |
//! | `invalid-layer` | 422 | the layer cannot take these instances |
//! | `rate-limited` | 429 | too many failed sign-ins; `Retry-After` says when to retry |
//! | `internal` | 500 | the server failed; the log has the details |
//! | `signature-invalid` | 502 | a plugin index is unsigned or its signature does not verify |
//! | `integrity-failed` | 502 | a plugin archive is not what the index declares |
//! | `index-unavailable` | 502 | a plugin index or an archive could not be downloaded |

use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

/// The media type of a problem document.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// The prefix of every problem `type`.
const TYPE_PREFIX: &str = "https://hearthsound.app/problems/";

/// How long a rate-limited client is told to wait, in seconds.
const RATE_LIMIT_RETRY_SECS: u32 = 60;

/// An RFC 9457 problem document.
///
/// # Examples
///
/// ```
/// use hearthsound_api_native::problem::Problem;
///
/// let problem = Problem::not_found("No such user.");
/// assert_eq!(problem.status, 404);
/// assert_eq!(problem.code, "not-found");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct Problem {
    /// `https://hearthsound.app/problems/<code>`.
    #[serde(rename = "type")]
    pub kind: String,
    /// A short human-readable summary of the problem kind.
    pub title: String,
    /// The HTTP status code.
    pub status: u16,
    /// The stable machine-readable problem code clients branch on.
    pub code: String,
    /// What happened in this occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Problem {
    /// Builds a problem.
    ///
    /// # Parameters
    ///
    /// - `status`: the HTTP status.
    /// - `code`: the stable problem code.
    /// - `title`: the human-readable summary.
    /// - `detail`: what happened in this occurrence.
    ///
    /// # Returns
    ///
    /// The problem.
    pub fn new(status: StatusCode, code: &str, title: &str, detail: impl Into<String>) -> Problem {
        let detail = detail.into();
        Problem {
            kind: format!("{TYPE_PREFIX}{code}"),
            title: title.to_owned(),
            status: status.as_u16(),
            code: code.to_owned(),
            detail: (!detail.is_empty()).then_some(detail),
        }
    }

    /// The caller presented no credential, or one that does not resolve.
    pub fn unauthenticated(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "Authentication required",
            detail,
        )
    }

    /// The caller is not an administrator.
    pub fn forbidden(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Administrator access required",
            detail,
        )
    }

    /// A cookie-authenticated unsafe request lacked the CSRF header.
    pub fn csrf_failed() -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "csrf-failed",
            "CSRF check failed",
            "An unsafe request authenticated by the session cookie must carry the \
             session's token in the X-Hearthsound-CSRF header.",
        )
    }

    /// The caller lacks the permission the action needs.
    pub fn permission_denied(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "permission-denied",
            "Permission denied",
            detail,
        )
    }

    /// The caller's daily acquisition quota is spent.
    pub fn quota_exceeded(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "quota-exceeded",
            "Acquisition quota exceeded",
            detail,
        )
    }

    /// Plugin sideloading is switched off.
    pub fn sideload_disabled(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "sideload-disabled",
            "Plugin sideloading disabled",
            detail,
        )
    }

    /// Third-party plugin repositories are switched off.
    pub fn repositories_disabled(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::FORBIDDEN,
            "repositories-disabled",
            "Third-party plugin repositories disabled",
            detail,
        )
    }

    /// The resource does not exist.
    pub fn not_found(detail: impl Into<String>) -> Problem {
        Problem::new(StatusCode::NOT_FOUND, "not-found", "Not found", detail)
    }

    /// The request is malformed or names something invalid.
    pub fn invalid_request(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::BAD_REQUEST,
            "invalid-request",
            "Invalid request",
            detail,
        )
    }

    /// The request contradicts the current state.
    pub fn conflict(detail: impl Into<String>) -> Problem {
        Problem::new(StatusCode::CONFLICT, "conflict", "Conflict", detail)
    }

    /// An upload exceeds the size limit.
    pub fn too_large(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too-large",
            "Upload too large",
            detail,
        )
    }

    /// An upload is not audio the library indexes.
    pub fn unsupported_media(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported-media",
            "Unsupported media",
            detail,
        )
    }

    /// The archive is not a plugin this server can host.
    pub fn invalid_plugin(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-plugin",
            "Invalid plugin",
            detail,
        )
    }

    /// The instance cannot be configured this way.
    pub fn invalid_provider(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-provider",
            "Invalid provider configuration",
            detail,
        )
    }

    /// The layer cannot take these instances.
    pub fn invalid_layer(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-layer",
            "Invalid layer assignment",
            detail,
        )
    }

    /// A plugin index is unsigned or its signature does not verify.
    pub fn signature_invalid(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::BAD_GATEWAY,
            "signature-invalid",
            "Plugin index signature invalid",
            detail,
        )
    }

    /// A plugin archive is not what the index declares.
    pub fn integrity_failed(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::BAD_GATEWAY,
            "integrity-failed",
            "Plugin archive integrity check failed",
            detail,
        )
    }

    /// A plugin index or an archive could not be downloaded.
    pub fn index_unavailable(detail: impl Into<String>) -> Problem {
        Problem::new(
            StatusCode::BAD_GATEWAY,
            "index-unavailable",
            "Plugin index unavailable",
            detail,
        )
    }

    /// Too many failed sign-ins.
    pub fn rate_limited() -> Problem {
        Problem::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate-limited",
            "Too many attempts",
            "Too many failed sign-ins; try again later.",
        )
    }

    /// The server failed.
    ///
    /// The detail never carries the underlying error, which may name paths or
    /// internals; the caller logs it instead.
    pub fn internal() -> Problem {
        Problem::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error",
            "",
        )
    }
}

impl IntoResponse for Problem {
    /// Answers with the problem document, its status, and `Retry-After` for a
    /// rate limit.
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self).unwrap_or_default();
        let mut response = (status, body).into_response();
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        if status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(RATE_LIMIT_RETRY_SECS));
        }
        response
    }
}

/// Logs an unexpected failure and turns it into an `internal` problem.
///
/// # Parameters
///
/// - `context`: what the handler was doing, for the log line.
/// - `error`: the failure.
///
/// # Returns
///
/// The `internal` problem.
pub fn internal(context: &str, error: &dyn std::fmt::Display) -> Problem {
    tracing::error!(%error, "{context} failed");
    Problem::internal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_problem_has_the_rfc_9457_shape_and_media_type() {
        let response = Problem::conflict("The key is pinned.").into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()[CONTENT_TYPE], PROBLEM_JSON);
        let json = serde_json::to_value(Problem::conflict("The key is pinned.")).unwrap();
        assert_eq!(json["type"], "https://hearthsound.app/problems/conflict");
        assert_eq!(json["status"], 409);
        assert_eq!(json["code"], "conflict");
        assert_eq!(json["detail"], "The key is pinned.");
    }

    #[test]
    fn a_rate_limit_says_when_to_retry() {
        let response = Problem::rate_limited().into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "60");
    }

    #[test]
    fn an_internal_problem_carries_no_detail() {
        let json = serde_json::to_value(Problem::internal()).unwrap();
        assert!(json.get("detail").is_none());
    }
}
