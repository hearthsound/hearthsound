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

use hearthsound_core::EntityId;
use hearthsound_db::Database;
use hearthsound_library::query::{
    AlbumDetail, AlbumSort, ArtistDetail, ArtistSummary, Scope, TrackView,
};
use hearthsound_library::{Album, Artist, LibraryError, MediaFile, artists, media_files, query};

use crate::response::SubsonicError;

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
    pub(crate) files: Vec<Option<MediaFile>>,
}

impl LoadedAlbum {
    /// Reports whether the album has cover art.
    ///
    /// # Returns
    ///
    /// `true` when any of the album's preferred files carries an embedded
    /// picture. A sidecar image beside a file is not consulted here; the
    /// endpoint that serves the image resolves it.
    pub(crate) fn has_art(&self) -> bool {
        self.files.iter().any(|file| {
            matches!(
                file.as_ref().and_then(|file| file.has_embedded_art),
                Some(true)
            )
        })
    }
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
