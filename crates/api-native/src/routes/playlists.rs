//! `/api/v1/playlists`: editing the caller's own playlists.
//!
//! A playlist belongs to the account that created it. These routes change one
//! in a way the player needs and the Subsonic surface cannot express: a reorder
//! rewrites every position in one request instead of removing and re-adding
//! the entries, and adding a whole album, playlist or artist appends their
//! tracks in the collection's own order.
//!
//! Every edit answers a problem document, publishes `playlist.changed`, and
//! leaves the playlist pin of each affected track to the activity layer, which
//! refreshes it in the same transaction as the write.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_activity::ActivityError;
use hearthsound_core::EntityId;
use hearthsound_core::IdPrefix;
use hearthsound_core::events::{PlaylistChanged, ServerEvent};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::parse_id;

/// The new order of a playlist's entries.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReorderRequest {
    /// The playlist's current zero-based positions, in their new order: the
    /// entry at `order[0]` becomes first. It must be a permutation of
    /// `0..trackCount`.
    pub order: Vec<u32>,
}

/// Rewrites the order of a playlist's entries.
///
/// One request carries the whole new order, so the playlist is never left
/// half-reordered: every position is written in one transaction, the entries
/// keep the instant they were added, and the playlist's `changed` timestamp
/// moves to now.
///
/// # Parameters
///
/// - `authed`: the caller, who must own the playlist.
/// - `state`: the shared services.
/// - `id`: the playlist's client id (`pl_...`).
/// - `body`: the new order.
///
/// # Returns
///
/// `204 No Content` once the order is stored.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `not-found` (404) when the id
/// names no playlist or belongs to another account, `invalid-order` (400) when
/// `order` is not a permutation of the playlist's positions, and `internal`
/// when the write fails.
#[utoipa::path(
    put,
    path = "/api/v1/playlists/{id}/order",
    tag = "playlists",
    params(("id" = String, Path, description = "The playlist's client id")),
    request_body = ReorderRequest,
    responses(
        (status = 204, description = "The playlist is reordered"),
        (status = 400, description = "The order is not a permutation of the playlist's positions", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No playlist with that id belongs to the caller", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn reorder_playlist(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<ReorderRequest>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::Playlist)?;
    let id = EntityId::from_uuid(IdPrefix::Playlist, id);
    let user = authed.user_id();
    let playlists = state.activity().playlists();
    let playlist = playlists
        .get(id, user)
        .await
        .map_err(|error| problem::internal("reading a playlist", &error))?
        .ok_or_else(missing)?;
    if playlist.owner_user_id != user {
        return Err(missing());
    }
    let order: Vec<usize> = body
        .order
        .iter()
        .map(|position| *position as usize)
        .collect();
    playlists
        .reorder(id, user, &order)
        .await
        .map_err(|error| reorder_problem(&error))?;
    state
        .services()
        .events
        .publish(ServerEvent::PlaylistChanged(PlaylistChanged {
            playlist_id: id.to_string(),
            change: "reordered".to_owned(),
        }));
    Ok(StatusCode::NO_CONTENT)
}

/// The problem an unknown or foreign playlist answers with.
///
/// # Returns
///
/// The `not-found` problem.
fn missing() -> Problem {
    Problem::not_found("No playlist with that id belongs to this account.")
}

/// Maps a playlist edit failure onto the problem document the route answers.
///
/// # Parameters
///
/// - `error`: the failure the activity layer reported.
///
/// # Returns
///
/// The problem: `not-found` for an unknown or foreign playlist,
/// `invalid-order` for a list that is not a permutation, and `internal` for a
/// failed read or write.
fn reorder_problem(error: &ActivityError) -> Problem {
    match error {
        ActivityError::UnknownPlaylist { .. } | ActivityError::NotOwner { .. } => missing(),
        ActivityError::InvalidOrder { len } => Problem::invalid_order(format!(
            "`order` must list every position from 0 to {len} exactly once."
        )),
        other => problem::internal("reordering a playlist", other),
    }
}
