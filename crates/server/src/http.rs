//! The surfaces the composition root mounts itself.
//!
//! Three routes belong to no domain crate: `/healthz`, `/readyz`, and `/`, plus
//! the fallback that serves the web client. [`router`] builds the whole
//! application from them plus the `/rest` router [`hearthsound_api_subsonic`]
//! owns and the `/api/v1` router (with its OpenAPI document and Swagger UI)
//! [`hearthsound_api_native`] owns, and adds the request-tracing layer and, when
//! `server.cors_origins` allows any origin, the CORS layer.
//!
//! `/healthz` is the liveness probe: the process is up and the HTTP listener
//! answers. `/readyz` is the readiness probe: the database is open and migrated
//! to the schema version this binary bundles. Neither probe authenticates, and
//! neither touches the plugin host, so a slow or broken plugin cannot make a
//! container look dead.
//!
//! `GET /` serves the web client's entry document to a request that accepts
//! HTML, and the JSON object `{"status":"ok"}` to every other request. A client
//! which probes the server root to learn whether it is talking to a Hearthsound
//! server (Amperfy does) asks for JSON and keeps getting it, and a browser that
//! opens the server gets the application.
//!
//! Every other path is the fallback: it serves the client's file when one
//! exists under `/_app/`, and the entry document when the request accepts HTML,
//! which is how the client's own routes (`/setup`, `/library/albums`) reach the
//! single-page application. A request that accepts HTML and names a missing
//! asset therefore gets the application rather than a 404, which is what makes
//! a deep link work; a request that does not gets an empty 404. Anything under
//! `/rest` or `/api/v1` is claimed by those surfaces' own catch-all routes and
//! never reaches the fallback.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::trace::TraceLayer;

use crate::services::Services;
use crate::web::normalize_path;

/// The body `GET /` answers with.
#[derive(Debug, Serialize)]
struct RootStatus {
    /// Always `ok`: reaching this handler at all means the process is serving.
    status: &'static str,
}

/// Builds the whole application.
///
/// # Parameters
///
/// - `services`: the composed services. The `/rest` router gets a
///   [`SubsonicState`](hearthsound_api_subsonic::SubsonicState) over them, the
///   health probes read the database through them, and the root and the
///   fallback serve the web client's files from them.
///
/// # Returns
///
/// The router, ready for `axum::serve`.
///
/// # Examples
///
/// ```no_run
/// # use std::sync::Arc;
/// # async fn example(services: Arc<hearthsound_server::Services>) {
/// let app = hearthsound_server::http::router(&services);
/// let _ = app;
/// # }
/// ```
pub fn router(services: &Arc<Services>) -> Router {
    let subsonic = hearthsound_api_subsonic::router(services.subsonic_state());
    let native = hearthsound_api_native::router(services.native_state());
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/", get(root))
        // Added before the state is applied: the fallback serves the client's
        // files, so it reads the same services the root handler does.
        .fallback(fallback)
        .with_state(Arc::clone(services))
        .merge(subsonic)
        .merge(native)
        .layer(TraceLayer::new_for_http());
    match services.cors() {
        Some(cors) => app.layer(cors.clone()),
        None => app,
    }
}

/// Answers the liveness probe.
///
/// # Returns
///
/// HTTP 200 with the body `ok`. The handler does no work: being reached proves
/// the runtime is serving requests.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Answers the readiness probe.
///
/// # Returns
///
/// HTTP 200 with the body `ready` when the database is open and migrated to the
/// bundled schema version, and HTTP 503 with the body `not ready` otherwise.
async fn readyz(State(services): State<Arc<Services>>) -> Response {
    if services.is_ready().await {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready").into_response()
    }
}

/// Answers a probe of the server root.
///
/// # Parameters
///
/// - `services`: the services holding the web client's files.
/// - `headers`: the request headers, whose `Accept` decides between the client
///   and the status object.
///
/// # Returns
///
/// HTTP 200 with the client's entry document when the request accepts HTML and
/// a client is available, and HTTP 200 with `{"status":"ok"}` otherwise.
async fn root(State(services): State<Arc<Services>>, headers: HeaderMap) -> Response {
    if accepts_html(&headers)
        && let Some(response) = services.web().index(&headers).await
    {
        return response;
    }
    (StatusCode::OK, Json(RootStatus { status: "ok" })).into_response()
}

/// Answers a path no other surface claimed.
///
/// # Parameters
///
/// - `services`: the services holding the web client's files.
/// - `request`: the request, whose method selects whether it is served, whose
///   path names the file, and whose `Accept` decides whether a missing file
///   falls back to the entry document.
///
/// # Returns
///
/// The client's file when the path names one, the client's entry document when
/// the request accepts HTML and a client is available, and an empty 404
/// otherwise.
async fn fallback(State(services): State<Arc<Services>>, request: Request) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(path) = normalize_path(request.uri().path())
        && let Some(response) = services.web().serve(&path, request.headers()).await
    {
        return response;
    }
    if accepts_html(request.headers())
        && let Some(response) = services.web().index(request.headers()).await
    {
        return response;
    }
    StatusCode::NOT_FOUND.into_response()
}

/// Reports whether a request asks for HTML.
///
/// The `Accept` header holds a comma-separated list of media ranges, possibly
/// with parameters, so the test is a case-insensitive substring search rather
/// than a parse: a browser sends `text/html,application/xhtml+xml,...`, and a
/// client that wants JSON does not mention `text/html` at all.
///
/// # Parameters
///
/// - `headers`: the request headers.
///
/// # Returns
///
/// `true` when any `Accept` header mentions `text/html`.
fn accepts_html(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("text/html"))
}
