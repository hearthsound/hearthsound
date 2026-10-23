//! `/api/v1/library`: the caller's own music, in the player's shape.
//!
//! The queue resource answers one collection in full so the web client can
//! hand the player a whole queue from a collection's Play or Shuffle button.
//! The library views page their lists; a queue cannot, because the player needs
//! every item and their order up front. The answer is the same item shape
//! `libraryItem` has in the client: the title, the artist, the album, the
//! length, the cover art, the availability, and the caller's rating and
//! feedback.
//!
//! Every source is read through the caller's personal library, never the server
//! scope: `songs` and `liked` mean that caller's library, and a playlist, album
//! or artist outside it answers `not-found`, so one account cannot queue
//! another account's collection.

use axum::Json;
use axum::extract::State;
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_library::query::{self, QueueCollection, QueueItem};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::QueryParams;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::parse_id;

/// The source that queues the caller's whole library.
const SOURCE_SONGS: &str = "songs";

/// The source that queues the caller's likes.
const SOURCE_LIKED: &str = "liked";

/// The source that queues the caller's uploads.
const SOURCE_UPLOADS: &str = "uploads";

/// The prefix of the source that queues a playlist.
const SOURCE_PLAYLIST: &str = "playlist";

/// The prefix of the source that queues an album.
const SOURCE_ALBUM: &str = "album";

/// The prefix of the source that queues an artist's top songs and the rest.
const SOURCE_ARTIST: &str = "artist";

/// The prefix of the source that queues an artist's top songs only.
const SOURCE_ARTIST_TOP: &str = "artist-top";

/// The query a queue request carries.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueQuery {
    /// The collection to queue: `songs`, `liked`, `uploads`,
    /// `playlist:<id>`, `album:<id>`, `artist:<id>` or `artist-top:<id>`.
    pub source: String,
    /// The track the player starts at; absent starts at the first item.
    #[serde(default)]
    pub from: Option<String>,
}

/// One track of a queue.
///
/// The fields are the ones a library row draws: the player streams by `id`,
/// shows `title`, `artist`, `album` and `coverArt`, uses `duration` for its
/// timeline, and reads `availability`, `rating`, `liked` and `disliked` for its
/// badges and controls.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueItemView {
    /// The track's client id (`tr_...`), which `/rest/stream` accepts.
    pub id: String,
    /// The track's title.
    pub title: String,
    /// The credited artists, comma separated; `null` when unknown.
    pub artist: Option<String>,
    /// The album's client id (`al_...`); `null` when the track has no album.
    pub album_id: Option<String>,
    /// The primary credited artist's client id (`ar_...`); `null` when the
    /// track has no credit.
    pub artist_id: Option<String>,
    /// The album's title; `null` when the track has no album.
    pub album: Option<String>,
    /// The track's length in seconds; `null` when unknown.
    pub duration: Option<i64>,
    /// The id to pass to `/rest/getCoverArt`: the album's id, or the track's
    /// own id when it has no album.
    pub cover_art: String,
    /// The track's availability: `local`, `cached`, `acquiring`, `remote` or
    /// `unavailable`.
    pub availability: String,
    /// The caller's rating, from 1 to 5; `null` when the track is unrated.
    pub rating: Option<i64>,
    /// Whether the caller liked the track.
    pub liked: bool,
    /// Whether the caller disliked the track.
    pub disliked: bool,
}

impl QueueItemView {
    /// Builds the view of one queue item.
    ///
    /// # Parameters
    ///
    /// - `item`: the repository row.
    ///
    /// # Returns
    ///
    /// The view, with the duration in whole seconds.
    fn of(item: QueueItem) -> QueueItemView {
        QueueItemView {
            id: item.track_id.to_string(),
            title: item.title,
            artist: item.artist,
            album_id: item.album_id.map(|id| id.to_string()),
            artist_id: item.artist_id.map(|id| id.to_string()),
            album: item.album,
            duration: item.duration_ms.map(|millis| millis / 1000),
            cover_art: item.album_id.unwrap_or(item.track_id).to_string(),
            availability: item.availability.as_str().to_owned(),
            rating: item.rating,
            liked: item.liked,
            disliked: item.disliked,
        }
    }
}

/// A whole collection, ready to hand to the player.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QueueView {
    /// Every track of the collection, in the collection's own order.
    pub items: Vec<QueueItemView>,
    /// The position of the requested `from` track; `0` when none was named.
    pub start_index: usize,
    /// The source the items were read from, echoed back.
    pub source: String,
}

/// Reads the collection one queue source names.
///
/// # Parameters
///
/// - `source`: the source the request named.
///
/// # Returns
///
/// The collection to read.
///
/// # Errors
///
/// Answers `unknown-source` when the name is not one of the listed sources, and
/// `not-found` when a prefixed source does not carry a valid id of its kind.
fn collection_of(source: &str) -> Result<QueueCollection, Problem> {
    match source {
        SOURCE_SONGS => Ok(QueueCollection::Songs),
        SOURCE_LIKED => Ok(QueueCollection::Liked),
        SOURCE_UPLOADS => Ok(QueueCollection::Uploads),
        _ => {
            let Some((kind, value)) = source.split_once(':') else {
                return Err(unknown_source(source));
            };
            match kind {
                SOURCE_PLAYLIST => Ok(QueueCollection::Playlist(entity(
                    value,
                    IdPrefix::Playlist,
                )?)),
                SOURCE_ALBUM => Ok(QueueCollection::Album(entity(value, IdPrefix::Album)?)),
                SOURCE_ARTIST => Ok(QueueCollection::Artist(entity(value, IdPrefix::Artist)?)),
                SOURCE_ARTIST_TOP => {
                    Ok(QueueCollection::ArtistTop(entity(value, IdPrefix::Artist)?))
                }
                _ => Err(unknown_source(source)),
            }
        }
    }
}

/// Reads one client id as an entity id of its kind.
///
/// # Parameters
///
/// - `value`: the id part of the source.
/// - `prefix`: the kind the source names.
///
/// # Returns
///
/// The entity id.
///
/// # Errors
///
/// Answers `not-found` when the value is not a client id of that kind.
fn entity(value: &str, prefix: IdPrefix) -> Result<EntityId, Problem> {
    parse_id(value, prefix).map(|uuid| EntityId::from_uuid(prefix, uuid))
}

/// The problem an unknown source answers with.
///
/// # Parameters
///
/// - `source`: the source the request named.
///
/// # Returns
///
/// The `unknown-source` problem.
fn unknown_source(source: &str) -> Problem {
    Problem::unknown_source(format!(
        "`{source}` is not a queue source; expected `songs`, `liked`, `uploads`, \
         `playlist:<id>`, `album:<id>`, `artist:<id>` or `artist-top:<id>`."
    ))
}

/// The problem a collection the caller may not see answers with.
///
/// # Parameters
///
/// - `collection`: the collection that was not found.
///
/// # Returns
///
/// The `not-found` problem.
fn missing(collection: QueueCollection) -> Problem {
    match collection {
        QueueCollection::Playlist(_) => {
            Problem::not_found("No playlist with that id is visible to this account.")
        }
        QueueCollection::Album(_) => {
            Problem::not_found("No album with that id is in this account's library.")
        }
        QueueCollection::Artist(_) | QueueCollection::ArtistTop(_) => {
            Problem::not_found("No artist with that id is in this account's library.")
        }
        QueueCollection::Songs | QueueCollection::Liked | QueueCollection::Uploads => {
            Problem::not_found("No collection lives at that source.")
        }
    }
}

/// Queues a whole collection for the caller's player.
///
/// One request answers every track of the collection, in the collection's own
/// order: the library's sort order for `songs`, the like order for `liked`, the
/// upload order for `uploads`, the playlist's own order, the album's disc and
/// track order, and the artist's play count order. Nothing is paged and nothing
/// is truncated.
///
/// # Parameters
///
/// - `authed`: the caller, whose library the collection is read from.
/// - `state`: the shared services.
/// - `query`: the source and, optionally, the track to start at.
///
/// # Returns
///
/// The collection's items, the position of `from` in them (`0` when absent),
/// and the source.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `unknown-source` (400) for a
/// source this server does not offer, `not-found` (404) when the playlist,
/// album or artist is unknown or outside the caller's library, `not-in-source`
/// (404) when `from` is not one of the collection's tracks, and `internal` when
/// the read fails.
#[utoipa::path(
    get,
    path = "/api/v1/library/queue",
    tag = "library",
    params(
        ("source" = String, Query, description = "The collection: `songs`, `liked`, `uploads`, `playlist:<id>`, `album:<id>`, `artist:<id>` or `artist-top:<id>`"),
        ("from" = Option<String>, Query, description = "The track id the player starts at; absent starts at the first item"),
    ),
    responses(
        (status = 200, description = "The whole collection, in its own order", body = QueueView),
        (status = 400, description = "The source is unknown", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "The playlist, album or artist is unknown, or `from` is not in the collection", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn collection_queue(
    authed: Authed,
    State(state): State<NativeState>,
    QueryParams(query): QueryParams<QueueQuery>,
) -> Result<Json<QueueView>, Problem> {
    let collection = collection_of(&query.source)?;
    let read = {
        let mut conn = state
            .database()
            .readers()
            .acquire()
            .await
            .map_err(|error| problem::internal("acquiring a reader for a queue", &error))?;
        query::queue(&mut conn, authed.user_id(), collection)
            .await
            .map_err(|error| problem::internal("reading a collection queue", &error))?
    };
    let Some(items) = read else {
        return Err(missing(collection));
    };
    let items: Vec<QueueItemView> = items.into_iter().map(QueueItemView::of).collect();
    let start_index = match query.from.as_deref() {
        None => 0,
        Some(from) => items
            .iter()
            .position(|item| item.id == from)
            .ok_or_else(|| {
                Problem::not_in_source(format!(
                    "`{from}` is not in the `{}` collection.",
                    query.source
                ))
            })?,
    };
    Ok(Json(QueueView {
        items,
        start_index,
        source: query.source,
    }))
}
