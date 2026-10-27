//! The `getCoverArt` endpoint: the image of one stored entity.
//!
//! `getCoverArt` is one of the three binary endpoints, so its failures are HTTP
//! status codes rather than a Subsonic envelope at HTTP 200: a client that
//! treats any 200 body as an image would render an error document as one.
//!
//! The `id` names a stored track, album, artist or playlist:
//!
//! - a track is served the cover of its album, or its own image when it has no
//!   album;
//! - an album is served its cover;
//! - an artist is served its portrait, or the cover of its newest album that has
//!   one when the server holds no portrait;
//! - a playlist is served the image of its first entry that has one, which is
//!   what a client shows for a saved queue.
//!
//! The catalog layer's [`ImageService`](hearthsound_catalog::images::ImageService)
//! owns the choice: it orders the stored images by the administrator's
//! precedence and artwork layer order, fetches the bytes a provider described
//! when the server has not stored them yet, and scales them to the requested
//! size. This endpoint therefore only parses the request and writes the
//! response; an image the server could not produce is a `404`, and a provider
//! failure with nothing stored is a `503`.
//!
//! `size` is the requested size in pixels; a request for nothing, or for more
//! than the largest cached size, is served the stored bytes.
//!
//! A served image carries its MIME type, its length, the strong `ETag` the image
//! service computed over the bytes, and `Cache-Control: public, max-age=86400`;
//! a request whose `If-None-Match` already names that tag is answered with HTTP
//! 304 and no body. An entity with no image is answered with HTTP 404 and a
//! `Cache-Control` that makes the client revalidate, and a provider failure is
//! HTTP 503 with `Retry-After`. A refusal is HTTP 400 for a missing `id` or an
//! unparsable `size`. Credential failures are answered by the extractor before
//! the handler runs, with HTTP 401.

use axum::body::Body;
use axum::http::{
    HeaderMap, HeaderValue, StatusCode, header::CACHE_CONTROL, header::CONTENT_LENGTH,
    header::CONTENT_TYPE, header::ETAG, header::IF_NONE_MATCH, header::RETRY_AFTER,
};
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_catalog::CatalogError;
use hearthsound_catalog::images::ServedImage;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};

use crate::auth::BinaryCall;

/// The `Cache-Control` of an image a client may keep.
///
/// An image is fresh for a day; afterwards the strong `ETag` revalidates it, so
/// a client never renders a changed image from its own cache without asking.
/// The value is `public` because an image is the same for every user of the
/// server.
const STORED_CACHE_CONTROL: &str = "public, max-age=86400";

/// The `Cache-Control` of a refusal.
///
/// A stored entity that has no image today may gain one when its next track is
/// imported, so the client revalidates rather than caching the refusal.
const NOT_FOUND_CACHE_CONTROL: &str = "no-cache";

/// How long a client waits before asking again for an image a provider could
/// not deliver.
const RETRY_AFTER_SECONDS: u64 = 5;

/// Answers `getCoverArt`: the image of one stored entity.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` is required and names a track, album,
///   artist or playlist; `size` is the requested size in pixels.
///
/// # Returns
///
/// The image bytes with their type, length, `ETag`, and `Cache-Control`, HTTP
/// 304 when the client's `If-None-Match` already has the image, or one of the
/// refusal codes described in the module header. The body of a refusal is empty.
pub async fn get_cover_art(call: BinaryCall) -> HttpResponse {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return bad_request("id");
    };
    let size = match requested_size(&call) {
        Ok(size) => size,
        Err(()) => return bad_request("size"),
    };
    let Ok(id) = EntityId::parse(value) else {
        tracing::debug!(
            id = value,
            "a cover art request named an id that is not an entity id"
        );
        return not_found(&value);
    };
    match id.kind() {
        IdPrefix::Track | IdPrefix::Album | IdPrefix::Artist => entity_image(&call, id, size).await,
        IdPrefix::Playlist => playlist(&call, id, size).await,
        _ => not_found(&value),
    }
}

/// Serves the image of one stored entity.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `entity`: the entity the request named.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image response, or one of the refusal codes of the module header.
async fn entity_image(call: &BinaryCall, entity: EntityId, size: Option<u32>) -> HttpResponse {
    let Some(kind) = entity.kind().entity_kind() else {
        return not_found(&entity);
    };
    let row = match call.state().catalog().canonical(kind, entity).await {
        Ok(Some(row)) => row,
        Ok(None) => return not_found(&entity),
        Err(error) => return unavailable(&entity, &error),
    };
    match call
        .state()
        .catalog()
        .images()
        .serve(kind, row.uuid(), size)
        .await
    {
        Ok(Some(served)) => image_response(call.headers(), served),
        Ok(None) => not_found(&entity),
        Err(error) => unavailable(&entity, &error),
    }
}

/// Serves the cover image of a playlist.
///
/// A playlist has no image of its own, so it answers with the image of the
/// first entry that has one, which is what a client shows for a saved queue.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `playlist_id`: the playlist the request named.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image response, HTTP 404 when the playlist does not exist, when the
/// caller may not see it, or when none of its entries has an image, or another
/// refusal code of the module header.
async fn playlist(call: &BinaryCall, playlist_id: EntityId, size: Option<u32>) -> HttpResponse {
    let viewer = call.principal().user_id.uuid();
    let playlist = match call
        .state()
        .activity()
        .playlists()
        .get(playlist_id, viewer)
        .await
    {
        Ok(Some(playlist)) => playlist,
        Ok(None) => return not_found(&playlist_id),
        Err(error) => return internal("reading a playlist for its cover", &error),
    };
    let images = call.state().catalog().images();
    for entry in &playlist.entries {
        match images
            .serve(EntityKind::Track, entry.track_id.uuid(), size)
            .await
        {
            Ok(Some(served)) => return image_response(call.headers(), served),
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    playlist = %playlist_id,
                    track = %entry.track_id,
                    %error,
                    "an entry of a playlist has no cover image"
                );
            }
        }
    }
    not_found(&playlist_id)
}

/// Writes one image with the headers every served image carries.
///
/// # Parameters
///
/// - `headers`: the request's head, which may carry `If-None-Match`.
/// - `served`: the image to write.
///
/// # Returns
///
/// HTTP 200 with the bytes, or HTTP 304 with no body when the request's
/// `If-None-Match` names the image's tag.
fn image_response(headers: &HeaderMap, served: ServedImage) -> HttpResponse {
    let tag = format!("\"{}\"", served.etag);
    let fresh = is_fresh(headers, &tag);
    let length = served.bytes.len();
    let (status, body) = if fresh {
        (StatusCode::NOT_MODIFIED, Body::empty())
    } else {
        (StatusCode::OK, Body::from(served.bytes))
    };
    let mut builder = HttpResponse::builder()
        .status(status)
        .header(CACHE_CONTROL, STORED_CACHE_CONTROL)
        .header(ETAG, tag.as_str());
    if !fresh {
        builder = builder
            .header(CONTENT_TYPE, content_type(&served.content_type))
            .header(CONTENT_LENGTH, length);
    }
    builder
        .body(body)
        .unwrap_or_else(|error| internal("building a cover art response", &error))
}

/// Returns the value of a response's `Content-Type`.
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
/// The value, or `application/octet-stream` when it cannot be a header value.
fn content_type(mime_type: &str) -> HeaderValue {
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
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The size in pixels, or `None` when the parameter is absent or zero, which
/// clients send to mean "any size".
///
/// # Errors
///
/// Returns `Err(())` when the parameter is present but is not a size in pixels.
fn requested_size(call: &BinaryCall) -> Result<Option<u32>, ()> {
    let Some(value) = call.get("size") else {
        return Ok(None);
    };
    match value.trim().parse::<u32>() {
        Ok(0) => Ok(None),
        Ok(size) => Ok(Some(size)),
        Err(_) => Err(()),
    }
}

/// Answers that an image could not be produced right now.
///
/// # Parameters
///
/// - `id`: the entity the client asked cover art of.
/// - `error`: the failure the catalog layer reported.
///
/// # Returns
///
/// HTTP 503 with `Retry-After`, because a later request may get the image: the
/// provider may answer then, or the stored bytes may have arrived meanwhile.
fn unavailable(id: &dyn std::fmt::Display, error: &CatalogError) -> HttpResponse {
    tracing::warn!(id = %id, error = %error, "could not serve a cover image");
    HttpResponse::builder()
        .status(StatusCode::SERVICE_UNAVAILABLE)
        .header(RETRY_AFTER, RETRY_AFTER_SECONDS)
        .body(Body::empty())
        .unwrap_or_else(|error| internal("building a cover art refusal", &error))
}

/// Answers that a required parameter is missing or unusable.
///
/// # Parameters
///
/// - `parameter`: the parameter that could not be read.
///
/// # Returns
///
/// HTTP 400 with an empty body.
fn bad_request(parameter: &'static str) -> HttpResponse {
    tracing::debug!(parameter, "a cover art request carried a bad parameter");
    StatusCode::BAD_REQUEST.into_response()
}

/// Answers that the id names no entity with cover art.
///
/// # Parameters
///
/// - `id`: the id the request named, for the log line.
///
/// # Returns
///
/// HTTP 404 with an empty body and a `Cache-Control` that makes the client
/// revalidate, because an entity the server knows may gain an image later.
fn not_found(id: &dyn std::fmt::Display) -> HttpResponse {
    tracing::debug!(id = %id, "no cover art for that id");
    HttpResponse::builder()
        .status(StatusCode::NOT_FOUND)
        .header(CACHE_CONTROL, NOT_FOUND_CACHE_CONTROL)
        .body(Body::empty())
        .unwrap_or_else(|error| internal("building a cover art refusal", &error))
}

/// Answers that this server could not produce the image.
///
/// # Parameters
///
/// - `context`: what the server was doing, for the log line.
/// - `error`: the failure, for the log line.
///
/// # Returns
///
/// HTTP 500 with an empty body.
fn internal(context: &'static str, error: &dyn std::fmt::Display) -> HttpResponse {
    tracing::error!(context, error = %error, "cover art failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the request head a revalidation sends.
    ///
    /// # Parameters
    ///
    /// - `tag`: the entity tag the client stored.
    fn head(tag: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_str(tag).unwrap());
        headers
    }

    #[test]
    fn a_stored_tag_is_recognized() {
        assert!(is_fresh(&head("\"abc\""), "\"abc\""));
        assert!(is_fresh(&head("W/\"abc\""), "\"abc\""));
        assert!(is_fresh(&head("*"), "\"abc\""));
        assert!(!is_fresh(&head("\"def\""), "\"abc\""));
        assert!(!is_fresh(&HeaderMap::new(), "\"abc\""));
    }
}
