//! The genre endpoints: `getGenres`, `getSongsByGenre`, and `getRandomSongs`.

use hearthsound_core::EntityKind;
use hearthsound_library::query::{Scope, TrackView};
use uuid::Uuid;

use crate::auth::Call;
use crate::endpoints::browse::number;
use crate::response::{Encoded, Response, SubsonicError};
use crate::scope;
use crate::value::{Array, Object};
use crate::{browse, projection};

/// The number of songs a genre or random listing returns when `count` or `size`
/// is absent.
const DEFAULT_SONGS: i64 = 10;

/// The largest number of songs a genre or random listing returns.
const MAX_SONGS: i64 = 500;

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
    call.encode(Response::ok().member("songsByGenre", Object::new().set("song", Array::new(songs))))
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
    call.encode(Response::ok().member("randomSongs", Object::new().set("song", Array::new(songs))))
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
