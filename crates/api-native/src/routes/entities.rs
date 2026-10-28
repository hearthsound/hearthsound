//! `/api/v1/artists`, `/api/v1/albums`, `/api/v1/tracks`: the entity pages.
//!
//! An entity is one stored row whatever surface named it: the ids in a page are
//! the row ids every other resource uses, and a merged-away id is answered the
//! row it now resolves to. A page is composed from what the server stored - the
//! indexer keeps a page's data fresh in the background - and the caller's own
//! library, feedback and ratings are reported beside it, from the same facts the
//! Subsonic surface reads.
//!
//! A page whose data the server never fetched is queued for the indexer and
//! answered with `complete: false`; the rest arrives as `entity.updated` once
//! stored, and the page re-reads.

use axum::Json;
use axum::extract::{Path, State};
use hearthsound_catalog::CatalogError;
use hearthsound_catalog::views::Viewer;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::QueryParams;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{
    AlbumSummary, ArtistRefView, ArtistSummary, LinkView, TrackSummary, entity_facts, entity_id,
};

/// The most top tracks a top-tracks request may ask for.
const MAX_TOP_TRACKS: u32 = 100;

/// One artist's page.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ArtistPage {
    /// The artist's client id (`ar_...`).
    pub id: String,
    /// The artist's display name.
    pub name: String,
    /// The ordering key derived from the name.
    pub sort_name: String,
    /// The biography a provider stated, when one is stored.
    pub biography: Option<String>,
    /// The external pages the artist is linked from.
    pub links: Vec<LinkView>,
    /// Whether the server holds a portrait.
    pub has_image: bool,
    /// Whether the caller's personal library holds the artist.
    pub in_library: bool,
    /// Whether the caller likes the artist.
    pub liked: bool,
    /// Whether the caller dislikes the artist.
    pub disliked: bool,
    /// The caller's rating, from 1 to 5.
    pub rating: Option<u8>,
    /// The artist's most popular tracks, at most five.
    pub top_tracks: Vec<TrackSummary>,
    /// The discography, one album per release group, newest first.
    pub albums: Vec<AlbumSummary>,
    /// Similar artists, at most twelve.
    pub similar_artists: Vec<ArtistSummary>,
    /// Whether the stored data covers the page.
    pub complete: bool,
}

/// One album's page.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AlbumPage {
    /// The album's client id (`al_...`).
    pub id: String,
    /// The album's title.
    pub title: String,
    /// The edition label, when the release states one.
    pub edition: Option<String>,
    /// The album artist as it is displayed.
    pub display_artist: Option<String>,
    /// The credited artists, in order.
    pub artists: Vec<ArtistRefView>,
    /// The release date as the provider stated it, when known.
    pub release_date: Option<String>,
    /// The release year, when known.
    pub year: Option<i64>,
    /// The release type: `album`, `ep`, `single` or a secondary type.
    pub album_type: Option<String>,
    /// The album's genres.
    pub genres: Vec<String>,
    /// How many tracks the release holds.
    pub track_count: Option<i64>,
    /// How long the album plays, in milliseconds, from the lengths the stored
    /// tracks state.
    pub duration_ms: Option<i64>,
    /// Whether the server holds a cover.
    pub has_image: bool,
    /// Whether the caller's personal library holds the album.
    pub in_library: bool,
    /// Whether the caller likes the album.
    pub liked: bool,
    /// Whether the caller dislikes the album.
    pub disliked: bool,
    /// The caller's rating, from 1 to 5.
    pub rating: Option<u8>,
    /// The album's tracks, in disc and track order.
    pub tracks: Vec<TrackSummary>,
    /// Other albums by the primary artist, at most twelve.
    pub more_by_artist: Vec<AlbumSummary>,
    /// Whether the stored data covers the page.
    pub complete: bool,
}

/// One artist's top tracks.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopTracksView {
    /// The tracks, most popular first.
    pub tracks: Vec<TrackSummary>,
}

/// The query a top-tracks request carries.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TopTracksQuery {
    /// How many tracks to return, from 1 to 100; absent returns 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Answers `GET /artists/{id}`: one artist's page.
///
/// # Parameters
///
/// - `authed`: the caller, whose library and feedback the page reports.
/// - `state`: the shared services.
/// - `id`: the artist's client id (`ar_...`); a merged-away id answers the
///   surviving artist.
///
/// # Returns
///
/// The artist's page.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `not-found` (404) when the id
/// names no stored artist, and `internal` when a read fails.
#[utoipa::path(
    get,
    path = "/api/v1/artists/{id}",
    tag = "entities",
    params(("id" = String, Path, description = "The artist's client id")),
    responses(
        (status = 200, description = "The artist's page", body = ArtistPage),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No artist with that id is stored", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn artist_page(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<Json<ArtistPage>, Problem> {
    let user = authed.user_id();
    let catalog = state.services().catalog.as_ref();
    let id = entity_id(catalog, &id, IdPrefix::Artist).await?;
    let viewer = Viewer {
        user,
        vendor: false,
    };
    let view = catalog
        .views()
        .artist(id, viewer)
        .await
        .map_err(page_problem)?
        .ok_or_else(|| missing("artist"))?;

    let mut artist_ids = vec![view.artist.id];
    artist_ids.extend(view.similar.iter().map(|reference| reference.artist.id));
    let artist_facts = entity_facts(&state, user, EntityKind::Artist, &artist_ids).await?;
    let track_ids: Vec<EntityId> = view
        .top_tracks
        .iter()
        .map(|reference| reference.track.id)
        .collect();
    let track_facts = entity_facts(&state, user, EntityKind::Track, &track_ids).await?;
    let album_ids: Vec<EntityId> = view
        .albums
        .iter()
        .map(|reference| reference.album.id)
        .collect();
    let album_facts = entity_facts(&state, user, EntityKind::Album, &album_ids).await?;

    let facts = crate::views::Facts::of(&artist_facts, view.artist.id);
    Ok(Json(ArtistPage {
        id: view.artist.id.to_string(),
        name: view.artist.name.clone(),
        sort_name: view.artist.sort_name.clone(),
        biography: view.artist.biography.clone(),
        links: view.links.iter().map(LinkView::from).collect(),
        has_image: view.has_image,
        in_library: facts.in_library,
        liked: facts.liked,
        disliked: facts.disliked,
        rating: facts.rating,
        top_tracks: view
            .top_tracks
            .iter()
            .map(|reference| {
                TrackSummary::of(
                    reference,
                    crate::views::Facts::of(&track_facts, reference.track.id),
                    1.0,
                )
            })
            .collect(),
        albums: view
            .albums
            .iter()
            .map(|reference| {
                AlbumSummary::of(
                    reference,
                    crate::views::Facts::of(&album_facts, reference.album.id),
                    1.0,
                )
            })
            .collect(),
        similar_artists: view
            .similar
            .iter()
            .map(|reference| {
                ArtistSummary::of(
                    reference,
                    crate::views::Facts::of(&artist_facts, reference.artist.id),
                    1.0,
                )
            })
            .collect(),
        complete: view.complete,
    }))
}

/// Answers `GET /artists/{id}/top-tracks`: one artist's most popular tracks.
///
/// # Parameters
///
/// - `authed`: the caller, whose library and feedback the tracks report.
/// - `state`: the shared services.
/// - `id`: the artist's client id (`ar_...`).
/// - `query`: how many tracks to return.
///
/// # Returns
///
/// The tracks, most popular first; empty when the artist has none stored.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `invalid-request` (400) when
/// `limit` is outside 1..=100, `not-found` (404) when the id names no stored
/// artist, and `internal` when a read fails.
#[utoipa::path(
    get,
    path = "/api/v1/artists/{id}/top-tracks",
    tag = "entities",
    params(
        ("id" = String, Path, description = "The artist's client id"),
        ("limit" = Option<u32>, Query, description = "How many tracks to return, from 1 to 100; absent returns 100"),
    ),
    responses(
        (status = 200, description = "The artist's top tracks", body = TopTracksView),
        (status = 400, description = "`limit` is outside 1..=100", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No artist with that id is stored", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn artist_top_tracks(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    QueryParams(query): QueryParams<TopTracksQuery>,
) -> Result<Json<TopTracksView>, Problem> {
    let limit = top_tracks_limit(query.limit)?;
    let user = authed.user_id();
    let catalog = state.services().catalog.as_ref();
    let id = entity_id(catalog, &id, IdPrefix::Artist).await?;
    let refs = catalog
        .views()
        .top_tracks(
            id,
            limit,
            Viewer {
                user,
                vendor: false,
            },
        )
        .await
        .map_err(page_problem)?;
    let track_ids: Vec<EntityId> = refs.iter().map(|reference| reference.track.id).collect();
    let facts = entity_facts(&state, user, EntityKind::Track, &track_ids).await?;
    let tracks = refs
        .iter()
        .map(|reference| {
            TrackSummary::of(
                reference,
                crate::views::Facts::of(&facts, reference.track.id),
                1.0,
            )
        })
        .collect();
    Ok(Json(TopTracksView { tracks }))
}

/// Answers `GET /albums/{id}`: one album's page.
///
/// # Parameters
///
/// - `authed`: the caller, whose library and feedback the page reports.
/// - `state`: the shared services.
/// - `id`: the album's client id (`al_...`); a merged-away id answers the
///   surviving album.
///
/// # Returns
///
/// The album's page.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `not-found` (404) when the id
/// names no stored album, and `internal` when a read fails.
#[utoipa::path(
    get,
    path = "/api/v1/albums/{id}",
    tag = "entities",
    params(("id" = String, Path, description = "The album's client id")),
    responses(
        (status = 200, description = "The album's page", body = AlbumPage),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No album with that id is stored", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn album_page(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<Json<AlbumPage>, Problem> {
    let user = authed.user_id();
    let catalog = state.services().catalog.as_ref();
    let id = entity_id(catalog, &id, IdPrefix::Album).await?;
    let view = catalog
        .views()
        .album(
            id,
            Viewer {
                user,
                vendor: false,
            },
        )
        .await
        .map_err(page_problem)?
        .ok_or_else(|| missing("album"))?;

    let mut album_ids = vec![view.album.id];
    album_ids.extend(
        view.more_by_artist
            .iter()
            .map(|reference| reference.album.id),
    );
    let album_facts = entity_facts(&state, user, EntityKind::Album, &album_ids).await?;
    let track_ids: Vec<EntityId> = view
        .tracks
        .iter()
        .map(|reference| reference.track.id)
        .collect();
    let track_facts = entity_facts(&state, user, EntityKind::Track, &track_ids).await?;

    let facts = crate::views::Facts::of(&album_facts, view.album.id);
    let duration_ms = view
        .tracks
        .iter()
        .filter_map(|reference| reference.track.duration_ms)
        .reduce(|total, length| total + length);
    let track_count = view
        .album
        .track_count
        .or_else(|| (!view.tracks.is_empty()).then_some(view.tracks.len() as i64));
    Ok(Json(AlbumPage {
        id: view.album.id.to_string(),
        title: view.album.title.clone(),
        edition: view.album.edition.clone(),
        display_artist: view.album.display_artist.clone(),
        artists: view.artists.iter().map(ArtistRefView::from).collect(),
        release_date: view.album.release_date.clone(),
        year: view.album.year,
        album_type: view.album.album_type.clone(),
        genres: view.genres.clone(),
        track_count,
        duration_ms,
        has_image: view.has_image,
        in_library: facts.in_library,
        liked: facts.liked,
        disliked: facts.disliked,
        rating: facts.rating,
        tracks: view
            .tracks
            .iter()
            .map(|reference| {
                TrackSummary::of(
                    reference,
                    crate::views::Facts::of(&track_facts, reference.track.id),
                    1.0,
                )
            })
            .collect(),
        more_by_artist: view
            .more_by_artist
            .iter()
            .map(|reference| {
                AlbumSummary::of(
                    reference,
                    crate::views::Facts::of(&album_facts, reference.album.id),
                    1.0,
                )
            })
            .collect(),
        complete: view.complete,
    }))
}

/// Answers `GET /tracks/{id}`: one track, in the shape every list uses.
///
/// # Parameters
///
/// - `authed`: the caller, whose library and feedback the track reports.
/// - `state`: the shared services.
/// - `id`: the track's client id (`tr_...`); a merged-away id answers the
///   surviving track.
///
/// # Returns
///
/// The track's summary.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `not-found` (404) when the id
/// names no stored track, and `internal` when a read fails.
#[utoipa::path(
    get,
    path = "/api/v1/tracks/{id}",
    tag = "entities",
    params(("id" = String, Path, description = "The track's client id")),
    responses(
        (status = 200, description = "The track", body = TrackSummary),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No track with that id is stored", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn track_page(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<Json<TrackSummary>, Problem> {
    let user = authed.user_id();
    let catalog = state.services().catalog.as_ref();
    let id = entity_id(catalog, &id, IdPrefix::Track).await?;
    let reference = catalog
        .views()
        .track(
            id,
            Viewer {
                user,
                vendor: false,
            },
        )
        .await
        .map_err(page_problem)?
        .ok_or_else(|| missing("track"))?;
    let facts = entity_facts(&state, user, EntityKind::Track, &[reference.track.id]).await?;
    let summary = TrackSummary::of(
        &reference,
        crate::views::Facts::of(&facts, reference.track.id),
        1.0,
    );
    Ok(Json(summary))
}

/// Reads a top-tracks request's `limit`.
///
/// # Parameters
///
/// - `limit`: the value the request carried.
///
/// # Returns
///
/// The number of tracks to read: 100 when the request names none.
///
/// # Errors
///
/// Answers `invalid-request` (400) when the value is zero or larger than 100.
fn top_tracks_limit(limit: Option<u32>) -> Result<u32, Problem> {
    match limit {
        None => Ok(MAX_TOP_TRACKS),
        Some(limit) if (1..=MAX_TOP_TRACKS).contains(&limit) => Ok(limit),
        Some(limit) => Err(Problem::invalid_request(format!(
            "`limit` must be from 1 to {MAX_TOP_TRACKS}; `{limit}` is not."
        ))),
    }
}

/// Maps a catalog failure onto the problem a page route answers.
///
/// # Parameters
///
/// - `error`: the failure the catalog layer reported.
///
/// # Returns
///
/// `not-found` for an entity that vanished between the redirect lookup and the
/// read, and `internal` for anything else.
fn page_problem(error: CatalogError) -> Problem {
    match error {
        CatalogError::NotFound => missing("entity"),
        other => problem::internal("composing an entity page", &other),
    }
}

/// The problem a path names no stored entity with.
///
/// # Parameters
///
/// - `kind`: the entity kind the path named, for the message.
///
/// # Returns
///
/// The `not-found` problem.
fn missing(kind: &str) -> Problem {
    Problem::not_found(format!("No {kind} with that id is stored."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_tracks_limit_defaults_to_a_hundred_and_stays_in_range() {
        assert_eq!(top_tracks_limit(None).unwrap(), MAX_TOP_TRACKS);
        assert_eq!(top_tracks_limit(Some(1)).unwrap(), 1);
        assert_eq!(
            top_tracks_limit(Some(MAX_TOP_TRACKS)).unwrap(),
            MAX_TOP_TRACKS
        );
        assert_eq!(top_tracks_limit(Some(0)).unwrap_err().status, 400);
        assert_eq!(top_tracks_limit(Some(101)).unwrap_err().status, 400);
    }
}
