//! The discovery vendor endpoints: recommended albums and the albums of the
//! artists similar to what the listener last played.
//!
//! Both endpoints are Home surfaces: a client draws a "Recommended" shelf and a
//! "Because you listen to" shelf from them, so neither takes a seed the client
//! supplies. The recommendations are the discovery layer's answer for the
//! requesting account, resolved into stored album rows; the similar-artist
//! section seeds itself with the artist of that account's most recent play and
//! reads the similar artists the indexer stored for it.
//!
//! Every album is rendered as an `AlbumID3` entry from its own row, so a client
//! gets the same id it would get from the artist page. The endpoints belong to
//! the `hearthsound` vendor extension, so they are answered only when the
//! request carries `hearthsound=1` and answer Subsonic error 70 without it.

use hearthsound_core::{EntityId, EntityKind};

use crate::auth::Call;
use crate::browse;
use crate::catalog;
use crate::endpoints::browse::{number, viewer};
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};

/// The number of albums a listing returns when `size` is absent.
const DEFAULT_ALBUM_SIZE: i64 = 20;

/// The largest number of albums a listing returns.
const MAX_ALBUM_SIZE: i64 = 100;

/// How many similar artists seed the second shelf.
const SIMILAR_ARTISTS: usize = 5;

/// Answers `hsGetRecommendedAlbums`: the albums of the listener's recommended
/// tracks.
///
/// # Parameters
///
/// - `call`: the authenticated call. `size` is the number of albums (default
///   20, between 1 and 100). The request must carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `recommendedAlbums`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// error 0 when `size` is not a number, and a generic error when a discovery or
/// database read fails.
pub async fn hs_get_recommended_albums(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let size = match number(&call, "size", DEFAULT_ALBUM_SIZE) {
        Ok(size) => size.clamp(1, MAX_ALBUM_SIZE),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(discovery) = call.state().discovery() else {
        return empty_listing(&call, "recommendedAlbums");
    };
    let user = call.principal().user_id.uuid();
    let albums = match discovery
        .recommended_albums(user, u32::try_from(size).unwrap_or(u32::MAX))
        .await
    {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let albums = match browse::albums_id3(&call, &albums).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member(
        "recommendedAlbums",
        Object::new().set("album", Array::new(albums)),
    ))
    .await
}

/// Answers `hsGetSimilarArtistAlbums`: the albums of the artists similar to the
/// listener's most recent play.
///
/// The answer names the seed artist and lists the albums of its similar
/// artists, one artist at a time, newest first, up to the requested count.
///
/// # Parameters
///
/// - `call`: the authenticated call. `size` is the number of albums (default
///   20, between 1 and 100). The request must carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `similarArtistAlbums`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// error 0 when `size` is not a number, and a generic error when a library read
/// fails.
pub async fn hs_get_similar_artist_albums(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let size = match number(&call, "size", DEFAULT_ALBUM_SIZE) {
        Ok(size) => size.clamp(1, MAX_ALBUM_SIZE),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let seed = match recently_played_artist(&call).await {
        Ok(Some(seed)) => seed,
        Ok(None) => return empty_listing(&call, "similarArtistAlbums"),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let views = call.state().catalog().views();
    let seed_view = match views.artist(seed, viewer(&call)).await {
        Ok(Some(view)) => view,
        Ok(None) => return empty_listing(&call, "similarArtistAlbums"),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let mut album_ids: Vec<EntityId> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for similar in seed_view.similar.iter().take(SIMILAR_ARTISTS) {
        let view = match views.artist(similar.artist.id, viewer(&call)).await {
            Ok(Some(view)) => view,
            Ok(None) => continue,
            Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
        };
        for album in view.albums {
            if seen.insert(album.album.id) {
                album_ids.push(album.album.id);
            }
        }
        if album_ids.len() >= usize::try_from(size).unwrap_or(usize::MAX) {
            break;
        }
    }
    album_ids.truncate(usize::try_from(size).unwrap_or(usize::MAX));
    let albums = match browse::albums_id3(&call, &album_ids).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let answer = match seed_object(&call, seed, seed_view.albums.len() as i64).await {
        Ok(Some(answer)) => answer,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(
        Response::ok().member(
            "similarArtistAlbums",
            Object::new()
                .set("seedArtist", answer)
                .set("album", Array::new(albums)),
        ),
    )
    .await
}

/// Reads the artist of the listener's most recent play.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The artist's id, or `None` when the listener has played nothing.
///
/// # Errors
///
/// Answers a generic error when the read fails.
async fn recently_played_artist(call: &Call) -> Result<Option<EntityId>, SubsonicError> {
    let mut conn = match call.state().database().readers().acquire().await {
        Ok(conn) => conn,
        Err(error) => return Err(browse::failure("hsGetSimilarArtistAlbums", &error)),
    };
    let found = hearthsound_library::query::most_recent_played_artist(
        &mut conn,
        call.principal().user_id.uuid(),
    )
    .await
    .map_err(|error| browse::failure("hsGetSimilarArtistAlbums", &error))?;
    Ok(found.map(|artist| artist.id))
}

/// Builds the `seedArtist` member of a similar-artist answer.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `seed`: the seed artist.
/// - `album_count`: how many albums the seed artist's page lists.
///
/// # Returns
///
/// The `ArtistID3` member, or `None` when the artist has no row.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn seed_object(
    call: &Call,
    seed: EntityId,
    album_count: i64,
) -> Result<Option<Object>, SubsonicError> {
    let database = call.state().database();
    let artist = match browse::artist_any(database, seed).await {
        Ok(Some(detail)) => detail.artist,
        Ok(None) => return Ok(None),
        Err(error) => return Err(browse::failure("hsGetSimilarArtistAlbums", &error)),
    };
    let annotation = browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        seed.uuid(),
    )
    .await
    .map_err(|error| browse::failure("hsGetSimilarArtistAlbums", &error))?;
    let ids = browse::identifiers_of(database, EntityKind::Artist, seed.uuid())
        .await
        .map_err(|error| browse::failure("hsGetSimilarArtistAlbums", &error))?;
    Ok(Some(crate::projection::artist_id3(
        &artist,
        album_count,
        &annotation,
        &ids,
    )))
}

/// Answers an album listing that has nothing to list.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the listing is carried in.
///
/// # Returns
///
/// A successful envelope carrying no albums.
fn empty_listing(call: &Call, member: &'static str) -> Encoded {
    call.encode(Response::ok().member(
        member,
        Object::new().set("album", Array::new(Vec::<Object>::new())),
    ))
}
