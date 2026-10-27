//! `/api/v1/playlists`: editing the caller's own playlists.
//!
//! A playlist belongs to the account that created it. These routes change one
//! in a way the player needs and the Subsonic surface cannot express: a reorder
//! rewrites every position in one request instead of removing and re-adding
//! the entries, and adding a whole album, playlist or artist appends their
//! tracks in the collection's own order.
//!
//! Every edit answers a problem document, publishes `playlist.changed` to the
//! playlist's owner and to administrators, and leaves the playlist pin of each
//! affected track to the activity layer, which refreshes it in the same
//! transaction as the write.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_acquire::playlist_add::acquire_playlist_additions;
use hearthsound_activity::{ActivityError, PlaylistUpdate};
use hearthsound_catalog::CatalogError;
use hearthsound_core::EntityId;
use hearthsound_core::IdPrefix;
use hearthsound_core::events::{Audience, PlaylistChanged, ServerEvent};
use hearthsound_library::query::{self, QueueCollection, TrackSort};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::Authed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::routes::library;
use crate::state::NativeState;
use crate::views::{entity_id, parse_id};

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
        .map_err(|error| edit_problem(&error, "reordering a playlist"))?;
    state.services().events.publish_for(
        Audience::User(user),
        ServerEvent::PlaylistChanged(PlaylistChanged {
            playlist_id: id.to_string(),
            change: "reordered".to_owned(),
        }),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// The collection whose tracks a request adds to a playlist.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddTracksRequest {
    /// The collection to append: `album:<id>`, `playlist:<id>` or
    /// `artist:<id>`. Any album id the server accepts is an album row id; a
    /// merged-away id is answered the surviving row.
    pub source: String,
}

/// What one add-to-playlist request appended.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AddTracksView {
    /// How many tracks the request appended.
    pub added: usize,
    /// How many tracks the playlist holds afterwards.
    pub track_count: usize,
}

/// Appends a whole album, playlist or artist to a playlist.
///
/// The source is resolved exactly as `GET /api/v1/library/queue` resolves it:
/// through the caller's personal library, in the collection's own order. An
/// album's stored tracklist is completed from the catalog first, so an album the
/// server holds a page for adds its whole tracklist.
///
/// The edit applies the same playlist pin and the same
/// `library.acquire_on_playlist_add` step as an edit through `updatePlaylist`,
/// so a missing track that a user keeps in a playlist is acquired at priority
/// `queue`.
///
/// # Parameters
///
/// - `authed`: the caller, who must own the playlist.
/// - `state`: the shared services.
/// - `id`: the playlist's client id (`pl_...`).
/// - `body`: the collection to append.
///
/// # Returns
///
/// How many tracks were appended and how many the playlist holds afterwards.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `not-found` (404) when the
/// playlist, album, playlist source or artist is unknown or outside the
/// caller's library, `unknown-source` (400) for a source this route does not
/// accept, and `internal` when a read, an index walk or a write fails.
#[utoipa::path(
    post,
    path = "/api/v1/playlists/{id}/tracks",
    tag = "playlists",
    params(("id" = String, Path, description = "The playlist's client id")),
    request_body = AddTracksRequest,
    responses(
        (status = 200, description = "The tracks the request appended and the playlist's size", body = AddTracksView),
        (status = 400, description = "The source is not one this route accepts", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "The playlist, or a collection in the source, is unknown to the caller", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn add_playlist_tracks(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<AddTracksRequest>,
) -> Result<Json<AddTracksView>, Problem> {
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
    let tracks = source_tracks(&state, user, &body.source).await?;
    let playlist = playlists
        .update(
            id,
            user,
            &PlaylistUpdate {
                add: tracks.clone(),
                ..PlaylistUpdate::default()
            },
        )
        .await
        .map_err(|error| edit_problem(&error, "adding tracks to a playlist"))?;
    acquire_playlist_additions(
        Some(state.services().orchestrator.as_ref()),
        user,
        authed.principal.can_acquire,
        &tracks,
    )
    .await;
    state.services().events.publish_for(
        Audience::User(user),
        ServerEvent::PlaylistChanged(PlaylistChanged {
            playlist_id: id.to_string(),
            change: "tracks-added".to_owned(),
        }),
    );
    Ok(Json(AddTracksView {
        added: tracks.len(),
        track_count: playlist.entries.len(),
    }))
}

/// Reads the tracks of the collection a source names, in the source's order.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `user`: the account whose personal library the collection is read from.
/// - `source`: the source the request named.
///
/// # Returns
///
/// The tracks, in the collection's own order. An album source is the album's
/// stored tracklist after the catalog completed its page.
///
/// # Errors
///
/// Answers `unknown-source` (400) for a source this route does not accept, and
/// `not-found` (404) for a collection that is unknown or outside the caller's
/// library.
async fn source_tracks(
    state: &NativeState,
    user: Uuid,
    source: &str,
) -> Result<Vec<EntityId>, Problem> {
    if let Some(value) = source.strip_prefix("album:") {
        return album_tracks(state, value).await;
    }
    let collection = library::collection_of(
        source,
        library::SongsQueue {
            sort: TrackSort::Title,
            genre: None,
        },
    )?;
    match collection {
        QueueCollection::Songs { .. }
        | QueueCollection::Liked
        | QueueCollection::Uploads
        | QueueCollection::ArtistTop(_) => Err(library::unknown_source(source)),
        _ => {
            let mut conn = state
                .database()
                .readers()
                .acquire()
                .await
                .map_err(|error| {
                    problem::internal("acquiring a reader for a playlist source", &error)
                })?;
            let items = query::queue(&mut conn, user, collection.clone())
                .await
                .map_err(|error| problem::internal("reading a playlist source", &error))?
                .ok_or_else(|| library::missing(collection))?;
            Ok(items.into_iter().map(|item| item.track_id).collect())
        }
    }
}

/// Reads an album's stored tracklist, completing its page from the catalog first.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `value`: the album id the source named.
///
/// # Returns
///
/// The album's tracks, in disc and track order.
///
/// # Errors
///
/// Answers `not-found` (404) when the value is not an album id or names no
/// stored album, and `internal` when a read or an index walk fails.
async fn album_tracks(state: &NativeState, value: &str) -> Result<Vec<EntityId>, Problem> {
    let catalog = state.services().catalog.as_ref();
    let id = entity_id(catalog, value, IdPrefix::Album).await?;
    catalog
        .ensure_album_page(id, std::time::Duration::from_secs(10))
        .await
        .map_err(|error| catalog_problem(&error))?;
    let mut conn = state
        .database()
        .readers()
        .acquire()
        .await
        .map_err(|error| problem::internal("acquiring a reader for an album", &error))?;
    let tracks = hearthsound_library::tracks::list_tracks_for_album(&mut conn, id)
        .await
        .map_err(|error| problem::internal("reading an album's tracks", &error))?;
    Ok(tracks.into_iter().map(|track| track.id).collect())
}

/// Maps a catalog failure onto the problem document the route answers.
///
/// # Parameters
///
/// - `error`: the failure the catalog layer reported.
///
/// # Returns
///
/// `not-found` when the album vanished between the redirect lookup and the walk,
/// and `internal` for anything else.
fn catalog_problem(error: &CatalogError) -> Problem {
    match error {
        CatalogError::NotFound => Problem::not_found("No album with that id is stored."),
        other => problem::internal("completing an album's page", other),
    }
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
/// - `operation`: what the handler was doing, for the logged internal failure.
///
/// # Returns
///
/// The problem: `not-found` for an unknown or foreign playlist,
/// `invalid-order` for a list that is not a permutation, and `internal` for a
/// failed read or write.
fn edit_problem(error: &ActivityError, operation: &'static str) -> Problem {
    match error {
        ActivityError::UnknownPlaylist { .. } | ActivityError::NotOwner { .. } => missing(),
        ActivityError::InvalidOrder { len } => Problem::invalid_order(format!(
            "`order` must list every position from 0 to {len} exactly once."
        )),
        other => problem::internal(operation, other),
    }
}
