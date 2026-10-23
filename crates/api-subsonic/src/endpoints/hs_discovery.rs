//! The discovery vendor endpoints: recommended albums and the albums of the
//! artists similar to what the listener last played.
//!
//! Both endpoints are Home surfaces: a client draws a "Recommended" shelf and
//! a "Because you listen to" shelf from them, so neither takes a seed the
//! client supplies. The recommendations are the discovery layer's answer for
//! the requesting account, and the similar-artist section seeds itself with the
//! artist of that account's most recent play.
//!
//! Every album is rendered as an `AlbumID3` entry: an album of the listener's
//! personal library from its stored rows, and one the catalog knows from the
//! provider's album reference. The endpoints belong to the `hearthsound` vendor
//! extension, so they are answered only when the request carries
//! `hearthsound=1` and answer Subsonic error 70 without it.

use hearthsound_catalog::ResolvedAlbum;
use hearthsound_core::{EntityId, EntityKind};
use hearthsound_db::Database;
use hearthsound_library::query::Scope;

use crate::auth::Call;
use crate::browse;
use crate::endpoints::browse::number;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::{catalog, projection, scope};

/// The number of albums a listing returns when `size` is absent.
const DEFAULT_ALBUM_SIZE: i64 = 20;

/// The largest number of albums a listing returns.
const MAX_ALBUM_SIZE: i64 = 100;

/// Answers `hsGetRecommendedAlbums`: the distinct albums of the listener's
/// recommended tracks.
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
/// catalog read fails.
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
    let suffix = match catalog::title_suffix(&call.state().settings_snapshot()) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let albums = match discovery
        .recommended_albums(user, u32::try_from(size).unwrap_or(u32::MAX))
        .await
    {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let albums: Vec<Object> = albums
        .iter()
        .map(|album| projection::catalog_album_ref(album, &suffix))
        .collect();
    call.respond(Response::ok().member(
        "recommendedAlbums",
        Object::new().set("album", Array::new(albums)),
    ))
    .await
}

/// Answers `hsGetSimilarArtistAlbums`: the albums of the artists similar to the
/// listener's most recent play.
///
/// The answer names the seed artist and lists the albums one artist at a time,
/// each artist's personal library albums before its catalog albums.
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
/// error 0 when `size` is not a number, and a generic error when a discovery,
/// library or catalog read fails.
pub async fn hs_get_similar_artist_albums(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let size = match number(&call, "size", DEFAULT_ALBUM_SIZE) {
        Ok(size) => size.clamp(1, MAX_ALBUM_SIZE),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(discovery) = call.state().discovery() else {
        return empty_listing(&call, "similarArtistAlbums");
    };
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let suffix = match catalog::title_suffix(&call.state().settings_snapshot()) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let similar = match discovery
        .similar_artist_albums(user, &scopes, u32::try_from(size).unwrap_or(u32::MAX))
        .await
    {
        Ok(similar) => similar,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let albums = match album_objects(&call, &scopes, &similar.albums, &suffix).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut answer = Object::new();
    if let Some(seed) = similar.seed {
        let annotations =
            match browse::annotations(call.state(), user, EntityKind::Artist, &[seed.id.uuid()])
                .await
            {
                Ok(annotations) => annotations,
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("an artist", &error)));
                }
            };
        let album_count = match album_count(call.state().database(), &scopes, seed.id).await {
            Ok(count) => count,
            Err(error) => return call.encode(Response::failed(error)),
        };
        answer = answer.set(
            "seedArtist",
            projection::artist_id3(
                &seed,
                album_count,
                &annotations
                    .get(&seed.id.uuid())
                    .cloned()
                    .unwrap_or_default(),
            ),
        );
    }
    call.respond(Response::ok().member(
        "similarArtistAlbums",
        answer.set("album", Array::new(albums)),
    ))
    .await
}

/// Renders a similar-artist album list as `AlbumID3` objects.
///
/// A stored album is read in the first scope that contains it, so the object
/// reports what the caller can browse; a catalog album is rendered from the
/// provider's reference. A stored album that left every scope between the
/// discovery answer and this read is left out rather than served from a scope
/// the caller cannot see.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scopes`: the scopes the caller can see.
/// - `albums`: the albums, in the order they are listed.
/// - `suffix`: the configured title suffix catalog entities carry.
///
/// # Returns
///
/// One object per album, in the same order.
///
/// # Errors
///
/// Returns a generic error when a library read fails and a Subsonic error when
/// the caller's annotations cannot be read.
async fn album_objects(
    call: &Call,
    scopes: &[Scope],
    albums: &[ResolvedAlbum],
    suffix: &str,
) -> Result<Vec<Object>, SubsonicError> {
    let stored: Vec<EntityId> = albums
        .iter()
        .filter_map(|album| match album {
            ResolvedAlbum::Stored(album) => Some(album.id),
            ResolvedAlbum::Catalog(_) => None,
        })
        .collect();
    let ids: Vec<uuid::Uuid> = stored.iter().map(EntityId::uuid).collect();
    let annotations = if ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        match browse::annotations(
            call.state(),
            call.principal().user_id.uuid(),
            EntityKind::Album,
            &ids,
        )
        .await
        {
            Ok(annotations) => annotations,
            Err(error) => return Err(browse::failure("a similar artist's albums", &error)),
        }
    };
    let database = call.state().database();
    let mut objects = Vec::with_capacity(albums.len());
    for album in albums {
        match album {
            ResolvedAlbum::Catalog(result) => {
                objects.push(projection::catalog_album_ref(result, suffix));
            }
            ResolvedAlbum::Stored(album) => {
                let mut detail = None;
                for scope in scopes {
                    match browse::album_summary(database, *scope, album.id).await {
                        Ok(Some(found)) => {
                            detail = Some(found);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Err(browse::failure("a similar artist's albums", &error));
                        }
                    }
                }
                if let Some(detail) = detail {
                    objects.push(projection::album_id3(
                        &detail.album,
                        &detail.artists,
                        &detail.tracks,
                        &annotations
                            .get(&album.id.uuid())
                            .cloned()
                            .unwrap_or_default(),
                    ));
                }
            }
        }
    }
    Ok(objects)
}

/// Counts an artist's albums in the first scope that contains it.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scopes`: the scopes the caller can see.
/// - `artist`: the artist.
///
/// # Returns
///
/// The album count, or zero when no scope contains the artist.
///
/// # Errors
///
/// Returns a generic error when a read fails.
async fn album_count(
    database: &Database,
    scopes: &[Scope],
    artist: EntityId,
) -> Result<i64, SubsonicError> {
    for scope in scopes {
        match browse::artist(database, *scope, artist).await {
            Ok(Some(detail)) => return Ok(detail.albums.len() as i64),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("a similar artist's albums", &error)),
        }
    }
    Ok(0)
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
