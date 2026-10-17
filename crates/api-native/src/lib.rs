//! The native JSON API: `/api/v1`, described by OpenAPI.
//!
//! This is the control plane of the server for the first-party web client,
//! administration tooling and custom clients: authentication, personal
//! settings and credentials, uploads, and the administration of users,
//! folders, settings, storage and acquisitions. Audio is never served here;
//! clients stream through `/rest/stream`.
//!
//! # Conventions
//!
//! - JSON field names are camelCase.
//! - Every error is an RFC 9457 `application/problem+json` document with a
//!   stable `code` member ([`problem`]).
//! - Lists that can grow page with `?limit=&cursor=` and answer `nextCursor`
//!   while more rows follow ([`pagination`]).
//! - A request authenticates with `Authorization: Bearer hs_...` or with the
//!   `hs_session` cookie plus `X-Hearthsound-CSRF` on unsafe methods
//!   ([`auth`]).
//! - The OpenAPI document is served at `/api/v1/openapi.json`; every handler
//!   is annotated, so the document is complete.
//!
//! # Resources
//!
//! | Resource | Who |
//! |---|---|
//! | `GET /api/v1/system/info` | any user |
//! | `POST /api/v1/auth/login`, `POST /api/v1/auth/logout`, `GET /api/v1/auth/session` | anyone / any user |
//! | `GET`/`POST /api/v1/me/api-keys`, `DELETE /api/v1/me/api-keys/{id}` | any user, own keys |
//! | `GET`/`POST /api/v1/me/app-passwords`, `DELETE /api/v1/me/app-passwords/{id}` | any user, own app passwords |
//! | `GET`/`PATCH /api/v1/me/settings` | any user, own overrides |

#![forbid(unsafe_code)]

pub mod auth;
pub mod extract;
pub mod pagination;
pub mod problem;
pub mod routes;
mod state;
pub mod views;

use std::sync::Arc;

use axum::routing::{any, get};
use axum::{Json, Router};
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub use state::{NativeServices, NativeState};

use crate::problem::Problem;

/// The path the OpenAPI document is served at.
pub const OPENAPI_PATH: &str = "/api/v1/openapi.json";

/// The OpenAPI document's fixed parts; the paths come from the routes.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hearthsound native API",
        description = "The control plane of a Hearthsound server. Audio is streamed through the Subsonic surface at /rest."
    ),
    modifiers(&SecuritySchemes),
    tags(
        (name = "system", description = "What the server is and offers"),
        (name = "auth", description = "Web sessions"),
        (name = "me", description = "The caller's own credentials and settings"),
    )
)]
struct ApiDoc;

/// Adds the two authentication schemes to the document.
struct SecuritySchemes;

impl Modify for SecuritySchemes {
    /// Declares `bearer` (an API key in `Authorization: Bearer`) and `session`
    /// (the `hs_session` cookie).
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("hs_<base32>")
                    .build(),
            ),
        );
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(auth::SESSION_COOKIE))),
        );
    }
}

/// Collects every annotated route with its OpenAPI path.
fn api_router() -> OpenApiRouter<NativeState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(routes::system::system_info))
        .routes(routes!(routes::session::login))
        .routes(routes!(routes::session::logout))
        .routes(routes!(routes::session::session))
        .routes(routes!(
            routes::me::list_api_keys,
            routes::me::create_api_key
        ))
        .routes(routes!(routes::me::revoke_api_key))
        .routes(routes!(
            routes::me::list_app_passwords,
            routes::me::create_app_password
        ))
        .routes(routes!(routes::me::revoke_app_password))
        .routes(routes!(
            routes::me::get_settings,
            routes::me::patch_settings
        ))
}

/// Returns the OpenAPI document of the native API.
///
/// # Returns
///
/// The document, the same one `/api/v1/openapi.json` serves.
///
/// # Examples
///
/// ```
/// let document = hearthsound_api_native::openapi();
/// assert!(document.paths.paths.contains_key("/api/v1/system/info"));
/// ```
pub fn openapi() -> utoipa::openapi::OpenApi {
    api_router().split_for_parts().1
}

/// Builds the `/api/v1` router and the OpenAPI document route.
///
/// An unknown path under `/api/v1` answers a `not-found` problem.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Returns
///
/// The router, ready to merge into the server's.
pub fn router(state: NativeState) -> Router {
    let (api, document) = api_router().split_for_parts();
    let document = Arc::new(document);
    api.route(
        OPENAPI_PATH,
        get(move || {
            let document = Arc::clone(&document);
            async move { Json(document.as_ref().clone()) }
        }),
    )
    .route("/api/v1", any(not_found))
    .route("/api/v1/{*rest}", any(not_found))
    .with_state(state)
}

/// Answers a path under `/api/v1` that names no resource.
async fn not_found() -> Problem {
    Problem::not_found("No resource lives at this path.")
}
