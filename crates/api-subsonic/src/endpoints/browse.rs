//! The browse endpoints: the artist index and detail, the album detail, a
//! single song, and the album list.
//!
//! Every endpoint reads the scope [`crate::scope`] resolves, asks
//! [`crate::browse`] for the rows, and builds its payload from
//! [`crate::projection`], so the five responses share one scope rule and one
//! set of members.

use hearthsound_activity::Annotation;
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query::{AlbumSort, Scope};
use uuid::Uuid;

use crate::auth::Call;
use crate::catalog;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, projection, scope};
use hearthsound_library::query::ArtistSummary;

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
    call.respond(
        Response::ok().member("artists", projection::artists_id3(&summaries, &annotations)),
    )
    .await
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

    // The artist and its albums carry the user's own stars, ratings, and play
    // counts, which one read per entity kind answers for the whole response.
    let album_ids: Vec<Uuid> = detail.albums.iter().map(|album| album.id.uuid()).collect();
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Album,
        &album_ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getArtist", &error)));
        }
    };
    let artist_annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        detail.artist.id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
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
                annotations
                    .get(&loaded.detail.album.id.uuid())
                    .unwrap_or(&Annotation::default()),
            )),
            Ok(None) => {}
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getArtist", &error)));
            }
        }
    }

    let artist = projection::artist_id3(&detail.artist, albums.len() as i64, &artist_annotation)
        .set("album", Array::new(albums));
    call.respond(Response::ok().member("artist", artist)).await
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

    let user_id = call.principal().user_id.uuid();
    let album_annotation = match browse::annotation(
        call.state(),
        user_id,
        EntityKind::Album,
        loaded.detail.album.id.uuid(),
    )
    .await
    {
        Ok(annotation) => annotation,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getAlbum", &error)));
        }
    };
    let track_ids: Vec<Uuid> = loaded
        .detail
        .tracks
        .iter()
        .map(|view| view.track.id.uuid())
        .collect();
    let annotations =
        match browse::annotations(call.state(), user_id, EntityKind::Track, &track_ids).await {
            Ok(annotations) => annotations,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getAlbum", &error)));
            }
        };

    let album = projection::album_id3(
        &loaded.detail.album,
        &loaded.detail.artists,
        &loaded.detail.tracks,
        &album_annotation,
    );
    let songs = loaded
        .detail
        .tracks
        .iter()
        .zip(&loaded.files)
        .map(|(view, file)| {
            projection::child(
                view,
                file.as_ref(),
                &loaded.detail.artists,
                annotations
                    .get(&view.track.id.uuid())
                    .unwrap_or(&Annotation::default()),
            )
        });
    record_listing(
        &call,
        loaded
            .detail
            .tracks
            .iter()
            .map(|view| ClientId::Entity(view.track.id)),
    );
    let album = album.set("song", Array::new(songs));
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
        .map(|(loaded, annotation)| {
            projection::album_id3(
                &loaded.detail.album,
                &loaded.detail.artists,
                &loaded.detail.tracks,
                annotation,
            )
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
        .map(|(loaded, annotation)| {
            projection::album_dir(
                &loaded.detail.album,
                &loaded.detail.artists,
                &loaded.detail.tracks,
                annotation,
            )
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

    let summaries = match browse::artists(database, scope).await {
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
/// An artist lists its albums, an album lists its songs, and a catalog artist
/// or album lists the same children `getArtist` and `getAlbum` would answer
/// with, because the catalog is browsed through the same directory shape.
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
/// neither an artist nor an album, is not in scope, or names a catalog entity
/// the catalog does not have, and a generic error when a read fails.
pub async fn get_music_directory(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(value) = call.get("id") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    // A directory is an artist or an album, and a client may name either by
    // its stored id or by a catalog id; anything else is answered as unknown.
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) => match id.kind() {
            IdPrefix::Artist => artist_directory(&call, scope, ClientId::Entity(id)).await,
            IdPrefix::Album => album_directory(&call, scope, ClientId::Entity(id)).await,
            _ => call.encode(Response::failed(SubsonicError::not_found())),
        },
        Ok(ClientId::Catalog(id)) => match id.kind() {
            EntityKind::Artist | EntityKind::Album => catalog_directory(&call, id).await,
            _ => call.encode(Response::failed(SubsonicError::not_found())),
        },
        Err(error) => {
            tracing::debug!(%error, "a getMusicDirectory request named an unknown id");
            call.encode(Response::failed(SubsonicError::not_found()))
        }
    }
}

/// Answers `getMusicDirectory` for a stored artist.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scope`: the scope to resolve the artist in.
/// - `id`: the artist's client id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the artist is not in scope, and a generic
/// error when a read fails.
async fn artist_directory(call: &Call, scope: Scope, id: ClientId) -> Encoded {
    let Some(artist_id) = entity_id(&id) else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    let database = call.state().database();
    let detail = match browse::artist(database, scope, artist_id).await {
        Ok(Some(detail)) => detail,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let album_ids: Vec<Uuid> = detail.albums.iter().map(|album| album.id.uuid()).collect();
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Album,
        &album_ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };
    let artist_annotation = match browse::annotation(
        call.state(),
        call.principal().user_id.uuid(),
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

    let mut children = Vec::new();
    for album in &detail.albums {
        match browse::album(database, scope, album.id).await {
            Ok(Some(loaded)) => children.push(projection::album_dir(
                &loaded.detail.album,
                &loaded.detail.artists,
                &loaded.detail.tracks,
                annotations
                    .get(&loaded.detail.album.id.uuid())
                    .unwrap_or(&Annotation::default()),
            )),
            Ok(None) => {}
            Err(error) => {
                return call.encode(Response::failed(browse::failure(
                    "getMusicDirectory",
                    &error,
                )));
            }
        }
    }
    let directory = projection::directory(
        &id.to_string(),
        "",
        &detail.artist.name,
        Some(&artist_annotation),
        children,
    );
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// Answers `getMusicDirectory` for a stored album.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scope`: the scope to resolve the album in.
/// - `id`: the album's client id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the album is not in scope, and a generic error
/// when a read fails.
async fn album_directory(call: &Call, scope: Scope, id: ClientId) -> Encoded {
    let Some(album_id) = entity_id(&id) else {
        return call.encode(Response::failed(SubsonicError::not_found()));
    };
    let loaded = match browse::album(call.state().database(), scope, album_id).await {
        Ok(Some(loaded)) => loaded,
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
        call.principal().user_id.uuid(),
        EntityKind::Album,
        album_id.uuid(),
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
    let track_ids: Vec<Uuid> = loaded
        .detail
        .tracks
        .iter()
        .map(|view| view.track.id.uuid())
        .collect();
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &track_ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getMusicDirectory",
                &error,
            )));
        }
    };

    let songs: Vec<Object> = loaded
        .detail
        .tracks
        .iter()
        .zip(&loaded.files)
        .map(|(view, file)| {
            projection::child(
                view,
                file.as_ref(),
                &loaded.detail.artists,
                annotations
                    .get(&view.track.id.uuid())
                    .unwrap_or(&Annotation::default()),
            )
        })
        .collect();
    let parent = loaded
        .detail
        .artists
        .first()
        .map(|artist| artist.id.to_string())
        .unwrap_or_default();
    let directory = projection::directory(
        &id.to_string(),
        &parent,
        &loaded.detail.album.title,
        Some(&annotation),
        songs,
    );
    call.respond(Response::ok().member("directory", directory))
        .await
}

/// Answers `getMusicDirectory` for an unmaterialized catalog entity.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `id`: the catalog id of an artist or an album.
///
/// # Errors
///
/// Answers Subsonic error 70 for a catalog id that names something else or an
/// entity the catalog does not have, and a generic error when the issuing
/// instance cannot answer.
async fn catalog_directory(call: &Call, id: CatalogId) -> Encoded {
    let snapshot = call.state().settings_snapshot();
    let suffix = match catalog::title_suffix(&snapshot) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    match id.kind() {
        EntityKind::Artist => {
            let catalog = call.state().catalog();
            let artist = match catalog.artist(&id).await {
                Ok(artist) => artist,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            let albums = match catalog.artist_albums(&id, MAX_CATALOG_ALBUMS, 0).await {
                Ok(albums) => albums,
                Err(error) => {
                    tracing::debug!(error = %error, "could not list a catalog artist's albums");
                    return call.encode(Response::failed(catalog::failure(&error)));
                }
            };
            // The catalog already named each album with the id a client uses,
            // so the children need no new ids built for them.
            let children: Vec<Object> = albums
                .iter()
                .map(|album| projection::catalog_album_dir(&album.id, album, &suffix))
                .collect();
            let directory =
                projection::directory(&id.to_string(), "", &artist.artist.name, None, children);
            call.respond(Response::ok().member("directory", directory))
                .await
        }
        EntityKind::Album => {
            let album = match call.state().catalog().album(&id).await {
                Ok(album) => album,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            let songs: Vec<Object> = album
                .album
                .tracks
                .iter()
                .filter_map(|track| {
                    let track_id =
                        CatalogId::new(id.slug().clone(), EntityKind::Track, track.id.clone())
                            .ok()?;
                    Some(projection::catalog_child(&track_id, track, &suffix))
                })
                .collect();
            let name = projection::catalog_album_name(&album, &suffix);
            let directory = projection::directory(&id.to_string(), "", &name, None, songs);
            call.respond(Response::ok().member("directory", directory))
                .await
        }
        _ => call.encode(Response::failed(SubsonicError::not_found())),
    }
}

/// The entity id of a catalog id, which a directory read cannot answer.
///
/// # Parameters
///
/// - `id`: the client id.
///
/// # Returns
///
/// The entity id, or `None` when the id names a catalog entity.
fn entity_id(id: &ClientId) -> Option<EntityId> {
    match id {
        ClientId::Entity(id) => Some(*id),
        ClientId::Catalog(_) => None,
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
) -> Result<Vec<(browse::LoadedAlbum, Annotation)>, SubsonicError> {
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

    let mut albums = Vec::with_capacity(listed.len());
    for album in listed {
        match browse::album(database, scope, album.id).await {
            Ok(Some(loaded)) => {
                let annotation = annotations
                    .get(&loaded.detail.album.id.uuid())
                    .copied()
                    .unwrap_or_default();
                albums.push((loaded, annotation));
            }
            Ok(None) => {}
            Err(error) => return Err(browse::failure(operation, &error)),
        }
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
    let track = match call.state().catalog().track(&id).await {
        Ok(track) => track,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let song = projection::catalog_child(&track.id, &track.track, &suffix);
    call.respond(Response::ok().member("song", song)).await
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
        songs.push(projection::catalog_child(&track_id, track, &suffix));
        listing.push(ClientId::Catalog(track_id));
    }
    record_listing(call, listing);
    let album = projection::catalog_album(&album, &suffix).set("song", Array::new(songs));
    call.respond(Response::ok().member("album", album)).await
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
    call.respond(Response::ok().member("artist", artist)).await
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
