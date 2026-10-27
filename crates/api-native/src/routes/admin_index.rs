//! `/api/v1/admin/index`: the entity index queue, and a forced refresh.
//!
//! The indexer walks one entity at a time: a page the server never fetched, a
//! library item changed by a scan, or a row an administrator asked to refresh.
//! `GET /admin/index` reports what is waiting, what failed and what this process
//! has done, which is what the administration overview draws its progress card
//! from; `POST /admin/entities/{id}/refresh` queues one entity at the front of
//! the queue.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_core::{EntityId, IdPrefix};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::AdminAuthed;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::entity_id;

/// How the entity index queue stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    /// How many entities are waiting to be indexed.
    pub pending: u64,
    /// How many entities failed their last pass and wait for a retry.
    pub failed: u64,
    /// How many entities this process has indexed.
    pub done: u64,
    /// How many entities this process has claimed.
    pub total: u64,
}

/// What one refresh request queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RefreshView {
    /// Whether the entity was queued; always `true` on a 202.
    pub queued: bool,
}

/// Answers `GET /admin/index`: the index queue's state.
///
/// # Parameters
///
/// - `_authed`: the administrator the extractor requires.
/// - `state`: the shared services.
///
/// # Returns
///
/// What is queued, what failed, and what this process has done.
///
/// # Errors
///
/// Answers `forbidden` (403) for a caller who is not an administrator,
/// `unauthenticated` without credentials, and `internal` when the queue cannot
/// be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/index",
    tag = "admin",
    responses(
        (status = 200, description = "The index queue's state", body = IndexStatus),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The caller is not an administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn index_status(
    AdminAuthed(_authed): AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<IndexStatus>, Problem> {
    let indexer = state.services().catalog.indexer();
    let (pending, failed) = indexer
        .queue_counts()
        .await
        .map_err(|error| problem::internal("reading the index queue", &error))?;
    let (done, total) = indexer.progress();
    Ok(Json(IndexStatus {
        pending: pending.max(0) as u64,
        failed: failed.max(0) as u64,
        done,
        total,
    }))
}

/// Answers `POST /admin/entities/{id}/refresh`: queue one entity for a refresh.
///
/// # Parameters
///
/// - `_authed`: the administrator the extractor requires.
/// - `state`: the shared services.
/// - `id`: the entity's client id (`tr_`, `al_` or `ar_...`); a merged-away id
///   queues the surviving row.
///
/// # Returns
///
/// `202 Accepted` with `{ "queued": true }`.
///
/// # Errors
///
/// Answers `forbidden` (403) for a caller who is not an administrator,
/// `unauthenticated` without credentials, `not-found` (404) when the id names
/// no stored track, album or artist, and `internal` when the write fails.
#[utoipa::path(
    post,
    path = "/api/v1/admin/entities/{id}/refresh",
    tag = "admin",
    params(("id" = String, Path, description = "The entity's client id")),
    responses(
        (status = 202, description = "The entity is queued for a refresh", body = RefreshView),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The caller is not an administrator", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No track, album or artist with that id is stored", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn refresh_entity(
    AdminAuthed(_authed): AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<RefreshView>), Problem> {
    let catalog = state.services().catalog.as_ref();
    let prefix = match EntityId::parse(&id) {
        Ok(entity) => entity.kind(),
        Err(_) => {
            return Err(Problem::not_found(format!("`{id}` is not a client id.")));
        }
    };
    let kind = match prefix {
        IdPrefix::Track => hearthsound_core::EntityKind::Track,
        IdPrefix::Album => hearthsound_core::EntityKind::Album,
        IdPrefix::Artist => hearthsound_core::EntityKind::Artist,
        _ => {
            return Err(Problem::not_found(format!(
                "`{id}` does not name a track, an album or an artist."
            )));
        }
    };
    let id = entity_id(catalog, &id, prefix).await?;
    catalog
        .refresh(kind, id)
        .await
        .map_err(|error| problem::internal("queueing an entity refresh", &error))?;
    Ok((StatusCode::ACCEPTED, Json(RefreshView { queued: true })))
}
