//! The genre and suggestion endpoints: `getGenres`, `getSongsByGenre`,
//! `getRandomSongs`, `getTopSongs`, `getSimilarSongs`, and `getSimilarSongs2`.
//!
//! A suggestion answers from the discovery layer when the server has one: a
//! track id seeds similar tracks, and an album or artist id seeds similar
//! artists whose popular tracks are interleaved. Every suggestion is resolved to
//! a row before it is projected, so a suggested song is served as the row it
//! names and a song the layer suggests twice is listed once. Without a discovery
//! layer the suggestion endpoints answer empty.
//!
//! `getTopSongs` reads the artist's page instead: its top tracks are the
//! popularity the indexer stored, followed by the artist's own library tracks,
//! so a panel does not wait for a provider.

use std::collections::HashSet;

use hearthsound_core::providers::{ArtistSeed, ExternalId, ISRC, MB_RECORDING, TrackSeed};
use hearthsound_core::{EntityId, EntityKind, IdPrefix};

use crate::auth::Call;
use crate::endpoints::browse::{number, viewer};
use crate::response::{Encoded, Response, SubsonicError};
use crate::scope;
use crate::value::{Array, Object};
use crate::{browse, catalog};

/// The number of songs a genre or random listing returns when `count` or `size`
/// is absent.
const DEFAULT_SONGS: i64 = 10;

/// The largest number of songs a genre or random listing returns.
const MAX_SONGS: i64 = 500;

/// The number of songs `getTopSongs` returns without a count parameter.
const DEFAULT_TOP_SONGS: i64 = 50;

/// The number of similar songs a request returns without a count parameter.
const DEFAULT_SIMILAR_SONGS: i64 = 50;

/// The largest number of similar songs a request may ask for.
const MAX_SIMILAR_SONGS: i64 = 500;

/// How many similar artists seed the songs of an album or artist request.
const SIMILAR_ARTIST_SEEDS: u32 = 5;

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
    let ids: Vec<EntityId> = views.into_iter().map(|view| view.track.id).collect();
    let songs = match browse::songs(&call, &ids).await {
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
    let ids: Vec<EntityId> = views.into_iter().map(|view| view.track.id).collect();
    let songs = match browse::songs(&call, &ids).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member("randomSongs", Object::new().set("song", Array::new(songs))))
        .await
}

/// Answers `getTopSongs`: the most popular songs of one artist.
///
/// The artist's page answers: its stored top tracks first, then the artist's own
/// library tracks, so the panel is the same list the artist page shows.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names an artist and takes precedence
///   over `artist`, a name; `count` is the number of songs (default 50, capped
///   at 500).
///
/// # Returns
///
/// A successful envelope carrying `topSongs`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when neither `id` nor `artist` is given, error 0
/// when `count` is not a number, error 70 when `id` is not an artist id, and a
/// generic error when a read fails. A name no artist has answers an empty
/// listing.
pub async fn get_top_songs(call: Call) -> Encoded {
    let count = match number(&call, "count", DEFAULT_TOP_SONGS) {
        Ok(count) => count.clamp(0, MAX_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let artist = match requested_artist(&call).await {
        Ok(Some(artist)) => artist,
        Ok(None) => return empty_top_songs(&call).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let tracks = match call
        .state()
        .catalog()
        .views()
        .top_tracks(
            artist,
            u32::try_from(count).unwrap_or(u32::MAX),
            viewer(&call),
        )
        .await
    {
        Ok(tracks) => tracks,
        Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
    };
    let ids: Vec<EntityId> = tracks.into_iter().map(|track| track.track.id).collect();
    let songs = match browse::songs(&call, &ids).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member("topSongs", Object::new().set("song", Array::new(songs))))
        .await
}

/// Answers `getSimilarSongs` and `getSimilarSongs2` from the discovery layer.
///
/// A track id seeds similar tracks. An album or artist id seeds five similar
/// artists, asks each for its popular tracks, and interleaves the answers, which
/// is what makes an artist's similar-songs panel a mix instead of one artist's
/// discography; a song two of them name is listed once, at its first position.
/// A server with no discovery instance answers an empty list rather than
/// failing, so a client can call the endpoint unconditionally.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a track, album or artist;
///   `count` is the number of songs (default 50, capped at 500).
///
/// # Returns
///
/// A successful envelope carrying `similarSongs` or `similarSongs2`, or a
/// failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 0 when `count` is not
/// a number, error 70 when `id` names no track, album or artist, and a generic
/// error when a read or a discovery call fails.
pub async fn get_similar_songs(call: Call) -> Encoded {
    similar_songs(&call, "similarSongs").await
}

/// Answers `getSimilarSongs2`: the ID3 shape of the same answer.
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

/// Answers a similar-songs request.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the listing is carried in.
///
/// # Returns
///
/// A successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`get_similar_songs`] documents.
async fn similar_songs(call: &Call, member: &'static str) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let count = match number(call, "count", DEFAULT_SIMILAR_SONGS) {
        Ok(count) => count.clamp(0, MAX_SIMILAR_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    if count == 0 {
        return empty_similar_songs(call, member).await;
    }
    let Some(discovery) = call.state().discovery() else {
        return empty_similar_songs(call, member).await;
    };
    let seed = match similar_seed(call, value).await {
        Ok(Some(seed)) => seed,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let limit = u32::try_from(count).unwrap_or(u32::MAX);
    let ids = match seed {
        SimilarSeed::Track(seed) => {
            let suggestions = match discovery.similar_tracks(&seed, limit).await {
                Ok(suggestions) => suggestions,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            match discovery.resolve_tracks(&suggestions, limit).await {
                Ok(resolved) => resolved.into_iter().flatten().collect(),
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            }
        }
        SimilarSeed::Artist(seed) => {
            let seeds = match discovery.similar_artists(&seed, SIMILAR_ARTIST_SEEDS).await {
                Ok(artists) => artists,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            let mut lists = Vec::with_capacity(seeds.len());
            for artist in seeds {
                let seed = ArtistSeed {
                    name: artist.name,
                    ids: artist.ids,
                };
                let tracks = match discovery.artist_top_tracks(&seed, limit).await {
                    Ok(tracks) => tracks,
                    Err(error) => {
                        return call.encode(Response::failed(catalog::failure(&error)));
                    }
                };
                match discovery.resolve_tracks(&tracks, limit).await {
                    Ok(resolved) => lists.push(resolved.into_iter().flatten().collect()),
                    Err(error) => {
                        return call.encode(Response::failed(catalog::failure(&error)));
                    }
                }
            }
            interleave(lists, usize::try_from(count).unwrap_or(usize::MAX))
        }
    };
    let songs = match browse::songs(call, &ids).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    call.respond(Response::ok().member(member, Object::new().set("song", Array::new(songs))))
        .await
}

/// Answers an empty similar-songs listing.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `member`: the member the listing is carried in.
///
/// # Returns
///
/// A successful envelope carrying no songs.
async fn empty_similar_songs(call: &Call, member: &'static str) -> Encoded {
    call.encode(Response::ok().member(
        member,
        Object::new().set("song", Array::new(Vec::<Object>::new())),
    ))
}

/// Answers an empty top-songs listing.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying no songs.
async fn empty_top_songs(call: &Call) -> Encoded {
    call.encode(Response::ok().member(
        "topSongs",
        Object::new().set("song", Array::new(Vec::<Object>::new())),
    ))
}

/// Reads the artist a `getTopSongs` request names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The artist, or `None` when the request named a name no stored artist has.
///
/// # Errors
///
/// Answers Subsonic error 10 when neither parameter is given, error 70 when
/// `id` is not an artist id or names no artist, and a generic error when a read
/// fails.
async fn requested_artist(call: &Call) -> Result<Option<EntityId>, SubsonicError> {
    if let Some(value) = call.get("id").filter(|value| !value.is_empty()) {
        return match browse::entity_id(call.state(), IdPrefix::Artist, value).await {
            Ok(artist) => Ok(artist),
            Err(error) => Err(error),
        };
    }
    let Some(name) = call.get("artist").filter(|name| !name.is_empty()) else {
        return Err(SubsonicError::missing_parameter("artist"));
    };
    match browse::find_artist_by_name(call.state().database(), name).await {
        Ok(found) => Ok(found.map(|artist| artist.id)),
        Err(error) => Err(browse::failure("getTopSongs", &error)),
    }
}

/// The seed a similar-songs request names.
enum SimilarSeed {
    /// A track id seeded a similar-track query.
    Track(TrackSeed),
    /// An album or artist id seeded a similar-artist query.
    Artist(ArtistSeed),
}

/// Reads the seed a similar-songs request names.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the `id` parameter.
///
/// # Returns
///
/// The seed, or `None` when the id names no track, album or artist.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn similar_seed(call: &Call, value: &str) -> Result<Option<SimilarSeed>, SubsonicError> {
    let id = match browse::any_entity(call.state(), value).await {
        Ok(Some(id)) => id,
        Ok(None) => return Ok(None),
        Err(error) => return Err(error),
    };
    let database = call.state().database();
    match id.kind() {
        IdPrefix::Track => {
            let Some(track) = (match browse::track_any(database, id).await {
                Ok(track) => track,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            }) else {
                return Ok(None);
            };
            let ids = match browse::identifiers_of(database, EntityKind::Track, id.uuid()).await {
                Ok(ids) => ids,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            };
            Ok(Some(SimilarSeed::Track(track_seed(&track.view, &ids))))
        }
        IdPrefix::Album => {
            let Some(album) = (match browse::album_any(database, id).await {
                Ok(album) => album,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            }) else {
                return Ok(None);
            };
            let Some(artist) = album.artists.first() else {
                return Ok(None);
            };
            let ids = match browse::identifiers_of(database, EntityKind::Artist, artist.id.uuid())
                .await
            {
                Ok(ids) => ids,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            };
            Ok(Some(SimilarSeed::Artist(ArtistSeed {
                name: artist.name.clone(),
                ids,
            })))
        }
        IdPrefix::Artist => {
            let Some(artist) = (match browse::artist_any(database, id).await {
                Ok(artist) => artist,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            }) else {
                return Ok(None);
            };
            let ids = match browse::identifiers_of(database, EntityKind::Artist, id.uuid()).await {
                Ok(ids) => ids,
                Err(error) => return Err(browse::failure("getSimilarSongs", &error)),
            };
            Ok(Some(SimilarSeed::Artist(ArtistSeed {
                name: artist.artist.name,
                ids,
            })))
        }
        _ => Ok(None),
    }
}

/// Builds the seed a similar-track query is made with.
///
/// # Parameters
///
/// - `view`: the stored track.
/// - `ids`: the track's stored identifiers.
///
/// # Returns
///
/// The title, the credit names in order, and the identifiers the entity states.
fn track_seed(view: &hearthsound_library::query::TrackView, ids: &[ExternalId]) -> TrackSeed {
    let mut artists: Vec<String> = view
        .artists
        .iter()
        .map(|artist| artist.name.clone())
        .collect();
    if artists.is_empty() {
        artists.extend(view.track.display_artist.clone());
    }
    TrackSeed {
        title: view.track.title.clone(),
        artists,
        ids: canonical_seed_ids(ids),
    }
}

/// Returns the identifiers a discovery query answers from.
///
/// # Parameters
///
/// - `ids`: the stored identifiers of a track.
///
/// # Returns
///
/// The ISRC and MusicBrainz recording identifiers, which are the ones a
/// discovery service matches on.
fn canonical_seed_ids(ids: &[ExternalId]) -> Vec<ExternalId> {
    ids.iter()
        .filter(|id| matches!(id.namespace.as_str(), ISRC | MB_RECORDING) && !id.value.is_empty())
        .cloned()
        .collect()
}

/// Interleaves one list of songs per artist.
///
/// # Parameters
///
/// - `lists`: the per-artist lists, in the order the artists were suggested.
/// - `limit`: the maximum number of songs.
///
/// # Returns
///
/// The first song of every artist, then the second of every artist, and so on,
/// up to `limit` distinct songs.
fn interleave(lists: Vec<Vec<EntityId>>, limit: usize) -> Vec<EntityId> {
    let mut songs = Vec::new();
    let mut seen = HashSet::new();
    let mut index = 0;
    loop {
        let mut advanced = false;
        for list in &lists {
            if let Some(song) = list.get(index) {
                advanced = true;
                if seen.insert(*song) {
                    songs.push(*song);
                    if songs.len() >= limit {
                        return songs;
                    }
                }
            }
        }
        if !advanced {
            break;
        }
        index += 1;
    }
    songs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a track id from a fresh UUIDv7.
    fn track() -> EntityId {
        EntityId::new(IdPrefix::Track)
    }

    #[test]
    fn interleaving_takes_one_song_of_each_artist_at_a_time() {
        let first = vec![track(), track()];
        let second = vec![track(), track()];
        let listed = interleave(vec![first.clone(), second.clone()], 3);

        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0], first[0]);
        assert_eq!(listed[1], second[0]);
        assert_eq!(listed[2], first[1]);
    }

    #[test]
    fn interleaving_lists_a_song_once() {
        let shared = track();
        let listed = interleave(vec![vec![shared], vec![shared]], 10);

        assert_eq!(listed, vec![shared]);
    }
}
