//! The information endpoints: `getArtistInfo`, `getArtistInfo2`,
//! `getAlbumInfo`, and `getAlbumInfo2`.
//!
//! A client shows an artist's biography and the pages about them, which come
//! from the metadata layer, and an album's identifiers, which the library holds.
//! The protocol's response shape asks for the fields as child elements rather
//! than attributes, for the artist's portrait URLs, and for a list of similar
//! artists. The portrait comes from the artwork layer, and the similar artists
//! from the discovery layer; a server with neither answers the same empty
//! fields, which is what a client already handles.

use hearthsound_catalog::{DiscoveryService, ResolvedArtist};
use hearthsound_core::providers::{ArtistInfoRequest, ArtistSeed, ExternalId, Image};
use hearthsound_core::{ClientId, EntityKind, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use uuid::Uuid;

use crate::auth::Call;
use crate::endpoints::browse::number;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::{browse, catalog, projection, scope};

/// The link kind a last.fm page is reported under.
const LASTFM: &str = "lastfm";

/// The number of similar artists `getArtistInfo` returns without a count
/// parameter.
const DEFAULT_SIMILAR_ARTISTS: i64 = 20;

/// The largest number of similar artists a request may ask for.
const MAX_SIMILAR_ARTISTS: i64 = 100;

/// The portrait widths a client asks for, as small, medium and large.
const PORTRAIT_SIZES: [u32; 3] = [96, 300, 600];

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
    let count = match number(call, "count", DEFAULT_SIMILAR_ARTISTS) {
        Ok(count) => count.clamp(0, MAX_SIMILAR_ARTISTS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let include_not_present = matches!(
        call.get("includeNotPresent"),
        Some(value) if value == "true" || value == "1"
    );
    let named = match named_artist(call, value).await {
        Ok(named) => named,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some((client, name, ids)) = named else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };

    let vendor = call.params().vendor_enabled();
    let request = ArtistInfoRequest {
        name: name.clone(),
        ids: ids.clone(),
    };
    // A first-party page never waits for the metadata or artwork layers: it shows
    // what is already cached and asks for the rest through an enrichment job.
    let info = if vendor {
        call.state().catalog().cached_artist_info(&request).await
    } else {
        match call.state().catalog().artist_info(&request).await {
            Ok(info) => info,
            Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
        }
    };
    if vendor {
        catalog::request_enrichment(call, &client).await;
    }
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

    let portraits = if vendor {
        match call
            .state()
            .catalog()
            .cached_artist_portraits(&client)
            .await
        {
            Ok(images) => images.unwrap_or_default(),
            Err(error) => {
                tracing::debug!(%error, "no cached artist portrait was found");
                Vec::new()
            }
        }
    } else {
        match call.state().catalog().artist_portraits(&client).await {
            Ok(images) => images,
            Err(error) => {
                tracing::debug!(%error, "no artwork instance described an artist portrait");
                Vec::new()
            }
        }
    };
    let (small, medium, large) = portrait_urls(&portraits);
    let mut answer = Object::new()
        .set_element("biography", biography)
        .set_element("musicBrainzId", mbid)
        .set_element("lastFmUrl", last_fm)
        .set_element("smallImageUrl", small)
        .set_element("mediumImageUrl", medium)
        .set_element("largeImageUrl", large);
    let discovery = if count > 0 {
        call.state().discovery()
    } else {
        None
    };
    if let Some(discovery) = discovery {
        let similar =
            match similar_artist_objects(call, discovery, &name, &ids, count, include_not_present)
                .await
            {
                Ok(similar) => similar,
                Err(error) => return call.encode(Response::failed(error)),
            };
        if !similar.is_empty() {
            answer = answer.set("similarArtist", Array::new(similar));
        }
    }
    call.encode(Response::ok().member(member, answer))
}

/// Lists the similar artists of one artist, resolved to what the caller can see.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `discovery`: the discovery layer.
/// - `name`: the artist the request is about.
/// - `ids`: the identifiers the server has for the artist.
/// - `count`: the maximum number of similar artists.
/// - `include_not_present`: whether catalog artists are listed too.
///
/// # Returns
///
/// One `ArtistID3` per similar artist: the stored artists first, each with its
/// album count in the caller's scope and the caller's annotation, and then the
/// catalog artists when `include_not_present` is set.
///
/// # Errors
///
/// Answers a generic error when a read fails and a Subsonic error when the
/// caller's annotations cannot be read.
async fn similar_artist_objects(
    call: &Call,
    discovery: &DiscoveryService,
    name: &str,
    ids: &[ExternalId],
    count: i64,
    include_not_present: bool,
) -> Result<Vec<Object>, SubsonicError> {
    let limit = u32::try_from(count).unwrap_or(u32::MAX);
    let user = call.principal().user_id.uuid();
    let seed = ArtistSeed {
        name: name.to_owned(),
        ids: ids.to_vec(),
    };
    let suggestions = match discovery.similar_artists(user, &seed, limit).await {
        Ok(suggestions) => suggestions,
        Err(error) => return Err(catalog::failure(&error)),
    };
    if suggestions.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = scope::search_scopes(call).await?;
    let resolved = match discovery
        .resolve_artists(&scopes, &suggestions, limit)
        .await
    {
        Ok(resolved) => resolved,
        Err(error) => return Err(catalog::failure(&error)),
    };
    let stored_ids: Vec<Uuid> = resolved
        .iter()
        .filter_map(|artist| match artist {
            ResolvedArtist::Stored(artist) => Some(artist.id.uuid()),
            ResolvedArtist::Catalog(_) => None,
        })
        .collect();
    let annotations =
        match browse::annotations(call.state(), user, EntityKind::Artist, &stored_ids).await {
            Ok(annotations) => annotations,
            Err(error) => return Err(browse::failure("getArtistInfo", &error)),
        };
    let suffix = catalog::title_suffix(&call.state().settings_snapshot())?;
    let database = call.state().database();
    let mut objects = Vec::with_capacity(resolved.len());
    for artist in resolved {
        match artist {
            ResolvedArtist::Stored(artist) => {
                let album_count = albums_in_scope(database, &scopes, artist.id).await?;
                objects.push(projection::artist_id3(
                    &artist,
                    album_count,
                    &annotations
                        .get(&artist.id.uuid())
                        .cloned()
                        .unwrap_or_default(),
                ));
            }
            ResolvedArtist::Catalog(result) if include_not_present => {
                objects.push(projection::catalog_artist(&result, None, &suffix));
            }
            ResolvedArtist::Catalog(_) => {}
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
/// Answers a generic error when a read fails.
async fn albums_in_scope(
    database: &Database,
    scopes: &[hearthsound_library::query::Scope],
    artist: hearthsound_core::EntityId,
) -> Result<i64, SubsonicError> {
    for scope in scopes {
        match browse::artist(database, *scope, artist).await {
            Ok(Some(detail)) => return Ok(detail.albums.len() as i64),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("getArtistInfo", &error)),
        }
    }
    Ok(0)
}

/// Picks the portrait URLs for the three sizes a client asks for.
///
/// # Parameters
///
/// - `images`: the portrait renditions the artwork layer described.
///
/// # Returns
///
/// The URLs nearest 96, 300 and 600 pixels wide. An image without a stated
/// width is used when nothing else is; a layer that described one image fills
/// all three URLs with it; an empty list fills all three with empty strings.
fn portrait_urls(images: &[Image]) -> (String, String, String) {
    let nearest = |target: u32| {
        images
            .iter()
            .min_by_key(|image| match image.width {
                Some(width) => u64::from(width.abs_diff(target)),
                None => u64::MAX,
            })
            .map(|image| image.url.clone())
            .unwrap_or_default()
    };
    let small = nearest(PORTRAIT_SIZES[0]);
    let medium = nearest(PORTRAIT_SIZES[1]);
    let large = nearest(PORTRAIT_SIZES[2]);
    (small, medium, large)
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
    if call.params().vendor_enabled()
        && let Ok(id) = ClientId::parse(value)
    {
        catalog::request_enrichment(call, &id).await;
    }

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

/// Reads the identity, name and identifiers of the artist an id names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The parsed id, the name and the identifiers, or `None` when the id names no
/// artist. The parsed id is what the artwork layer is asked for the artist's
/// portrait.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not an artist id, and a generic
/// error when a read fails.
async fn named_artist(
    call: &Call,
    value: &str,
) -> Result<Option<(ClientId, String, Vec<ExternalId>)>, SubsonicError> {
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
                (ClientId::Entity(artist.id), artist.name, ids)
            }))
        }
        Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Artist => {
            if call.params().vendor_enabled() {
                let artist = match call.state().catalog().cached_artist(&id).await {
                    Ok(Some(artist)) => artist,
                    Ok(None) => return Err(SubsonicError::not_found()),
                    Err(error) => return Err(catalog::failure(&error)),
                };
                return Ok(Some((
                    ClientId::Catalog(id),
                    artist.artist.name,
                    artist.artist.ids,
                )));
            }
            match call.state().catalog().artist(&id).await {
                Ok(artist) => Ok(Some((
                    ClientId::Catalog(id),
                    artist.artist.name,
                    artist.artist.ids,
                ))),
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
        Ok(ClientId::Catalog(id))
            if id.kind() == EntityKind::Album && call.params().vendor_enabled() =>
        {
            // A cached reference carries the title and the credits, not the
            // album's identifiers, so a cached tracklist answers the id and a
            // reference alone answers an empty one.
            match call.state().catalog().cached_album(&id).await {
                Ok(Some(album)) => Ok(Some(
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
                Ok(None) => match call.state().catalog().cached_album_ref(&id).await {
                    Ok(Some(_)) => Ok(Some(String::new())),
                    Ok(None) => Err(SubsonicError::not_found()),
                    Err(error) => Err(catalog::failure(&error)),
                },
                Err(error) => Err(catalog::failure(&error)),
            }
        }
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
