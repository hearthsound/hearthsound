//! The browse endpoints: the artist index and detail, the album detail, a
//! single song, and the album list.
//!
//! Every page is composed by the catalog layer's [`EntityViews`]: the artist,
//! album and song endpoints read one view each, which is what makes the
//! Subsonic page and the native page the same page. A view walks a cold entity
//! inline under `index.view_timeout_ms` and answers what is stored, so a page
//! that could not finish is still served.
//!
//! Every entity a response describes is a stored row with an [`EntityId`], so
//! the id a client sends resolves through [`browse::entity_id`]: an id merged
//! into an older row answers the survivor, and an id that names nothing is a
//! not-found. A row without a file is projected as the row it is, with the
//! file-derived fields empty and the configured catalog title suffix on its
//! title when the caller is a third-party client.
//!
//! The payload is built from [`crate::projection`], so the endpoints share one
//! set of members.

use std::collections::HashMap;

use hearthsound_activity::Annotation;
use hearthsound_catalog::views::Viewer;
use hearthsound_core::providers::ExternalId;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::{AlbumSort, ArtistSort, Scope, TrackSort};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, catalog, projection, scope};
use hearthsound_library::query::ArtistSummary;

/// The number of albums `getAlbumList2` returns without a `size` parameter.
const DEFAULT_ALBUM_SIZE: i64 = 10;

/// The largest number of albums `getAlbumList2` returns.
const MAX_ALBUM_SIZE: i64 = 500;

/// The message an unsupported album list type is answered with.
const UNSUPPORTED_LIST_TYPE: &str = "Unsupported album list type.";

/// Returns the viewer a page is composed for.
///
/// A request that carries `hearthsound=1` is this server's own client and sees
/// everything the server stores, and so does an administrator who asked for the
/// server view (`musicFolderId=2`). Any other request is a third-party client
/// and sees only what its own scopes hold, which is the rule a Subsonic server
/// without the vendor extension answers by.
///
/// # Parameters
///
/// - `call`: the authenticated call.
pub(crate) fn viewer(call: &Call) -> Viewer {
    let everything = call.params().vendor_enabled() || server_view(call);
    Viewer {
        user: call.principal().user_id.uuid(),
        vendor: !everything,
    }
}

/// Reports whether the request asks for the server view as an administrator.
///
/// # Parameters
///
/// - `call`: the authenticated call.
fn server_view(call: &Call) -> bool {
    call.principal().is_admin
        && call
            .get("musicFolderId")
            .is_some_and(|folder| folder == "2")
}

/// Answers `getArtists`: the artist index over the accessible scope.
///
/// The index is grouped by the first letter of the article-stripped sort name
/// and reports the ignored articles beside it, so a client can explain the
/// grouping to a user.
///
/// A request carrying `hearthsound=1` may pass `sort` to order the index by
/// `name` (the default, the alphabetical index), `added` (the artist stored
/// most recently first) or `played` (the artist the caller played most often
/// first). A listing in an order other than the alphabetical one keeps that
/// order and starts a new group where the letter an artist is filed under
/// changes, so a client that flattens the index reads the requested order. A
/// request without `hearthsound=1` is answered exactly as a server without the
/// vendor extension would answer it.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `sort` names the order.
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
    let sort = artist_sort(&call);
    let summaries = match browse::artists(call.state().database(), scope, sort).await {
        Ok(summaries) => summaries,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtists", &error)));
        }
    };
    let projections = match ArtistProjections::of(&call, &summaries).await {
        Ok(projections) => projections,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let artists = if sort == ArtistSort::Name {
        projection::artists_id3(
            &summaries,
            &projections.annotations,
            &projections.identifiers,
        )
    } else {
        projection::artists_id3_ordered(
            &summaries,
            &projections.annotations,
            &projections.identifiers,
        )
    };
    call.respond(Response::ok().member("artists", artists))
        .await
}

/// Answers `getArtist`: one artist with its whole discography.
///
/// The page is the catalog layer's artist view: the artist's row, its links,
/// its top tracks, one album per release group newest first, and its similar
/// artists. A third-party client sees only the albums and tracks its own scopes
/// hold.
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
/// malformed, names another kind of entity, or names no artist, and a generic
/// error when a read fails or the catalog instance cannot answer.
pub async fn get_artist(call: Call) -> Encoded {
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let id = match browse::entity_id(call.state(), IdPrefix::Artist, value).await {
        Ok(Some(id)) => id,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let view = match call
        .state()
        .catalog()
        .views()
        .artist(id, viewer(&call))
        .await
    {
        Ok(Some(view)) => view,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let album_ids: Vec<EntityId> = view.albums.iter().map(|album| album.album.id).collect();
    let albums = match browse::albums_id3(&call, &album_ids).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let album_count = albums.len() as i64;
    let annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => return call.encode(Response::failed(browse::failure("getArtist", &error))),
    };
    let ids = match browse::identifiers_of(call.state().database(), EntityKind::Artist, id.uuid())
        .await
    {
        Ok(ids) => ids,
        Err(error) => return call.encode(Response::failed(browse::failure("getArtist", &error))),
    };
    let mut artist = projection::artist_id3(&view.artist, album_count, &annotation, &ids);
    // A page that could not list the whole discography says so, so a client
    // offers a reload instead of presenting the partial work as the artist's.
    if call.params().vendor_enabled() && !view.complete {
        artist = artist.set("hearthsound", Object::new().set("complete", false));
    }
    call.respond(Response::ok().member("artist", artist.set("album", Array::new(albums))))
        .await
}

/// Answers `getAlbum`: one album with its whole tracklist.
///
/// The page is the catalog layer's album view: the album's row, its credits,
/// its genres, its tracklist in playing order, and the other albums of its
/// primary artist. A third-party client sees only the tracks its own scopes
/// hold.
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
/// malformed, names another kind of entity, or names no album, and a generic
/// error when a read fails.
pub async fn get_album(call: Call) -> Encoded {
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let id = match browse::entity_id(call.state(), IdPrefix::Album, value).await {
        Ok(Some(id)) => id,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let view = match call
        .state()
        .catalog()
        .views()
        .album(id, viewer(&call))
        .await
    {
        Ok(Some(view)) => view,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let track_ids: Vec<EntityId> = view.tracks.iter().map(|track| track.track.id).collect();
    let tracklist = match browse::SongPage::of(&call, &track_ids).await {
        Ok(tracklist) => tracklist,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Album,
        id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => return call.encode(Response::failed(browse::failure("getAlbum", &error))),
    };
    let ids = match browse::identifiers_of(call.state().database(), EntityKind::Album, id.uuid())
        .await
    {
        Ok(ids) => ids,
        Err(error) => return call.encode(Response::failed(browse::failure("getAlbum", &error))),
    };
    let artists: Vec<hearthsound_library::Artist> = view
        .artists
        .iter()
        .map(|artist| artist.artist.clone())
        .collect();
    let views = tracklist.views();
    let album = projection::album_id3(&view.album, &artists, &views, &annotation, &ids)
        .set("songCount", views.len() as i64)
        .set("duration", tracklist.duration_ms() / 1000)
        .set("song", Array::new(tracklist.objects()));
    record_listing(&call, tracklist.ids());
    call.respond(Response::ok().member("album", album)).await
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
/// malformed, names another kind of entity, or names no track, and a generic
/// error when a read fails.
pub async fn get_song(call: Call) -> Encoded {
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let id = match browse::entity_id(call.state(), IdPrefix::Track, value).await {
        Ok(Some(id)) => id,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    // The view answers whether the track exists and whether a third-party
    // caller's scopes hold it; the song itself is projected from its row.
    match call
        .state()
        .catalog()
        .views()
        .track(id, viewer(&call))
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    }
    let songs = match browse::songs(&call, &[id]).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(song) = songs.into_iter().next() else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    record_listing(&call, vec![id]);
    call.respond(Response::ok().member("song", song)).await
}

/// Answers `getAlbumList2`: the albums in scope in a fixed order.
///
/// The supported types are `newest`, `alphabeticalByName`, `alphabeticalByArtist`,
/// `random`, `highest`, `frequent`, `recent`, `starred`, `byYear` and `byGenre`.
/// The four activity types list only the albums that carry their activity:
/// `highest` the albums the account rated, `frequent` and `recent` the albums it
/// played, and `starred` the albums it liked.
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
    let listed = match listed_albums(&call, scope, "getAlbumList2").await {
        Ok(listed) => listed,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let albums = match browse::albums_id3(&call, &listed).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(
        Response::ok().member("albumList2", Object::new().set("album", Array::new(albums))),
    )
    .await
}

/// Answers `getAlbumList`: the albums in scope as folders.
///
/// The list types are the ones `getAlbumList2` accepts; the difference is the
/// shape of the answer, which reports each album as a directory `Child` that a
/// client browses into with `getMusicDirectory`.
///
/// # Parameters
///
/// - `call`: the authenticated call. `type` selects the order or filter, `size`
///   the number of albums (default 10, capped at 500), `offset` how many to
///   skip, and `fromYear`/`toYear`/`genre` narrow a `byYear` or `byGenre` list.
///
/// # Returns
///
/// A successful envelope carrying `albumList`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_album_list_2`] documents.
pub async fn get_album_list(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let listed = match listed_albums(&call, scope, "getAlbumList").await {
        Ok(listed) => listed,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let albums = match browse::albums_dir(&call, &listed).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member("albumList", Object::new().set("album", Array::new(albums))))
        .await
}

/// Answers `getIndexes`: the artist index with the instant it last changed.
///
/// A client that sends `ifModifiedSince` with an instant at or after
/// `lastModified` is told that nothing changed and gets no index, which is what
/// makes a repeated browse cheap.
///
/// # Parameters
///
/// - `call`: the authenticated call. `ifModifiedSince` is the instant the
///   client already has, in unix milliseconds.
///
/// # Returns
///
/// A successful envelope carrying `indexes`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 for an unknown `musicFolderId` and a generic error
/// when a read fails.
pub async fn get_indexes(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let database = call.state().database();
    let last_modified = match browse::last_modified(database).await {
        Ok(instant) => instant,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getIndexes", &error)));
        }
    };
    if let Some(since) = match number(&call, "ifModifiedSince", 0) {
        Ok(since) => Some(since),
        Err(error) => return call.encode(Response::failed(error)),
    } && since >= last_modified
    {
        let unchanged = Object::new()
            .set("lastModified", last_modified)
            .set("ignoredArticles", projection::IGNORED_ARTICLES);
        return call.encode(Response::ok().member("indexes", unchanged));
    }

    let summaries = match browse::artists(database, scope, ArtistSort::Name).await {
        Ok(summaries) => summaries,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getIndexes", &error)));
        }
    };
    let projections = match ArtistProjections::of(&call, &summaries).await {
        Ok(projections) => projections,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let indexes = projection::indexes(
        &summaries,
        &projections.annotations,
        &projections.identifiers,
        last_modified,
    );
    call.encode(Response::ok().member("indexes", indexes))
}

/// Answers `getMusicDirectory`: one directory with its children.
///
/// An artist lists its albums and an album lists its songs, with the same
/// content `getArtist` and `getAlbum` answer with.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `id` is the directory to read.
///
/// # Returns
///
/// A successful envelope carrying `directory`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when it names
/// neither an artist nor an album or names no row, and a generic error when a
/// read fails.
pub async fn get_music_directory(call: Call) -> Encoded {
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let id = match browse::any_entity(call.state(), value).await {
        Ok(Some(id)) => id,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    match id.kind() {
        IdPrefix::Artist => artist_directory(&call, id).await,
        IdPrefix::Album => album_directory(&call, id).await,
        _ => call.encode(Response::failed(SubsonicError::not_found())),
    }
}

/// Answers `getMusicDirectory` for an artist.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `id`: the artist's id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the artist has no row, and a generic error
/// when a read fails.
async fn artist_directory(call: &Call, id: EntityId) -> Encoded {
    let view = match call
        .state()
        .catalog()
        .views()
        .artist(id, viewer(call))
        .await
    {
        Ok(Some(view)) => view,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let album_ids: Vec<EntityId> = view.albums.iter().map(|album| album.album.id).collect();
    let children = match browse::albums_dir(call, &album_ids).await {
        Ok(children) => children,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let directory = projection::directory(
        &id.to_string(),
        "",
        &view.artist.name,
        Some(&annotation),
        children,
    );
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// Answers `getMusicDirectory` for an album.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `id`: the album's id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the album has no row, and a generic error
/// when a read fails.
async fn album_directory(call: &Call, id: EntityId) -> Encoded {
    let view = match call.state().catalog().views().album(id, viewer(call)).await {
        Ok(Some(view)) => view,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let track_ids: Vec<EntityId> = view.tracks.iter().map(|track| track.track.id).collect();
    let tracklist = match browse::SongPage::of(call, &track_ids).await {
        Ok(tracklist) => tracklist,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Album,
        id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let parent = view
        .artists
        .first()
        .map(|artist| artist.artist.id.to_string())
        .unwrap_or_default();
    let directory = projection::directory(
        &id.to_string(),
        &parent,
        &view.album.title,
        Some(&annotation),
        tracklist.objects(),
    );
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// The annotations and identifiers of an artist page.
struct ArtistProjections {
    /// The caller's annotations, keyed by artist id.
    annotations: HashMap<Uuid, Annotation>,
    /// The stored identifiers, keyed by artist id.
    identifiers: HashMap<Uuid, Vec<ExternalId>>,
}

impl ArtistProjections {
    /// Reads the projections of one artist listing.
    ///
    /// # Parameters
    ///
    /// - `call`: the authenticated call.
    /// - `summaries`: the artists of the listing.
    ///
    /// # Errors
    ///
    /// Answers a generic error when a read fails.
    async fn of(
        call: &Call,
        summaries: &[ArtistSummary],
    ) -> Result<ArtistProjections, SubsonicError> {
        let ids: Vec<Uuid> = summaries
            .iter()
            .map(|summary| summary.artist.id.uuid())
            .collect();
        let annotations = browse::annotations(
            call.state(),
            call.principal().user_id.uuid(),
            EntityKind::Artist,
            &ids,
        )
        .await
        .map_err(|error| browse::failure("an artist listing", &error))?;
        let identifiers = browse::identifiers(call.state().database(), EntityKind::Artist, &ids)
            .await
            .map_err(|error| browse::failure("an artist listing", &error))?;
        Ok(ArtistProjections {
            annotations,
            identifiers,
        })
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
/// - `listing`: the listing's track ids in playing order.
fn record_listing(call: &Call, listing: Vec<EntityId>) {
    let Some(prefetch) = call.state().prefetch() else {
        return;
    };
    prefetch.record_listing(
        call.principal().user_id.uuid(),
        call.client_name().unwrap_or_default(),
        listing,
    );
}

/// Loads the albums one album-list request asks for.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose parameters decide the order and the
///   window.
/// - `scope`: the scope to list.
/// - `operation`: the endpoint name a failure is logged under.
///
/// # Returns
///
/// The albums, in the order the request asked for.
///
/// # Errors
///
/// Answers Subsonic error 10 when `type` is missing or a parameter the type
/// needs is missing, error 0 when `type` names no known list type or a number
/// cannot be parsed, and a generic error when a read fails.
async fn listed_albums(
    call: &Call,
    scope: Scope,
    operation: &'static str,
) -> Result<Vec<EntityId>, SubsonicError> {
    let Some(list_type) = call.get("type") else {
        return Err(SubsonicError::missing_parameter("type"));
    };
    let sort = album_sort(call, list_type)?;
    let size = number(call, "size", DEFAULT_ALBUM_SIZE)?.clamp(0, MAX_ALBUM_SIZE);
    let offset = number(call, "offset", 0)?.max(0);

    let listed = browse::albums(call.state().database(), scope, sort, size, offset)
        .await
        .map_err(|error| browse::failure(operation, &error))?;
    Ok(listed.into_iter().map(|album| album.id).collect())
}

/// Reads the list type of an album-list request.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `type` is the list type and whose
///   `fromYear`, `toYear`, and `genre` narrow two of them.
/// - `list_type`: the value of `type`.
///
/// # Returns
///
/// The order to list the albums in.
///
/// # Errors
///
/// Answers Subsonic error 10 when a type needs a parameter that is missing, and
/// error 0 when the type is not one this server supports.
fn album_sort(call: &Call, list_type: &str) -> Result<AlbumSort, SubsonicError> {
    let user = call.principal().user_id.uuid();
    match list_type {
        "newest" => Ok(AlbumSort::Newest),
        "alphabeticalByName" => Ok(AlbumSort::AlphabeticalByName),
        "alphabeticalByArtist" => Ok(AlbumSort::AlphabeticalByArtist),
        "random" => Ok(AlbumSort::Random),
        "highest" => Ok(AlbumSort::Highest { user }),
        "frequent" => Ok(AlbumSort::Frequent { user }),
        "recent" => Ok(AlbumSort::Recent { user }),
        "starred" => Ok(AlbumSort::Starred { user }),
        "byYear" => {
            let from = number(call, "fromYear", 0)?;
            let to = number(call, "toYear", 0)?;
            if from == 0 && to == 0 {
                return Err(SubsonicError::missing_parameter("fromYear"));
            }
            // One year given alone lists that year, so a client may send either
            // end of the range.
            let (from, to) = if from == 0 {
                (to, to)
            } else if to == 0 {
                (from, from)
            } else {
                (from, to)
            };
            Ok(AlbumSort::ByYear { from, to })
        }
        "byGenre" => match call.get("genre") {
            Some(genre) if !genre.is_empty() => Ok(AlbumSort::ByGenre(genre.to_owned())),
            _ => Err(SubsonicError::missing_parameter("genre")),
        },
        other => {
            tracing::debug!(
                list_type = other,
                "an unsupported album list type was requested"
            );
            Err(SubsonicError::new(codes::GENERIC, UNSUPPORTED_LIST_TYPE))
        }
    }
}

/// Reads the order of a song listing request.
///
/// The order is a vendor parameter: it is read only when the request carries
/// `hearthsound=1`, because the standard search has no order and a request that
/// did not opt in must be answered exactly as a server without the extension
/// would answer it.
///
/// An unknown value is ignored rather than refused, because a sort is a display
/// preference a client may have learned from a newer server.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `sort` names the order.
///
/// # Returns
///
/// The order to list the songs in; [`TrackSort::Title`] for an absent or
/// unknown value.
pub(super) fn track_sort(call: &Call) -> TrackSort {
    if !call.params().vendor_enabled() {
        return TrackSort::Title;
    }
    let user = call.principal().user_id.uuid();
    match call.get("sort").map(str::trim) {
        Some("added") => TrackSort::Added,
        Some("played") => TrackSort::Played { user },
        Some("artist") => TrackSort::Artist,
        Some("album") => TrackSort::Album,
        Some("year") => TrackSort::Year,
        Some("title") | Some("") | None => TrackSort::Title,
        Some(other) => {
            tracing::debug!(sort = other, "an unknown song order was ignored");
            TrackSort::Title
        }
    }
}

/// Reads the genre a song listing is narrowed to.
///
/// Like [`track_sort`], the filter is a vendor parameter: without
/// `hearthsound=1` it is not read, so a standard search answer is unchanged.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `genre` names the genre.
///
/// # Returns
///
/// The genre to filter on, or `None` when the request names none.
pub(super) fn song_genre(call: &Call) -> Option<&str> {
    if !call.params().vendor_enabled() {
        return None;
    }
    call.get("genre")
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
}

/// Reads the order of an artist index request.
///
/// The order is a vendor parameter, read like [`track_sort`]: only with
/// `hearthsound=1`, and an unknown value is ignored.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `sort` names the order.
///
/// # Returns
///
/// The order to list the artists in; [`ArtistSort::Name`] for an absent or
/// unknown value.
fn artist_sort(call: &Call) -> ArtistSort {
    if !call.params().vendor_enabled() {
        return ArtistSort::Name;
    }
    match call.get("sort").map(str::trim) {
        Some("added") => ArtistSort::Added,
        Some("played") => ArtistSort::Played {
            user: call.principal().user_id.uuid(),
        },
        Some("name") | Some("") | None => ArtistSort::Name,
        Some(other) => {
            tracing::debug!(sort = other, "an unknown artist order was ignored");
            ArtistSort::Name
        }
    }
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
