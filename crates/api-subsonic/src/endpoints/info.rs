//! The information endpoints: `getArtistInfo`, `getArtistInfo2`,
//! `getAlbumInfo`, and `getAlbumInfo2`.
//!
//! A client shows an artist's biography and the pages about them, which come
//! from the metadata layer, and an album's identifiers, which the library holds.
//! The protocol's response shape asks for the fields as child elements rather
//! than attributes, and for the image URLs and the "similar artists" list, which
//! this server answers empty: cover art is served by `getCoverArt`, and nothing
//! computes similar artists.

use hearthsound_core::providers::{ArtistInfoRequest, ExternalId};
use hearthsound_core::{ClientId, EntityKind, IdPrefix};
use hearthsound_library::LibraryRepository;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::Object;
use crate::{browse, catalog};

/// The link kind a last.fm page is reported under.
const LASTFM: &str = "lastfm";

/// Answers `getArtistInfo`: what the metadata layer knows about one artist.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored artist or a catalog
///   artist.
///
/// # Returns
///
/// A successful envelope carrying `artistInfo`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when `id` is not an
/// artist id or names no artist, and a generic error when a read fails.
pub async fn get_artist_info(call: Call) -> Encoded {
    artist_info(&call, "artistInfo").await
}

/// Answers `getArtistInfo2`: the same answer in the ID3 response shape.
///
/// The parameters and the errors are the ones [`get_artist_info`] documents.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `artistInfo2`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_artist_info`] documents.
pub async fn get_artist_info_2(call: Call) -> Encoded {
    artist_info(&call, "artistInfo2").await
}

/// Answers `getAlbumInfo`: the identifiers the library holds for one album.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored album, a stored track,
///   or a catalog album.
///
/// # Returns
///
/// A successful envelope carrying `albumInfo`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when `id` is not an
/// album or track id and when it names no album, and a generic error when a read
/// fails.
pub async fn get_album_info(call: Call) -> Encoded {
    album_info(&call, "albumInfo").await
}

/// Answers `getAlbumInfo2`: the same answer in the ID3 response shape.
///
/// The parameters and the errors are the ones [`get_album_info`] documents.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `albumInfo2`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_album_info`] documents.
pub async fn get_album_info_2(call: Call) -> Encoded {
    album_info(&call, "albumInfo2").await
}

/// Answers an artist-information request.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the answer is carried in.
///
/// # Returns
///
/// A successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_artist_info`] documents.
async fn artist_info(call: &Call, member: &'static str) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let named = match named_artist(call, value).await {
        Ok(named) => named,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some((name, ids)) = named else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };

    let info = match call
        .state()
        .catalog()
        .artist_info(&ArtistInfoRequest {
            name,
            ids: ids.clone(),
        })
        .await
    {
        Ok(info) => info,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let biography = info
        .as_ref()
        .and_then(|info| info.biography.clone())
        .unwrap_or_default();
    let last_fm = info
        .as_ref()
        .and_then(|info| {
            info.links
                .iter()
                .find(|link| link.kind.eq_ignore_ascii_case(LASTFM))
        })
        .map(|link| link.url.clone())
        .unwrap_or_default();
    // The server's own identifiers win: they are what the library recorded, and
    // a provider's answer is what fills in for an artist it never saw.
    let mbid = ids
        .iter()
        .find(|id| id.namespace == hearthsound_core::providers::MB_ARTIST)
        .map(|id| id.value.clone())
        .or_else(|| {
            info.as_ref().and_then(|info| {
                info.ids
                    .iter()
                    .find(|id| id.namespace == hearthsound_core::providers::MB_ARTIST)
                    .map(|id| id.value.clone())
            })
        })
        .unwrap_or_default();

    let answer = Object::new()
        .set_element("biography", biography)
        .set_element("musicBrainzId", mbid)
        .set_element("lastFmUrl", last_fm)
        .set_element("smallImageUrl", String::new())
        .set_element("mediumImageUrl", String::new())
        .set_element("largeImageUrl", String::new());
    call.encode(Response::ok().member(member, answer))
}

/// Answers an album-information request.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the answer is carried in.
///
/// # Returns
///
/// A successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_album_info`] documents.
async fn album_info(call: &Call, member: &'static str) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let mbid = match named_album(call, value).await {
        Ok(mbid) => mbid,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(mbid) = mbid else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };

    let answer = Object::new()
        .set_element("notes", String::new())
        .set_element("musicBrainzId", mbid)
        // Nothing looks an album up outside the library, so there is no last.fm
        // page to point at, and cover art is served by `getCoverArt`.
        .set_element("lastFmUrl", String::new())
        .set_element("smallImageUrl", String::new())
        .set_element("mediumImageUrl", String::new())
        .set_element("largeImageUrl", String::new());
    call.encode(Response::ok().member(member, answer))
}

/// Reads the name and identifiers of the artist an id names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The name and the identifiers, or `None` when the id names no artist.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an artist id, and a generic
/// error when a read fails.
async fn named_artist(
    call: &Call,
    value: &str,
) -> Result<Option<(String, Vec<ExternalId>)>, SubsonicError> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Artist => {
            let library = LibraryRepository::new(call.state().database());
            let artist = match library.get_artist(id).await {
                Ok(artist) => artist,
                Err(error) => return Err(browse::failure("getArtistInfo", &error)),
            };
            Ok(artist.map(|artist| {
                let ids = artist
                    .mb_artist_id
                    .filter(|id| !id.is_empty())
                    .map(|value| ExternalId {
                        namespace: hearthsound_core::providers::MB_ARTIST.to_owned(),
                        value,
                    })
                    .into_iter()
                    .collect();
                (artist.name, ids)
            }))
        }
        Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Artist => {
            match call.state().catalog().artist(&id).await {
                Ok(artist) => Ok(Some((artist.artist.name, artist.artist.ids))),
                Err(error) => Err(catalog::failure(&error)),
            }
        }
        _ => {
            tracing::debug!("an artist info request named an id that is not an artist");
            Err(SubsonicError::not_found())
        }
    }
}

/// Reads the MusicBrainz release id of the album an id names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The identifier, which is empty when the album carries none, or `None` when
/// the id names no album.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an album or track id, and a
/// generic error when a read fails.
async fn named_album(call: &Call, value: &str) -> Result<Option<String>, SubsonicError> {
    let library = LibraryRepository::new(call.state().database());
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) => match id.kind() {
            IdPrefix::Album => match library.get_album(id).await {
                Ok(album) => Ok(album.map(|album| album.mb_release_id.unwrap_or_default())),
                Err(error) => Err(browse::failure("getAlbumInfo", &error)),
            },
            // A track answers for its album, which is what a client that is
            // playing it asks about.
            IdPrefix::Track => {
                let track = match library.get_track(id).await {
                    Ok(track) => track,
                    Err(error) => return Err(browse::failure("getAlbumInfo", &error)),
                };
                let Some(album) = track.and_then(|track| track.album_id) else {
                    return Ok(None);
                };
                match library.get_album(album).await {
                    Ok(album) => Ok(album.map(|album| album.mb_release_id.unwrap_or_default())),
                    Err(error) => Err(browse::failure("getAlbumInfo", &error)),
                }
            }
            _ => {
                tracing::debug!("an album info request named an id that is not an album");
                Err(SubsonicError::not_found())
            }
        },
        Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Album => {
            match call.state().catalog().album(&id).await {
                Ok(album) => Ok(Some(
                    album
                        .album
                        .ids
                        .iter()
                        .find(|external| {
                            external.namespace == hearthsound_core::providers::MB_RELEASE
                        })
                        .map(|external| external.value.clone())
                        .unwrap_or_default(),
                )),
                Err(error) => Err(catalog::failure(&error)),
            }
        }
        _ => {
            tracing::debug!("an album info request named an id that is not an album");
            Err(SubsonicError::not_found())
        }
    }
}
