//! `/api/v1/search`: one query across the caller's library and the catalog.
//!
//! A search answers stored rows. The caller's own library is read first; when
//! the catalog layer is enabled the first catalog instance in the
//! administrator's order is asked next and its hits are resolved into rows as
//! they arrive, so a result is always an id the rest of the API accepts. A hit
//! the library already holds is not repeated.
//!
//! `limit` caps each kind separately. A `cursor` continues one kind's list and
//! is therefore accepted only when the request names exactly one kind; it is an
//! opaque encoding of the number of hits already seen.

use axum::Json;
use axum::extract::State;
use hearthsound_core::{EntityId, EntityKind};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::QueryParams;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{AlbumSummary, ArtistSummary, TrackSummary, entity_facts};

/// The most hits a request returns per kind.
const DEFAULT_LIMIT: u32 = 20;

/// The largest `limit` a request may name.
const MAX_LIMIT: u32 = 60;

/// The query a search request carries.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchQuery {
    /// The text to search for.
    pub q: String,
    /// The kinds to return, comma separated: `artists`, `albums`, `tracks`.
    /// Absent returns all three.
    #[serde(default)]
    pub kinds: Option<String>,
    /// The most hits to return per kind, from 1 to 60; absent returns 20.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Opaque continuation of one kind's list, as a previous answer's
    /// `nextCursor` carried it. Accepted only with exactly one kind.
    #[serde(default)]
    pub cursor: Option<String>,
}

/// One search's answer.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SearchView {
    /// Matching artists, best first.
    pub artists: Vec<ArtistSummary>,
    /// Matching albums, best first.
    pub albums: Vec<AlbumSummary>,
    /// Matching tracks, best first.
    pub tracks: Vec<TrackSummary>,
    /// The continuation of the one kind the request narrowed to, when more of
    /// its hits remain; `null` otherwise.
    pub next_cursor: Option<String>,
}

/// Answers `GET /search`: one query across the library and the catalog.
///
/// # Parameters
///
/// - `authed`: the caller, whose library is searched first.
/// - `state`: the shared services.
/// - `query`: the text, the kinds, the per-kind limit and the cursor.
///
/// # Returns
///
/// Each kind's hits, best first, and the cursor for the next page of the one
/// kind the request narrowed to.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials, `invalid-request` (400) for a
/// kind this server does not know, a `limit` outside 1..=60, or a `cursor` that
/// is malformed or sent with more than one kind, and `internal` when the search
/// fails.
#[utoipa::path(
    get,
    path = "/api/v1/search",
    tag = "search",
    params(
        ("q" = String, Query, description = "The text to search for"),
        ("kinds" = Option<String>, Query, description = "The kinds to return, comma separated: `artists`, `albums`, `tracks`; absent returns all three"),
        ("limit" = Option<u32>, Query, description = "The most hits to return per kind, from 1 to 60; absent returns 20"),
        ("cursor" = Option<String>, Query, description = "Opaque continuation of one kind's list, from a previous answer's `nextCursor`"),
    ),
    responses(
        (status = 200, description = "The search's hits and the next cursor", body = SearchView),
        (status = 400, description = "A kind, a limit or a cursor the request may not use", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn search(
    authed: Authed,
    State(state): State<NativeState>,
    QueryParams(query): QueryParams<SearchQuery>,
) -> Result<Json<SearchView>, Problem> {
    let user = authed.user_id();
    let kinds = kinds_of(query.kinds.as_deref())?;
    let limit = limit_of(query.limit)?;
    let offset = offset_of(query.cursor.as_deref(), &kinds)?;
    let text = query.q.trim();
    if text.is_empty() {
        return Ok(Json(SearchView {
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
            next_cursor: None,
        }));
    }
    let answer = state
        .services()
        .catalog
        .search(user, text, &kinds, limit, offset)
        .await
        .map_err(|error| problem::internal("searching the library and the catalog", &error))?;

    let artist_ids: Vec<EntityId> = answer
        .artists
        .iter()
        .map(|reference| reference.artist.id)
        .collect();
    let album_ids: Vec<EntityId> = answer
        .albums
        .iter()
        .map(|reference| reference.album.id)
        .collect();
    let track_ids: Vec<EntityId> = answer
        .tracks
        .iter()
        .map(|reference| reference.track.id)
        .collect();
    let artist_facts = entity_facts(&state, user, EntityKind::Artist, &artist_ids).await?;
    let album_facts = entity_facts(&state, user, EntityKind::Album, &album_ids).await?;
    let track_facts = entity_facts(&state, user, EntityKind::Track, &track_ids).await?;

    let next_cursor = next_cursor(
        &kinds,
        match kinds.as_slice() {
            [EntityKind::Artist] => answer.artists.len(),
            [EntityKind::Album] => answer.albums.len(),
            [EntityKind::Track] => answer.tracks.len(),
            _ => 0,
        },
        limit,
        offset,
    );
    Ok(Json(SearchView {
        artists: answer
            .artists
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                ArtistSummary::of(
                    reference,
                    crate::views::Facts::of(&artist_facts, reference.artist.id),
                    hit_score(offset, position),
                )
            })
            .collect(),
        albums: answer
            .albums
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                AlbumSummary::of(
                    reference,
                    crate::views::Facts::of(&album_facts, reference.album.id),
                    hit_score(offset, position),
                )
            })
            .collect(),
        tracks: answer
            .tracks
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                TrackSummary::of(
                    reference,
                    crate::views::Facts::of(&track_facts, reference.track.id),
                    hit_score(offset, position),
                )
            })
            .collect(),
        next_cursor,
    }))
}

/// Scores one search hit from the position it came back at.
///
/// The catalog layer answers each kind best first, so the position is the only
/// relevance signal an answer carries today. The first hit of the first page is
/// `1.0`; each later one is the reciprocal of its one-based rank, which is
/// strictly descending and keeps a later page's hits below an earlier page's.
///
/// # Parameters
///
/// - `offset`: how many hits of that kind the request skipped.
/// - `position`: the hit's zero-based position in this page.
///
/// # Returns
///
/// The hit's score, from `1.0` down towards zero.
fn hit_score(offset: u32, position: usize) -> f32 {
    1.0 / (offset as f32 + position as f32 + 1.0)
}

/// Reads the kinds a request asks for.
///
/// # Parameters
///
/// - `kinds`: the comma-separated value the request carried.
///
/// # Returns
///
/// The kinds, in the order the caller named them, without repetitions; every
/// kind when the request names none.
///
/// # Errors
///
/// Answers `invalid-request` (400) for a token that names no kind.
fn kinds_of(kinds: Option<&str>) -> Result<Vec<EntityKind>, Problem> {
    let Some(kinds) = kinds.map(str::trim).filter(|kinds| !kinds.is_empty()) else {
        return Ok(vec![
            EntityKind::Artist,
            EntityKind::Album,
            EntityKind::Track,
        ]);
    };
    let mut out = Vec::new();
    for token in kinds.split(',') {
        let kind = match token.trim() {
            "artists" => EntityKind::Artist,
            "albums" => EntityKind::Album,
            "tracks" => EntityKind::Track,
            other => {
                return Err(Problem::invalid_request(format!(
                    "`kinds` does not name `{other}`; expected `artists`, `albums` or `tracks`."
                )));
            }
        };
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    Ok(out)
}

/// Reads a search request's `limit`.
///
/// # Parameters
///
/// - `limit`: the value the request carried.
///
/// # Returns
///
/// The most hits to return per kind: 20 when the request names none.
///
/// # Errors
///
/// Answers `invalid-request` (400) when the value is zero or larger than 60.
fn limit_of(limit: Option<u32>) -> Result<u32, Problem> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
        Some(limit) => Err(Problem::invalid_request(format!(
            "`limit` must be from 1 to {MAX_LIMIT}; `{limit}` is not."
        ))),
    }
}

/// Reads the number of hits already seen from a request's cursor.
///
/// # Parameters
///
/// - `cursor`: the value the request carried.
/// - `kinds`: the kinds the request narrowed to.
///
/// # Returns
///
/// The number of hits to skip: zero when the request carries no cursor.
///
/// # Errors
///
/// Answers `invalid-request` (400) when a cursor is sent with more than one kind
/// or does not decode to an offset.
fn offset_of(cursor: Option<&str>, kinds: &[EntityKind]) -> Result<u32, Problem> {
    // A client that has no cursor to send may still send `cursor=`; a blank
    // value is the first page, not a malformed cursor.
    let Some(cursor) = cursor.map(str::trim).filter(|cursor| !cursor.is_empty()) else {
        return Ok(0);
    };
    if kinds.len() != 1 {
        return Err(Problem::invalid_request(
            "`cursor` may only be sent with exactly one kind: its offset is per kind.",
        ));
    }
    let invalid = || Problem::invalid_request("`cursor` is not a cursor this server issued.");
    let bytes = data_encoding::BASE64URL_NOPAD
        .decode(cursor.as_bytes())
        .map_err(|_| invalid())?;
    std::str::from_utf8(&bytes)
        .map_err(|_| invalid())?
        .parse::<u32>()
        .map_err(|_| invalid())
}

/// Builds the continuation of a one-kind answer.
///
/// # Parameters
///
/// - `kinds`: the kinds the request narrowed to.
/// - `returned`: how many hits the answer holds.
/// - `limit`: the most hits the request asked for.
/// - `offset`: the number of hits the request skipped.
///
/// # Returns
///
/// The cursor for the next page, when the request named exactly one kind and
/// that kind returned a full page; `None` otherwise.
fn next_cursor(kinds: &[EntityKind], returned: usize, limit: u32, offset: u32) -> Option<String> {
    if kinds.len() != 1 || returned as u32 != limit {
        return None;
    }
    let next = offset.saturating_add(limit);
    Some(data_encoding::BASE64URL_NOPAD.encode(next.to_string().as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_is_searched_by_default() {
        assert_eq!(
            kinds_of(None).unwrap(),
            vec![EntityKind::Artist, EntityKind::Album, EntityKind::Track]
        );
        assert_eq!(kinds_of(Some("  ")).unwrap().len(), 3);
    }

    #[test]
    fn a_named_kind_list_keeps_its_order_without_repeats() {
        assert_eq!(
            kinds_of(Some("tracks,artists,tracks")).unwrap(),
            vec![EntityKind::Track, EntityKind::Artist]
        );
    }

    #[test]
    fn a_kind_nobody_knows_is_an_invalid_request() {
        let problem = kinds_of(Some("tracks,playlists")).unwrap_err();
        assert_eq!(problem.status, 400);
        assert!(problem.code == "invalid-request");
    }

    #[test]
    fn the_limit_defaults_to_twenty_and_stays_in_range() {
        assert_eq!(limit_of(None).unwrap(), DEFAULT_LIMIT);
        assert_eq!(limit_of(Some(1)).unwrap(), 1);
        assert_eq!(limit_of(Some(MAX_LIMIT)).unwrap(), MAX_LIMIT);
        assert_eq!(limit_of(Some(0)).unwrap_err().status, 400);
        assert_eq!(limit_of(Some(MAX_LIMIT + 1)).unwrap_err().status, 400);
    }

    #[test]
    fn a_cursor_carries_the_offset_it_starts_at() {
        let cursor = data_encoding::BASE64URL_NOPAD.encode(b"40");
        assert_eq!(offset_of(Some(&cursor), &[EntityKind::Album]).unwrap(), 40);
        assert_eq!(offset_of(None, &[EntityKind::Album]).unwrap(), 0);
        assert_eq!(
            offset_of(Some("  "), &[EntityKind::Album, EntityKind::Track]).unwrap(),
            0
        );
    }

    #[test]
    fn a_cursor_needs_exactly_one_kind_and_a_known_encoding() {
        let cursor = data_encoding::BASE64URL_NOPAD.encode(b"40");
        let problem =
            offset_of(Some(&cursor), &[EntityKind::Album, EntityKind::Track]).unwrap_err();
        assert_eq!(problem.status, 400);
        assert!(problem.code == "invalid-request");

        let problem = offset_of(Some("not-base64!"), &[EntityKind::Track]).unwrap_err();
        assert_eq!(problem.status, 400);
        let problem = offset_of(
            Some(&data_encoding::BASE64URL_NOPAD.encode(b"ten")),
            &[EntityKind::Track],
        )
        .unwrap_err();
        assert_eq!(problem.status, 400);
    }

    #[test]
    fn a_hit_scores_by_its_rank_across_pages() {
        assert_eq!(hit_score(0, 0), 1.0);
        assert_eq!(hit_score(0, 1), 0.5);
        assert_eq!(hit_score(20, 0), 1.0 / 21.0);
        assert!(hit_score(0, 0) > hit_score(0, 1));
        assert!(hit_score(0, 19) > hit_score(20, 0));
        assert!(hit_score(20, 39) > 0.0);
    }

    #[test]
    fn a_full_single_kind_page_names_its_successor() {
        assert!(next_cursor(&[EntityKind::Track], 20, 20, 0).is_some());
        assert!(next_cursor(&[EntityKind::Track], 7, 20, 0).is_none());
        assert!(next_cursor(&[EntityKind::Track, EntityKind::Album], 20, 20, 0).is_none());
        let cursor = next_cursor(&[EntityKind::Track], 20, 20, 40).unwrap();
        assert_eq!(
            data_encoding::BASE64URL_NOPAD
                .decode(cursor.as_bytes())
                .unwrap(),
            b"60"
        );
    }
}
