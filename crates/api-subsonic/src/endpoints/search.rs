//! The `search2` and `search3` endpoints: the library's matches, then the
//! catalog layer's.
//!
//! A search answers three lists (artists, albums, and songs) in a fixed order
//! (05-api.md, section 3.5). The catalog layer's
//! [`CatalogService::search`](hearthsound_catalog::CatalogService::search) is the
//! one search the server runs: it answers the stored rows first and then the
//! first catalog instance that can answer, resolves every provider hit to a row,
//! and caches the answer as ids. This module only reads the parameters, applies
//! the per-kind windows, and projects the rows.
//!
//! Both endpoints answer the same matches and differ only in the shape of the
//! response: `search3` lists ID3 entities, and `search2` lists the artist
//! references, album folders, and songs the older clients read.
//!
//! A search failure is answered with the failure envelope, because the catalog
//! layer already answers from what it has stored when an instance cannot be
//! reached. An empty `query` lists the library instead of searching it, because
//! the OpenSubsonic protocol requires servers to return all their data for an
//! empty query.

use std::collections::HashMap;

use hearthsound_core::{EntityId, EntityKind};
use hearthsound_library::query::{AlbumSort, ArtistSort, TrackSort};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::{browse, projection, scope};

use super::browse::{number, song_genre, track_sort};

/// The number of results `search3` returns per kind without a count parameter.
const DEFAULT_COUNT: i64 = 20;

/// How many results of one kind a request asked for, and where it starts.
#[derive(Debug, Clone, Copy)]
struct Window {
    /// The maximum number of results of this kind.
    count: i64,
    /// How many results of this kind to skip.
    offset: i64,
}

/// What one search request asked for.
#[derive(Debug)]
struct Search<'a> {
    /// The search text, possibly empty.
    text: &'a str,
    /// The artist window.
    artists: Window,
    /// The album window.
    albums: Window,
    /// The song window.
    songs: Window,
    /// The order an empty-query song listing is read in. A full-text search
    /// answers in relevance order, so the order applies to the listing only.
    sort: TrackSort,
    /// The genre an empty-query song listing is narrowed to, when the request
    /// named one through the vendor extension.
    genre: Option<&'a str>,
}

/// The entities one search matched, by kind.
#[derive(Debug, Default)]
struct Matches {
    /// The matched artists, each with how many albums it has.
    artists: Vec<(EntityId, i64)>,
    /// The matched albums.
    albums: Vec<EntityId>,
    /// The matched tracks.
    tracks: Vec<EntityId>,
}

/// The matches of one search request, and the text they were matched against.
#[derive(Debug)]
struct Collected {
    /// The text the request searched for, possibly empty.
    text: String,
    /// The matched entities.
    matches: Matches,
}

/// Answers `search3`: the artists, albums, and songs matching a query.
///
/// An empty `query` lists the library instead of searching it, and that listing
/// is the one a client with a sort menu shows: a request carrying
/// `hearthsound=1` may pass `sort` (`title`, the default, `added`, `played`,
/// `artist`, `album` or `year`) and `genre` to order and narrow the songs it
/// lists. A full-text query answers in relevance order, so neither parameter
/// applies to it, and a request without `hearthsound=1` is answered exactly as
/// a server without the vendor extension would answer it.
///
/// # Parameters
///
/// - `call`: the authenticated call. `query` is the search text;
///   `artistCount`/`artistOffset`, `albumCount`/`albumOffset`, and
///   `songCount`/`songOffset` page each kind and default to 20 and 0;
///   `musicFolderId` restricts the listing to one virtual folder.
///
/// # Returns
///
/// A successful envelope carrying `searchResult3`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `query` is missing, error 70 for an unknown
/// `musicFolderId`, a generic error when a count or offset is not a number, and
/// a generic error when a read fails.
pub async fn search3(call: Call) -> Encoded {
    let collected = match collect(&call).await {
        Ok(collected) => collected,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut scores = HashMap::new();
    let artists = match artist_objects(&call, &collected, false, &mut scores).await {
        Ok(artists) => artists,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let albums = match album_objects(&call, &collected, true, &mut scores).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let songs = match song_objects(&call, &collected, &mut scores).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let search_result = Object::new()
        .set("artist", Array::new(artists))
        .set("album", Array::new(albums))
        .set("song", Array::new(songs));
    call.respond_scored(
        Response::ok().member("searchResult3", search_result),
        &scores,
    )
    .await
}

/// Answers `search2`: the same matches as `search3`, in the older shape.
///
/// The protocol's first search response lists an artist as a bare reference, an
/// album as the folder a client browses into, and a song as a `Child`. The
/// parameters and the windows are the ones [`search3`] documents.
///
/// # Parameters
///
/// - `call`: the authenticated call, with the parameters [`search3`] documents.
///
/// # Returns
///
/// A successful envelope carrying `searchResult2`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`search3`] documents.
pub async fn search2(call: Call) -> Encoded {
    let collected = match collect(&call).await {
        Ok(collected) => collected,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut scores = HashMap::new();
    let artists = match artist_objects(&call, &collected, true, &mut scores).await {
        Ok(artists) => artists,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let albums = match album_objects(&call, &collected, false, &mut scores).await {
        Ok(albums) => albums,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let songs = match song_objects(&call, &collected, &mut scores).await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let search_result = Object::new()
        .set("artist", Array::new(artists))
        .set("album", Array::new(albums))
        .set("song", Array::new(songs));
    call.respond_scored(
        Response::ok().member("searchResult2", search_result),
        &scores,
    )
    .await
}

/// Collects the matches of one search request.
///
/// An empty query lists the library, which is the one answer a client reads as
/// "everything here": the full-text index has nothing to match against. A
/// non-empty query is answered by the catalog layer, which searches the stored
/// rows first and appends the catalog instance's hits.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose parameters are the ones [`search3`]
///   documents.
///
/// # Errors
///
/// Answers Subsonic error 10 when `query` is missing, error 70 for an unknown
/// `musicFolderId`, a generic error when a count or offset is not a number, and
/// a generic error when a read fails.
async fn collect(call: &Call) -> Result<Collected, SubsonicError> {
    let Some(text) = call.get("query") else {
        return Err(SubsonicError::missing_parameter("query"));
    };
    let query = Search {
        text,
        artists: window(call, "artistCount", "artistOffset")?,
        albums: window(call, "albumCount", "albumOffset")?,
        songs: window(call, "songCount", "songOffset")?,
        sort: track_sort(call),
        genre: song_genre(call),
    };
    let matches = if text.is_empty() {
        listed(call, &query).await?
    } else {
        searched(call, &query).await?
    };
    Ok(Collected {
        text: text.to_owned(),
        matches,
    })
}

/// Lists the library for an empty search query.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `query`: the windows and the order the request asked for.
///
/// # Returns
///
/// The listed entities.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn listed(call: &Call, query: &Search<'_>) -> Result<Matches, SubsonicError> {
    let scopes = scope::search_scopes(call).await?;
    let mut matches = Matches::default();
    let mut seen_artists = std::collections::HashSet::new();
    let mut seen_albums = std::collections::HashSet::new();
    let mut seen_tracks = std::collections::HashSet::new();
    for scope in scopes {
        // The windows are widened through the end of each kind so the merged
        // listing can be cut once, after the scopes are concatenated in order.
        let limit = query
            .artists
            .offset
            .saturating_add(query.artists.count)
            .max(query.albums.offset.saturating_add(query.albums.count))
            .max(query.songs.offset.saturating_add(query.songs.count));
        let summaries =
            browse::artists_page(call.state().database(), scope, ArtistSort::Name, limit, 0)
                .await
                .map_err(|error| browse::failure("a search listing", &error))?;
        for summary in summaries {
            if seen_artists.insert(summary.artist.id) {
                matches
                    .artists
                    .push((summary.artist.id, summary.album_count));
            }
        }
        let albums = browse::albums(
            call.state().database(),
            scope,
            AlbumSort::AlphabeticalByName,
            limit,
            0,
        )
        .await
        .map_err(|error| browse::failure("a search listing", &error))?;
        for album in albums {
            if seen_albums.insert(album.id) {
                matches.albums.push(album.id);
            }
        }
        let tracks = browse::list_tracks(
            call.state().database(),
            scope,
            query.sort,
            query.genre,
            limit,
            0,
        )
        .await
        .map_err(|error| browse::failure("a search listing", &error))?;
        for view in tracks {
            if seen_tracks.insert(view.track.id) {
                matches.tracks.push(view.track.id);
            }
        }
    }
    let artists = usize::try_from(query.artists.offset).unwrap_or(usize::MAX);
    let albums = usize::try_from(query.albums.offset).unwrap_or(usize::MAX);
    let songs = usize::try_from(query.songs.offset).unwrap_or(usize::MAX);
    matches.artists = matches
        .artists
        .into_iter()
        .skip(artists)
        .take(usize::try_from(query.artists.count).unwrap_or(usize::MAX))
        .collect();
    matches.albums = matches
        .albums
        .into_iter()
        .skip(albums)
        .take(usize::try_from(query.albums.count).unwrap_or(usize::MAX))
        .collect();
    matches.tracks = matches
        .tracks
        .into_iter()
        .skip(songs)
        .take(usize::try_from(query.songs.count).unwrap_or(usize::MAX))
        .collect();
    Ok(matches)
}

/// Searches the library and the catalog layer for a non-empty query.
///
/// Each kind is searched with its own window, so a client that asks for more
/// songs than albums gets more songs.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `query`: the text and the windows the request asked for.
///
/// # Returns
///
/// The matched entities.
///
/// # Errors
///
/// Answers a generic error when a search fails.
async fn searched(call: &Call, query: &Search<'_>) -> Result<Matches, SubsonicError> {
    let user = call.principal().user_id.uuid();
    let mut matches = Matches::default();
    let windows = [
        (EntityKind::Artist, query.artists),
        (EntityKind::Album, query.albums),
        (EntityKind::Track, query.songs),
    ];
    for (kind, window) in windows {
        if window.count <= 0 {
            continue;
        }
        let answer = call
            .state()
            .catalog()
            .search(
                user,
                query.text,
                &[kind],
                u32::try_from(window.count).unwrap_or(u32::MAX),
                u32::try_from(window.offset).unwrap_or(0),
            )
            .await
            .map_err(|error| crate::catalog::failure(&error))?;
        match kind {
            EntityKind::Artist => {
                for artist in answer.artists {
                    let album_count =
                        match browse::artist_any(call.state().database(), artist.artist.id).await {
                            Ok(Some(detail)) => detail.albums.len() as i64,
                            Ok(None) => 0,
                            Err(error) => {
                                return Err(browse::failure("a search", &error));
                            }
                        };
                    matches.artists.push((artist.artist.id, album_count));
                }
            }
            EntityKind::Album => {
                matches
                    .albums
                    .extend(answer.albums.into_iter().map(|album| album.album.id));
            }
            _ => {
                matches
                    .tracks
                    .extend(answer.tracks.into_iter().map(|track| track.track.id));
            }
        }
    }
    Ok(matches)
}

/// Projects the matched artists.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `collected`: the matches of the request.
/// - `reference`: whether the older shape is wanted, where an artist is a bare
///   reference with the caller's star.
/// - `scores`: the relevance of each result, filled in for the vendor member.
///
/// # Returns
///
/// One member per artist, in match order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn artist_objects(
    call: &Call,
    collected: &Collected,
    reference: bool,
    scores: &mut HashMap<String, f64>,
) -> Result<Vec<Object>, SubsonicError> {
    if collected.matches.artists.is_empty() {
        return Ok(Vec::new());
    }
    let database = call.state().database();
    let ids: Vec<Uuid> = collected
        .matches
        .artists
        .iter()
        .map(|(id, _)| id.uuid())
        .collect();
    let annotations = browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Artist,
        &ids,
    )
    .await
    .map_err(|error| browse::failure("a search", &error))?;
    let identifiers = browse::identifiers(database, EntityKind::Artist, &ids)
        .await
        .map_err(|error| browse::failure("a search", &error))?;
    let mut objects = Vec::with_capacity(ids.len());
    for (id, album_count) in &collected.matches.artists {
        let artist = match browse::artist_any(database, *id).await {
            Ok(Some(detail)) => detail.artist,
            Ok(None) => continue,
            Err(error) => return Err(browse::failure("a search", &error)),
        };
        scores.insert(
            id.to_string(),
            hearthsound_catalog::relevance::relevance(&collected.text, &artist.name, None),
        );
        let annotation = annotations.get(&id.uuid()).copied().unwrap_or_default();
        if reference {
            objects.push(projection::starred_artist_ref(&artist, &annotation));
        } else {
            objects.push(projection::artist_id3(
                &artist,
                *album_count,
                &annotation,
                identifiers.get(&id.uuid()).map_or(&[], Vec::as_slice),
            ));
        }
    }
    Ok(objects)
}

/// Projects the matched albums.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `collected`: the matches of the request.
/// - `id3`: whether the `AlbumID3` shape is wanted, rather than a directory
///   child.
/// - `scores`: the relevance of each result, filled in for the vendor member.
///
/// # Returns
///
/// One member per album, in match order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn album_objects(
    call: &Call,
    collected: &Collected,
    id3: bool,
    scores: &mut HashMap<String, f64>,
) -> Result<Vec<Object>, SubsonicError> {
    let page = browse::AlbumPage::of(call, &collected.matches.albums).await?;
    let mut objects = Vec::with_capacity(page.details.len());
    for index in 0..page.details.len() {
        let detail = &page.details[index];
        scores.insert(
            detail.album.id.to_string(),
            hearthsound_catalog::relevance::relevance(
                &collected.text,
                &detail.album.title,
                detail.artists.first().map(|artist| artist.name.as_str()),
            ),
        );
        objects.push(if id3 {
            page.id3(index)
        } else {
            page.dir(index)
        });
    }
    Ok(objects)
}

/// Projects the matched songs and records their relevance.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `collected`: the matches of the request.
/// - `scores`: the relevance of each result, filled in for the vendor member.
///
/// # Returns
///
/// One `Child` per song, in match order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn song_objects(
    call: &Call,
    collected: &Collected,
    scores: &mut HashMap<String, f64>,
) -> Result<Vec<Object>, SubsonicError> {
    let page = browse::SongPage::of(call, &collected.matches.tracks).await?;
    let mut songs = Vec::with_capacity(page.rows.len());
    for index in 0..page.rows.len() {
        let track = &page.rows[index].view.track;
        scores.insert(
            track.id.to_string(),
            hearthsound_catalog::relevance::relevance(
                &collected.text,
                &track.title,
                page.rows[index]
                    .view
                    .artists
                    .first()
                    .map(|artist| artist.name.as_str()),
            ),
        );
        songs.push(page.object(index));
    }
    Ok(songs)
}

/// Reads a count and an offset parameter into a [`Window`].
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `count`: the count parameter's name.
/// - `offset`: the offset parameter's name.
///
/// # Returns
///
/// The window, with a negative count or offset clamped to zero.
///
/// # Errors
///
/// Returns a generic Subsonic error when either value is not a number.
fn window(call: &Call, count: &'static str, offset: &'static str) -> Result<Window, SubsonicError> {
    Ok(Window {
        count: number(call, count, DEFAULT_COUNT)?.max(0),
        offset: number(call, offset, 0)?.max(0),
    })
}
