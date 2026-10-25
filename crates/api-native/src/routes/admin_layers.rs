//! `/api/v1/admin/layers`: which instances serve which layer, in which order.
//!
//! A layer is served by the instances assigned to it, in order: the first is
//! the primary, and the ones after it are tried when the first cannot answer.
//! The assignment is all-or-nothing per layer, so a `PUT` replaces the whole
//! order, and the provider registry is rebuilt so the new order is in service
//! before the answer is sent.
//!
//! The listing reports the assignment as it is stored, including instances that
//! are switched off or whose plugin does not load right now: what is serving is
//! [`GET /api/v1/system/info`](crate::routes::system::system_info)'s business,
//! and this resource is what an administrator edits.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use hearthsound_core::Layer;
use hearthsound_plugin_host::{HOSTED_LAYERS, InstanceError};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::AdminAuthed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;

/// One layer and the instances assigned to it, in the order the layer tries
/// them.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LayerView {
    /// The layer, for example `catalog` or `acquisition`.
    pub layer: String,
    /// The slugs of the assigned instances, first is primary.
    pub instances: Vec<String>,
}

/// Every layer this server can drive.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LayerList {
    /// The layers.
    pub layers: Vec<LayerView>,
}

/// Lists every layer with the instances assigned to it.
///
/// A layer nothing is assigned to is reported with an empty list, so an
/// administrator sees every layer the server can drive.
///
/// # Errors
///
/// Answers `internal` when the instance rows or the assignments cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/layers",
    tag = "admin",
    responses(
        (status = 200, description = "The layers and their instances", body = LayerList),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_layers(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<LayerList>, Problem> {
    Ok(Json(LayerList {
        layers: assigned_layers(&state).await?,
    }))
}

/// Replaces one layer's ordered instances.
///
/// The body is a JSON array of instance slugs, first is primary; an empty array
/// leaves the layer with nothing assigned. Every instance must exist and its
/// plugin must provide the layer.
///
/// # Errors
///
/// Answers `invalid-request` for a body that is not an array of strings,
/// `not-found` for a slug that names no instance, `invalid-layer` for a layer
/// this server cannot drive or an instance whose plugin does not provide it,
/// and `internal` when the assignment cannot be written.
#[utoipa::path(
    put,
    path = "/api/v1/admin/layers/{layer}",
    tag = "admin",
    params(("layer" = String, Path, description = "The layer, for example `catalog`")),
    request_body = Vec<String>,
    responses(
        (status = 200, description = "The layer's instances, in order", body = LayerView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "A slug names no instance", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The layer or an instance cannot serve it", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn put_layer(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(layer): UrlPath<String>,
    JsonBody(slugs): JsonBody<Vec<String>>,
) -> Result<Json<LayerView>, Problem> {
    let layer = hostable_layer(&layer)?;
    state
        .services()
        .instances
        .set_layer(layer, &slugs)
        .await
        .map_err(|error| layer_problem(&error))?;
    rebuild(&state).await?;
    Ok(Json(LayerView {
        layer: layer.as_str().to_owned(),
        instances: slugs,
    }))
}

/// Reads the layer a path names.
///
/// # Parameters
///
/// - `name`: the layer from the path.
///
/// # Returns
///
/// The layer.
///
/// # Errors
///
/// Answers `invalid-layer` when the name is not a layer, and when it is one
/// this server cannot drive yet.
fn hostable_layer(name: &str) -> Result<Layer, Problem> {
    let layer = name
        .parse::<Layer>()
        .map_err(|error| Problem::invalid_layer(error.to_string()))?;
    if !HOSTED_LAYERS.contains(&layer) {
        return Err(Problem::invalid_layer(format!(
            "This server cannot drive the `{name}` layer yet."
        )));
    }
    Ok(layer)
}

/// Builds the listing of every layer this server can drive.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Errors
///
/// Answers `internal` when the instance rows or the assignments cannot be read.
async fn assigned_layers(state: &NativeState) -> Result<Vec<LayerView>, Problem> {
    let instances = state
        .services()
        .instances
        .list()
        .await
        .map_err(|error| problem::internal("listing the provider instances", &error))?;
    let slugs: HashMap<Uuid, String> = instances
        .into_iter()
        .map(|instance| (instance.id, instance.slug))
        .collect();
    let assignments = state
        .services()
        .instances
        .assignments()
        .await
        .map_err(|error| problem::internal("reading the layer assignments", &error))?;

    // An assignment whose instance is gone cannot happen, because the foreign
    // key cascades; if one is read anyway, the layer simply lists what exists.
    let mut order: HashMap<Layer, Vec<(i64, String)>> = HashMap::new();
    for assignment in assignments {
        let Some(slug) = slugs.get(&assignment.instance_id) else {
            continue;
        };
        order
            .entry(assignment.layer)
            .or_default()
            .push((assignment.position, slug.clone()));
    }

    let mut layers = Vec::with_capacity(HOSTED_LAYERS.len());
    for layer in HOSTED_LAYERS {
        let mut assigned = order.remove(&layer).unwrap_or_default();
        assigned.sort_by_key(|(position, _)| *position);
        layers.push(LayerView {
            layer: layer.as_str().to_owned(),
            instances: assigned.into_iter().map(|(_, slug)| slug).collect(),
        });
    }
    Ok(layers)
}

/// Rebuilds the provider registry so an assignment is in service.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Errors
///
/// Answers `internal` when the plugin tables cannot be read.
async fn rebuild(state: &NativeState) -> Result<(), Problem> {
    state
        .services()
        .registry
        .rebuild()
        .await
        .map_err(|error| problem::internal("rebuilding the provider registry", &error))?;
    if let Err(error) = state.services().orchestrator.providers_changed().await {
        tracing::error!(%error, "could not bring the acquisitions in line with the registry");
    }
    Ok(())
}

/// Maps an assignment failure onto the problem a client sees.
///
/// # Parameters
///
/// - `error`: the error the instance service returned.
///
/// # Returns
///
/// `not-found` for a slug that names no instance, `invalid-layer` for an
/// assignment the layer cannot take - a slug listed twice, an instance whose
/// plugin does not provide the layer, or an instance whose plugin is not
/// installed - and `internal` when the write fails.
fn layer_problem(error: &InstanceError) -> Problem {
    match error {
        InstanceError::UnknownInstance(_) => Problem::not_found(error.to_string()),
        InstanceError::DuplicateInstance(_)
        | InstanceError::LayerNotProvided { .. }
        | InstanceError::UnknownPlugin(_)
        | InstanceError::StoredManifest { .. } => Problem::invalid_layer(error.to_string()),
        _ => problem::internal("the layer assignment", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_describes_the_layer_routes() {
        let document = crate::openapi();
        for route in ["/api/v1/admin/layers", "/api/v1/admin/layers/{layer}"] {
            assert!(
                document.paths.paths.contains_key(route),
                "{route} is missing"
            );
        }
    }

    #[test]
    fn only_the_layers_this_server_drives_are_accepted() {
        for (name, layer) in [
            ("catalog", Layer::Catalog),
            ("metadata", Layer::Metadata),
            ("acquisition", Layer::Acquisition),
            ("artwork", Layer::Artwork),
            ("verification", Layer::Verification),
            ("scrobble", Layer::Scrobble),
            ("discovery", Layer::Discovery),
        ] {
            assert_eq!(hostable_layer(name).unwrap(), layer);
        }

        let problem = hostable_layer("nonsense").unwrap_err();
        assert_eq!(problem.status, 422);
        assert_eq!(problem.code, "invalid-layer");

        // A layer of the architecture the host has no adapter for is refused
        // with the same answer.
        let problem = hostable_layer("lyrics").unwrap_err();
        assert_eq!(problem.status, 422);
        assert!(problem.detail.unwrap().contains("lyrics"));
    }

    #[test]
    fn an_assignment_error_maps_to_its_problem() {
        let problem = layer_problem(&InstanceError::UnknownInstance("hello".to_owned()));
        assert_eq!(problem.status, 404);
        assert_eq!(problem.code, "not-found");

        let problem = layer_problem(&InstanceError::DuplicateInstance("hello".to_owned()));
        assert_eq!(problem.status, 422);
        assert_eq!(problem.code, "invalid-layer");

        let problem = layer_problem(&InstanceError::LayerNotProvided {
            plugin: "com.example.plugin".to_owned(),
            layer: Layer::Catalog,
        });
        assert_eq!(problem.status, 422);
    }
}
