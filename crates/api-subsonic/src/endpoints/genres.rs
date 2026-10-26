//! The genre and suggestion endpoints: `getGenres`, `getSongsByGenre`,
//! `getRandomSongs`, `getTopSongs`, `getSimilarSongs`, and `getSimilarSongs2`.
//!
//! A suggestion answers from the discovery layer when the server has one: a
//! track id seeds similar tracks, and an album or artist id seeds similar
//! artists whose popular tracks are interleaved. Every resolved suggestion is
//! rendered through the same local-then-catalog merge `search3` uses, so a
//! suggested song the server already stores is served as the stored song and the
//! rest are served as catalog songs, and a song the layer suggests twice is
//! listed once. Without a discovery layer the suggestion endpoints answer empty,
//! and `getTopSongs` falls back to the library's own play and like counts.

use std::collections::HashSet;

use hearthsound_catalog::{CatalogSearch, CatalogTrackResult, LocalMatches, LocalTrackHit};
use hearthsound_core::providers::{
    ArtistSeed, ExternalId, ISRC, MB_ARTIST, MB_RECORDING, MB_RELEASE, TrackSeed,
};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::query::{self, Scope, TrackView};
use hearthsound_library::{Artist, LibraryRepository};
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

/// The number of similar songs a request returns without a count parameter.
const DEFAULT_SIMILAR_SONGS: i64 = 50;

/// The largest number of similar songs a request may ask for.
const MAX_SIMILAR_SONGS: i64 = 500;

/// How many similar artists seed the songs of an album or artist request.
const SIMILAR_ARTIST_SEEDS: u32 = 5;

/// How many local tracks are read to decide whether a suggestion is stored.
const LOCAL_MATCH_LIMIT: i64 = 5;

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

/// Answers `getTopSongs`: the most popular songs of one artist.
///
/// The discovery layer answers first when one is configured: its
/// `artist-top-tracks` result is the popularity source, and every suggestion is
/// resolved and rendered like a similar song, so a song the layer names twice is
/// listed once. A server with no discovery instance, or one whose instances
/// have nothing to say, falls back to the library. A stored artist's songs come
/// from `top_tracks_for_artist`, ordered
/// by the server's own play count, then by how many users like the song, then
/// by the oldest album, disc and track. A catalog artist's songs keep the order
/// the provider returned, which is the catalog's own popularity.
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
pub async fn get_top_songs(call: Call) -> Encoded {
    let count = match number(&call, "count", DEFAULT_TOP_SONGS) {
        Ok(count) => count.clamp(0, MAX_SONGS),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let requested = match requested_artist(&call).await {
        Ok(Some(requested)) => requested,
        Ok(None) => return empty_top_songs(&call).await,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let seed = match requested_seed(&call, &requested).await {
        Ok(seed) => seed,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let discovery = if count > 0 {
        call.state().discovery()
    } else {
        None
    };
    if let Some(discovery) = discovery {
        let limit = u32::try_from(count).unwrap_or(u32::MAX);
        let user = call.principal().user_id.uuid();
        match discovery.artist_top_tracks(user, &seed, limit).await {
            Ok(suggestions) if !suggestions.is_empty() => {
                match discovery.resolve_tracks(&suggestions, limit).await {
                    Ok(resolved) if !resolved.is_empty() => {
                        let songs = match resolved_song_objects(&call, resolved).await {
                            Ok(songs) => songs,
                            Err(error) => return call.encode(Response::failed(error)),
                        };
                        return call
                            .respond(
                                Response::ok().member(
                                    "topSongs",
                                    Object::new().set("song", Array::new(songs)),
                                ),
                            )
                            .await;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        return call.encode(Response::failed(catalog::failure(&error)));
                    }
                }
            }
            Ok(_) => {}
            Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
        }
    }

    let songs = match requested {
        RequestedArtist::Stored(id) => {
            let database = call.state().database();
            match browse::top_tracks_for_artist(database, scope, id, count).await {
                Ok(views) => match songs(&call, scope, views).await {
                    Ok(songs) => songs,
                    Err(error) => return call.encode(Response::failed(error)),
                },
                Err(error) => {
                    return call.encode(Response::failed(browse::failure("getTopSongs", &error)));
                }
            }
        }
        RequestedArtist::Catalog(id) => {
            let catalog = call.state().catalog();
            let limit = u32::try_from(count.max(0)).unwrap_or(u32::MAX);
            match catalog.artist_top_tracks(&id, limit).await {
                Ok(tracks) => {
                    let suffix = match catalog::title_suffix(&call.state().settings_snapshot()) {
                        Ok(suffix) => suffix,
                        Err(error) => return call.encode(Response::failed(error)),
                    };
                    tracks
                        .iter()
                        .map(|track| projection::catalog_child(&track.id, &track.track, &suffix))
                        .collect()
                }
                Err(error) => {
                    return call.encode(Response::failed(catalog::failure(&error)));
                }
            }
        }
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
/// A server with no discovery instance answers an empty list rather
/// than failing, so a client can call the endpoint unconditionally.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored or catalog track,
///   album or artist; `count` is the number of songs (default 50, capped at
///   500).
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
/// error when a read or a catalog call fails.
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
    let Some(discovery) = call.state().discovery() else {
        return empty_similar_songs(call, member).await;
    };
    if count == 0 {
        return empty_similar_songs(call, member).await;
    }
    let seed = match similar_seed(call, value).await {
        Ok(Some(seed)) => seed,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let limit = u32::try_from(count).unwrap_or(u32::MAX);
    let resolved = match seed {
        SimilarSeed::Track(seed) => {
            let suggestions = match discovery.similar_tracks(user, &seed, limit).await {
                Ok(suggestions) => suggestions,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            match discovery.resolve_tracks(&suggestions, limit).await {
                Ok(resolved) => resolved,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            }
        }
        SimilarSeed::Artist(seed) => {
            let seeds = match discovery
                .similar_artists(user, &seed, SIMILAR_ARTIST_SEEDS)
                .await
            {
                Ok(artists) => artists,
                Err(error) => return call.encode(Response::failed(catalog::failure(&error))),
            };
            let mut lists = Vec::with_capacity(seeds.len());
            for artist in seeds {
                let seed = ArtistSeed {
                    name: artist.name,
                    ids: artist.ids,
                };
                let tracks = match discovery.artist_top_tracks(user, &seed, limit).await {
                    Ok(tracks) => tracks,
                    Err(error) => {
                        return call.encode(Response::failed(catalog::failure(&error)));
                    }
                };
                match discovery.resolve_tracks(&tracks, limit).await {
                    Ok(resolved) => lists.push(resolved),
                    Err(error) => {
                        return call.encode(Response::failed(catalog::failure(&error)));
                    }
                }
            }
            interleave(lists, usize::try_from(count).unwrap_or(usize::MAX))
        }
    };
    let songs = match resolved_song_objects(call, resolved).await {
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

/// The artist a top-songs request names.
enum RequestedArtist {
    /// A stored artist of the library.
    Stored(EntityId),
    /// An artist the catalog layer knows.
    Catalog(CatalogId),
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
/// `id` is not an artist id, and a generic error when a read fails.
async fn requested_artist(call: &Call) -> Result<Option<RequestedArtist>, SubsonicError> {
    if let Some(value) = call.get("id").filter(|value| !value.is_empty()) {
        return match ClientId::parse(value) {
            Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Artist => {
                Ok(Some(RequestedArtist::Stored(id)))
            }
            Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Artist => {
                Ok(Some(RequestedArtist::Catalog(id)))
            }
            _ => {
                tracing::debug!("a top songs request named an id that is not an artist id");
                Err(SubsonicError::not_found())
            }
        };
    }
    let Some(name) = call.get("artist").filter(|name| !name.is_empty()) else {
        return Err(SubsonicError::missing_parameter("artist"));
    };
    match browse::find_artist_by_name(call.state().database(), name).await {
        Ok(found) => Ok(found.map(|artist| RequestedArtist::Stored(artist.id))),
        Err(error) => Err(browse::failure("getTopSongs", &error)),
    }
}

/// Builds the seed a top-songs discovery query is made with.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `requested`: the artist the request named.
///
/// # Returns
///
/// The artist's name and the identifiers the server knows.
///
/// # Errors
///
/// Answers Subsonic error 70 when a stored artist has disappeared and a generic
/// error when a read fails or the catalog cannot answer.
async fn requested_seed(
    call: &Call,
    requested: &RequestedArtist,
) -> Result<ArtistSeed, SubsonicError> {
    match requested {
        RequestedArtist::Stored(id) => {
            let library = LibraryRepository::new(call.state().database());
            match library.get_artist(*id).await {
                Ok(Some(artist)) => Ok(artist_seed(&artist)),
                Ok(None) => Err(SubsonicError::not_found()),
                Err(error) => Err(browse::failure("getTopSongs", &error)),
            }
        }
        RequestedArtist::Catalog(id) => match call.state().catalog().artist(id).await {
            Ok(artist) => Ok(ArtistSeed {
                name: artist.artist.name,
                ids: artist.artist.ids,
            }),
            Err(error) => Err(catalog::failure(&error)),
        },
    }
}

/// The seed a similar-songs request is made with.
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
/// Answers a generic error when a read or a catalog call fails.
async fn similar_seed(call: &Call, value: &str) -> Result<Option<SimilarSeed>, SubsonicError> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) => {
            let library = LibraryRepository::new(call.state().database());
            match id.kind() {
                IdPrefix::Track => {
                    let mut conn =
                        call.state()
                            .database()
                            .readers()
                            .acquire()
                            .await
                            .map_err(|error| {
                                browse::failure(
                                    "getSimilarSongs",
                                    &hearthsound_library::LibraryError::from(error),
                                )
                            })?;
                    match query::track_any(&mut conn, id).await {
                        Ok(Some(view)) => Ok(Some(SimilarSeed::Track(track_seed(&view)))),
                        Ok(None) => Ok(None),
                        Err(error) => Err(browse::failure("getSimilarSongs", &error)),
                    }
                }
                IdPrefix::Album => match library.get_album(id).await {
                    Ok(Some(album)) => match library.list_album_artists(album.id).await {
                        Ok(artists) => Ok(artists
                            .first()
                            .map(|artist| SimilarSeed::Artist(artist_seed(artist)))),
                        Err(error) => Err(browse::failure("getSimilarSongs", &error)),
                    },
                    Ok(None) => Ok(None),
                    Err(error) => Err(browse::failure("getSimilarSongs", &error)),
                },
                IdPrefix::Artist => match library.get_artist(id).await {
                    Ok(Some(artist)) => Ok(Some(SimilarSeed::Artist(artist_seed(&artist)))),
                    Ok(None) => Ok(None),
                    Err(error) => Err(browse::failure("getSimilarSongs", &error)),
                },
                _ => Ok(None),
            }
        }
        Ok(ClientId::Catalog(id)) => match id.kind() {
            EntityKind::Track => match call.state().catalog().track(&id).await {
                Ok(track) => Ok(Some(SimilarSeed::Track(catalog_track_seed(&track.track)))),
                Err(error) => Err(catalog::failure(&error)),
            },
            EntityKind::Album => match call.state().catalog().album(&id).await {
                Ok(album) => Ok(album.album.artists.first().map(|credit| {
                    SimilarSeed::Artist(ArtistSeed {
                        name: credit.name.clone(),
                        ids: credit.ids.clone(),
                    })
                })),
                Err(error) => Err(catalog::failure(&error)),
            },
            EntityKind::Artist => match call.state().catalog().artist(&id).await {
                Ok(artist) => Ok(Some(SimilarSeed::Artist(ArtistSeed {
                    name: artist.artist.name,
                    ids: artist.artist.ids,
                }))),
                Err(error) => Err(catalog::failure(&error)),
            },
            _ => Ok(None),
        },
        Err(error) => {
            tracing::debug!(%error, "a similar songs request named an id that is not a client id");
            Ok(None)
        }
    }
}

/// Builds a seed from a stored track.
///
/// # Parameters
///
/// - `view`: the stored track.
///
/// # Returns
///
/// The title, the credited artists in credit order, and the canonical
/// identifiers the row carries.
fn track_seed(view: &TrackView) -> TrackSeed {
    let mut ids = Vec::new();
    push_id(&mut ids, ISRC, view.track.isrc.as_ref());
    push_id(&mut ids, MB_RECORDING, view.track.mb_recording_id.as_ref());
    if let Some(album) = &view.album {
        push_id(&mut ids, MB_RELEASE, album.mb_release_id.as_ref());
    }
    if let Some(artist) = view.artists.first() {
        push_id(&mut ids, MB_ARTIST, artist.mb_artist_id.as_ref());
    }
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
        ids,
    }
}

/// Builds a seed from a catalog track.
///
/// # Parameters
///
/// - `track`: the catalog track.
///
/// # Returns
///
/// The title, the credit names in order, and the identifiers the entity and the
/// provider state.
fn catalog_track_seed(track: &hearthsound_core::providers::CatalogTrack) -> TrackSeed {
    let mut ids = track.ids.clone();
    push_id(&mut ids, ISRC, track.isrc.as_ref());
    TrackSeed {
        title: track.title.clone(),
        artists: track
            .artists
            .iter()
            .map(|credit| credit.name.clone())
            .collect(),
        ids,
    }
}

/// Pushes one canonical identifier when the row carries a value for it.
///
/// # Parameters
///
/// - `ids`: the identifier list to extend.
/// - `namespace`: the identifier's namespace.
/// - `value`: the stored value, when the row has one.
fn push_id(ids: &mut Vec<ExternalId>, namespace: &str, value: Option<&String>) {
    if let Some(value) = value
        && let Some(id) = filtered(namespace, value)
    {
        ids.push(id);
    }
}

/// Builds a seed from a stored artist.
///
/// # Parameters
///
/// - `artist`: the stored artist.
///
/// # Returns
///
/// The name and the MusicBrainz artist id, when the row carries one.
fn artist_seed(artist: &Artist) -> ArtistSeed {
    ArtistSeed {
        name: artist.name.clone(),
        ids: artist
            .mb_artist_id
            .as_deref()
            .and_then(|value| filtered(MB_ARTIST, value))
            .into_iter()
            .collect(),
    }
}

/// Builds an external identifier unless the value is empty.
///
/// # Parameters
///
/// - `namespace`: the identifier's namespace.
/// - `value`: the identifier's value.
///
/// # Returns
///
/// The identifier, or `None` when the value is empty.
fn filtered(namespace: &str, value: &str) -> Option<ExternalId> {
    (!value.is_empty()).then(|| ExternalId {
        namespace: namespace.to_owned(),
        value: value.to_owned(),
    })
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
/// up to `limit` songs.
fn interleave(lists: Vec<Vec<CatalogTrackResult>>, limit: usize) -> Vec<CatalogTrackResult> {
    let mut songs = Vec::new();
    let mut index = 0;
    loop {
        let mut advanced = false;
        for list in &lists {
            if let Some(song) = list.get(index) {
                songs.push(song.clone());
                advanced = true;
                if songs.len() >= limit {
                    return songs;
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

/// Reads the local tracks a suggestion's title names.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scopes`: the scopes the caller can see.
/// - `title`: the suggested title.
///
/// # Returns
///
/// One hit per matching stored track, reduced to the fields deduplication
/// compares.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn local_hits(
    database: &Database,
    scopes: &[Scope],
    title: &str,
) -> Result<Vec<LocalTrackHit>, SubsonicError> {
    let mut hits = Vec::new();
    for scope in scopes {
        let found = match browse::search(
            database,
            *scope,
            title,
            EntityKind::Track,
            LOCAL_MATCH_LIMIT,
            0,
        )
        .await
        {
            Ok(found) => found,
            Err(error) => return Err(browse::failure("a suggestion", &error)),
        };
        for view in found.tracks {
            hits.push(LocalTrackHit {
                track_id: view.track.id,
                isrc: view.track.isrc.clone(),
                title: view.track.title.clone(),
                artist: view
                    .track
                    .display_artist
                    .clone()
                    .or_else(|| view.artists.first().map(|artist| artist.name.clone())),
                duration_ms: view.track.duration_ms,
            });
        }
    }
    Ok(hits)
}

/// Renders resolved suggestions as songs, stored ones first.
///
/// A suggestion whose recording the caller can already see is served as the
/// stored song, with the caller's annotation; the rest are served as catalog
/// songs. This is the same local-then-catalog merge `search3` applies, so a
/// client never sees one recording twice and the songs it can play immediately
/// carry the ids it already knows. When two results name the same stored track
/// or the same catalog entity, the later one is dropped and the song keeps its
/// first position.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `results`: the resolved catalog entities, in suggestion order.
///
/// # Returns
///
/// One `Child` per distinct song, in the order of its first result.
///
/// # Errors
///
/// Answers a generic error when a read fails and a Subsonic error when the
/// caller's annotations cannot be read.
async fn resolved_song_objects(
    call: &Call,
    results: Vec<CatalogTrackResult>,
) -> Result<Vec<Object>, SubsonicError> {
    if results.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = scope::search_scopes(call).await?;
    let database = call.state().database();
    let catalog_layer = call.state().catalog();
    let suffix = catalog::title_suffix(&call.state().settings_snapshot())?;

    let mut placements = Vec::with_capacity(results.len());
    let mut stored_ids = Vec::new();
    for result in results {
        let hits = local_hits(database, &scopes, &result.track.title).await?;
        let search = CatalogSearch {
            tracks: vec![result.clone()],
            ..CatalogSearch::default()
        };
        let local = LocalMatches {
            tracks: hits,
            ..LocalMatches::default()
        };
        let deduped = match catalog_layer.deduplicate(&local, &search).await {
            Ok(deduped) => deduped,
            Err(error) => {
                tracing::warn!(%error, "could not deduplicate a suggested song");
                return Ok(Vec::new());
            }
        };
        let stored = deduped.stored_entity(&result.id);
        if let Some(id) = stored {
            stored_ids.push(id.uuid());
        }
        placements.push((result, stored));
    }
    // Two suggestions can name the same recording: a service may list a
    // playlist twice under different spellings, and two catalog entities can
    // both be the stored track. The client sees one id per song, so a
    // suggestion whose entity an earlier one already named is dropped and the
    // first position wins.
    let mut seen_stored = HashSet::new();
    let mut seen_catalog = HashSet::new();
    placements.retain(|(result, stored)| match stored {
        Some(id) => seen_stored.insert(*id),
        None => seen_catalog.insert(result.id.clone()),
    });
    let annotations = match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &stored_ids,
    )
    .await
    {
        Ok(annotations) => annotations,
        Err(error) => return Err(browse::failure("a suggestion", &error)),
    };

    let mut songs = Vec::with_capacity(placements.len());
    for (result, stored) in placements {
        let Some(id) = stored else {
            songs.push(projection::catalog_child(
                &result.id,
                &result.track,
                &suffix,
            ));
            continue;
        };
        let mut loaded = None;
        for scope in &scopes {
            match browse::track(database, *scope, id).await {
                Ok(Some(track)) => {
                    loaded = Some(track);
                    break;
                }
                Ok(None) => {}
                Err(error) => return Err(browse::failure("a suggestion", &error)),
            }
        }
        match loaded {
            Some(track) => songs.push(projection::child(
                &track.view,
                track.file.as_ref(),
                &track.album_artists,
                &annotations.get(&id.uuid()).cloned().unwrap_or_default(),
            )),
            // The track left every scope between the deduplication and the
            // read, so the catalog entity is the honest answer.
            None => songs.push(projection::catalog_child(
                &result.id,
                &result.track,
                &suffix,
            )),
        }
    }
    Ok(songs)
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
