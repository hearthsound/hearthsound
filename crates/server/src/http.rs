//! The surfaces the composition root mounts itself.
//!
//! Three routes belong to no domain crate: `/healthz`, `/readyz`, and `/`.
//! [`router`] builds the whole application from them plus the `/rest` router
//! [`hearthsound_api_subsonic`] owns and the `/api/v1` router (with its OpenAPI
//! document and Swagger UI) [`hearthsound_api_native`] owns, and adds the
//! request-tracing layer and, when `server.cors_origins` allows any origin, the
//! CORS layer.
//!
//! `/healthz` is the liveness probe: the process is up and the HTTP listener
//! answers. `/readyz` is the readiness probe: the database is open and migrated
//! to the schema version this binary bundles. Neither probe authenticates, and
//! neither touches the plugin host, so a slow or broken plugin cannot make a
//! container look dead.
//!
//! `GET /` answers the JSON object `{"status":"ok"}`. The embedded web client
//! lands later; until then a client that probes the server root (Amperfy does)
//! gets the object it expects rather than an empty 404.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tower_http::trace::TraceLayer;

use crate::services::Services;

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
///   [`SubsonicState`](hearthsound_api_subsonic::SubsonicState) over them, and
///   the health probes read the database through them.
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
/// # Returns
///
/// HTTP 200 with `{"status":"ok"}`.
async fn root() -> Json<RootStatus> {
    Json(RootStatus { status: "ok" })
}
