//! The browse endpoints: the artist index and detail, the album detail, a
//! single song, and the album list.
//!
//! Every endpoint reads the scope [`crate::scope`] resolves, asks
//! [`crate::browse`] for the rows, and builds its payload from
//! [`crate::projection`], so the five responses share one scope rule and one
//! set of members.

use std::collections::HashMap;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use hearthsound_activity::Annotation;
use hearthsound_catalog::{AlbumHead, ArtistHead, PageAlbum, PageDepth, PageTrack};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_library::query::{AlbumDetail, AlbumSort, ArtistSort, Scope, TrackSort};
use uuid::Uuid;

use crate::auth::Call;
use crate::browse::LoadedTrack;
use crate::catalog;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, projection, scope};
use hearthsound_library::query::ArtistSummary;

/// The number of albums `getAlbumList2` returns without a `size` parameter.
const DEFAULT_ALBUM_SIZE: i64 = 10;

/// The largest number of albums `getAlbumList2` returns.
const MAX_ALBUM_SIZE: i64 = 500;

/// How many albums a listing reads at once.
///
/// Four, which is the number of reader connections the database pool holds: a
/// listing then never waits for a connection it is itself holding, and the
/// reads of one album still run in parallel with the reads of the next.
const READ_PARALLELISM: usize = 4;

/// The message an unsupported album list type is answered with.
const UNSUPPORTED_LIST_TYPE: &str = "Unsupported album list type.";

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
    let ids: Vec<Uuid> = summaries
        .iter()
        .map(|summary| summary.artist.id.uuid())
        .collect();
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        &ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtists", &error)));
        }
    };
    call.respond(Response::ok().member(
        "artists",
        if sort == ArtistSort::Name {
            projection::artists_id3(&summaries, &annotations)
        } else {
            projection::artists_id3_ordered(&summaries, &annotations)
        },
    ))
    .await
}

/// Answers `getArtist`: one artist with its whole discography.
///
/// The page lists the artist's catalog discography with the stored albums in
/// the places they belong, followed by the stored albums the catalog does not
/// list, so a stored id and a catalog id answer the same listing. A request
/// carrying `hearthsound=1` pairs the two by name and records the pairing; any
/// other request answers a stored artist from the library alone and reads a
/// catalog artist through the catalog.
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
/// malformed, names another kind of entity, or names an artist neither the
/// caller's scopes nor the catalog can produce, and a generic error when a read
/// fails or the catalog instance cannot answer.
pub async fn get_artist(call: Call) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let id = match requested(&call, IdPrefix::Artist) {
        Ok(Requested::Entity(id)) => ClientId::Entity(id),
        Ok(Requested::Catalog(id)) => ClientId::Catalog(id),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let page = match call
        .state()
        .catalog()
        .artist_page(&id, &scopes, page_depth(&call))
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    catalog::request_enrichment(&call, &id).await;
    let database = call.state().database();
    let user_id = call.principal().user_id.uuid();

    // Each stored album is read on its own connection, a few at a time: the
    // reads are independent, and an artist with twenty albums should cost one
    // round of reads rather than twenty.
    let reads: Vec<_> = page
        .albums
        .iter()
        .filter_map(|album| match album {
            PageAlbum::Stored(id) => Some(browse::album_summary_any(database, *id)),
            PageAlbum::Catalog(_) => None,
        })
        .collect();
    let details = match stream::iter(reads)
        .buffered(READ_PARALLELISM)
        .try_collect::<Vec<_>>()
        .await
    {
        Ok(details) => details,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtist", &error)));
        }
    };
    let mut by_id: HashMap<EntityId, AlbumDetail> = HashMap::new();
    for detail in details.into_iter().flatten() {
        by_id.insert(detail.album.id, detail);
    }
    let ids: Vec<Uuid> = by_id.keys().map(|id| id.uuid()).collect();
    let annotations = if ids.is_empty() {
        HashMap::new()
    } else {
        match browse::annotations(call.state(), user_id, EntityKind::Album, &ids).await {
            Ok(annotations) => annotations,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getArtist", &error)));
            }
        }
    };
    let mut albums = Vec::with_capacity(page.albums.len());
    for album in &page.albums {
        match album {
            PageAlbum::Stored(id) => {
                if let Some(detail) = by_id.get(id) {
                    albums.push(projection::album_id3(
                        &detail.album,
                        &detail.artists,
                        &detail.tracks,
                        annotations
                            .get(&id.uuid())
                            .unwrap_or(&Annotation::default()),
                    ));
                }
            }
            PageAlbum::Catalog(reference) => {
                albums.push(projection::catalog_album_ref(reference, &suffix));
            }
        }
    }
    let album_count = albums.len() as i64;
    let mut artist = match &page.head {
        ArtistHead::Stored(artist_id) => {
            let detail = match browse::artist_any(database, *artist_id).await {
                Ok(Some(detail)) => detail,
                Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("getArtist", &error)));
                }
            };
            let annotation = match browse::annotation(
                call.state(),
                user_id,
                EntityKind::Artist,
                artist_id.uuid(),
            )
            .await
            {
                Ok(annotation) => annotation,
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("getArtist", &error)));
                }
            };
            projection::artist_id3(&detail.artist, album_count, &annotation)
        }
        ArtistHead::Catalog(found) => projection::catalog_artist(found, Some(album_count), &suffix),
    };
    // A page that could not list the whole discography says so, and a cut listing
    // too, so a client offers a reload instead of presenting the partial work as
    // the artist's.
    if call.params().vendor_enabled() {
        let mut flags = Object::new();
        if !page.discography_complete {
            flags = flags.set("discographyComplete", false);
        }
        if page.discography_truncated {
            flags = flags.set("discographyTruncated", true);
        }
        if !flags.members().is_empty() {
            artist = artist.set("hearthsound", flags);
        }
    }
    call.respond(Response::ok().member("artist", artist.set("album", Array::new(albums))))
        .await
}

/// Answers `getAlbum`: one album with its whole tracklist.
///
/// The page lists the album's catalog tracklist with the stored tracks in the
/// places they belong, followed by the stored tracks the catalog does not list,
/// so a stored id and a catalog id answer the same listing.
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
/// malformed, names another kind of entity, or names an album neither the
/// caller's scopes nor the catalog can produce, and a generic error when a read
/// fails or the catalog instance cannot answer.
pub async fn get_album(call: Call) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let id = match requested(&call, IdPrefix::Album) {
        Ok(Requested::Entity(id)) => ClientId::Entity(id),
        Ok(Requested::Catalog(id)) => ClientId::Catalog(id),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let page = match call
        .state()
        .catalog()
        .album_page(&id, &scopes, page_depth(&call))
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    catalog::request_enrichment(&call, &id).await;
    let database = call.state().database();
    let user_id = call.principal().user_id.uuid();

    let reads: Vec<_> = page
        .tracks
        .iter()
        .filter_map(|track| match track {
            PageTrack::Stored(id) => Some(browse::track_any(database, *id)),
            PageTrack::Catalog(_) => None,
        })
        .collect();
    let loaded = match stream::iter(reads)
        .buffered(READ_PARALLELISM)
        .try_collect::<Vec<_>>()
        .await
    {
        Ok(loaded) => loaded,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getAlbum", &error)));
        }
    };
    let mut by_id: HashMap<EntityId, LoadedTrack> = HashMap::new();
    for track in loaded.into_iter().flatten() {
        by_id.insert(track.view.track.id, track);
    }
    let ids: Vec<Uuid> = by_id.keys().map(|id| id.uuid()).collect();
    let annotations = if ids.is_empty() {
        HashMap::new()
    } else {
        match browse::annotations(call.state(), user_id, EntityKind::Track, &ids).await {
            Ok(annotations) => annotations,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getAlbum", &error)));
            }
        }
    };

    let mut duration_ms = 0i64;
    let mut songs = Vec::with_capacity(page.tracks.len());
    for track in &page.tracks {
        match track {
            PageTrack::Stored(id) => {
                if let Some(loaded) = by_id.get(id) {
                    duration_ms += loaded
                        .file
                        .as_ref()
                        .and_then(|file| file.duration_ms)
                        .or(loaded.view.track.duration_ms)
                        .unwrap_or(0);
                    songs.push(projection::child(
                        &loaded.view,
                        loaded.file.as_ref(),
                        &loaded.album_artists,
                        annotations
                            .get(&id.uuid())
                            .unwrap_or(&Annotation::default()),
                    ));
                }
            }
            PageTrack::Catalog(track) => {
                duration_ms += i64::from(track.track.duration_ms.unwrap_or(0));
                songs.push(projection::catalog_child(&track.id, &track.track, &suffix));
            }
        }
    }
    let song_count = songs.len() as i64;

    let album = match &page.head {
        AlbumHead::Stored(album_id) => {
            let detail = match browse::album_summary_any(database, *album_id).await {
                Ok(Some(detail)) => detail,
                Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("getAlbum", &error)));
                }
            };
            let annotation =
                match browse::annotation(call.state(), user_id, EntityKind::Album, album_id.uuid())
                    .await
                {
                    Ok(annotation) => annotation,
                    Err(error) => {
                        return call.encode(Response::failed(browse::failure("getAlbum", &error)));
                    }
                };
            projection::album_id3(&detail.album, &detail.artists, &detail.tracks, &annotation)
        }
        AlbumHead::Catalog(found) => projection::catalog_album(found, &suffix),
    };
    record_listing(&call, page.tracks.iter().map(page_track_id));
    let album = album
        .set("songCount", song_count)
        .set("duration", duration_ms / 1000)
        .set("song", Array::new(songs));
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
/// malformed, names another kind of entity, or is not in scope, error 70 when a
/// catalog id names an entity the catalog does not have, and a generic error
/// when a read fails or the catalog instance cannot answer.
pub async fn get_song(call: Call) -> Encoded {
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let track_id = match requested(&call, IdPrefix::Track) {
        Ok(Requested::Entity(id)) => id,
        Ok(Requested::Catalog(id)) => return catalog_song(&call, id).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some((_scope, loaded)) =
        (match browse::track_in_scopes(call.state().database(), &scopes, track_id).await {
            Ok(found) => found,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getSong", &error)));
            }
        })
    else {
        return match catalog_alias(&call, track_id).await {
            Ok(Some(id)) => catalog_song(&call, id).await,
            Ok(None) => call.encode(Response::failed(SubsonicError::not_found())),
            Err(error) => call.encode(Response::failed(error)),
        };
    };

    let song_annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        loaded.view.track.id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getSong", &error)));
        }
    };
    let song = projection::child(
        &loaded.view,
        loaded.file.as_ref(),
        &loaded.album_artists,
        &song_annotation,
    );
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

    let albums: Vec<Object> = listed
        .iter()
        .map(|(detail, annotation)| {
            projection::album_id3(&detail.album, &detail.artists, &detail.tracks, annotation)
        })
        .collect();
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

    let albums: Vec<Object> = listed
        .iter()
        .map(|(detail, annotation)| {
            projection::album_dir(&detail.album, &detail.artists, &detail.tracks, annotation)
        })
        .collect();
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
    let annotations = match annotations_for_artists(&call, &summaries).await {
        Ok(annotations) => annotations,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let indexes = projection::indexes(&summaries, &annotations, last_modified);
    call.encode(Response::ok().member("indexes", indexes))
}

/// Answers `getMusicDirectory`: one directory with its children.
///
/// An artist lists its albums and an album lists its songs, in both id forms
/// and with the same content `getArtist` and `getAlbum` answer with.
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
/// neither an artist nor an album or names one neither the caller's scopes nor
/// the catalog can produce, and a generic error when a read fails.
pub async fn get_music_directory(call: Call) -> Encoded {
    let scopes = match scope::search_scopes(&call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    // A directory is an artist or an album, and a client may name either by
    // its stored id or by a catalog id; anything else is answered as unknown.
    let id = match ClientId::parse(value) {
        Ok(id) => id,
        Err(error) => {
            tracing::debug!(%error, "a getMusicDirectory request named an unknown id");
            return call.encode(Response::failed(SubsonicError::not_found()));
        }
    };
    artist_or_album_directory(&call, &scopes, id).await
}

/// The kind of directory an id names.
///
/// # Parameters
///
/// - `id`: the client id.
///
/// # Returns
///
/// Whether the id names an artist or an album.
fn directory_kind(id: &ClientId) -> Option<(bool, EntityKind)> {
    match id {
        ClientId::Entity(entity) => match entity.kind() {
            IdPrefix::Artist => Some((true, EntityKind::Artist)),
            IdPrefix::Album => Some((false, EntityKind::Album)),
            _ => None,
        },
        ClientId::Catalog(catalog) => match catalog.kind() {
            EntityKind::Artist => Some((true, EntityKind::Artist)),
            EntityKind::Album => Some((false, EntityKind::Album)),
            _ => None,
        },
    }
}

/// Answers `getMusicDirectory` for an artist or an album, in either id form.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scopes`: the scopes to resolve the directory in, in preference order.
/// - `id`: the directory's client id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the id names nothing the caller may see, and
/// a generic error when a read fails.
async fn artist_or_album_directory(call: &Call, scopes: &[Scope], id: ClientId) -> Encoded {
    match directory_kind(&id) {
        Some((true, _)) => artist_directory(call, scopes, id).await,
        Some((false, _)) => album_directory(call, scopes, id).await,
        None => call.encode(Response::failed(SubsonicError::not_found())),
    }
}

/// Answers `getMusicDirectory` for an artist.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scopes`: the scopes to resolve the artist in, in preference order.
/// - `id`: the artist's client id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the artist is reachable through neither the
/// scopes nor the catalog, and a generic error when a read fails.
async fn artist_directory(call: &Call, scopes: &[Scope], id: ClientId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let page = match call
        .state()
        .catalog()
        .artist_page(&id, scopes, page_depth(call))
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let database = call.state().database();
    let user_id = call.principal().user_id.uuid();

    let reads: Vec<_> = page
        .albums
        .iter()
        .filter_map(|album| match album {
            PageAlbum::Stored(id) => Some(browse::album_summary_any(database, *id)),
            PageAlbum::Catalog(_) => None,
        })
        .collect();
    let details = match stream::iter(reads)
        .buffered(READ_PARALLELISM)
        .try_collect::<Vec<_>>()
        .await
    {
        Ok(details) => details,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let mut by_id: HashMap<EntityId, AlbumDetail> = HashMap::new();
    for detail in details.into_iter().flatten() {
        by_id.insert(detail.album.id, detail);
    }
    let ids: Vec<Uuid> = by_id.keys().map(|id| id.uuid()).collect();
    let annotations = if ids.is_empty() {
        HashMap::new()
    } else {
        match browse::annotations(call.state(), user_id, EntityKind::Album, &ids).await {
            Ok(annotations) => annotations,
            Err(error) => {
                return call.encode(Response::failed(browse::failure(
                    "getMusicDirectory",
                    &error,
                )));
            }
        }
    };

    let mut children = Vec::with_capacity(page.albums.len());
    for album in &page.albums {
        match album {
            PageAlbum::Stored(id) => {
                if let Some(detail) = by_id.get(id) {
                    children.push(projection::album_dir(
                        &detail.album,
                        &detail.artists,
                        &detail.tracks,
                        annotations
                            .get(&id.uuid())
                            .unwrap_or(&Annotation::default()),
                    ));
                }
            }
            PageAlbum::Catalog(reference) => {
                children.push(projection::catalog_album_dir(
                    &reference.id,
                    reference,
                    &suffix,
                ));
            }
        }
    }
    let (head_id, name, annotation) = match &page.head {
        ArtistHead::Stored(artist_id) => {
            let detail = match browse::artist_any(database, *artist_id).await {
                Ok(Some(detail)) => detail,
                Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
                Err(error) => {
                    return call.encode(Response::failed(browse::failure(
                        "getMusicDirectory",
                        &error,
                    )));
                }
            };
            let annotation = match browse::annotation(
                call.state(),
                user_id,
                EntityKind::Artist,
                artist_id.uuid(),
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
            (artist_id.to_string(), detail.artist.name, Some(annotation))
        }
        ArtistHead::Catalog(found) => (
            found.id.to_string(),
            format!("{}{suffix}", found.artist.name),
            None,
        ),
    };
    let directory = projection::directory(&head_id, "", &name, annotation.as_ref(), children);
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// Answers `getMusicDirectory` for an album.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scopes`: the scopes to resolve the album in, in preference order.
/// - `id`: the album's client id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the album is reachable through neither the
/// scopes nor the catalog, and a generic error when a read fails.
async fn album_directory(call: &Call, scopes: &[Scope], id: ClientId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let page = match call
        .state()
        .catalog()
        .album_page(&id, scopes, page_depth(call))
        .await
    {
        Ok(Some(page)) => page,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let database = call.state().database();
    let user_id = call.principal().user_id.uuid();

    let reads: Vec<_> = page
        .tracks
        .iter()
        .filter_map(|track| match track {
            PageTrack::Stored(id) => Some(browse::track_any(database, *id)),
            PageTrack::Catalog(_) => None,
        })
        .collect();
    let loaded = match stream::iter(reads)
        .buffered(READ_PARALLELISM)
        .try_collect::<Vec<_>>()
        .await
    {
        Ok(loaded) => loaded,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let mut by_id: HashMap<EntityId, LoadedTrack> = HashMap::new();
    for track in loaded.into_iter().flatten() {
        by_id.insert(track.view.track.id, track);
    }
    let ids: Vec<Uuid> = by_id.keys().map(|id| id.uuid()).collect();
    let annotations = if ids.is_empty() {
        HashMap::new()
    } else {
        match browse::annotations(call.state(), user_id, EntityKind::Track, &ids).await {
            Ok(annotations) => annotations,
            Err(error) => {
                return call.encode(Response::failed(browse::failure(
                    "getMusicDirectory",
                    &error,
                )));
            }
        }
    };

    let mut songs = Vec::with_capacity(page.tracks.len());
    for track in &page.tracks {
        match track {
            PageTrack::Stored(id) => {
                if let Some(loaded) = by_id.get(id) {
                    songs.push(projection::child(
                        &loaded.view,
                        loaded.file.as_ref(),
                        &loaded.album_artists,
                        annotations
                            .get(&id.uuid())
                            .unwrap_or(&Annotation::default()),
                    ));
                }
            }
            PageTrack::Catalog(track) => {
                songs.push(projection::catalog_child(&track.id, &track.track, &suffix));
            }
        }
    }
    let (head_id, name, parent, annotation) = match &page.head {
        AlbumHead::Stored(album_id) => {
            let detail = match browse::album_summary_any(database, *album_id).await {
                Ok(Some(detail)) => detail,
                Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
                Err(error) => {
                    return call.encode(Response::failed(browse::failure(
                        "getMusicDirectory",
                        &error,
                    )));
                }
            };
            let annotation =
                match browse::annotation(call.state(), user_id, EntityKind::Album, album_id.uuid())
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
            let parent = detail
                .artists
                .first()
                .map(|artist| artist.id.to_string())
                .unwrap_or_default();
            (
                album_id.to_string(),
                detail.album.title,
                parent,
                Some(annotation),
            )
        }
        AlbumHead::Catalog(found) => (
            found.id.to_string(),
            projection::catalog_album_name(found, &suffix),
            String::new(),
            None,
        ),
    };
    let directory = projection::directory(&head_id, &parent, &name, annotation.as_ref(), songs);
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// Returns the client id one album-page track answers to.
///
/// # Parameters
///
/// - `track`: the track of the page.
///
/// # Returns
///
/// The stored id for a stored track, the catalog id otherwise.
fn page_track_id(track: &PageTrack) -> ClientId {
    match track {
        PageTrack::Stored(id) => ClientId::Entity(*id),
        PageTrack::Catalog(track) => ClientId::Catalog(track.id.clone()),
    }
}

/// Returns how much a page may pair and write for one request.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// [`PageDepth::Cached`] when the client opted into the vendor extension, and
/// [`PageDepth::Library`] otherwise. A first-party page answers from the library
/// plus the cached catalog answers; a cold entity opens with one bounded read of
/// its own record, and never waits for its extras, which the read asks for
/// through a `catalog.enrich` job. A third-party client keeps the library-only
/// answer for a stored id.
fn page_depth(call: &Call) -> PageDepth {
    if call.params().vendor_enabled() {
        PageDepth::Cached
    } else {
        PageDepth::Library
    }
}

/// One song of a catalog view, with the id a client should use for it.
struct CatalogSong {
    /// The projected child.
    object: Object,
    /// The id the child answers to: the stored track's id when the recording is
    /// already stored, the catalog id otherwise.
    id: ClientId,
}

/// Renders the songs of a catalog album, or one catalog track, as the rows the
/// server already holds when their recording is stored.
///
/// A catalog view is built from the provider's answer, but a recording the
/// server stores is the same recording: it is projected as the stored track,
/// with its `tr_` id and its availability, instead of as a remote duplicate.
/// A stored track the caller's scopes do not contain stays a catalog entry, so
/// a non-administrator never sees content outside their library.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scopes`: the scopes the stored rows are looked for in.
/// - `slug`: the instance that issued the tracks.
/// - `tracks`: the catalog tracks to render.
/// - `suffix`: the suffix a catalog title carries.
///
/// # Returns
///
/// One rendered song per catalog track, in order.
///
/// # Errors
///
/// Answers a generic error when a stored row or an annotation cannot be read.
async fn catalog_songs(
    call: &Call,
    scopes: &[Scope],
    slug: &Slug,
    tracks: &[hearthsound_core::providers::CatalogTrack],
    suffix: &str,
) -> Result<Vec<CatalogSong>, SubsonicError> {
    let database = call.state().database();
    let stored = call
        .state()
        .catalog()
        .stored_recordings(slug, tracks)
        .await
        .map_err(|error| catalog::failure(&error))?;
    let mut loaded = Vec::with_capacity(tracks.len());
    for stored_id in &stored {
        let Some(stored_id) = stored_id else {
            loaded.push(None);
            continue;
        };
        match browse::track_in_scopes(database, scopes, *stored_id).await {
            Ok(Some((_scope, row))) => loaded.push(Some(row)),
            Ok(None) => loaded.push(None),
            Err(error) => return Err(browse::failure("a catalog view", &error)),
        }
    }
    let ids: Vec<Uuid> = loaded
        .iter()
        .flatten()
        .map(|loaded| loaded.view.track.id.uuid())
        .collect();
    let annotations = browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &ids,
    )
    .await
    .map_err(|error| browse::failure("a catalog view", &error))?;

    let mut songs = Vec::with_capacity(tracks.len());
    for (index, track) in tracks.iter().enumerate() {
        if let Some(loaded) = &loaded[index] {
            songs.push(CatalogSong {
                object: projection::child(
                    &loaded.view,
                    loaded.file.as_ref(),
                    &loaded.album_artists,
                    annotations
                        .get(&loaded.view.track.id.uuid())
                        .unwrap_or(&Annotation::default()),
                ),
                id: ClientId::Entity(loaded.view.track.id),
            });
            continue;
        }
        let Ok(track_id) = CatalogId::new(slug.clone(), EntityKind::Track, track.id.clone()) else {
            continue;
        };
        songs.push(CatalogSong {
            object: projection::catalog_child(&track_id, track, suffix),
            id: ClientId::Catalog(track_id),
        });
    }
    Ok(songs)
}

/// The catalog id through which a stored entity can also be read.
///
/// A browse request may name a stored entity the caller's scope does not
/// contain. Materialization records the provider id of the instance that
/// issued the entity, so the same entity can be answered from that instance's
/// catalog view instead of failing: the id a response handed out still opens.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `entity`: the stored entity.
///
/// # Returns
///
/// The catalog id naming the issuing instance's copy, or `None` when no
/// instance issued the entity.
///
/// # Errors
///
/// Answers a generic error when the alias lookup fails.
async fn catalog_alias(call: &Call, entity: EntityId) -> Result<Option<CatalogId>, SubsonicError> {
    let Some(kind) = entity.kind().entity_kind() else {
        return Ok(None);
    };
    let aliases = call
        .state()
        .catalog()
        .provider_aliases(entity)
        .await
        .map_err(|error| catalog::failure(&error))?;
    let Some(alias) = aliases.into_iter().next() else {
        return Ok(None);
    };
    match CatalogId::new(alias.slug, kind, alias.provider_id) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            tracing::debug!(%error, entity = %entity, "a stored entity's catalog alias is unusable");
            Ok(None)
        }
    }
}

/// Loads the user's annotations for a page of artists.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `summaries`: the artists of the page.
///
/// # Errors
///
/// Answers a generic Subsonic error when the read fails.
async fn annotations_for_artists(
    call: &Call,
    summaries: &[ArtistSummary],
) -> Result<std::collections::HashMap<Uuid, Annotation>, SubsonicError> {
    let ids: Vec<Uuid> = summaries
        .iter()
        .map(|summary| summary.artist.id.uuid())
        .collect();
    browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        &ids,
    )
    .await
    .map_err(|error| browse::failure("getIndexes", &error))
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
/// The albums with their details, in the order the request asked for, each with
/// the user's annotation.
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
) -> Result<Vec<(AlbumDetail, Annotation)>, SubsonicError> {
    let Some(list_type) = call.get("type") else {
        return Err(SubsonicError::missing_parameter("type"));
    };
    let sort = album_sort(call, list_type)?;
    let size = number(call, "size", DEFAULT_ALBUM_SIZE)?.clamp(0, MAX_ALBUM_SIZE);
    let offset = number(call, "offset", 0)?.max(0);

    let database = call.state().database();
    let listed = browse::albums(database, scope, sort, size, offset)
        .await
        .map_err(|error| browse::failure(operation, &error))?;

    let user_id = call.principal().user_id.uuid();
    let ids: Vec<Uuid> = listed.iter().map(|album| album.id.uuid()).collect();
    let annotations = browse::annotations(call.state(), user_id, EntityKind::Album, &ids)
        .await
        .map_err(|error| browse::failure(operation, &error))?;

    let ids: Vec<EntityId> = listed.iter().map(|album| album.id).collect();
    let reads: Vec<_> = ids
        .into_iter()
        .map(|album_id| browse::album_summary(database, scope, album_id))
        .collect();
    let details = stream::iter(reads)
        .buffered(READ_PARALLELISM)
        .try_collect::<Vec<_>>()
        .await
        .map_err(|error| browse::failure(operation, &error))?;

    let mut albums = Vec::with_capacity(listed.len());
    for detail in details.into_iter().flatten() {
        let annotation = annotations
            .get(&detail.album.id.uuid())
            .copied()
            .unwrap_or_default();
        albums.push((detail, annotation));
    }
    Ok(albums)
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
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let scopes = match scope::search_scopes(call).await {
        Ok(scopes) => scopes,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let track = match call.state().catalog().track(&id).await {
        Ok(track) => track,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let songs = match catalog_songs(call, &scopes, id.slug(), &[track.track], &suffix).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(song) = songs.into_iter().next() else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    record_listing(call, [song.id]);
    call.respond(Response::ok().member("song", song.object))
        .await
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
