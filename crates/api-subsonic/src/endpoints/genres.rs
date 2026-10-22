//! The genre endpoints: `getGenres`, `getSongsByGenre`, and `getRandomSongs`.

use hearthsound_core::{ClientId, EntityKind, IdPrefix};
use hearthsound_library::query::{Scope, TrackView};
use uuid::Uuid;

use crate::auth::Call;
use crate::endpoints::browse::number;
use crate::response::{Encoded, Response, SubsonicError};
use crate::scope;
use crate::value::{Array, Object};
use crate::{browse, catalog, projection};

/// The number of songs a genre or random listing returns when `count` or `size`
/// is absent.
const DEFAULT_SONGS: i64 = 10;

/// The largest number of songs a genre or random listing returns.
const MAX_SONGS: i64 = 500;

/// The number of songs `getTopSongs` returns without a count parameter.
const DEFAULT_TOP_SONGS: i64 = 50;

/// Answers `getGenres`: the genres of the tracks in scope with their counts.
///
/// The list is what a client offers to filter by, and the counts are what the
/// scope holds, so a personal listing never offers a genre only another
/// library's tracks carry.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `genres`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 for an unknown `musicFolderId` and a generic error
/// when a read fails.
pub async fn get_genres(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let counted = match browse::genres(call.state().database(), scope).await {
        Ok(counted) => counted,
        Err(error) => return call.encode(Response::failed(browse::failure("getGenres", &error))),
    };
    let genres: Vec<Object> = counted
        .iter()
        .map(|genre| {
            Object::new()
                .set("songCount", genre.song_count)
                .set("albumCount", genre.album_count)
                .set_content(genre.name.clone())
        })
        .collect();
    call.encode(Response::ok().member("genres", Object::new().set("genre", Array::new(genres))))
}

/// Answers `getSongsByGenre`: the songs in scope that carry one genre.
///
/// # Parameters
///
/// - `call`: the authenticated call. `genre` is required, `count` is the number
///   of songs (default 10, capped at 500), `offset` how many to skip, and
///   `musicFolderId` the scope.
///
/// # Returns
///
/// A successful envelope carrying `songsByGenre`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `genre` is missing, error 0 when `count` or
/// `offset` is not a number, error 70 for an unknown `musicFolderId`, and a
/// generic error when a read fails.
pub async fn get_songs_by_genre(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(genre) = call.get("genre").filter(|genre| !genre.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("genre")));
    };
    let count = match number(&call, "count", DEFAULT_SONGS) {
        Ok(count) => count.clamp(0, MAX_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let offset = match number(&call, "offset", 0) {
        Ok(offset) => offset.max(0),
        Err(error) => return call.encode(Response::failed(error)),
    };

    let views =
        match browse::tracks_by_genre(call.state().database(), scope, genre, count, offset).await {
            Ok(views) => views,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getSongsByGenre", &error)));
            }
        };
    let songs = match songs(&call, scope, views).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(
        Response::ok().member("songsByGenre", Object::new().set("song", Array::new(songs))),
    )
    .await
}

/// Answers `getRandomSongs`: a random sample of the songs in scope.
///
/// # Parameters
///
/// - `call`: the authenticated call. `size` is the number of songs (default 10,
///   capped at 500), `genre`, `fromYear`, and `toYear` narrow the sample, and
///   `musicFolderId` is the scope.
///
/// # Returns
///
/// A successful envelope carrying `randomSongs`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 0 when `size`, `fromYear`, or `toYear` is not a
/// number, error 70 for an unknown `musicFolderId`, and a generic error when a
/// read fails.
pub async fn get_random_songs(call: Call) -> Encoded {
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let size = match number(&call, "size", DEFAULT_SONGS) {
        Ok(size) => size.clamp(0, MAX_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let genre = call.get("genre").filter(|genre| !genre.is_empty());
    // An absent year is no filter at all, so zero is read as "no bound".
    let from_year = match number(&call, "fromYear", 0) {
        Ok(year) => (year != 0).then_some(year),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let to_year = match number(&call, "toYear", 0) {
        Ok(year) => (year != 0).then_some(year),
        Err(error) => return call.encode(Response::failed(error)),
    };

    let views = match browse::random_tracks(
        call.state().database(),
        scope,
        size,
        genre,
        from_year,
        to_year,
    )
    .await
    {
        Ok(views) => views,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getRandomSongs", &error)));
        }
    };
    let songs = match songs(&call, scope, views).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member("randomSongs", Object::new().set("song", Array::new(songs))))
        .await
}

/// Answers `getTopSongs`: the most played songs of one artist.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored artist or a catalog
///   artist and takes precedence over `artist`, a name; `count` is the number of
///   songs (default 50, capped at 500).
///
/// # Returns
///
/// A successful envelope carrying `topSongs`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when neither `id` nor `artist` is given, error 0
/// when `count` is not a number, error 70 when `id` is not an artist id, and a
/// generic error when a catalog artist cannot be reached or a read fails. A
/// name no artist has answers an empty listing.
///
/// # Stub
///
/// No popularity source exists yet, so this endpoint does not yet conform to
/// the Subsonic meaning of `getTopSongs`. A stored artist's songs come from
/// `top_tracks_for_artist`, ordered by the server's own play counts and then by
/// the oldest album, disc, and track; a catalog artist's songs keep the order
/// the provider returned. A real popularity source - play counts and/or catalog
/// popularity - must replace both orderings.
pub async fn get_top_songs(call: Call) -> Encoded {
    let count = match number(&call, "count", DEFAULT_TOP_SONGS) {
        Ok(count) => count.clamp(0, MAX_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let database = call.state().database();

    let views = match call.get("id").filter(|value| !value.is_empty()) {
        Some(value) => {
            match ClientId::parse(value) {
                Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Artist => {
                    match browse::top_tracks_for_artist(database, scope, id, count).await {
                        Ok(views) => views,
                        Err(error) => {
                            return call
                                .encode(Response::failed(browse::failure("getTopSongs", &error)));
                        }
                    }
                }
                Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Artist => {
                    let catalog = call.state().catalog();
                    let limit = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
                    match catalog.artist_top_tracks(&id, limit).await {
                        Ok(tracks) => {
                            let suffix =
                                match catalog::title_suffix(&call.state().settings_snapshot()) {
                                    Ok(suffix) => suffix,
                                    Err(error) => return call.encode(Response::failed(error)),
                                };
                            let songs: Vec<Object> = tracks
                                .iter()
                                .map(|track| {
                                    projection::catalog_child(&track.id, &track.track, &suffix)
                                })
                                .collect();
                            return call
                                .respond(Response::ok().member(
                                    "topSongs",
                                    Object::new().set("song", Array::new(songs)),
                                ))
                                .await;
                        }
                        Err(error) => {
                            return call.encode(Response::failed(catalog::failure(&error)));
                        }
                    }
                }
                _ => {
                    tracing::debug!("a top songs request named an id that is not an artist id");
                    return call.encode(Response::failed(SubsonicError::not_found()));
                }
            }
        }
        None => match call.get("artist").filter(|name| !name.is_empty()) {
            Some(name) => match browse::find_artist_by_name(database, name).await {
                Ok(Some(artist)) => {
                    match browse::top_tracks_for_artist(database, scope, artist.id, count).await {
                        Ok(views) => views,
                        Err(error) => {
                            return call
                                .encode(Response::failed(browse::failure("getTopSongs", &error)));
                        }
                    }
                }
                Ok(None) => {
                    return call.encode(Response::ok().member(
                        "topSongs",
                        Object::new().set("song", Array::new(Vec::<Object>::new())),
                    ));
                }
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("getTopSongs", &error)));
                }
            },
            None => {
                return call.encode(Response::failed(SubsonicError::missing_parameter("artist")));
            }
        },
    };

    let songs = match songs(&call, scope, views).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member("topSongs", Object::new().set("song", Array::new(songs))))
        .await
}

/// Answers `getSimilarSongs` and `getSimilarSongs2`: nothing yet.
///
/// The protocol asks a server for songs similar to one track. This server has
/// no similarity model, so it answers an empty list rather than failing: a
/// client that fills a "similar songs" panel shows nothing, which is honest
/// about what the server knows.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the track the request is about.
///
/// # Returns
///
/// A successful envelope carrying `similarSongs` or `similarSongs2`, or a
/// failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing.
pub async fn get_similar_songs(call: Call) -> Encoded {
    similar_songs(&call, "similarSongs").await
}

/// Answers `getSimilarSongs2`: the ID3 shape of the same empty answer.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `similarSongs2`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_similar_songs`] documents.
pub async fn get_similar_songs_2(call: Call) -> Encoded {
    similar_songs(&call, "similarSongs2").await
}

/// Answers a similar-songs request with an empty listing.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the empty listing is carried in.
///
/// # Returns
///
/// A successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing.
async fn similar_songs(call: &Call, member: &'static str) -> Encoded {
    if call.get("id").filter(|value| !value.is_empty()).is_none() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    }
    call.encode(Response::ok().member(
        member,
        Object::new().set("song", Array::new(Vec::<Object>::new())),
    ))
}

/// Projects a listing of songs, with the user's annotations for each.
///
/// A song a listing returned is re-read in scope so that its preferred file and
/// its album's artists are projected the same way every other song listing
/// projects them; one that a concurrent write took out of scope is left out.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `scope`: the scope the listing was read in.
/// - `views`: the songs to project.
///
/// # Returns
///
/// One `Child` per song, in the order the listing returned them.
///
/// # Errors
///
/// Returns a library failure when a read fails and a Subsonic error when the
/// user's annotations cannot be read.
async fn songs(
    call: &Call,
    scope: Scope,
    views: Vec<TrackView>,
) -> Result<Vec<Object>, SubsonicError> {
    let ids: Vec<Uuid> = views.iter().map(|view| view.track.id.uuid()).collect();
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => return Err(browse::failure("a song listing", &error)),
    };
    let mut songs = Vec::with_capacity(views.len());
    for view in views {
        let loaded = match browse::track(call.state().database(), scope, view.track.id).await {
            Ok(Some(loaded)) => loaded,
            Ok(None) => continue,
            Err(error) => return Err(browse::failure("a song listing", &error)),
        };
        songs.push(projection::child(
            &loaded.view,
            loaded.file.as_ref(),
            &loaded.album_artists,
            &annotations
                .get(&loaded.view.track.id.uuid())
                .cloned()
                .unwrap_or_default(),
        ));
    }
    Ok(songs)
}
