//! `/api/v1/images/{id}`: the stored picture of an entity.
//!
//! Every image the server shows is stored and served from its own store - a
//! scan reads a file's cover, the indexer stores what a provider named - so this
//! endpoint is a read and, for a rendition other than the original, a thumbnail.
//! The bytes and headers are the ones `/rest/getCoverArt` answers: the image's
//! media type, its length, a strong `ETag` over the bytes, and a `Cache-Control`;
//! a request whose `If-None-Match` already names that tag is answered with HTTP
//! 304 and no body. An entity with no image is HTTP 404 with `no-cache`, and an
//! image that cannot be fetched right now is HTTP 503 with `Retry-After`.
//!
//! A track's image is its album's cover, an artist's is its portrait, and a
//! playlist's is the first entry's image. A merged-away id answers the surviving
//! row's image.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH, RETRY_AFTER,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hearthsound_catalog::CatalogError;
use hearthsound_catalog::images::ServedImage;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::auth::Authed;
use crate::extract::QueryParams;
use crate::state::NativeState;

/// The `Cache-Control` of an image a client may keep.
///
/// A stored image is fresh for a day; afterwards the strong `ETag` revalidates
/// it, so a client never renders a changed image from its own cache without
/// asking. The value is `public` because an image is the same for every user of
/// the server.
const STORED_CACHE_CONTROL: &str = "public, max-age=86400";

/// The `Cache-Control` of a refusal that says the entity has no image.
///
/// The entity may gain one when its page is indexed or its next file is
/// imported, so the client asks again instead of remembering the refusal.
const NOT_FOUND_CACHE_CONTROL: &str = "no-cache";

/// How long a client waits before asking again for an image that could not be
/// fetched.
const RETRY_AFTER_SECONDS: u64 = 5;

/// The query an image request carries.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImageQuery {
    /// The requested size in pixels; absent or zero asks for any size.
    #[serde(default)]
    pub size: Option<String>,
}

/// Answers `GET /images/{id}`: one entity's stored picture.
///
/// # Parameters
///
/// - `authed`: the caller, whose playlists may be named.
/// - `state`: the shared services.
/// - `id`: the entity's client id (`tr_`, `al_`, `ar_` or `pl_...`); a
///   merged-away id answers the surviving row's image.
/// - `query`: the requested size in pixels.
/// - `headers`: the request's head, which may carry `If-None-Match`.
///
/// # Returns
///
/// The image with its type, length, `ETag` and `Cache-Control`, HTTP 304 when
/// the client's `If-None-Match` already has it, HTTP 400 for an unusable
/// `size`, HTTP 404 when the entity has no image, or HTTP 503 with
/// `Retry-After` when the image cannot be fetched right now. A refusal has an
/// empty body.
#[utoipa::path(
    get,
    path = "/api/v1/images/{id}",
    tag = "images",
    params(
        ("id" = String, Path, description = "The entity's client id"),
        ("size" = Option<u32>, Query, description = "The requested size in pixels; absent or zero asks for any size"),
    ),
    responses(
        (status = 200, description = "The image bytes"),
        (status = 304, description = "The client's cached image is current"),
        (status = 400, description = "`size` is not a size in pixels"),
        (status = 401, description = "No valid credential"),
        (status = 404, description = "The entity has no image"),
        (status = 503, description = "The image cannot be fetched right now"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn entity_image(
    authed: Authed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    QueryParams(query): QueryParams<ImageQuery>,
    headers: HeaderMap,
) -> Response {
    let Ok(entity) = EntityId::parse(&id) else {
        tracing::debug!(id, "an image request named an id that is not a client id");
        return not_found();
    };
    let size = match requested_size(query.size.as_deref()) {
        Ok(size) => size,
        Err(()) => return bad_request(),
    };
    let catalog = state.services().catalog.as_ref();
    let image = match entity.kind() {
        IdPrefix::Track => serve_entity(catalog, EntityKind::Track, entity, size).await,
        IdPrefix::Album => serve_entity(catalog, EntityKind::Album, entity, size).await,
        IdPrefix::Artist => serve_entity(catalog, EntityKind::Artist, entity, size).await,
        IdPrefix::Playlist => {
            let playlists = state.activity().playlists();
            match playlists.get(entity, authed.user_id()).await {
                Ok(Some(playlist)) => {
                    let entries: Vec<uuid::Uuid> = playlist
                        .track_ids()
                        .into_iter()
                        .map(|id| id.uuid())
                        .collect();
                    catalog.images().playlist(&entries).await
                }
                Ok(None) => return not_found(),
                Err(error) => {
                    tracing::error!(%error, "could not read a playlist's image");
                    return unavailable(RETRY_AFTER_SECONDS);
                }
            }
        }
        IdPrefix::User
        | IdPrefix::Acquisition
        | IdPrefix::ApiKey
        | IdPrefix::AppPassword
        | IdPrefix::LibraryFolder
        | IdPrefix::Upload
        | IdPrefix::PluginRepository => return not_found(),
    };
    match image {
        Ok(Some(served)) => image_response(&headers, served),
        Ok(None) | Err(CatalogError::NotFound) => not_found(),
        Err(CatalogError::RateLimited { retry_after_ms, .. }) => {
            unavailable(retry_after_ms.div_ceil(1_000).max(1))
        }
        Err(error) => {
            tracing::warn!(%error, "could not serve an entity image");
            unavailable(RETRY_AFTER_SECONDS)
        }
    }
}

/// Serves a track's, album's or artist's image through its surviving row.
///
/// # Parameters
///
/// - `catalog`: the catalog layer, which owns the redirects and the store.
/// - `kind`: the entity kind.
/// - `id`: the id the request named.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image, or `None` when the id names no stored row or the row has none.
///
/// # Errors
///
/// Returns [`CatalogError`] when a read or a fetch fails.
async fn serve_entity(
    catalog: &hearthsound_catalog::CatalogService,
    kind: EntityKind,
    id: EntityId,
    size: Option<u32>,
) -> Result<Option<ServedImage>, CatalogError> {
    let Some(canonical) = catalog.canonical(kind, id).await? else {
        return Ok(None);
    };
    catalog.images().serve(kind, canonical.uuid(), size).await
}

/// Writes one image, or answers that the client already has it.
///
/// # Parameters
///
/// - `headers`: the request's head, which may carry `If-None-Match`.
/// - `served`: the image and its validator.
///
/// # Returns
///
/// HTTP 200 with the bytes, or HTTP 304 with no body when the request's
/// `If-None-Match` names the image's tag.
fn image_response(headers: &HeaderMap, served: ServedImage) -> Response {
    let ServedImage {
        bytes,
        content_type,
        etag,
    } = served;
    let etag = format!("\"{etag}\"");
    let fresh = is_fresh(headers, &etag);
    let length = bytes.len();
    let (status, body) = if fresh {
        (StatusCode::NOT_MODIFIED, Body::empty())
    } else {
        (StatusCode::OK, Body::from(bytes))
    };
    let mut builder = Response::builder()
        .status(status)
        .header(CACHE_CONTROL, STORED_CACHE_CONTROL)
        .header(ETAG, etag.as_str());
    if !fresh {
        builder = builder
            .header(CONTENT_TYPE, header_value(&content_type))
            .header(CONTENT_LENGTH, length);
    }
    builder.body(body).unwrap_or_else(|error| {
        tracing::error!(%error, "could not build an image response");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })
}

/// Returns a MIME type as a header value, falling back to a generic type.
///
/// A MIME type that arrived from a provider and is not a valid header value
/// would turn the whole response into an error, so an unusable value falls back
/// to a generic content type: bytes a client can display are more useful than a
/// failed request.
///
/// # Parameters
///
/// - `mime_type`: the image's MIME type.
///
/// # Returns
///
/// The value, or `application/octet-stream` when it cannot be one.
fn header_value(mime_type: &str) -> HeaderValue {
    HeaderValue::from_str(mime_type)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

/// Reports whether a request already has the image it asked for.
///
/// `If-None-Match` is a list of entity tags, each optionally marked weak, and
/// `*` matches any image. The comparison is the weak one that header prescribes
/// for a GET, so a client that stored this server's tag unchanged, or marked it
/// weak itself, is answered with HTTP 304.
///
/// # Parameters
///
/// - `headers`: the request's head.
/// - `etag`: the served image's tag, quoted.
///
/// # Returns
///
/// `true` when the header names the image.
fn is_fresh(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// Reads the requested image size.
///
/// # Parameters
///
/// - `size`: the value the request carried.
///
/// # Returns
///
/// The size in pixels, or `None` when the parameter is absent or zero, which
/// clients send to mean "any size".
///
/// # Errors
///
/// Returns `Err(())` when the parameter is present but is not a size in pixels.
fn requested_size(size: Option<&str>) -> Result<Option<u32>, ()> {
    let Some(value) = size else {
        return Ok(None);
    };
    match value.trim().parse::<u32>() {
        Ok(0) => Ok(None),
        Ok(size) => Ok(Some(size)),
        Err(_) => Err(()),
    }
}

/// Answers that a parameter is unusable.
///
/// # Returns
///
/// HTTP 400 with an empty body.
fn bad_request() -> Response {
    StatusCode::BAD_REQUEST.into_response()
}

/// Answers that the id names no entity with an image.
///
/// # Returns
///
/// HTTP 404 with an empty body and `no-cache`.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)],
    )
        .into_response()
}

/// Answers that the image cannot be fetched right now.
///
/// # Parameters
///
/// - `seconds`: the number of seconds the client waits.
///
/// # Returns
///
/// HTTP 503 with an empty body and `Retry-After`.
fn unavailable(seconds: u64) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, seconds.to_string())],
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_zero_size_asks_for_the_original() {
        assert_eq!(requested_size(None), Ok(None));
        assert_eq!(requested_size(Some("0")), Ok(None));
        assert_eq!(requested_size(Some(" 96 ")), Ok(Some(96)));
        assert_eq!(requested_size(Some("large")), Err(()));
    }

    #[test]
    fn a_weak_or_strong_tag_both_revalidate() {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"abc\""));
        assert!(is_fresh(&headers, "\"abc\""));

        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("W/\"abc\""));
        assert!(is_fresh(&headers, "\"abc\""));

        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(is_fresh(&headers, "\"abc\""));

        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        assert!(!is_fresh(&headers, "\"abc\""));

        assert!(!is_fresh(&HeaderMap::new(), "\"abc\""));
    }

    #[test]
    fn a_refusal_that_names_no_image_is_not_cached() {
        let response = not_found();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()[CACHE_CONTROL], NOT_FOUND_CACHE_CONTROL);
    }
}
