//! The browse endpoints: the artist index and detail, the album detail, a
//! single song, and the album list.
//!
//! Every endpoint reads the scope [`crate::scope`] resolves, asks
//! [`crate::browse`] for the rows, and builds its payload from
//! [`crate::projection`], so the five responses share one scope rule and one
//! set of members.

use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::AlbumSort;

use crate::auth::Call;
use crate::catalog;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, projection, scope};

/// The number of albums `getAlbumList2` returns without a `size` parameter.
const DEFAULT_ALBUM_SIZE: i64 = 10;

/// The largest number of albums `getAlbumList2` returns.
const MAX_ALBUM_SIZE: i64 = 500;

/// The largest number of albums a catalog artist lookup lists.
const MAX_CATALOG_ALBUMS: u32 = 500;

/// The message an unsupported album list type is answered with.
const UNSUPPORTED_LIST_TYPE: &str = "Unsupported album list type.";

/// Answers `getArtists`: the artist index over the accessible scope.
///
/// The index is grouped by the first letter of the article-stripped sort name
/// and reports the ignored articles beside it, so a client can explain the
/// grouping to a user.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `artists` (ArtistsID3), or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 for an unknown `musicFolderId` and a generic error
/// when a read fails.
pub async fn get_artists(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let summaries = match browse::artists(call.state().database(), scope).await {
        Ok(summaries) => summaries,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtists", &error)));
        }
    };
    call.encode(Response::ok().member("artists", projection::artists_id3(&summaries)))
}

/// Answers `getArtist`: one artist with its albums in scope.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `id` is the artist to read.
///
/// # Returns
///
/// A successful envelope carrying `artist` (ArtistID3 with its `album`
/// children), or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when it is
/// malformed, names another kind of entity, or is not in scope, error 70 when a
/// catalog id names an entity the catalog does not have, and a generic error
/// when a read fails or the catalog instance cannot answer.
pub async fn get_artist(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let artist_id = match requested(&call, IdPrefix::Artist) {
        Ok(Requested::Entity(id)) => id,
        Ok(Requested::Catalog(id)) => return catalog_artist(&call, id).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let database = call.state().database();
    let detail = match browse::artist(database, scope, artist_id).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtist", &error)));
        }
    };

    let mut albums = Vec::new();
    for album in &detail.albums {
        match browse::album(database, scope, album.id).await {
            Ok(Some(loaded)) => albums.push(projection::album_id3(
                &loaded.detail.album,
                &loaded.detail.artists,
                &loaded.detail.tracks,
                loaded.has_art(),
            )),
            Ok(None) => {}
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getArtist", &error)));
            }
        }
    }

    let artist = projection::artist_id3(&detail.artist, albums.len() as i64)
        .set("album", Array::new(albums));
    call.encode(Response::ok().member("artist", artist))
}

/// Answers `getAlbum`: one album with its songs.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `id` is the album to read.
///
/// # Returns
///
/// A successful envelope carrying `album` (AlbumID3 with its `song` children),
/// or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when it is
/// malformed, names another kind of entity, or is not in scope, error 70 when a
/// catalog id names an entity the catalog does not have, and a generic error
/// when a read fails or the catalog instance cannot answer.
pub async fn get_album(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let album_id = match requested(&call, IdPrefix::Album) {
        Ok(Requested::Entity(id)) => id,
        Ok(Requested::Catalog(id)) => return catalog_album(&call, id).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let loaded = match browse::album(call.state().database(), scope, album_id).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getAlbum", &error)));
        }
    };

    let album = projection::album_id3(
        &loaded.detail.album,
        &loaded.detail.artists,
        &loaded.detail.tracks,
        loaded.has_art(),
    );
    let songs = loaded
        .detail
        .tracks
        .iter()
        .zip(&loaded.files)
        .map(|(view, file)| projection::child(view, file.as_ref(), &loaded.detail.artists));
    record_listing(
        &call,
        loaded
            .detail
            .tracks
            .iter()
            .map(|view| ClientId::Entity(view.track.id)),
    );
    let album = album.set("song", Array::new(songs));
    call.encode(Response::ok().member("album", album))
}

/// Answers `getSong`: one song.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `id` is the track to read.
///
/// # Returns
///
/// A successful envelope carrying `song` (a Child), or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when it is
/// malformed, names another kind of entity, or is not in scope, error 70 when a
/// catalog id names an entity the catalog does not have, and a generic error
/// when a read fails or the catalog instance cannot answer.
pub async fn get_song(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let track_id = match requested(&call, IdPrefix::Track) {
        Ok(Requested::Entity(id)) => id,
        Ok(Requested::Catalog(id)) => return catalog_song(&call, id).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let loaded = match browse::track(call.state().database(), scope, track_id).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getSong", &error)));
        }
    };

    let song = projection::child(&loaded.view, loaded.file.as_ref(), &loaded.album_artists);
    call.encode(Response::ok().member("song", song))
}

/// Answers `getAlbumList2`: the albums in scope in a fixed order.
///
/// This milestone supports `newest`, `alphabeticalByName`, and `random`; the
/// remaining types of the base protocol are added by later work.
///
/// # Parameters
///
/// - `call`: the authenticated call. `type` selects the order, `size` the
///   number of albums (default 10, capped at 500), and `offset` how many to
///   skip.
///
/// # Returns
///
/// A successful envelope carrying `albumList2`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `type` is missing and error 0 when its value
/// is a list type this server does not support yet, because the parameter is
/// present but the request is one the server cannot satisfy; an unparseable
/// `size` or `offset` is answered with error 0 as well. Error 70 is answered
/// for an unknown `musicFolderId`, and a generic error when a read fails.
pub async fn get_album_list_2(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(list_type) = call.get("type") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("type")));
    };
    let sort = match list_type {
        "newest" => AlbumSort::Newest,
        "alphabeticalByName" => AlbumSort::AlphabeticalByName,
        "random" => AlbumSort::Random,
        other => {
            tracing::debug!(
                list_type = other,
                "an unsupported album list type was requested"
            );
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                UNSUPPORTED_LIST_TYPE,
            )));
        }
    };
    let size = match number(&call, "size", DEFAULT_ALBUM_SIZE) {
        Ok(size) => size.clamp(0, MAX_ALBUM_SIZE),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let offset = match number(&call, "offset", 0) {
        Ok(offset) => offset.max(0),
        Err(error) => return call.encode(Response::failed(error)),
    };

    let database = call.state().database();
    let listed = match browse::albums(database, scope, sort, size, offset).await {
        Ok(listed) => listed,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getAlbumList2", &error)));
        }
    };

    let mut albums = Vec::new();
    for album in listed {
        match browse::album(database, scope, album.id).await {
            Ok(Some(loaded)) => albums.push(projection::album_id3(
                &loaded.detail.album,
                &loaded.detail.artists,
                &loaded.detail.tracks,
                loaded.has_art(),
            )),
            Ok(None) => {}
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getAlbumList2", &error)));
            }
        }
    }

    call.encode(Response::ok().member("albumList2", Object::new().set("album", Array::new(albums))))
}

/// The id a browse request named.
#[derive(Debug, Clone)]
enum Requested {
    /// A stored entity, which the local scope queries answer.
    Entity(EntityId),
    /// An unmaterialized catalog entity, which the catalog layer answers.
    Catalog(CatalogId),
}

/// Reads the `id` parameter as a stored or catalog entity of `expected` kind.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `expected`: the record kind the endpoint can answer for.
///
/// # Returns
///
/// The stored entity id or the catalog id the request named.
///
/// # Errors
///
/// Returns Subsonic error 10 when `id` is absent, and error 70 when it is
/// malformed or names a record of another kind.
fn requested(call: &Call, expected: IdPrefix) -> Result<Requested, SubsonicError> {
    let Some(value) = call.get("id") else {
        return Err(SubsonicError::missing_parameter("id"));
    };
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == expected => Ok(Requested::Entity(id)),
        Ok(ClientId::Entity(id)) => {
            tracing::debug!(
                id = %id,
                expected = expected.as_str(),
                "a browse request named an entity of another kind"
            );
            Err(SubsonicError::not_found())
        }
        Ok(ClientId::Catalog(id)) if expected.entity_kind() == Some(id.kind()) => {
            Ok(Requested::Catalog(id))
        }
        Ok(ClientId::Catalog(id)) => {
            tracing::debug!(
                id = %id,
                expected = expected.as_str(),
                "a browse request named a catalog entity of another kind"
            );
            Err(SubsonicError::not_found())
        }
        Err(error) => {
            tracing::debug!(value, error = %error, "a browse request named a malformed id");
            Err(SubsonicError::not_found())
        }
    }
}

/// Records a listing as the client's implied queue.
///
/// The prefetch scheduler infers what a client is about to play from the
/// listing it last read, so every endpoint that answers with an ordered
/// tracklist reports it. Nothing is stored: the scheduler keeps the most recent
/// listing per user and client instance in memory, and a state built without a
/// scheduler records nothing.
///
/// # Parameters
///
/// - `call`: the authenticated call. Its principal names the user, and its `c`
///   parameter names the client instance, the empty string when the client
///   named none.
/// - `listing`: the listing's track ids in playing order, as the client will
///   name them: stored track ids for a local listing and `x_` catalog ids for a
///   catalog one.
fn record_listing(call: &Call, listing: impl IntoIterator<Item = ClientId>) {
    let Some(prefetch) = call.state().prefetch() else {
        return;
    };
    prefetch.record_listing(
        call.principal().user_id.uuid(),
        call.client_name().unwrap_or_default(),
        listing.into_iter().collect(),
    );
}

/// Answers `getSong` for an unmaterialized catalog id.
///
/// The track is looked up through the instance that issued the id and
/// projected with the same rules a `search3` catalog result uses; nothing is
/// stored, so a stream of the id is what acquires it.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `id`: the catalog id of the track.
///
/// # Returns
///
/// A successful envelope carrying `song` (a catalog Child), or a failed
/// envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the catalog does not have the track, and a
/// generic error when the issuing instance cannot answer.
async fn catalog_song(call: &Call, id: CatalogId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let format = match catalog::pending_format(&snapshot) {
        Ok(format) => format,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let track = match call.state().catalog().track(&id).await {
        Ok(track) => track,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let song = projection::catalog_child(
        &track.id,
        &track.track,
        &format.suffix,
        &format.content_type,
        &suffix,
    );
    call.encode(Response::ok().member("song", song))
}

/// Answers `getAlbum` for an unmaterialized catalog id.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `id`: the catalog id of the album.
///
/// # Returns
///
/// A successful envelope carrying `album` (a catalog AlbumID3 with its `song`
/// children), or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the catalog does not have the album, and a
/// generic error when the issuing instance cannot answer.
async fn catalog_album(call: &Call, id: CatalogId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let format = match catalog::pending_format(&snapshot) {
        Ok(format) => format,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let album = match call.state().catalog().album(&id).await {
        Ok(album) => album,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let mut songs = Vec::new();
    let mut listing = Vec::new();
    for track in &album.album.tracks {
        let Some(track_id) =
            CatalogId::new(album.id.slug().clone(), EntityKind::Track, track.id.clone()).ok()
        else {
            continue;
        };
        songs.push(projection::catalog_child(
            &track_id,
            track,
            &format.suffix,
            &format.content_type,
            &suffix,
        ));
        listing.push(ClientId::Catalog(track_id));
    }
    record_listing(call, listing);
    let album = projection::catalog_album(&album, &suffix).set("song", Array::new(songs));
    call.encode(Response::ok().member("album", album))
}

/// Answers `getArtist` for an unmaterialized catalog id.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `id`: the catalog id of the artist.
///
/// # Returns
///
/// A successful envelope carrying `artist` (a catalog ArtistID3 with its
/// `album` children), or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the catalog does not have the artist, and a
/// generic error when the issuing instance cannot answer. An instance that has
/// the artist but cannot list their albums is answered with no albums, because
/// the artist lookup itself succeeded.
async fn catalog_artist(call: &Call, id: CatalogId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let catalog = call.state().catalog();
    let artist = match catalog.artist(&id).await {
        Ok(artist) => artist,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let albums = match catalog.artist_albums(&id, MAX_CATALOG_ALBUMS, 0).await {
        Ok(albums) => albums,
        Err(error) => {
            tracing::debug!(error = %error, "could not list a catalog artist's albums");
            Vec::new()
        }
    };
    let albums: Vec<Object> = albums
        .iter()
        .map(|album| projection::catalog_album_ref(album, &suffix))
        .collect();
    let artist = projection::catalog_artist(&artist, albums.len() as i64, &suffix)
        .set("album", Array::new(albums));
    call.encode(Response::ok().member("artist", artist))
}

/// Reads a numeric parameter, defaulting when it is absent or empty.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `name`: the parameter name.
/// - `default`: the value an absent or empty parameter supplies.
///
/// # Returns
///
/// The parsed number.
///
/// # Errors
///
/// Returns a generic Subsonic error when the value is not a number.
pub(crate) fn number(call: &Call, name: &'static str, default: i64) -> Result<i64, SubsonicError> {
    let Some(value) = call.get(name).map(str::trim) else {
        return Ok(default);
    };
    if value.is_empty() {
        return Ok(default);
    }
    match value.parse::<i64>() {
        Ok(number) => Ok(number),
        Err(error) => {
            tracing::debug!(parameter = name, value, error = %error, "a browse request carried a non-numeric parameter");
            Err(SubsonicError::new(
                codes::GENERIC,
                format!("The `{name}` parameter must be a number."),
            ))
        }
    }
}
