//! The `search2` and `search3` endpoints: local full-text results first,
//! catalog results after them.
//!
//! A search answers three lists (artists, albums, and songs) in a fixed order
//! (05-api.md, section 3.5): the local full-text matches, restricted by the
//! request's scope, come first; the catalog layer's matches are appended when
//! `search.catalog.enabled` and the local count is below the requested count
//! per kind. Catalog results are deduplicated against the local hits, so a
//! catalog track whose recording the server already stores is represented once,
//! by its stored entity.
//!
//! Both endpoints answer the same matches and differ only in the shape of the
//! response: `search3` lists ID3 entities, and `search2` lists the artist
//! references, album folders, and songs the older clients read.
//!
//! A catalog failure never fails the search: the local results are still the
//! answer, and the cause is logged. An empty `query` lists the local library
//! instead of searching it, because the OpenSubsonic protocol requires servers
//! to return all their data for an empty query, and it returns no catalog
//! results at all.

use std::collections::{HashMap, HashSet};

use hearthsound_activity::Annotation;
use hearthsound_catalog::{
    CatalogAlbumRefResult, CatalogArtistResult, CatalogSearch, CatalogTrackResult, LocalMatches,
    LocalTrackHit,
};
use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::EntityId;
use hearthsound_core::EntityKind;
use hearthsound_core::providers::CatalogQuery;
use hearthsound_db::Database;
use hearthsound_library::Artist;
use hearthsound_library::query::{AlbumSort, ArtistSort, Scope, TrackSort, TrackView};
use uuid::Uuid;

use crate::auth::Call;
use crate::catalog;
use crate::response::{Encoded, Response, SubsonicError};
use crate::state::SubsonicState;
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

impl Window {
    /// Returns the window that reaches from the first result to the end of
    /// this one, which is what each scope of a merged search has to answer.
    fn through_end(self) -> Window {
        Window {
            count: self.offset.saturating_add(self.count),
            offset: 0,
        }
    }

    /// Keeps the part of a list this window covers.
    fn apply<T>(self, items: Vec<T>) -> Vec<T> {
        let offset = usize::try_from(self.offset).unwrap_or(0);
        let count = usize::try_from(self.count).unwrap_or(0);
        items.into_iter().skip(offset).take(count).collect()
    }
}

/// A stored artist a search matched, with the album count it reports.
#[derive(Debug)]
struct LocalArtist {
    /// The artist row.
    artist: Artist,
    /// How many albums of the artist the scope holds.
    album_count: i64,
}

/// The entities one search matched: the local matches first, then the catalog's.
///
/// Collection decides which entities answer a search and stops there, so the
/// endpoints that answer it project the same matches their own way. A response
/// lists the local matches of a kind before the catalog's, which is why the two
/// are kept apart here.
#[derive(Debug, Default)]
struct Matches {
    /// The stored artists that matched, in match order.
    artists: Vec<LocalArtist>,
    /// The catalog artists that matched, in match order.
    catalog_artists: Vec<CatalogArtistResult>,
    /// The stored albums that matched, in match order, each with the scope it
    /// was found in.
    albums: Vec<(EntityId, Scope)>,
    /// The catalog albums that matched, in match order.
    catalog_albums: Vec<CatalogAlbumRefResult>,
    /// The stored songs that matched, in match order, each with the scope it
    /// was found in.
    tracks: Vec<(TrackView, Scope)>,
    /// The catalog songs that matched, in match order.
    catalog_tracks: Vec<CatalogTrackResult>,
    /// The local track hits, reduced to the fields deduplication compares.
    hits: Vec<LocalTrackHit>,
}

impl Matches {
    /// Returns the ids of the matched stored artists.
    ///
    /// # Returns
    ///
    /// The artist ids, in match order.
    fn local_artist_ids(&self) -> Vec<EntityId> {
        self.artists
            .iter()
            .map(|matched| matched.artist.id)
            .collect()
    }

    /// Returns the ids of the matched stored albums.
    ///
    /// # Returns
    ///
    /// The album ids, in match order.
    fn local_album_ids(&self) -> Vec<EntityId> {
        self.albums.iter().map(|(id, _)| *id).collect()
    }

    /// Returns the ids of the matched stored songs.
    ///
    /// # Returns
    ///
    /// The track ids, in match order.
    fn local_track_ids(&self) -> Vec<EntityId> {
        self.tracks.iter().map(|(view, _)| view.track.id).collect()
    }
}

/// Answers `search3`: the artists, albums, and songs matching a query.
///
/// An empty `query` lists the scope instead of searching it, and that listing
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
///   `musicFolderId` restricts the local side to one virtual folder.
///
/// # Returns
///
/// A successful envelope carrying `searchResult3`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `query` is missing, error 70 for an unknown
/// `musicFolderId`, a generic error when a count or offset is not a number, and
/// a generic error when a local read fails. A catalog failure is never an
/// error: it is logged and the local results are returned.
pub async fn search3(call: Call) -> Encoded {
    let collected = match collect(&call).await {
        Ok(collected) => collected,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let title_suffix = match title_suffix(&call) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let annotations = match annotations_of(&call, &collected).await {
        Ok(annotations) => annotations,
        Err(error) => return call.encode(Response::failed(browse::failure("search3", &error))),
    };
    let matches = &collected.matches;
    let database = call.state().database();
    let query = call.get("query").unwrap_or_default();
    let mut scores = HashMap::new();

    let mut artists = Vec::with_capacity(matches.artists.len() + matches.catalog_artists.len());
    for matched in &matches.artists {
        scores.insert(
            matched.artist.id.to_string(),
            score_of(query, &matched.artist.name, None),
        );
        artists.push(projection::artist_id3(
            &matched.artist,
            matched.album_count,
            &annotations.artist(matched.artist.id),
        ));
    }
    for catalog in &matches.catalog_artists {
        scores.insert(
            catalog.id.to_string(),
            score_of(query, &catalog.artist.name, None),
        );
        artists.push(projection::catalog_artist(catalog, None, &title_suffix));
    }

    let mut albums = Vec::with_capacity(matches.albums.len() + matches.catalog_albums.len());
    for (id, scope) in &matches.albums {
        let object = match album_object(database, *scope, *id, &annotations, query).await {
            Ok(object) => object,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("search3", &error)));
            }
        };
        if let Some((object, score)) = object {
            scores.insert(id.to_string(), score);
            albums.push(object);
        }
    }
    for catalog in &matches.catalog_albums {
        scores.insert(
            catalog.id.to_string(),
            score_of(
                query,
                &catalog.album.title,
                catalog
                    .album
                    .artists
                    .first()
                    .map(|credit| credit.name.as_str()),
            ),
        );
        albums.push(projection::catalog_album_ref(catalog, &title_suffix));
    }

    let mut songs = Vec::with_capacity(matches.tracks.len() + matches.catalog_tracks.len());
    for (view, scope) in &matches.tracks {
        let object = match track_object(database, *scope, view.clone(), &annotations, query).await {
            Ok(object) => object,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("search3", &error)));
            }
        };
        if let Some((object, score)) = object {
            scores.insert(view.track.id.to_string(), score);
            songs.push(object);
        }
    }
    for catalog in &matches.catalog_tracks {
        scores.insert(
            catalog.id.to_string(),
            score_of(
                query,
                &catalog.track.title,
                catalog
                    .track
                    .artists
                    .first()
                    .map(|credit| credit.name.as_str()),
            ),
        );
        songs.push(projection::catalog_child(
            &catalog.id,
            &catalog.track,
            &title_suffix,
        ));
    }

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
/// parameters, the windows, and the catalog assembly are the ones [`search3`]
/// documents.
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
    let title_suffix = match title_suffix(&call) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let annotations = match annotations_of(&call, &collected).await {
        Ok(annotations) => annotations,
        Err(error) => return call.encode(Response::failed(browse::failure("search2", &error))),
    };
    let matches = &collected.matches;
    let database = call.state().database();
    let query = call.get("query").unwrap_or_default();
    let mut scores = HashMap::new();

    let mut artists = Vec::with_capacity(matches.artists.len() + matches.catalog_artists.len());
    for matched in &matches.artists {
        scores.insert(
            matched.artist.id.to_string(),
            score_of(query, &matched.artist.name, None),
        );
        artists.push(projection::starred_artist_ref(
            &matched.artist,
            &annotations.artist(matched.artist.id),
        ));
    }
    for catalog in &matches.catalog_artists {
        scores.insert(
            catalog.id.to_string(),
            score_of(query, &catalog.artist.name, None),
        );
        artists.push(projection::catalog_artist_ref(catalog));
    }

    let mut albums = Vec::with_capacity(matches.albums.len() + matches.catalog_albums.len());
    for (id, scope) in &matches.albums {
        let object = match album_dir_object(database, *scope, *id, &annotations, query).await {
            Ok(object) => object,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("search2", &error)));
            }
        };
        if let Some((object, score)) = object {
            scores.insert(id.to_string(), score);
            albums.push(object);
        }
    }
    for catalog in &matches.catalog_albums {
        scores.insert(
            catalog.id.to_string(),
            score_of(
                query,
                &catalog.album.title,
                catalog
                    .album
                    .artists
                    .first()
                    .map(|credit| credit.name.as_str()),
            ),
        );
        albums.push(projection::catalog_album_dir(
            &catalog.id,
            catalog,
            &title_suffix,
        ));
    }

    let mut songs = Vec::with_capacity(matches.tracks.len() + matches.catalog_tracks.len());
    for (view, scope) in &matches.tracks {
        let object = match track_object(database, *scope, view.clone(), &annotations, query).await {
            Ok(object) => object,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("search2", &error)));
            }
        };
        if let Some((object, score)) = object {
            scores.insert(view.track.id.to_string(), score);
            songs.push(object);
        }
    }
    for catalog in &matches.catalog_tracks {
        scores.insert(
            catalog.id.to_string(),
            score_of(
                query,
                &catalog.track.title,
                catalog
                    .track
                    .artists
                    .first()
                    .map(|credit| credit.name.as_str()),
            ),
        );
        songs.push(projection::catalog_child(
            &catalog.id,
            &catalog.track,
            &title_suffix,
        ));
    }

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

/// Collects the matches of one search request, local first and then catalog.
///
/// Without `musicFolderId` the local matches are the personal library's
/// followed, for an administrator, by the rest of the server's, so content a
/// user has not adopted is still found by searching; with `musicFolderId` only
/// that folder's scope is searched.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose parameters are the ones [`search3`]
///   documents.
///
/// # Returns
///
/// The matches.
///
/// # Errors
///
/// Answers Subsonic error 10 when `query` is missing, error 70 for an unknown
/// `musicFolderId`, a generic error when a count or offset is not a number, and
/// a generic error when a local read fails. A catalog failure is never an
/// error: it is logged and the local matches are the answer.
async fn collect(call: &Call) -> Result<Collected, SubsonicError> {
    let Some(text) = call.get("query") else {
        return Err(SubsonicError::missing_parameter("query"));
    };
    let scopes = scope::search_scopes(call).await?;
    let query = Search {
        text,
        artists: window(call, "artistCount", "artistOffset")?,
        albums: window(call, "albumCount", "albumOffset")?,
        songs: window(call, "songCount", "songOffset")?,
        sort: track_sort(call),
        genre: song_genre(call),
    };
    let settings = call.state().settings_snapshot();
    let mut matches = scoped_matches(call.state().database(), &scopes, &query)
        .await
        .map_err(|error| browse::failure("search", &error))?;

    // The catalog contributes at most the shortfall of each kind, and only for
    // a non-empty query. An empty query lists the local library and nothing
    // else.
    let shortfalls = [
        (EntityKind::Artist, query.artists, matches.artists.len()),
        (EntityKind::Album, query.albums, matches.albums.len()),
        (EntityKind::Track, query.songs, matches.tracks.len()),
    ];
    if !text.is_empty() && catalog_enabled(&settings) {
        let hits = matches.hits.clone();
        append_catalog(call, &hits, text, &shortfalls, &mut matches).await;
    }
    Ok(Collected { matches })
}

/// The matches of one search request.
#[derive(Debug)]
struct Collected {
    /// The matched entities, each stored one with the scope it was read in.
    matches: Matches,
}

/// Collects the local matches of a search over one or more scopes.
///
/// With one scope this is [`local_matches`]. With several, each scope is
/// searched for everything up to the end of the requested windows, the
/// results are concatenated in scope order with an entity found earlier kept
/// at its first place, and the windows are applied to the concatenation, so
/// the personal library's matches come before the server's and paging stays
/// stable.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scopes`: the scopes to search, in the order their matches are listed.
/// - `query`: the text and the windows the request asked for.
///
/// # Returns
///
/// The merged matches.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn scoped_matches(
    database: &Database,
    scopes: &[Scope],
    query: &Search<'_>,
) -> Result<Matches, LocalSideError> {
    if let [scope] = scopes {
        return local_matches(database, *scope, query).await;
    }
    let widened = Search {
        text: query.text,
        artists: query.artists.through_end(),
        albums: query.albums.through_end(),
        songs: query.songs.through_end(),
        sort: query.sort,
        genre: query.genre,
    };
    let mut merged = Matches::default();
    let mut artists = HashSet::new();
    let mut albums = HashSet::new();
    let mut tracks = HashSet::new();
    for scope in scopes {
        let part = local_matches(database, *scope, &widened).await?;
        for artist in part.artists {
            if artists.insert(artist.artist.id) {
                merged.artists.push(artist);
            }
        }
        for album in part.albums {
            if albums.insert(album.0) {
                merged.albums.push(album);
            }
        }
        for track in part.tracks {
            if tracks.insert(track.0.track.id) {
                merged.tracks.push(track);
            }
        }
    }
    merged.artists = query.artists.apply(merged.artists);
    merged.albums = query.albums.apply(merged.albums);
    merged.tracks = query.songs.apply(merged.tracks);
    merged.hits = merged.tracks.iter().map(|(view, _)| hit(view)).collect();
    Ok(merged)
}

/// Loads the user's annotations for the stored entities of a search response.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `collected`: the matches the response answers with.
///
/// # Returns
///
/// The annotations, keyed by entity id.
///
/// # Errors
///
/// Returns [`ActivityError`] when a read fails.
async fn annotations_of(
    call: &Call,
    collected: &Collected,
) -> Result<Annotations, hearthsound_activity::ActivityError> {
    annotations_for(
        call.state(),
        call.principal().user_id.uuid(),
        &collected.matches.local_artist_ids(),
        &collected.matches.local_album_ids(),
        &collected.matches.local_track_ids(),
    )
    .await
}

/// Reads the suffix catalog titles carry in a response.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state supplies the settings.
///
/// # Returns
///
/// The suffix, empty when the setting is empty.
///
/// # Errors
///
/// Answers a generic error when the setting cannot be read.
fn title_suffix(call: &Call) -> Result<String, SubsonicError> {
    catalog::title_suffix(&call.state().settings_snapshot())
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

/// Reads `search.catalog.enabled` from a settings snapshot.
///
/// A setting that cannot be read makes the search answer with its local results
/// rather than fail, because catalog results are an addition to a local search
/// that already succeeded.
///
/// # Parameters
///
/// - `snapshot`: the settings in effect.
///
/// # Returns
///
/// Whether catalog results are wanted.
fn catalog_enabled(snapshot: &SettingsStore) -> bool {
    match snapshot.get(keys::SEARCH_CATALOG_ENABLED) {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::warn!(error = %error, "could not read search.catalog.enabled");
            false
        }
    }
}

/// The user's annotations for one search response.
///
/// One read per entity kind fills it, so a page of results costs a bounded
/// number of statements however many entities it holds.
#[derive(Debug, Default)]
struct Annotations {
    /// The artist annotations, keyed by artist id.
    artists: HashMap<Uuid, Annotation>,
    /// The album annotations, keyed by album id.
    albums: HashMap<Uuid, Annotation>,
    /// The track annotations, keyed by track id.
    tracks: HashMap<Uuid, Annotation>,
}

impl Annotations {
    /// Returns the annotation of an artist.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist's client id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn artist(&self, id: EntityId) -> Annotation {
        self.artists.get(&id.uuid()).copied().unwrap_or_default()
    }

    /// Returns the annotation of an album.
    ///
    /// # Parameters
    ///
    /// - `id`: the album's client id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn album(&self, id: EntityId) -> Annotation {
        self.albums.get(&id.uuid()).copied().unwrap_or_default()
    }

    /// Returns the annotation of a track.
    ///
    /// # Parameters
    ///
    /// - `id`: the track's client id.
    ///
    /// # Returns
    ///
    /// The user's annotation, or the default when the user has none.
    fn track(&self, id: EntityId) -> Annotation {
        self.tracks.get(&id.uuid()).copied().unwrap_or_default()
    }
}

/// Why a local search side could not be built.
#[derive(Debug, thiserror::Error)]
enum LocalSideError {
    /// A library read failed.
    #[error(transparent)]
    Library(#[from] hearthsound_library::LibraryError),
    /// An annotation read failed.
    #[error(transparent)]
    Activity(#[from] hearthsound_activity::ActivityError),
}

/// Loads the user's annotations for the entities of one search response.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database the reads use.
/// - `user_id`: the account whose activity is read.
/// - `artist_ids`: the artists of the response.
/// - `album_ids`: the albums of the response.
/// - `track_ids`: the tracks of the response.
///
/// # Errors
///
/// Returns [`ActivityError`] when a read fails.
async fn annotations_for(
    state: &SubsonicState,
    user_id: Uuid,
    artist_ids: &[EntityId],
    album_ids: &[EntityId],
    track_ids: &[EntityId],
) -> Result<Annotations, hearthsound_activity::ActivityError> {
    Ok(Annotations {
        artists: browse::annotations(state, user_id, EntityKind::Artist, &uuids(artist_ids))
            .await?,
        albums: browse::annotations(state, user_id, EntityKind::Album, &uuids(album_ids)).await?,
        tracks: browse::annotations(state, user_id, EntityKind::Track, &uuids(track_ids)).await?,
    })
}

/// Returns the plain ids of some client ids.
///
/// # Parameters
///
/// - `ids`: the client ids.
///
/// # Returns
///
/// The ids the annotation read keys on.
fn uuids(ids: &[EntityId]) -> Vec<Uuid> {
    ids.iter().map(EntityId::uuid).collect()
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

/// Collects the local matches of a search.
///
/// An empty query lists the scope instead of searching it, because the FTS5
/// index has nothing to match. A non-empty query runs the full-text search per
/// kind, so each kind can carry its own window. Either way the entities that
/// answer the search are known before they are projected, so the user's
/// annotations are read once per entity kind.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to search.
/// - `query`: the text and the windows the request asked for.
///
/// # Returns
///
/// The matched artists, albums, and songs, and the track hits deduplication
/// compares.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn local_matches(
    database: &Database,
    scope: Scope,
    query: &Search<'_>,
) -> Result<Matches, LocalSideError> {
    let text = query.text;
    let (artist_window, album_window, song_window) = (&query.artists, &query.albums, &query.songs);
    let mut matches = Matches::default();
    if text.is_empty() {
        let summaries = browse::artists_page(
            database,
            scope,
            ArtistSort::Name,
            artist_window.count,
            artist_window.offset,
        )
        .await?;
        let albums = browse::albums(
            database,
            scope,
            AlbumSort::AlphabeticalByName,
            album_window.count,
            album_window.offset,
        )
        .await?;
        let views = browse::list_tracks(
            database,
            scope,
            query.sort,
            query.genre,
            song_window.count,
            song_window.offset,
        )
        .await?;
        matches.artists = summaries
            .into_iter()
            .map(|summary| LocalArtist {
                artist: summary.artist,
                album_count: summary.album_count,
            })
            .collect();
        matches.albums = albums.into_iter().map(|album| (album.id, scope)).collect();
        for view in views {
            matches.hits.push(hit(&view));
            matches.tracks.push((view, scope));
        }
        return Ok(matches);
    }

    let artists = browse::search(
        database,
        scope,
        text,
        EntityKind::Artist,
        artist_window.count,
        artist_window.offset,
    )
    .await?;
    let albums = browse::search(
        database,
        scope,
        text,
        EntityKind::Album,
        album_window.count,
        album_window.offset,
    )
    .await?;
    let tracks = browse::search(
        database,
        scope,
        text,
        EntityKind::Track,
        song_window.count,
        song_window.offset,
    )
    .await?;
    for artist in artists.artists {
        if let Some(detail) = browse::artist(database, scope, artist.id).await? {
            matches.artists.push(LocalArtist {
                album_count: detail.albums.len() as i64,
                artist: detail.artist,
            });
        }
    }
    matches.albums = albums
        .albums
        .into_iter()
        .map(|album| (album.id, scope))
        .collect();
    for view in tracks.tracks {
        matches.hits.push(hit(&view));
        matches.tracks.push((view, scope));
    }

    Ok(matches)
}

/// Loads one stored album and projects it as an `AlbumID3`.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to load.
/// - `annotations`: the user's annotations for the response.
/// - `query`: the search text the relevance score is measured against.
///
/// # Returns
///
/// The album with its relevance score, or `None` when the album left the scope
/// between the search and the read.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn album_object(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
    annotations: &Annotations,
    query: &str,
) -> Result<Option<(Object, f64)>, LocalSideError> {
    let Some(loaded) = browse::album(database, scope, album_id).await? else {
        return Ok(None);
    };
    let score = score_of(
        query,
        &loaded.album.title,
        loaded.artists.first().map(|artist| artist.name.as_str()),
    );
    Ok(Some((
        projection::album_id3(
            &loaded.album,
            &loaded.artists,
            &loaded.tracks,
            &annotations.album(loaded.album.id),
        ),
        score,
    )))
}

/// Loads one stored album and projects it as a directory `Child`.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to load.
/// - `annotations`: the user's annotations for the response.
/// - `query`: the search text the relevance score is measured against.
///
/// # Returns
///
/// The directory child with its relevance score, or `None` when the album left
/// the scope between the search and the read.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn album_dir_object(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
    annotations: &Annotations,
    query: &str,
) -> Result<Option<(Object, f64)>, LocalSideError> {
    let Some(loaded) = browse::album(database, scope, album_id).await? else {
        return Ok(None);
    };
    let score = score_of(
        query,
        &loaded.album.title,
        loaded.artists.first().map(|artist| artist.name.as_str()),
    );
    Ok(Some((
        projection::album_dir(
            &loaded.album,
            &loaded.artists,
            &loaded.tracks,
            &annotations.album(loaded.album.id),
        ),
        score,
    )))
}

/// Loads one stored track and projects it as a `Child`.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `view`: the track the local search or listing returned.
/// - `annotations`: the user's annotations for the response.
/// - `query`: the search text the relevance score is measured against.
///
/// # Returns
///
/// The song with its relevance score, or `None` when the track left the scope
/// between the search and the read.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn track_object(
    database: &Database,
    scope: Scope,
    view: TrackView,
    annotations: &Annotations,
    query: &str,
) -> Result<Option<(Object, f64)>, LocalSideError> {
    let Some(loaded) = browse::track(database, scope, view.track.id).await? else {
        return Ok(None);
    };
    let score = score_of(
        query,
        &loaded.view.track.title,
        loaded
            .view
            .artists
            .first()
            .map(|artist| artist.name.as_str()),
    );
    Ok(Some((
        projection::child(
            &loaded.view,
            loaded.file.as_ref(),
            &loaded.album_artists,
            &annotations.track(loaded.view.track.id),
        ),
        score,
    )))
}

/// Scores one result against the query.
///
/// # Parameters
///
/// - `query`: the text the listener typed.
/// - `name`: the result's title or name.
/// - `artist`: its primary artist, when it has one.
///
/// # Returns
///
/// The relevance, from 0 to 1.
fn score_of(query: &str, name: &str, artist: Option<&str>) -> f64 {
    hearthsound_catalog::relevance::relevance(query, name, artist)
}

/// Reduces a local track to the fields deduplication compares.
///
/// # Parameters
///
/// - `view`: the track view.
///
/// # Returns
///
/// The hit.
fn hit(view: &TrackView) -> LocalTrackHit {
    LocalTrackHit {
        track_id: view.track.id,
        isrc: view.track.isrc.clone(),
        title: view.track.title.clone(),
        artist: view
            .track
            .display_artist
            .clone()
            .or_else(|| view.artists.first().map(|artist| artist.name.clone())),
        duration_ms: view.track.duration_ms,
    }
}

/// Searches the catalog for the shortfall of each kind and appends what is new.
///
/// A per-kind failure is logged and skipped, because the local matches already
/// answered and a catalog instance that is down must not fail the search.
///
/// What the response already shows is what deduplication compares against: a
/// catalog entity whose stored row the local results and the twin fold already
/// list is left out, while one whose row is stored but not shown (a played song
/// nobody saved) is appended, so the play does not remove the hit it started
/// from.
///
/// One policy covers every failure here: a catalog fault never removes catalog
/// results. A dedupe that fails appends the provider's answer unfiltered, and a
/// twin comparison that fails keeps the empty set, so the same fault cannot empty
/// the list and duplicate it at once.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `hits`: the local track hits deduplication compares against.
/// - `text`: the search text.
/// - `shortfalls`: each kind with its window and how many local results it
///   already has.
/// - `matches`: the local matches to append the catalog results to.
async fn append_catalog(
    call: &Call,
    hits: &[LocalTrackHit],
    text: &str,
    shortfalls: &[(EntityKind, Window, usize)],
    matches: &mut Matches,
) {
    let catalog = call.state().catalog();
    let mut found = CatalogSearch::default();
    for (kind, window, local) in shortfalls {
        let shortfall = window.count - i64::try_from(*local).unwrap_or(i64::MAX);
        if shortfall <= 0 {
            continue;
        }
        let query = CatalogQuery {
            text: text.to_owned(),
            kinds: vec![*kind],
            limit: u32::try_from(shortfall).unwrap_or(u32::MAX),
            offset: u32::try_from(window.offset).unwrap_or(0),
        };
        match catalog.search(&query).await {
            Ok(results) => {
                if found.source.is_none() {
                    found.source = results.source.clone();
                }
                found.tracks.extend(results.tracks);
                found.albums.extend(results.albums);
                found.artists.extend(results.artists);
            }
            Err(error) => {
                tracing::debug!(error = %error, kind = ?kind, "a catalog search failed");
            }
        }
    }

    if found.is_empty() {
        return;
    }
    let local_matches = LocalMatches {
        tracks: hits.to_vec(),
        albums: matches.local_album_ids(),
        artists: matches.local_artist_ids(),
    };
    let deduped = match catalog.deduplicate(&local_matches, &found).await {
        Ok(deduped) => Some(deduped),
        Err(error) => {
            // A catalog failure must never remove catalog results: the lists are
            // appended unfiltered, so a database fault costs a duplicate at worst.
            tracing::warn!(error = %error, "could not deduplicate catalog results");
            None
        }
    };

    let (_, artist_window, _) = shortfalls[0];
    let (_, album_window, _) = shortfalls[1];
    let (_, song_window, _) = shortfalls[2];

    // A stored artist or album whose name the catalog also answers with is one
    // entry to the user, not two: the stored row is already in the list.
    let stored_artists: Vec<(EntityId, String)> = matches
        .artists
        .iter()
        .map(|matched| (matched.artist.id, matched.artist.name.clone()))
        .collect();
    let stored_albums = matches.local_album_ids();
    let twins = match catalog
        .name_twins(&stored_artists, &stored_albums, &found)
        .await
    {
        Ok(twins) => twins,
        Err(error) => {
            tracing::warn!(error = %error, "could not compare catalog results with stored names");
            HashSet::new()
        }
    };

    // Deduplication already left out every catalog entity the local results
    // show, so the lists are appended as they are; the twin fold is display
    // only and never removes a hit the caller can open. A dedupe that failed
    // appends the provider's answer instead of nothing.
    let (artists, albums, tracks) = match deduped {
        Some(deduped) => (deduped.artists, deduped.albums, deduped.tracks),
        None => (found.artists, found.albums, found.tracks),
    };
    matches.catalog_artists.extend(
        artists
            .iter()
            .filter(|artist| !twins.contains(&artist.id))
            .take(window_limit(artist_window))
            .cloned(),
    );
    matches.catalog_albums.extend(
        albums
            .iter()
            .filter(|album| !twins.contains(&album.id))
            .take(window_limit(album_window))
            .cloned(),
    );
    matches
        .catalog_tracks
        .extend(tracks.iter().take(window_limit(song_window)).cloned());
}

/// Returns how many catalog results of one kind are worth appending.
///
/// # Parameters
///
/// - `window`: the request's window for the kind.
///
/// # Returns
///
/// The requested count as a `usize`, saturating at [`usize::MAX`].
fn window_limit(window: Window) -> usize {
    usize::try_from(window.count).unwrap_or(usize::MAX)
}
