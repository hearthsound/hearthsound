//! The information endpoints: `getArtistInfo`, `getArtistInfo2`,
//! `getAlbumInfo`, and `getAlbumInfo2`.
//!
//! A client shows an artist's biography and the pages about them, which the
//! stored artist row and its links carry, and an album's identifiers, which the
//! library stores. Both answers are read from rows: the biography from the
//! entity's own row, the MusicBrainz identifier from `external_ids`, the similar
//! artists from the stored relationship the indexer filled, and the image URLs
//! empty because this server serves an entity's image from an authenticated
//! `getCoverArt` request under the entity's own id rather than from an
//! unauthenticated URL.

use hearthsound_catalog::views::{ArtistRef, Viewer};
use hearthsound_core::providers::MB_ARTIST;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::auth::Call;
use crate::endpoints::browse::number;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::{browse, catalog, projection};

/// The link kind a last.fm page is reported under.
const LASTFM: &str = "lastfm";

/// The number of similar artists `getArtistInfo` returns without a count
/// parameter.
const DEFAULT_SIMILAR_ARTISTS: i64 = 20;

/// The largest number of similar artists a request may ask for.
const MAX_SIMILAR_ARTISTS: i64 = 100;

/// Answers `getArtistInfo`: the biography and links of one artist.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names an artist.
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
/// - `call`: the authenticated call. `id` names an album or a track of one.
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
    let count = match number(call, "count", DEFAULT_SIMILAR_ARTISTS) {
        Ok(count) => count.clamp(0, MAX_SIMILAR_ARTISTS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let include_not_present = matches!(
        call.get("includeNotPresent"),
        Some(value) if value == "true" || value == "1"
    );
    let Some(id) = (match browse::entity_id(call.state(), IdPrefix::Artist, value).await {
        Ok(id) => id,
        Err(error) => return call.encode(Response::failed(error)),
    }) else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    // A third-party client sees only similar artists its own scopes hold unless
    // it asked for the ones it cannot open yet.
    let viewer = Viewer {
        user: call.principal().user_id.uuid(),
        vendor: !call.params().vendor_enabled() && !include_not_present,
    };
    let view = match call.state().catalog().views().artist(id, viewer).await {
        Ok(Some(view)) => view,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let last_fm = view
        .links
        .iter()
        .find(|link| link.kind.eq_ignore_ascii_case(LASTFM))
        .map(|link| link.url.clone())
        .unwrap_or_default();
    let ids = match browse::identifiers_of(call.state().database(), EntityKind::Artist, id.uuid())
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtistInfo", &error)));
        }
    };
    let mut answer = Object::new()
        .set_element(
            "biography",
            view.artist.biography.clone().unwrap_or_default(),
        )
        .set_element("musicBrainzId", catalog::identifier(&ids, MB_ARTIST))
        .set_element("lastFmUrl", last_fm)
        // The artist's own image is served by `getCoverArt` under its id, so no
        // unauthenticated URL is emitted for it.
        .set_element("smallImageUrl", String::new())
        .set_element("mediumImageUrl", String::new())
        .set_element("largeImageUrl", String::new());
    if count > 0 && !view.similar.is_empty() {
        let similar = match similar_artists(call, &view.similar, count).await {
            Ok(similar) => similar,
            Err(error) => return call.encode(Response::failed(error)),
        };
        if !similar.is_empty() {
            answer = answer.set("similarArtist", Array::new(similar));
        }
    }
    call.encode(Response::ok().member(member, answer))
}

/// Projects the stored similar artists of one artist as `ArtistID3` members.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `artists`: the similar artists, in the stored order.
/// - `count`: the maximum number of them.
///
/// # Returns
///
/// One member per artist, in the stored order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn similar_artists(
    call: &Call,
    artists: &[ArtistRef],
    count: i64,
) -> Result<Vec<Object>, SubsonicError> {
    let limit = usize::try_from(count).unwrap_or(usize::MAX);
    let listed: Vec<&ArtistRef> = artists.iter().take(limit).collect();
    let database = call.state().database();
    let ids: Vec<Uuid> = listed
        .iter()
        .map(|artist| artist.artist.id.uuid())
        .collect();
    let annotations = browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        &ids,
    )
    .await
    .map_err(|error| browse::failure("getArtistInfo", &error))?;
    let mut objects = Vec::with_capacity(listed.len());
    for artist in listed {
        let album_count = match browse::artist_any(database, artist.artist.id).await {
            Ok(Some(detail)) => detail.albums.len() as i64,
            Ok(None) => 0,
            Err(error) => return Err(browse::failure("getArtistInfo", &error)),
        };
        let artist_ids =
            match browse::identifiers_of(database, EntityKind::Artist, artist.artist.id.uuid())
                .await
            {
                Ok(ids) => ids,
                Err(error) => return Err(browse::failure("getArtistInfo", &error)),
            };
        objects.push(projection::artist_id3(
            &artist.artist,
            album_count,
            annotations
                .get(&artist.artist.id.uuid())
                .unwrap_or(&Default::default()),
            &artist_ids,
        ));
    }
    Ok(objects)
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
    let album = match album_of(call, value).await {
        Ok(Some(album)) => album,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let ids = match browse::identifiers_of(call.state().database(), EntityKind::Album, album.uuid())
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getAlbumInfo", &error)));
        }
    };
    let answer = Object::new()
        .set_element("notes", String::new())
        .set_element(
            "musicBrainzId",
            catalog::identifier(&ids, hearthsound_core::providers::MB_RELEASE),
        )
        // Nothing looks an album up outside the server, so there is no last.fm
        // page to point at, and cover art is served by `getCoverArt`.
        .set_element("lastFmUrl", String::new())
        .set_element("smallImageUrl", String::new())
        .set_element("mediumImageUrl", String::new())
        .set_element("largeImageUrl", String::new());
    call.encode(Response::ok().member(member, answer))
}

/// Reads the album an album-information request names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the id the client wrote, which names an album or a track of one.
///
/// # Returns
///
/// The album's id, or `None` when the id names no album or a track without one.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an album or track id, and a
/// generic error when a read fails.
async fn album_of(call: &Call, value: &str) -> Result<Option<EntityId>, SubsonicError> {
    let library = LibraryRepository::new(call.state().database());
    let Ok(id) = EntityId::parse(value) else {
        tracing::debug!("an album info request named an id that is not an entity id");
        return Ok(None);
    };
    match id.kind() {
        IdPrefix::Album => match browse::entity_id(call.state(), IdPrefix::Album, value).await {
            Ok(album) => Ok(album),
            Err(error) => Err(error),
        },
        // A track answers for its album, which is what a client that is playing
        // it asks about.
        IdPrefix::Track => {
            let track = match browse::entity_id(call.state(), IdPrefix::Track, value).await {
                Ok(track) => track,
                Err(error) => return Err(error),
            };
            let Some(track) = track else {
                return Ok(None);
            };
            let album = match library.get_track(track).await {
                Ok(track) => track.and_then(|track| track.album_id),
                Err(error) => return Err(browse::failure("getAlbumInfo", &error)),
            };
            match album {
                Some(album) => {
                    match browse::entity_id(call.state(), IdPrefix::Album, &album.to_string()).await
                    {
                        Ok(album) => Ok(album),
                        Err(error) => Err(error),
                    }
                }
                None => Ok(None),
            }
        }
        _ => {
            tracing::debug!("an album info request named an id that is not an album");
            Ok(None)
        }
    }
}
