//! The database reads and id resolutions the browse endpoints share.
//!
//! The scope-filtered queries live in `hearthsound_library::query`; this module
//! adds the preferred media file row a projection needs, which the query
//! projection does not carry in full, and maps a library failure onto the
//! Subsonic failure envelope. `search3` reuses [`track`] for its local results.
//!
//! It also owns the one rule every endpoint follows when it reads an `id`
//! parameter: the value is an [`EntityId`], and the row it names is the one
//! [`CatalogService::canonical`](hearthsound_catalog::CatalogService::canonical)
//! returns, so an id that was merged away answers the survivor and an id that
//! names nothing is a not-found. A client never writes a provider id: every
//! entity a response describes has a row.
//!
//! Each load acquires one reader connection for the whole entity, so a load
//! never holds more than one connection at a time and the pool cannot be
//! exhausted by a browse request.

use std::collections::HashMap;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use hearthsound_activity::{ActivityError, Annotation, load_annotations};
use hearthsound_core::EntityId;
use hearthsound_core::EntityKind;
use hearthsound_core::IdPrefix;
use hearthsound_core::providers::ExternalId;
use hearthsound_db::Database;
use hearthsound_library::query::{
    AlbumDetail, AlbumSort, ArtistDetail, ArtistSort, ArtistSummary, GenreCount, Scope, TrackSort,
    TrackView,
};
use hearthsound_library::{
    Album, Artist, LibraryError, MediaFile, artists, external_ids, media_files, query,
};
use uuid::Uuid;

use crate::auth::Call;
use crate::catalog;
use crate::projection;
use crate::response::SubsonicError;
use crate::state::SubsonicState;
use crate::value::Object;

/// How many rows one response reads at once.
///
/// Four, which is the number of reader connections the database pool holds: a
/// listing then never waits for a connection it is itself holding.
const READ_PARALLELISM: usize = 4;

/// Resolves the entity an `id` parameter names, of the kind the endpoint reads.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose catalog layer resolves the redirects a
///   merge leaves.
/// - `expected`: the id prefix the endpoint can answer for.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The row that stands for the entity now, or `None` when the value is not an
/// id of that kind or names no row.
///
/// # Errors
///
/// Returns a generic Subsonic error when the redirect lookup fails.
pub(crate) async fn entity_id(
    state: &SubsonicState,
    expected: IdPrefix,
    value: &str,
) -> Result<Option<EntityId>, SubsonicError> {
    let Ok(id) = EntityId::parse(value) else {
        tracing::debug!(value, "a request named an id that is not an entity id");
        return Ok(None);
    };
    if id.kind() != expected {
        tracing::debug!(
            id = %id,
            expected = expected.as_str(),
            "a request named an entity of another kind"
        );
        return Ok(None);
    }
    let Some(kind) = expected.entity_kind() else {
        return Ok(None);
    };
    match state.catalog().canonical(kind, id).await {
        Ok(found) => Ok(found),
        Err(error) => Err(catalog::failure(&error)),
    }
}

/// Resolves the entity an `id` parameter names, of any kind the server stores.
///
/// # Parameters
///
/// - `state`: the endpoint state, whose catalog layer resolves the redirects a
///   merge leaves.
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The row that stands for the entity now, or `None` when the value is not an
/// entity id of a catalog kind or names no row. The kind is the id's own, which
/// the survivor of a merge keeps.
///
/// # Errors
///
/// Returns a generic Subsonic error when the redirect lookup fails.
pub(crate) async fn any_entity(
    state: &SubsonicState,
    value: &str,
) -> Result<Option<EntityId>, SubsonicError> {
    let Ok(id) = EntityId::parse(value) else {
        tracing::debug!(value, "a request named an id that is not an entity id");
        return Ok(None);
    };
    if id.kind().entity_kind().is_none() {
        tracing::debug!(id = %id, "a request named a record that is not an entity");
        return Ok(None);
    }
    entity_id(state, id.kind(), value).await
}

/// Reads the identifiers of many entities of one kind.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `kind`: the entity kind.
/// - `ids`: the rows' raw primary keys.
///
/// # Returns
///
/// The identifiers of every row that has any, keyed by the row's primary key.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read fails.
pub(crate) async fn identifiers(
    database: &Database,
    kind: EntityKind,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<ExternalId>>, LibraryError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut conn = database.readers().acquire().await?;
    external_ids::ids_of_many(&mut conn, kind, ids).await
}

/// Reads the identifiers of one entity.
///
/// # Parameters
///
/// - `database`: the open database.
/// - `kind`: the entity kind.
/// - `id`: the row's raw primary key.
///
/// # Returns
///
/// The identifiers, empty when the entity carries none.
///
/// # Errors
///
/// Returns [`LibraryError`] when a read fails.
pub(crate) async fn identifiers_of(
    database: &Database,
    kind: EntityKind,
    id: Uuid,
) -> Result<Vec<ExternalId>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    external_ids::ids_of(&mut conn, kind, id).await
}

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
    sort: ArtistSort,
) -> Result<Vec<ArtistSummary>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_artists(&mut conn, scope, sort, -1, 0).await
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
    sort: ArtistSort,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArtistSummary>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_artists(&mut conn, scope, sort, limit, offset).await
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
    sort: TrackSort,
    genre: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut conn = database.readers().acquire().await?;
    query::list_tracks(&mut conn, scope, sort, genre, limit, offset).await
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

/// The projection inputs of one tracklist.
///
/// A tracklist is read once for the whole page: the track rows with their
/// preferred files, the caller's annotations, and the stored identifiers. An
/// album page, a directory, a search answer and a single song are then projected
/// from the same data, so they cannot disagree about a track.
pub(crate) struct SongPage {
    /// The track rows with their files, in playing order.
    pub(crate) rows: Vec<LoadedTrack>,
    /// The caller's annotations, keyed by track id.
    annotations: HashMap<Uuid, Annotation>,
    /// The stored identifiers, keyed by track id.
    identifiers: HashMap<Uuid, Vec<ExternalId>>,
    /// The suffix a remote title carries, when this caller is answered the way a
    /// third-party client expects.
    suffix: Option<String>,
}

impl SongPage {
    /// Reads the projection inputs of one tracklist.
    ///
    /// # Parameters
    ///
    /// - `call`: the authenticated call.
    /// - `ids`: the tracks of the list, in playing order.
    ///
    /// # Returns
    ///
    /// The inputs; a track that left the database between the view and this read
    /// is left out.
    ///
    /// # Errors
    ///
    /// Answers a generic error when a read fails.
    pub(crate) async fn of(call: &Call, ids: &[EntityId]) -> Result<SongPage, SubsonicError> {
        if ids.is_empty() {
            return Ok(SongPage {
                rows: Vec::new(),
                annotations: HashMap::new(),
                identifiers: HashMap::new(),
                suffix: None,
            });
        }
        let database = call.state().database();
        let reads: Vec<_> = ids.iter().map(|id| track_any(database, *id)).collect();
        let loaded = stream::iter(reads)
            .buffered(READ_PARALLELISM)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|error| failure("a tracklist", &error))?;
        let rows: Vec<LoadedTrack> = loaded.into_iter().flatten().collect();
        let plain: Vec<Uuid> = rows.iter().map(|row| row.view.track.id.uuid()).collect();
        let annotations = annotations(
            call.state(),
            call.principal().user_id.uuid(),
            EntityKind::Track,
            &plain,
        )
        .await
        .map_err(|error| failure("a tracklist", &error))?;
        let identifiers = identifiers(database, EntityKind::Track, &plain)
            .await
            .map_err(|error| failure("a tracklist", &error))?;
        Ok(SongPage {
            rows,
            annotations,
            identifiers,
            suffix: remote_suffix(call),
        })
    }

    /// Returns the identifier of each track, in playing order.
    ///
    /// # Returns
    ///
    /// The ids the prefetch scheduler records a listing with.
    pub(crate) fn ids(&self) -> Vec<EntityId> {
        self.rows.iter().map(|row| row.view.track.id).collect()
    }

    /// Returns the total length of the tracklist.
    ///
    /// # Returns
    ///
    /// The playing time in milliseconds, from each file when it has one and from
    /// the row otherwise.
    pub(crate) fn duration_ms(&self) -> i64 {
        self.rows
            .iter()
            .map(|row| {
                row.file
                    .as_ref()
                    .and_then(|file| file.duration_ms)
                    .or(row.view.track.duration_ms)
                    .unwrap_or(0)
            })
            .sum()
    }

    /// Returns the rows as the query projection reports them.
    ///
    /// # Returns
    ///
    /// One projection per track, in playing order.
    pub(crate) fn views(&self) -> Vec<TrackView> {
        self.rows.iter().map(|row| row.view.clone()).collect()
    }

    /// Returns the annotation of one track.
    ///
    /// # Parameters
    ///
    /// - `id`: the track's raw primary key.
    pub(crate) fn annotation(&self, id: Uuid) -> Annotation {
        self.annotations.get(&id).copied().unwrap_or_default()
    }

    /// Projects one track as a `Child` member.
    ///
    /// # Parameters
    ///
    /// - `index`: the row's position in the page.
    ///
    /// # Returns
    ///
    /// The member.
    pub(crate) fn object(&self, index: usize) -> Object {
        let row = &self.rows[index];
        let child = projection::child(
            &row.view,
            row.file.as_ref(),
            &row.album_artists,
            &self.annotation(row.view.track.id.uuid()),
            self.identifiers
                .get(&row.view.track.id.uuid())
                .map_or(&[], Vec::as_slice),
        );
        match (&self.suffix, row.file.is_none()) {
            (Some(suffix), true) => child.set("title", format!("{}{suffix}", row.view.track.title)),
            _ => child,
        }
    }

    /// Projects the page as `Child` members.
    ///
    /// # Returns
    ///
    /// One member per track, in playing order.
    pub(crate) fn objects(&self) -> Vec<Object> {
        (0..self.rows.len())
            .map(|index| self.object(index))
            .collect()
    }
}

/// The projection inputs of one album listing.
pub(crate) struct AlbumPage {
    /// The albums that are still stored, in the order they were listed.
    pub(crate) details: Vec<AlbumDetail>,
    /// The caller's annotations, keyed by album id.
    annotations: HashMap<Uuid, Annotation>,
    /// The stored identifiers, keyed by album id.
    identifiers: HashMap<Uuid, Vec<ExternalId>>,
    /// The suffix a remote title carries, when this caller is answered the way a
    /// third-party client expects.
    suffix: Option<String>,
}

impl AlbumPage {
    /// Reads the projection inputs of one album listing.
    ///
    /// # Parameters
    ///
    /// - `call`: the authenticated call.
    /// - `ids`: the albums, in the order they are listed.
    ///
    /// # Errors
    ///
    /// Answers a generic error when a read fails.
    pub(crate) async fn of(call: &Call, ids: &[EntityId]) -> Result<AlbumPage, SubsonicError> {
        if ids.is_empty() {
            return Ok(AlbumPage {
                details: Vec::new(),
                annotations: HashMap::new(),
                identifiers: HashMap::new(),
                suffix: None,
            });
        }
        let database = call.state().database();
        let reads: Vec<_> = ids
            .iter()
            .map(|id| album_summary_any(database, *id))
            .collect();
        let details = stream::iter(reads)
            .buffered(READ_PARALLELISM)
            .try_collect::<Vec<_>>()
            .await
            .map_err(|error| failure("an album listing", &error))?;
        let details: Vec<AlbumDetail> = details.into_iter().flatten().collect();
        let plain: Vec<Uuid> = details
            .iter()
            .map(|detail| detail.album.id.uuid())
            .collect();
        let annotations = annotations(
            call.state(),
            call.principal().user_id.uuid(),
            EntityKind::Album,
            &plain,
        )
        .await
        .map_err(|error| failure("an album listing", &error))?;
        let identifiers = identifiers(database, EntityKind::Album, &plain)
            .await
            .map_err(|error| failure("an album listing", &error))?;
        Ok(AlbumPage {
            details,
            annotations,
            identifiers,
            suffix: remote_suffix(call),
        })
    }

    /// Returns the annotation of one album.
    ///
    /// # Parameters
    ///
    /// - `id`: the album's raw primary key.
    pub(crate) fn annotation(&self, id: Uuid) -> Annotation {
        self.annotations.get(&id).copied().unwrap_or_default()
    }

    /// Projects one album as an `AlbumID3` member.
    ///
    /// # Parameters
    ///
    /// - `index`: the album's position in the page.
    ///
    /// # Returns
    ///
    /// The member.
    pub(crate) fn id3(&self, index: usize) -> Object {
        let detail = &self.details[index];
        let album = projection::album_id3(
            &detail.album,
            &detail.artists,
            &detail.tracks,
            &self.annotation(detail.album.id.uuid()),
            self.identifiers
                .get(&detail.album.id.uuid())
                .map_or(&[], Vec::as_slice),
        );
        self.with_suffix(detail, album)
    }

    /// Projects one album as a directory `Child` member.
    ///
    /// # Parameters
    ///
    /// - `index`: the album's position in the page.
    ///
    /// # Returns
    ///
    /// The member.
    pub(crate) fn dir(&self, index: usize) -> Object {
        let detail = &self.details[index];
        let album = projection::album_dir(
            &detail.album,
            &detail.artists,
            &detail.tracks,
            &self.annotation(detail.album.id.uuid()),
        );
        self.with_suffix(detail, album)
    }

    /// Marks an album that holds no local track with the configured suffix.
    ///
    /// # Parameters
    ///
    /// - `detail`: the album the member was projected from.
    /// - `album`: the projected member.
    ///
    /// # Returns
    ///
    /// The member, named with the suffix when the album is remote and this caller
    /// is answered the way a third-party client expects.
    fn with_suffix(&self, detail: &AlbumDetail, album: Object) -> Object {
        match (
            &self.suffix,
            detail.tracks.iter().any(|track| track.file.is_some()),
        ) {
            (Some(suffix), false) => album
                .set("name", format!("{}{suffix}", detail.album.title))
                .set("title", format!("{}{suffix}", detail.album.title)),
            _ => album,
        }
    }
}

/// Projects one tracklist as `Child` members.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the database and the
///   caller's annotations.
/// - `ids`: the tracks, in playing order.
///
/// # Returns
///
/// One member per track that is still stored, in the same order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub(crate) async fn songs(call: &Call, ids: &[EntityId]) -> Result<Vec<Object>, SubsonicError> {
    Ok(SongPage::of(call, ids).await?.objects())
}

/// Projects a list of albums as `AlbumID3` members.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the albums, in the order they are listed.
///
/// # Returns
///
/// One member per album that is still stored, in the same order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub(crate) async fn albums_id3(
    call: &Call,
    ids: &[EntityId],
) -> Result<Vec<Object>, SubsonicError> {
    let page = AlbumPage::of(call, ids).await?;
    Ok((0..page.details.len())
        .map(|index| page.id3(index))
        .collect())
}

/// Projects a list of albums as directory `Child` members.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the albums, in the order they are listed.
///
/// # Returns
///
/// One member per album that is still stored, in the same order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub(crate) async fn albums_dir(
    call: &Call,
    ids: &[EntityId],
) -> Result<Vec<Object>, SubsonicError> {
    let page = AlbumPage::of(call, ids).await?;
    Ok((0..page.details.len())
        .map(|index| page.dir(index))
        .collect())
}

/// Returns the suffix a remote entity's title carries for this request.
///
/// A third-party client is answered exactly as a server without the vendor
/// extension would answer it, and that server marks the entities it stores but
/// has not downloaded yet, so the client can tell them apart. This server's own
/// client reads the availability instead.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The suffix, or `None` when the caller is this server's own client or the
/// setting is empty.
fn remote_suffix(call: &Call) -> Option<String> {
    if call.params().vendor_enabled() {
        return None;
    }
    match catalog::title_suffix(&call.state().settings_snapshot()) {
        Ok(suffix) if !suffix.is_empty() => Some(suffix),
        Ok(_) => None,
        Err(_) => {
            tracing::warn!("could not read subsonic.catalog_title_suffix");
            None
        }
    }
}
