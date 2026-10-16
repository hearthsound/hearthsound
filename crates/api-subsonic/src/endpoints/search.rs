//! The `search3` endpoint: local full-text results first, catalog results
//! after them.
//!
//! A search answers three lists (artists, albums, and songs) in a fixed order
//! (05-api.md, section 3.5): the local full-text matches, restricted by the
//! request's scope, come first; the catalog layer's matches are appended when
//! `search.catalog.enabled` and the local count is below the requested count
//! per kind. Catalog results are deduplicated against the local hits, so a
//! catalog track whose recording the server already stores is represented once,
//! by its stored entity.
//!
//! A catalog failure never fails the search: the local results are still the
//! answer, and the cause is logged. An empty `query` lists the local library
//! instead of searching it, because the OpenSubsonic protocol requires servers
//! to return all their data for an empty query, and it returns no catalog
//! results at all.

use std::collections::HashMap;

use hearthsound_activity::Annotation;
use hearthsound_catalog::{CatalogSearch, LocalTrackHit};
use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::EntityId;
use hearthsound_core::EntityKind;
use hearthsound_core::providers::CatalogQuery;
use hearthsound_db::Database;
use hearthsound_library::query::{AlbumSort, Scope, TrackView};
use uuid::Uuid;

use crate::auth::Call;
use crate::catalog;
use crate::response::{Encoded, Response, SubsonicError};
use crate::state::SubsonicState;
use crate::value::{Array, Object};
use crate::{browse, projection, scope};

use super::browse::number;

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

/// The local side of a search, projected and ready to answer with.
#[derive(Debug, Default)]
struct LocalSide {
    /// The projected local artists.
    artists: Vec<Object>,
    /// The projected local albums.
    albums: Vec<Object>,
    /// The projected local songs.
    tracks: Vec<Object>,
    /// The local track hits, reduced to the fields deduplication compares.
    hits: Vec<LocalTrackHit>,
}

/// Answers `search3`: the artists, albums, and songs matching a query.
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
    let database = call.state().database();
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(query) = call.get("query") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("query")));
    };
    let text = query.to_owned();
    let artist_window = match window(&call, "artistCount", "artistOffset") {
        Ok(window) => window,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let album_window = match window(&call, "albumCount", "albumOffset") {
        Ok(window) => window,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let song_window = match window(&call, "songCount", "songOffset") {
        Ok(window) => window,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let query = Search {
        text: &text,
        artists: artist_window,
        albums: album_window,
        songs: song_window,
    };
    let mut side = match local_side(
        call.state(),
        call.principal().user_id.uuid(),
        database,
        scope,
        &query,
    )
    .await
    {
        Ok(side) => side,
        Err(error) => return call.encode(Response::failed(browse::failure("search3", &error))),
    };

    let settings = call.state().settings_snapshot();
    let title_suffix = match catalog::title_suffix(&settings) {
        Ok(suffix) => suffix,
        Err(error) => return call.encode(Response::failed(error)),
    };

    // The catalog contributes at most the shortfall of each kind, and only for
    // a non-empty query. An empty query lists the local library and nothing
    // else.
    let shortfalls = [
        (EntityKind::Artist, artist_window, side.artists.len()),
        (EntityKind::Album, album_window, side.albums.len()),
        (EntityKind::Track, song_window, side.tracks.len()),
    ];
    if !text.is_empty() && catalog_enabled(&settings) {
        let hits = side.hits.clone();
        append_catalog(&call, &hits, &text, &shortfalls, &title_suffix, &mut side).await;
    }

    let search_result = Object::new()
        .set("artist", Array::new(side.artists))
        .set("album", Array::new(side.albums))
        .set("song", Array::new(side.tracks));
    call.encode(Response::ok().member("searchResult3", search_result))
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
}

/// Builds the projected local side of a search.
///
/// An empty query lists the scope instead of searching it, because the FTS5
/// index has nothing to match. A non-empty query runs the full-text search per
/// kind, so each kind can carry its own window. Either way the entities the
/// response holds are known before they are projected, so the user's
/// annotations are read once per entity kind.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database the reads use.
/// - `user_id`: the account whose annotations are read.
/// - `database`: the open database.
/// - `scope`: the scope to search.
/// - `text`: the search text, possibly empty.
/// - `artist_window`: the artist window.
/// - `album_window`: the album window.
/// - `song_window`: the song window.
///
/// # Returns
///
/// The projected artists, albums, and songs, and the track hits deduplication
/// compares.
///
/// # Errors
///
/// Returns a library or activity failure when a read fails.
async fn local_side(
    state: &SubsonicState,
    user_id: Uuid,
    database: &Database,
    scope: Scope,
    query: &Search<'_>,
) -> Result<LocalSide, LocalSideError> {
    let text = query.text;
    let (artist_window, album_window, song_window) = (&query.artists, &query.albums, &query.songs);
    let mut side = LocalSide::default();
    if text.is_empty() {
        let summaries =
            browse::artists_page(database, scope, artist_window.count, artist_window.offset)
                .await?;
        let albums = browse::albums(
            database,
            scope,
            AlbumSort::AlphabeticalByName,
            album_window.count,
            album_window.offset,
        )
        .await?;
        let views =
            browse::list_tracks(database, scope, song_window.count, song_window.offset).await?;
        let artist_ids: Vec<EntityId> = summaries.iter().map(|summary| summary.artist.id).collect();
        let album_ids: Vec<EntityId> = albums.iter().map(|album| album.id).collect();
        let track_ids: Vec<EntityId> = views.iter().map(|view| view.track.id).collect();
        let annotations =
            annotations_for(state, user_id, &artist_ids, &album_ids, &track_ids).await?;

        for summary in &summaries {
            side.artists.push(projection::artist_id3(
                &summary.artist,
                summary.album_count,
                &annotations.artist(summary.artist.id),
            ));
        }
        for album in albums {
            push_album(database, scope, album.id, &annotations, &mut side).await?;
        }
        for view in views {
            push_track(database, scope, view, &annotations, &mut side).await?;
        }
        return Ok(side);
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
    let artist_ids: Vec<EntityId> = artists.artists.iter().map(|artist| artist.id).collect();
    let album_ids: Vec<EntityId> = albums.albums.iter().map(|album| album.id).collect();
    let track_ids: Vec<EntityId> = tracks.tracks.iter().map(|view| view.track.id).collect();
    let annotations = annotations_for(state, user_id, &artist_ids, &album_ids, &track_ids).await?;

    for artist in artists.artists {
        if let Some(detail) = browse::artist(database, scope, artist.id).await? {
            side.artists.push(projection::artist_id3(
                &detail.artist,
                detail.albums.len() as i64,
                &annotations.artist(detail.artist.id),
            ));
        }
    }
    for album in albums.albums {
        push_album(database, scope, album.id, &annotations, &mut side).await?;
    }
    for view in tracks.tracks {
        push_track(database, scope, view, &annotations, &mut side).await?;
    }

    Ok(side)
}

/// Loads one album and appends its projection to the local side.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to load.
/// - `annotations`: the user's annotations for the response.
/// - `side`: the side to append to.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn push_album(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
    annotations: &Annotations,
    side: &mut LocalSide,
) -> Result<(), LocalSideError> {
    if let Some(loaded) = browse::album(database, scope, album_id).await? {
        side.albums.push(projection::album_id3(
            &loaded.detail.album,
            &loaded.detail.artists,
            &loaded.detail.tracks,
            &annotations.album(loaded.detail.album.id),
        ));
    }
    Ok(())
}

/// Loads one track and appends its projection and hit to the local side.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `view`: the track the local search or listing returned.
/// - `annotations`: the user's annotations for the response.
/// - `side`: the side to append to.
///
/// # Errors
///
/// Returns a library failure when a read fails.
async fn push_track(
    database: &Database,
    scope: Scope,
    view: TrackView,
    annotations: &Annotations,
    side: &mut LocalSide,
) -> Result<(), LocalSideError> {
    if let Some(loaded) = browse::track(database, scope, view.track.id).await? {
        side.hits.push(hit(&loaded.view));
        side.tracks.push(projection::child(
            &loaded.view,
            loaded.file.as_ref(),
            &loaded.album_artists,
            &annotations.track(loaded.view.track.id),
        ));
    }
    Ok(())
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
/// A per-kind failure is logged and skipped, because the local side already
/// answered and a catalog instance that is down must not fail the search.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `hits`: the local track hits deduplication compares against.
/// - `text`: the search text.
/// - `shortfalls`: each kind with its window and how many local results it
///   already has.
/// - `title_suffix`: the suffix appended to catalog titles.
/// - `side`: the local side to append the catalog results to.
async fn append_catalog(
    call: &Call,
    hits: &[LocalTrackHit],
    text: &str,
    shortfalls: &[(EntityKind, Window, usize)],
    title_suffix: &str,
    side: &mut LocalSide,
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
    let deduped = match catalog.deduplicate(hits, &found).await {
        Ok(deduped) => deduped,
        Err(error) => {
            tracing::warn!(error = %error, "could not deduplicate catalog results");
            return;
        }
    };

    let (_, artist_window, _) = shortfalls[0];
    let (_, album_window, _) = shortfalls[1];
    let (_, song_window, _) = shortfalls[2];

    let mut artists: Vec<Object> = deduped
        .artists
        .iter()
        .filter(|artist| deduped.stored_entity(&artist.id).is_none())
        .take(window_limit(artist_window))
        .map(|artist| projection::catalog_artist(artist, 0, title_suffix))
        .collect();
    let mut albums: Vec<Object> = deduped
        .albums
        .iter()
        .filter(|album| deduped.stored_entity(&album.id).is_none())
        .take(window_limit(album_window))
        .map(|album| projection::catalog_album_ref(album, title_suffix))
        .collect();
    let mut tracks: Vec<Object> = deduped
        .tracks
        .iter()
        .take(window_limit(song_window))
        .map(|track| projection::catalog_child(&track.id, &track.track, title_suffix))
        .collect();

    side.artists.append(&mut artists);
    side.albums.append(&mut albums);
    side.tracks.append(&mut tracks);
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
