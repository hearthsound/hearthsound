//! `GET /api/v1/system/info`: what this server is and what it offers.

use axum::Json;
use axum::extract::State;
use hearthsound_config::keys;
use hearthsound_core::Layer;
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::problem::{self, Problem};
use crate::state::NativeState;

/// The version of the native API this server speaks.
pub const API_VERSION: u32 = 1;

/// What this server is and what it offers.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    /// The server version, a plain semantic version.
    pub version: String,
    /// Always `hearthsound`.
    pub server_type: String,
    /// The native API version.
    pub api_version: u32,
    /// The features the server's settings turn on.
    pub features: Features,
    /// The provider instances assigned to each layer, in layer order.
    pub layers: Vec<LayerSummary>,
}

/// The features this server offers.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Features {
    /// Whether users may upload files (`uploads.enabled`).
    pub uploads: bool,
    /// Whether the caller may browse the server-wide view, which is every
    /// stored track. Only administrators may.
    pub server_browse: bool,
    /// Whether saving or liking a remote item acquires it.
    pub acquire_on_save: bool,
    /// Whether adding a remote track to a playlist acquires it.
    pub acquire_on_playlist_add: bool,
}

/// The instances assigned to one layer.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayerSummary {
    /// The layer, for example `catalog` or `acquisition`.
    pub layer: String,
    /// The slugs of the enabled instances serving it, in policy order.
    pub instances: Vec<String>,
}

/// Answers what this server is and what it offers.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when a setting
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/system/info",
    tag = "system",
    responses(
        (status = 200, description = "The server's version, features and layers", body = SystemInfo),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn system_info(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<SystemInfo>, Problem> {
    let snapshot = state.settings().current();
    let read =
        |error: hearthsound_config::ConfigError| problem::internal("reading a setting", &error);
    let features = Features {
        uploads: snapshot.get(keys::UPLOADS_ENABLED).map_err(read)?,
        server_browse: authed.principal.is_admin,
        acquire_on_save: snapshot.get(keys::LIBRARY_ACQUIRE_ON_SAVE).map_err(read)?,
        acquire_on_playlist_add: snapshot
            .get(keys::LIBRARY_ACQUIRE_ON_PLAYLIST_ADD)
            .map_err(read)?,
    };
    let registry = &state.services().registry;
    let layers = Layer::ALL
        .iter()
        .map(|layer| LayerSummary {
            layer: layer.as_str().to_owned(),
            instances: registry
                .layers(*layer)
                .iter()
                .map(|instance| instance.slug().to_string())
                .collect(),
        })
        .collect();
    Ok(Json(SystemInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        server_type: "hearthsound".to_owned(),
        api_version: API_VERSION,
        features,
        layers,
    }))
}
