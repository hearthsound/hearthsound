//! The database reads the browse endpoints share.
//!
//! The scope-filtered queries live in `hearthsound_library::query`; this module
//! adds the preferred media file row a projection needs, which the query
//! projection does not carry in full, and maps a library failure onto the
//! Subsonic failure envelope. `search3` reuses [`track`] for its local results.
//!
//! Each load acquires one reader connection for the whole entity, so a load
//! never holds more than one connection at a time and the pool cannot be
//! exhausted by a browse request.

use std::collections::HashMap;

use hearthsound_activity::{ActivityError, Annotation, load_annotations};
use hearthsound_core::EntityId;
use hearthsound_core::EntityKind;
use hearthsound_db::Database;
use hearthsound_library::query::{
    AlbumDetail, AlbumSort, ArtistDetail, ArtistSummary, Scope, SearchResults, TrackView,
};
use hearthsound_library::{Album, Artist, LibraryError, MediaFile, artists, media_files, query};
use uuid::Uuid;

use crate::response::SubsonicError;
use crate::state::SubsonicState;

/// A track with the data a `Child` projection reports.
#[derive(Debug, Clone)]
pub(crate) struct LoadedTrack {
    /// The track with its album, credited artists, genres, and file projection.
    pub(crate) view: TrackView,
    /// The track's preferred media file, when it has one.
    pub(crate) file: Option<MediaFile>,
    /// The credited artists of the track's album, empty when it has no album.
    pub(crate) album_artists: Vec<Artist>,
}

/// An album detail with its tracks' preferred files.
#[derive(Debug, Clone)]
pub(crate) struct LoadedAlbum {
    /// The album with its credited artists and its tracks.
    pub(crate) detail: AlbumDetail,
    /// The preferred media file of each track, aligned with `detail.tracks`.
    ///
    /// The files are not consulted for a projection: an album and its songs
    /// always name their own cover art and the endpoint that serves the image
    /// resolves whether there is one.
    pub(crate) files: Vec<Option<MediaFile>>,
}

/// Reads one track, if the scope contains it.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The track with its file and its album's artists, or `None` when the track
/// does not exist or is outside the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn track(
    database: &Database,
    scope: Scope,
    track_id: EntityId,
) -> Result<Option<LoadedTrack>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    let Some(view) = query::track(&mut conn, scope, track_id).await? else {
        return Ok(None);
    };
    let file = media_files::preferred_media_file(&mut conn, track_id).await?;
    let album_artists = match view.album.as_ref() {
        Some(album) => artists::list_album_artists(&mut conn, album.id).await?,
        None => Vec::new(),
    };
    Ok(Some(LoadedTrack {
        view,
        file,
        album_artists,
    }))
}

/// Reads one album, if the scope contains it.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album with its tracks and their preferred files, or `None` when the
/// album does not exist or is outside the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn album(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
) -> Result<Option<LoadedAlbum>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    let Some(detail) = query::album_detail(&mut conn, scope, album_id).await? else {
        return Ok(None);
    };
    let mut files = Vec::with_capacity(detail.tracks.len());
    for view in &detail.tracks {
        files.push(media_files::preferred_media_file(&mut conn, view.track.id).await?);
    }
    Ok(Some(LoadedAlbum { detail, files }))
}

/// Reports a read failure on the Subsonic failure envelope.
///
/// The cause is for the server's log; the client is told only that the call
/// could not be completed, because neither a database failure nor an unreadable
/// setting is something a Subsonic client can act on.
///
/// # Parameters
///
/// - `operation`: the name of the read that failed, for the log.
/// - `error`: the failure.
///
/// # Returns
///
/// The generic Subsonic error.
pub(crate) fn failure(operation: &'static str, error: &dyn std::fmt::Display) -> SubsonicError {
    tracing::error!(operation, error = %error, "a browse read failed");
    SubsonicError::generic()
}

/// Returns when the browse index last changed.
///
/// # Parameters
///
/// - `database`: the open database.
///
/// # Returns
///
/// The instant, in unix milliseconds.
///
/// # Errors
///
/// Returns a library failure when the read fails.
pub(crate) async fn last_modified(database: &Database) -> Result<i64, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::library_last_modified(&mut conn).await
}

/// Loads one user's annotations for a page of entities of one kind.
///
/// A response that lists entities carries the user's own stars, ratings, and
/// play counts, which is one read for the whole page rather than one per
/// entity.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database the read uses.
/// - `user_id`: the account whose activity is read.
/// - `kind`: the kind of entity the page holds.
/// - `ids`: the entities of the page.
///
/// # Returns
///
/// The annotations, keyed by entity id. An entity the user has done nothing
/// with is absent, which a projection reads as [`Annotation::default`].
///
/// # Errors
///
/// Returns [`ActivityError`] when the read fails.
pub(crate) async fn annotations(
    state: &SubsonicState,
    user_id: Uuid,
    kind: EntityKind,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Annotation>, ActivityError> {
    let mut conn = state.database().readers().acquire().await?;
    load_annotations(&mut conn, user_id, kind, ids).await
}

/// Loads one user's annotation for a single entity.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose database the read uses.
/// - `user_id`: the account whose activity is read.
/// - `kind`: the kind of the entity.
/// - `id`: the entity.
///
/// # Returns
///
/// The annotation, or [`Annotation::default`] when the user has none.
///
/// # Errors
///
/// Returns [`ActivityError`] when the read fails.
pub(crate) async fn annotation(
    state: &SubsonicState,
    user_id: Uuid,
    kind: EntityKind,
    id: Uuid,
) -> Result<Annotation, ActivityError> {
    let mut annotations = annotations(state, user_id, kind, &[id]).await?;
    Ok(annotations.remove(&id).unwrap_or_default())
}

/// Lists the artists in scope with their album counts.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
///
/// # Returns
///
/// Every artist in scope, ordered by sort name.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn artists(
    database: &Database,
    scope: Scope,
) -> Result<Vec<ArtistSummary>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_artists(&mut conn, scope, -1, 0).await
}

/// Lists the artists in scope, paged.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `limit`: the maximum number of artists.
/// - `offset`: how many artists to skip.
///
/// # Returns
///
/// The artists in the requested window, ordered by sort name.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn artists_page(
    database: &Database,
    scope: Scope,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArtistSummary>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_artists(&mut conn, scope, limit, offset).await
}

/// Lists the tracks in scope, paged.
///
/// This is the listing an empty `search3` query answers with; the full-text
/// index has nothing to match an empty query against.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `limit`: the maximum number of tracks.
/// - `offset`: how many tracks to skip.
///
/// # Returns
///
/// The tracks in the requested window, ordered by sort title.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn list_tracks(
    database: &Database,
    scope: Scope,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_tracks(&mut conn, scope, limit, offset).await
}

/// Searches the local full-text index for one entity kind.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `text`: the raw search text.
/// - `kind`: the entity kind to search.
/// - `limit`: the maximum number of results.
/// - `offset`: how many results to skip.
///
/// # Returns
///
/// The matching entities, in relevance order, inside the one list the kind
/// names.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn search(
    database: &Database,
    scope: Scope,
    text: &str,
    kind: EntityKind,
    limit: i64,
    offset: i64,
) -> Result<SearchResults, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::search(&mut conn, scope, text, &[kind], limit, offset).await
}

/// Reads one artist with its albums, if the scope contains it.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `artist_id`: the artist to read.
///
/// # Returns
///
/// The artist detail, or `None` when the artist does not exist or is outside
/// the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn artist(
    database: &Database,
    scope: Scope,
    artist_id: EntityId,
) -> Result<Option<ArtistDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::artist_detail(&mut conn, scope, artist_id).await
}

/// Lists the albums in scope.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `sort`: how to order the albums.
/// - `size`: the maximum number of albums; zero returns none.
/// - `offset`: how many albums to skip.
///
/// # Returns
///
/// The albums, in the requested order.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn albums(
    database: &Database,
    scope: Scope,
    sort: AlbumSort,
    size: i64,
    offset: i64,
) -> Result<Vec<Album>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_albums(&mut conn, scope, sort, size, offset).await
}
