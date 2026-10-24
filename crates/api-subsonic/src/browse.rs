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
    AlbumDetail, AlbumSort, ArtistDetail, ArtistSummary, GenreCount, Scope, SearchResults,
    TrackView,
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
/// The album with its artists and tracks, or `None` when the album does not
/// exist or is outside the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn album(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::album_detail(&mut conn, scope, album_id).await
}

/// Reads one track from the first scope that contains it.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scopes`: the scopes to try, in preference order.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The track and the scope it was found in, or `None` when no scope contains
/// it.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn track_in_scopes(
    database: &Database,
    scopes: &[Scope],
    track_id: EntityId,
) -> Result<Option<(Scope, LoadedTrack)>, LibraryError> {
    for scope in scopes {
        if let Some(loaded) = track(database, *scope, track_id).await? {
            return Ok(Some((*scope, loaded)));
        }
    }
    Ok(None)
}

/// Reads one album for a listing that does not project its files.
///
/// A listing that reports an album - the artist detail, the album list - builds
/// each album's payload from its row, its artists and its track titles, and
/// never asks for the track's files: an album names its own cover art, and the
/// endpoint that serves the image resolves whether there is one. Reading each
/// track's preferred file here would double the reads of a listing and change
/// nothing in the response.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album with its artists and tracks, or `None` when the album does not
/// exist or is outside the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn album_summary(
    database: &Database,
    scope: Scope,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::album_detail(&mut conn, scope, album_id).await
}

/// Reads one album without an availability filter.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads an album the browse scope would leave out.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album with its artists and tracks, or `None` when the album does not
/// exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn album_any(
    database: &Database,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::album_detail_any(&mut conn, album_id).await
}

/// Reads one album without an availability filter, for a listing.
///
/// A page that already decided which albums the caller may see reads each
/// stored album by its id: the artist page filters its listing through the
/// caller's scopes once, and re-filtering each album per scope would repeat that
/// work for every row.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album with its artists and tracks, or `None` when the album does not
/// exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn album_summary_any(
    database: &Database,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::album_detail_any(&mut conn, album_id).await
}

/// Reads one artist without an availability filter.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads an artist the browse scope would leave out.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `artist_id`: the artist to read.
///
/// # Returns
///
/// The artist with its albums, or `None` when the artist does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn artist_any(
    database: &Database,
    artist_id: EntityId,
) -> Result<Option<ArtistDetail>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::artist_detail_any(&mut conn, artist_id).await
}

/// Reads one track without an availability filter.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads a track the browse scope would leave out.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The track with its file and its album's artists, or `None` when the track
/// does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub(crate) async fn track_any(
    database: &Database,
    track_id: EntityId,
) -> Result<Option<LoadedTrack>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    let Some(view) = query::track_any(&mut conn, track_id).await? else {
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

/// Lists the most played songs crediting an artist.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to count in.
/// - `artist_id`: the artist whose songs are listed.
/// - `count`: the maximum number of songs.
///
/// # Returns
///
/// The songs, most played first.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read fails.
pub(crate) async fn top_tracks_for_artist(
    database: &Database,
    scope: Scope,
    artist_id: EntityId,
    count: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::top_tracks_for_artist(&mut conn, scope, artist_id, count).await
}

/// Finds one artist by name.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `name`: the name to find, compared without regard to case.
///
/// # Returns
///
/// The artist, or `None` when no artist has that name.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read fails.
pub(crate) async fn find_artist_by_name(
    database: &Database,
    name: &str,
) -> Result<Option<Artist>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::find_artist_by_name(&mut conn, name).await
}

/// Lists the genres of the tracks in scope.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to count in.
///
/// # Returns
///
/// The genres, by name.
///
/// # Errors
///
/// Returns a library failure when the read fails.
pub(crate) async fn genres(
    database: &Database,
    scope: Scope,
) -> Result<Vec<GenreCount>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_genres(&mut conn, scope).await
}

/// Lists the songs in scope that carry a genre.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to filter by.
/// - `genre`: the genre to match.
/// - `count`: the maximum number of songs.
/// - `offset`: how many songs to skip.
///
/// # Returns
///
/// The songs, by title.
///
/// # Errors
///
/// Returns a library failure when the read fails.
pub(crate) async fn tracks_by_genre(
    database: &Database,
    scope: Scope,
    genre: &str,
    count: i64,
    offset: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::tracks_by_genre(&mut conn, scope, genre, count, offset).await
}

/// Lists a random sample of the songs in scope.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `scope`: the scope to sample from.
/// - `size`: how many songs to return.
/// - `genre`: only songs of this genre, when given.
/// - `from_year`: the earliest release year, when given.
/// - `to_year`: the latest release year, when given.
///
/// # Returns
///
/// The songs, in no order.
///
/// # Errors
///
/// Returns a library failure when the read fails.
pub(crate) async fn random_tracks(
    database: &Database,
    scope: Scope,
    size: i64,
    genre: Option<&str>,
    from_year: Option<i64>,
    to_year: Option<i64>,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::random_tracks(&mut conn, scope, size, genre, from_year, to_year).await
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
