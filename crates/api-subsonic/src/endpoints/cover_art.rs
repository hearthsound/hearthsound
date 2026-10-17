//! The `getCoverArt` endpoint: the cover image of a stored or catalog entity.
//!
//! `getCoverArt` is one of the three binary endpoints, so its failures are HTTP
//! status codes rather than a Subsonic envelope at HTTP 200: a client that
//! treats any 200 body as an image would render an error document as one.
//!
//! The `id` names either a stored entity (`tr_`, `al_`, or `ar_`) or a catalog
//! entity (`x_`):
//!
//! - a stored track is served the image of its preferred file: the embedded
//!   picture when the file carries one, otherwise the first sidecar image in the
//!   file's directory;
//! - a stored album is served its first track, in playing order, that has an
//!   image, and a stored artist the first track of its first album that has one,
//!   because neither kind stores an image of its own;
//! - a stored entity with no image of its own but a `p:<slug>` alias - one the
//!   catalog layer materialized - is served the image its issuing instance
//!   describes, exactly like a catalog id;
//! - a catalog id (`x_<slug>_<kind>_<payload>`) is proxied through the instance
//!   that issued it, under that instance's `host-http` permission and image
//!   cache policy: an image from an instance that declared `no-store` is passed
//!   through and never written to the artwork cache, and the response tells the
//!   client not to store it either.
//!
//! `size` is the requested size in pixels and is handed to the catalog layer,
//! which picks the smallest image its provider describes as at least that large.
//! Local images are served exactly as they are on disk: scaling is not
//! implemented yet, so `size` has no effect on them.
//!
//! A served image carries its MIME type, its length, a strong `ETag` over the
//! bytes, and a `Cache-Control`; a request whose `If-None-Match` already names
//! that tag is answered with HTTP 304 and no body. A refusal is HTTP 400 for a
//! missing `id` or an unparsable `size`, HTTP 404 for an id that names no entity
//! with an image, HTTP 500 for a read the server could not complete, and HTTP
//! 503 with `Retry-After` when the entity's catalog instance cannot answer right
//! now. Credential failures are answered by the extractor before the handler
//! runs, with HTTP 401.

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, ETAG, IF_NONE_MATCH, RETRY_AFTER,
};
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_catalog::{CatalogError, CoverArtRequest};
use hearthsound_core::{ClientId, EntityId, IdPrefix, Slug};
use hearthsound_library::LibraryRepository;
use hearthsound_library::artwork::{CoverArt, track_cover};
use hearthsound_plugin_host::manifest::ImageCachePolicy;
use sha2::{Digest, Sha256};

use crate::auth::BinaryCall;

/// The `Cache-Control` of an image a client may keep.
///
/// A stored or cacheable image is fresh for a day; afterwards the strong `ETag`
/// revalidates it, so a client never renders a changed image from its own cache
/// without asking. The value is `public` because an image is the same for every
/// user of the server.
const STORED_CACHE_CONTROL: &str = "public, max-age=86400";

/// The `Cache-Control` of an image whose provider forbids storing it.
///
/// The instance's manifest asked for this: its terms do not allow the image to
/// be kept, so neither the server nor the client stores it.
const NO_STORE_CACHE_CONTROL: &str = "no-store";

/// How long a client waits before asking again for an image the catalog could
/// not deliver.
const RETRY_AFTER_SECONDS: u64 = 5;

/// How many bytes of the image hash become the `ETag`.
const ENTITY_TAG_BYTES: usize = 16;

/// Answers `getCoverArt`: the cover image of one stored or catalog entity.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` is required and names a stored track,
///   album, or artist, or a catalog entity; `size` is the requested size in
///   pixels.
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
    match ClientId::parse(value) {
        Ok(ClientId::Catalog(catalog)) => {
            let slug = catalog.slug().clone();
            proxy(&call, &ClientId::Catalog(catalog), &slug, size).await
        }
        Ok(ClientId::Entity(entity)) => stored(&call, entity, size).await,
        Err(error) => {
            tracing::debug!(
                id = value,
                error = %error,
                "a cover art request named an id that is not a client id"
            );
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Serves the image of a stored entity, or the one its instance describes.
///
/// The entity's own image is looked for first. An entity that has none - either
/// because its files carry no artwork or because the id names no row at all -
/// is then asked of the instance that issued it, which is what makes the cover
/// art of an acquired-but-not-yet-stored entity available.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `entity`: the stored entity the request named.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image response, or one of the refusal codes of the module header.
async fn stored(call: &BinaryCall, entity: EntityId, size: Option<u32>) -> HttpResponse {
    if entity.kind() == IdPrefix::Playlist {
        return playlist(call, entity).await;
    }
    // An artist has no image of its own, so the artwork layer is asked for a
    // portrait before the local fallback that walks its albums is tried.
    if entity.kind() == IdPrefix::Artist
        && let Some(response) = artwork_layer(call, entity, size).await
    {
        return response;
    }
    match local(call, entity).await {
        Ok(Some(art)) => match load(art).await {
            Ok(served) => image(call.headers(), served),
            Err(error) => internal("reading a local cover image", &error),
        },
        Ok(None) => match artwork_layer(call, entity, size).await {
            Some(response) => response,
            None => not_found(&entity),
        },
        Err(error) => internal("resolving local cover art", &error),
    }
}

/// Asks the artwork layer, and the instance that issued the entity, for a
/// picture.
///
/// This is what a stored entity with no image of its own is served from: the
/// artwork layer first, in the order the administrator set, and then the
/// instance that issued the entity, which is where its own images are.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `entity`: the stored entity the request named.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image response, or `None` when no instance knows a picture of the entity,
/// which leaves the caller to answer its own refusal.
async fn artwork_layer(
    call: &BinaryCall,
    entity: EntityId,
    size: Option<u32>,
) -> Option<HttpResponse> {
    let catalog = call.state().catalog();
    let request = CoverArtRequest {
        id: ClientId::Entity(entity),
        size,
    };
    match catalog.cover_art(&request).await {
        Ok(cover) => Some(image(
            call.headers(),
            Served {
                mime_type: cover.mime_type,
                bytes: cover.bytes,
                cache_control: STORED_CACHE_CONTROL,
            },
        )),
        Err(CatalogError::NotFound) => None,
        Err(error) => Some(catalog_failure(&entity, &error)),
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
///
/// # Returns
///
/// The image response, HTTP 404 when the playlist does not exist, when the
/// caller may not see it, or when none of its entries has an image, or another
/// refusal code of the module header.
async fn playlist(call: &BinaryCall, playlist_id: EntityId) -> HttpResponse {
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
        Err(error) => return internal("reading a playlist's cover image", &error),
    };
    let library = LibraryRepository::new(call.state().database());
    let settings = call.state().settings_snapshot();
    let roots = call.state().storage_roots();
    for entry in &playlist.entries {
        match track_cover(&library, &settings, roots, entry.track_id).await {
            Ok(Some(art)) => match load(art).await {
                Ok(served) => return image(call.headers(), served),
                Err(error) => return internal("reading a local cover image", &error),
            },
            Ok(None) => {}
            Err(error) => return internal("resolving local cover art", &error),
        }
    }
    not_found(&playlist_id)
}

/// Finds the cover image a stored entity has of its own.
///
/// A track answers with the image of its preferred file. An album and an artist
/// have no image of their own, so they answer with the first track in playing
/// order that has one: for an artist, the tracks of its albums in the order the
/// albums are listed.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `entity`: the stored entity the request named.
///
/// # Returns
///
/// The image, or `None` when the entity has none. An id that names no row is
/// also `None`: the caller decides whether the catalog knows the entity before
/// it is reported as missing.
///
/// # Errors
///
/// Returns [`hearthsound_library::LibraryError`] when a read fails, including a
/// stored picture that cannot be extracted.
async fn local(
    call: &BinaryCall,
    entity: EntityId,
) -> Result<Option<CoverArt>, hearthsound_library::LibraryError> {
    let library = LibraryRepository::new(call.state().database());
    let settings = call.state().settings_snapshot();
    let roots = call.state().storage_roots();
    match entity.kind() {
        IdPrefix::Track => track_cover(&library, &settings, roots, entity).await,
        IdPrefix::Album => {
            if library.get_album(entity).await?.is_none() {
                return Ok(None);
            }
            first_track_cover(&library, &settings, roots, entity).await
        }
        IdPrefix::Artist => {
            if library.get_artist(entity).await?.is_none() {
                return Ok(None);
            }
            for album in library.list_albums_for_artist(entity).await? {
                if let Some(art) = first_track_cover(&library, &settings, roots, album.id).await? {
                    return Ok(Some(art));
                }
            }
            Ok(None)
        }
        IdPrefix::Playlist
        | IdPrefix::User
        | IdPrefix::Acquisition
        | IdPrefix::ApiKey
        | IdPrefix::AppPassword
        | IdPrefix::LibraryFolder
        | IdPrefix::Upload => Ok(None),
    }
}

/// Finds the cover image of the first track of an album that has one.
///
/// # Parameters
///
/// - `library`: the repository the album's tracks are read from.
/// - `settings`: the settings snapshot the cover file name is read from.
/// - `roots`: the managed storage roots.
/// - `album_id`: the album whose tracks are walked in playing order.
///
/// # Returns
///
/// The first image any of the album's tracks has, or `None` when none of them
/// has one.
///
/// # Errors
///
/// Returns [`hearthsound_library::LibraryError`] when a read fails.
async fn first_track_cover(
    library: &LibraryRepository,
    settings: &hearthsound_config::SettingsStore,
    roots: &hearthsound_library::StorageRoots,
    album_id: EntityId,
) -> Result<Option<CoverArt>, hearthsound_library::LibraryError> {
    for track in library.list_tracks_for_album(album_id).await? {
        if let Some(art) = track_cover(library, settings, roots, track.id).await? {
            return Ok(Some(art));
        }
    }
    Ok(None)
}

/// Serves an image through the catalog instance that issued the entity.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `id`: the entity to fetch cover art of, in the form the catalog layer
///   resolves: a catalog id by itself, or a stored entity id through its alias.
/// - `slug`: the instance that issued the entity, which supplies the image cache
///   policy of the response.
/// - `size`: the requested size in pixels.
///
/// # Returns
///
/// The image response, or one of the refusal codes of the module header.
async fn proxy(call: &BinaryCall, id: &ClientId, slug: &Slug, size: Option<u32>) -> HttpResponse {
    let catalog = call.state().catalog();
    let policy = catalog
        .registry()
        .instance(slug.as_str())
        .map(|instance| instance.image_cache_policy())
        .unwrap_or_default();
    let request = CoverArtRequest {
        id: id.clone(),
        size,
    };
    match catalog.cover_art(&request).await {
        Ok(cover) => image(
            call.headers(),
            Served {
                mime_type: cover.mime_type,
                bytes: cover.bytes,
                cache_control: cache_control(policy),
            },
        ),
        Err(error) => catalog_failure(slug, &error),
    }
}

/// Reads the bytes of a local cover image.
///
/// # Parameters
///
/// - `art`: the image the library layer found.
///
/// # Returns
///
/// The image with the `Cache-Control` a locally stored image is served with.
///
/// # Errors
///
/// Returns the I/O error of a sidecar image that could not be read.
async fn load(art: CoverArt) -> Result<Served, std::io::Error> {
    match art {
        CoverArt::Embedded { mime_type, data } => Ok(Served {
            mime_type,
            bytes: data,
            cache_control: STORED_CACHE_CONTROL,
        }),
        CoverArt::Sidecar { path, mime_type } => {
            let bytes = tokio::fs::read(&path).await?;
            Ok(Served {
                mime_type,
                bytes,
                cache_control: STORED_CACHE_CONTROL,
            })
        }
    }
}

/// Writes one image, or answers that the client already has it.
///
/// # Parameters
///
/// - `headers`: the request's head, which may carry `If-None-Match`.
/// - `served`: the image and the policy its response carries.
///
/// # Returns
///
/// HTTP 200 with the bytes, or HTTP 304 with no body when the request's
/// `If-None-Match` names the image's tag.
fn image(headers: &HeaderMap, served: Served) -> HttpResponse {
    let Served {
        mime_type,
        bytes,
        cache_control,
    } = served;
    let etag = etag(&bytes);
    let fresh = is_fresh(headers, &etag);
    let length = bytes.len();
    let (status, body) = if fresh {
        (StatusCode::NOT_MODIFIED, Body::empty())
    } else {
        (StatusCode::OK, Body::from(bytes))
    };
    let mut builder = HttpResponse::builder()
        .status(status)
        .header(CACHE_CONTROL, cache_control)
        .header(ETAG, etag.as_str());
    if !fresh {
        builder = builder
            .header(CONTENT_TYPE, content_type(&mime_type))
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
/// to a generic content type: bytes a client can play or display are more useful
/// than a failed request.
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

/// Returns the strong entity tag of an image.
///
/// The tag is the first [`ENTITY_TAG_BYTES`] bytes of the image's SHA-256,
/// hex encoded and quoted. The bytes decide the tag, so a client's revalidation
/// is exact: an image that changed always carries a new tag, and one that did
/// not keeps its tag across restarts.
///
/// # Parameters
///
/// - `bytes`: the encoded image.
///
/// # Returns
///
/// The quoted tag, without a weakness marker.
fn etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!(
        "\"{}\"",
        data_encoding::HEXLOWER.encode(&digest[..ENTITY_TAG_BYTES])
    )
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

/// Returns the `Cache-Control` a response carries for an instance's policy.
///
/// # Parameters
///
/// - `policy`: the image cache policy the issuing instance declared.
///
/// # Returns
///
/// [`NO_STORE_CACHE_CONTROL`] for [`ImageCachePolicy::NoStore`], and
/// [`STORED_CACHE_CONTROL`] otherwise.
fn cache_control(policy: ImageCachePolicy) -> &'static str {
    match policy {
        ImageCachePolicy::NoStore => NO_STORE_CACHE_CONTROL,
        ImageCachePolicy::Cache => STORED_CACHE_CONTROL,
    }
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

/// Answers that a catalog instance cannot serve an image right now.
///
/// # Parameters
///
/// - `slug`: the instance that failed.
/// - `error`: the failure the catalog layer reported.
///
/// # Returns
///
/// HTTP 404 when the entity has no image, and HTTP 503 with the delay the
/// instance asked for - or [`RETRY_AFTER_SECONDS`] when it named none - for
/// every failure a later retry may get past. A failure of this server rather
/// than of the instance is HTTP 500.
fn catalog_failure(id: &dyn std::fmt::Display, error: &CatalogError) -> HttpResponse {
    match error {
        CatalogError::NotFound => {
            tracing::debug!(
                id = %id,
                "the catalog has no cover art for that id"
            );
            StatusCode::NOT_FOUND.into_response()
        }
        CatalogError::RateLimited { retry_after_ms, .. } => {
            tracing::debug!(
                id = %id,
                retry_after_ms,
                "the catalog rate limited a cover art request"
            );
            unavailable(retry_after_ms.div_ceil(1_000).max(1))
        }
        CatalogError::Unavailable { .. }
        | CatalogError::Unauthorized { .. }
        | CatalogError::InvalidConfig { .. }
        | CatalogError::Internal { .. }
        | CatalogError::InvalidEntity { .. } => {
            tracing::warn!(
                id = %id,
                error = %error,
                "the catalog could not serve a cover image"
            );
            unavailable(RETRY_AFTER_SECONDS)
        }
        CatalogError::CorruptAlias
        | CatalogError::Library(_)
        | CatalogError::Setting(_)
        | CatalogError::Json(_)
        | CatalogError::Database(_) => internal("serving catalog cover art", error),
    }
}

/// Answers HTTP 503 with the delay the client should wait.
///
/// # Parameters
///
/// - `seconds`: the number of seconds the client waits.
///
/// # Returns
///
/// The response, with an empty body.
fn unavailable(seconds: u64) -> HttpResponse {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(RETRY_AFTER, seconds.to_string())],
    )
        .into_response()
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
    tracing::debug!(parameter, "a cover art request named no usable parameter");
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
/// HTTP 404 with an empty body.
fn not_found(id: &dyn std::fmt::Display) -> HttpResponse {
    tracing::debug!(id = %id, "no cover art for that id");
    StatusCode::NOT_FOUND.into_response()
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
    tracing::error!(context, error = %error, "could not serve cover art");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// One image and the policy its response carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    /// The image's MIME type.
    mime_type: String,
    /// The encoded image bytes.
    bytes: Vec<u8>,
    /// The `Cache-Control` the response carries.
    cache_control: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_follows_the_bytes() {
        assert_eq!(etag(b"one image"), etag(b"one image"));
        assert_ne!(etag(b"one image"), etag(b"another image"));
        assert!(
            !etag(b"one image").starts_with("W/"),
            "the tag is strong, so a client revalidates instead of guessing"
        );
    }

    #[test]
    fn a_rate_limited_instance_carries_its_delay() {
        let slug = Slug::new("deezer").unwrap();
        let response = catalog_failure(
            &slug,
            &CatalogError::RateLimited {
                instance: "deezer".to_owned(),
                retry_after_ms: 1_500,
            },
        );

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("2")
        );
    }

    #[test]
    fn an_instance_that_cannot_answer_counts_as_unavailable() {
        let slug = Slug::new("deezer").unwrap();
        let response = catalog_failure(
            &slug,
            &CatalogError::Unavailable {
                instance: "deezer".to_owned(),
            },
        );

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("5")
        );
    }
}
