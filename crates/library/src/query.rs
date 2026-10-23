//! The read queries behind the browse and search endpoints.
//!
//! Two scopes decide what a browse sees:
//!
//! - the **personal library** of one user, which is the union of the tracks the
//!   user saved or uploaded, the tracks the user liked, the tracks on the user's
//!   non-temporary playlists, and the tracks whose file lives in a folder
//!   associated with the user (`library_folders.owner_user_id`). Albums and
//!   artists follow by containment: an album is in the library when it is saved
//!   or liked or when it holds at least one library track, and an artist when it
//!   is saved or liked or is credited on one;
//! - the **server** scope, which holds everything stored and is offered to
//!   administrators only; everyone else reads nothing from it.
//!
//! Everything is read through one connection so a caller can run a whole browse
//! inside a read transaction. The queries return rows that are ready for the
//! Subsonic projection: a track view carries its album, its credited artists, its
//! genres, and the fields of its preferred file.

use std::collections::{HashMap, HashSet};

use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix, StorageClass};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{Album, Artist, MediaFile, StorageRoots, Track, decode_entity, entity_key};
use crate::{albums, artists, folders, media_files, tracks};

/// Which slice of the library a browse or a search reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The personal library of one user.
    Personal {
        /// The user whose library is read.
        user_id: Uuid,
    },
    /// The server-wide view.
    Server(ServerBrowse),
}

impl Scope {
    /// Builds the server scope a principal may read.
    ///
    /// # Parameters
    ///
    /// - `is_admin`: whether the principal administers the server.
    ///
    /// # Returns
    ///
    /// The server scope every stored track is in for an administrator, and one
    /// whose queries answer empty for everyone else.
    pub fn server_for_principal(is_admin: bool) -> Scope {
        Scope::Server(ServerBrowse::for_principal(is_admin))
    }
}

/// The content of the server browse view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerBrowse {
    /// Everything stored, including evictable cache.
    All,
    /// The server view is not offered.
    Disabled,
}

impl ServerBrowse {
    /// Returns the view a principal may read.
    ///
    /// The server-wide view belongs to administrators: it holds every stored
    /// track, including evictable cache. Everyone else has no server view, so
    /// the queries that take it answer empty.
    ///
    /// # Parameters
    ///
    /// - `is_admin`: whether the principal administers the server.
    ///
    /// # Returns
    ///
    /// [`ServerBrowse::All`] for an administrator, [`ServerBrowse::Disabled`]
    /// otherwise.
    pub const fn for_principal(is_admin: bool) -> ServerBrowse {
        if is_admin {
            ServerBrowse::All
        } else {
            ServerBrowse::Disabled
        }
    }

    /// Returns the two availability literals the view matches.
    ///
    /// # Returns
    ///
    /// `("local", "cached")` for [`ServerBrowse::All`] and `("local", "local")`
    /// for [`ServerBrowse::Disabled`], whose queries return before they read
    /// this; the pair is bound to an `IN (?, ?)` predicate.
    fn availability_pair(self) -> (&'static str, &'static str) {
        match self {
            ServerBrowse::All => (Availability::Local.as_str(), Availability::Cached.as_str()),
            ServerBrowse::Disabled => (Availability::Local.as_str(), Availability::Local.as_str()),
        }
    }
}

/// How an album list is ordered.
///
/// The variants a user's own activity decides carry the user they are read for,
/// because a rating, a play, or a like is per user.
///
/// # Examples
///
/// ```
/// use hearthsound_library::query::AlbumSort;
///
/// let by_year = AlbumSort::ByYear { from: 1990, to: 1999 };
/// assert_eq!(by_year.to_year(), Some((1990, 1999)));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlbumSort {
    /// Newest first, by creation time.
    Newest,
    /// Alphabetically by the album's sort title.
    AlphabeticalByName,
    /// Alphabetically by the album's first credited artist, then by title.
    AlphabeticalByArtist,
    /// A random sample of the requested size.
    Random,
    /// The user's highest rated albums first.
    Highest {
        /// The account whose ratings are read.
        user: Uuid,
    },
    /// The albums the user played most often first.
    Frequent {
        /// The account whose plays are read.
        user: Uuid,
    },
    /// The albums the user played most recently first.
    Recent {
        /// The account whose plays are read.
        user: Uuid,
    },
    /// The albums the user liked most recently first.
    Starred {
        /// The account whose likes are read.
        user: Uuid,
    },
    /// Albums released in a year range, newest first when the range counts
    /// down.
    ByYear {
        /// One end of the release-year range.
        from: i64,
        /// The other end; the range is inclusive either way round.
        to: i64,
    },
    /// Albums with a track of one genre, alphabetically by title.
    ByGenre(String),
}

impl AlbumSort {
    /// Returns the release years this order covers.
    ///
    /// # Returns
    ///
    /// The inclusive range, or `None` for every other order.
    pub fn to_year(&self) -> Option<(i64, i64)> {
        match self {
            AlbumSort::ByYear { from, to } => Some((*from, *to)),
            _ => None,
        }
    }

    /// Returns the genre this order filters on.
    ///
    /// # Returns
    ///
    /// The genre, or `None` for every other order.
    pub fn genre(&self) -> Option<&str> {
        match self {
            AlbumSort::ByGenre(genre) => Some(genre),
            _ => None,
        }
    }
}

/// An artist with the number of albums it has in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistSummary {
    /// The artist.
    pub artist: Artist,
    /// How many albums in scope the artist is credited on.
    pub album_count: i64,
}

/// An artist with its albums in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDetail {
    /// The artist.
    pub artist: Artist,
    /// The artist's albums in scope, oldest first.
    pub albums: Vec<Album>,
}

/// An album with its credited artists and its tracks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumDetail {
    /// The album.
    pub album: Album,
    /// The album's credited artists.
    pub artists: Vec<Artist>,
    /// The album's tracks, in playing order.
    pub tracks: Vec<TrackView>,
}

/// The fields of a media file a browse response needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFileProjection {
    /// The file's suffix, without the leading dot.
    pub suffix: String,
    /// The MIME type the suffix implies.
    pub content_type: String,
    /// The file size in bytes.
    pub size: i64,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: Option<i64>,
    /// The duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// The path relative to the file's root, never an absolute path.
    pub path: String,
}

/// A track with everything a Subsonic child needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackView {
    /// The track row.
    pub track: Track,
    /// The album the track belongs to, when it has one.
    pub album: Option<Album>,
    /// The track's credited artists, in credit order.
    pub artists: Vec<Artist>,
    /// The track's genre names.
    pub genres: Vec<String>,
    /// The track's preferred file, when it has one.
    pub file: Option<MediaFileProjection>,
}

/// The result of a local full-text search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchResults {
    /// Matching artists, in relevance order.
    pub artists: Vec<Artist>,
    /// Matching albums, in relevance order.
    pub albums: Vec<Album>,
    /// Matching tracks, in relevance order.
    pub tracks: Vec<TrackView>,
}

/// A media file with the absolute path it resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferredFile {
    /// The preferred file row.
    pub media_file: MediaFile,
    /// The absolute path the row resolves to.
    pub absolute_path: std::path::PathBuf,
}

/// Returns the MIME type a file suffix implies.
///
/// # Parameters
///
/// - `suffix`: the file extension without the leading dot.
///
/// # Returns
///
/// The MIME type, or `application/octet-stream` for a suffix that is not one of
/// the containers the server stores.
pub fn content_type_for(suffix: &str) -> &'static str {
    match suffix {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "wv" => "audio/x-wavpack",
        "ape" => "audio/x-ape",
        "aif" | "aiff" => "audio/aiff",
        "mpc" => "audio/x-musepack",
        _ => "application/octet-stream",
    }
}

/// Turns free text into an FTS5 prefix query.
///
/// The text is split on everything that is not a letter or a digit, and each
/// remaining token becomes a quoted prefix term, so the query matches the words
/// the user typed rather than FTS5 syntax they may have typed by accident. A
/// query with no usable token returns `None`, which means "no results" rather
/// than "everything".
///
/// # Parameters
///
/// - `text`: the raw search text.
///
/// # Returns
///
/// The FTS5 query, or `None` when the text holds no token.
///
/// # Examples
///
/// ```
/// use hearthsound_library::query::sanitize_match;
///
/// assert_eq!(sanitize_match("Cafe del Mar").unwrap(), "\"cafe\"* \"del\"* \"mar\"*");
/// assert_eq!(sanitize_match("  ...  "), None);
/// ```
pub fn sanitize_match(text: &str) -> Option<String> {
    let tokens: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"*", token.to_lowercase()))
        .collect();
    if tokens.is_empty() {
        None
    } else {
        Some(tokens.join(" "))
    }
}

/// Reads one track, if the scope contains it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The track view, or `None` when the track does not exist or is outside the
/// scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn track(
    conn: &mut SqliteConnection,
    scope: Scope,
    track_id: EntityId,
) -> Result<Option<TrackView>, LibraryError> {
    if !track_in_scope(conn, scope, track_id).await? {
        return Ok(None);
    }
    match tracks::get_track(conn, track_id).await? {
        Some(track) => Ok(Some(track_view(conn, track).await?)),
        None => Ok(None),
    }
}

/// Reads one album, if the scope contains it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album detail, or `None` when the album does not exist or is outside the
/// scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn album_detail(
    conn: &mut SqliteConnection,
    scope: Scope,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    if !album_in_scope(conn, scope, album_id).await? {
        return Ok(None);
    }
    let Some(album) = albums::get_album(conn, album_id).await? else {
        return Ok(None);
    };
    let artists = artists::list_album_artists(conn, album_id).await?;
    let mut tracks = Vec::new();
    for track in tracks::list_tracks_for_album(conn, album_id).await? {
        tracks.push(track_view_in_album(conn, track, Some(album.clone())).await?);
    }
    Ok(Some(AlbumDetail {
        album,
        artists,
        tracks,
    }))
}

/// Reads one album with its artists and tracks, whatever its availability.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads an album without the scope filter the browse endpoints apply.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_id`: the album to read.
///
/// # Returns
///
/// The album detail, or `None` when the album does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn album_detail_any(
    conn: &mut SqliteConnection,
    album_id: EntityId,
) -> Result<Option<AlbumDetail>, LibraryError> {
    let Some(album) = albums::get_album(conn, album_id).await? else {
        return Ok(None);
    };
    let artists = artists::list_album_artists(conn, album_id).await?;
    let mut tracks = Vec::new();
    for track in tracks::list_tracks_for_album(conn, album_id).await? {
        tracks.push(track_view(conn, track).await?);
    }
    Ok(Some(AlbumDetail {
        album,
        artists,
        tracks,
    }))
}

/// Reads one artist with its albums, whatever their availability.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads an artist without the scope filter the browse endpoints apply.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `artist_id`: the artist to read.
///
/// # Returns
///
/// The artist detail, or `None` when the artist does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn artist_detail_any(
    conn: &mut SqliteConnection,
    artist_id: EntityId,
) -> Result<Option<ArtistDetail>, LibraryError> {
    let Some(artist) = artists::get_artist(conn, artist_id).await? else {
        return Ok(None);
    };
    let albums = albums::list_albums_for_artist(conn, artist_id).await?;
    Ok(Some(ArtistDetail { artist, albums }))
}

/// Reads the artist credited first on a user's most recent play.
///
/// A discovery panel often needs one seed the listener chose themselves, and
/// the last thing they played describes their current taste best. The primary
/// credit - `track_artists` position 0 - is the artist such a seed stands for.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose play history is read.
///
/// # Returns
///
/// The artist of the most recent play, or `None` when the account has played
/// nothing or the track carries no credit.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails or a stored id does not decode.
pub async fn most_recent_played_artist(
    conn: &mut SqliteConnection,
    user_id: Uuid,
) -> Result<Option<Artist>, LibraryError> {
    let row = sqlx::query_as!(
        artists::ArtistRow,
        "SELECT ar.id, ar.name, ar.sort_name, ar.mb_artist_id, ar.origin, ar.created_at, \
                ar.updated_at \
         FROM plays p \
         JOIN track_artists ta ON ta.track_id = p.track_id AND ta.position = 0 \
         JOIN artists ar ON ar.id = ta.artist_id \
         WHERE p.user_id = ? \
         ORDER BY p.started_at DESC, p.id DESC \
         LIMIT 1",
        user_id.as_bytes().as_slice(),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(artists::ArtistRow::into_artist).transpose()
}

/// Reads one track, whatever its availability.
///
/// A starred listing reports the entities a user liked however they are stored,
/// so it reads a track without the scope filter the browse endpoints apply.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The track view, or `None` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn track_any(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Option<TrackView>, LibraryError> {
    let Some(track) = tracks::get_track(conn, track_id).await? else {
        return Ok(None);
    };
    Ok(Some(track_view(conn, track).await?))
}

/// Reads one artist with its albums, if the scope contains it.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `artist_id`: the artist to read.
///
/// # Returns
///
/// The artist detail, or `None` when the artist does not exist or is outside the
/// scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn artist_detail(
    conn: &mut SqliteConnection,
    scope: Scope,
    artist_id: EntityId,
) -> Result<Option<ArtistDetail>, LibraryError> {
    if !artist_in_scope(conn, scope, artist_id).await? {
        return Ok(None);
    }
    let Some(artist) = artists::get_artist(conn, artist_id).await? else {
        return Ok(None);
    };
    let albums = list_albums_for_artist(conn, scope, artist_id).await?;
    Ok(Some(ArtistDetail { artist, albums }))
}

/// Lists the artists in scope with their album counts.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `limit`: the maximum number of artists; a negative limit is unbounded.
/// - `offset`: how many artists to skip.
///
/// # Returns
///
/// The artists ordered by sort name, with the id as a tie breaker. An artist
/// whose albums and tracks are all outside the scope is not returned.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn list_artists(
    conn: &mut SqliteConnection,
    scope: Scope,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArtistSummary>, LibraryError> {
    let rows = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_as!(
                ArtistSummaryRow,
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT ar.id, ar.name, ar.sort_name, ar.mb_artist_id, ar.origin, ar.created_at, \
                        ar.updated_at, \
                        (SELECT COUNT(DISTINCT al.id) FROM albums al \
                           JOIN album_artists aa ON aa.album_id = al.id AND aa.artist_id = ar.id \
                           JOIN tracks t ON t.album_id = al.id \
                           JOIN scope_tracks st ON st.track_id = t.id) AS album_count \
                 FROM artists ar \
                 WHERE ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                   JOIN scope_tracks st ON st.track_id = ta.track_id) \
                    OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                           SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)) \
                    OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'artist' \
                                 AND entity_id = ar.id) \
                    OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'artist' \
                                 AND value = 'like' AND entity_id = ar.id) \
                 ORDER BY ar.sort_name, ar.id LIMIT ? OFFSET ?",
                user,
                user,
                user,
                user,
                user,
                user,
                limit,
                offset,
            )
            .fetch_all(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(Vec::new());
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_as!(
                ArtistSummaryRow,
                "WITH scope_tracks(track_id) AS ( \
                     SELECT id FROM tracks WHERE availability IN (?, ?) \
                 ) \
                 SELECT ar.id, ar.name, ar.sort_name, ar.mb_artist_id, ar.origin, ar.created_at, \
                        ar.updated_at, \
                        (SELECT COUNT(DISTINCT al.id) FROM albums al \
                           JOIN album_artists aa ON aa.album_id = al.id AND aa.artist_id = ar.id \
                           JOIN tracks t ON t.album_id = al.id \
                           JOIN scope_tracks st ON st.track_id = t.id) AS album_count \
                 FROM artists ar \
                 WHERE ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                   JOIN scope_tracks st ON st.track_id = ta.track_id) \
                    OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                           SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)) \
                 ORDER BY ar.sort_name, ar.id LIMIT ? OFFSET ?",
                available,
                cached,
                limit,
                offset,
            )
            .fetch_all(&mut *conn)
            .await?
        }
    };
    rows.into_iter()
        .map(|row| {
            let album_count = row.album_count;
            Ok(ArtistSummary {
                artist: row.into_artist()?,
                album_count,
            })
        })
        .collect()
}

/// Counts the artists in scope.
///
/// The count covers exactly the artists [`list_artists`] returns, so a client
/// can size a page of placeholders before it has read any artist. It applies the
/// same membership rules and drops only the `ORDER BY`, `LIMIT` and `OFFSET`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
///
/// # Returns
///
/// The number of artists in the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn count_artists(conn: &mut SqliteConnection, scope: Scope) -> Result<i64, LibraryError> {
    let counted: i64 = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT count(*) AS counted FROM artists ar \
                 WHERE ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                   JOIN scope_tracks st ON st.track_id = ta.track_id) \
                    OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                           SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)) \
                    OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'artist' \
                                 AND entity_id = ar.id) \
                    OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'artist' \
                                 AND value = 'like' AND entity_id = ar.id)",
                user,
                user,
                user,
                user,
                user,
                user,
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(0);
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT id FROM tracks WHERE availability IN (?, ?) \
                 ) \
                 SELECT count(*) AS counted FROM artists ar \
                 WHERE ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                   JOIN scope_tracks st ON st.track_id = ta.track_id) \
                    OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                           SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id))",
                available,
                cached,
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(counted)
}

/// Lists the albums in scope.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `sort`: how to order the albums.
/// - `size`: the maximum number of albums; a negative size is unbounded.
/// - `offset`: how many albums to skip.
///
/// # Returns
///
/// The albums. `Random` returns a random sample of `size` albums.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn list_albums(
    conn: &mut SqliteConnection,
    scope: Scope,
    sort: AlbumSort,
    size: i64,
    offset: i64,
) -> Result<Vec<Album>, LibraryError> {
    if scope == Scope::Server(ServerBrowse::Disabled) {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("WITH scope_tracks(track_id) AS (");
    push_album_scope(&mut builder, scope);
    builder.push(
        "SELECT al.id, al.title, al.sort_title, al.display_artist, al.year, al.release_date, \
         al.album_type, al.mb_release_id, al.mb_release_group_id, al.upc, al.origin, \
         al.created_at, al.updated_at \
         FROM albums al \
         WHERE ",
    );
    push_album_filter(&mut builder, scope, Some(&sort));
    builder.push(" ORDER BY ");
    match &sort {
        AlbumSort::Newest => {
            builder.push("al.created_at DESC, al.id DESC");
        }
        AlbumSort::AlphabeticalByName => {
            builder.push("al.sort_title, al.id");
        }
        AlbumSort::AlphabeticalByArtist => {
            builder.push(
                "(SELECT ar.sort_name FROM album_artists aa JOIN artists ar ON ar.id = aa.artist_id \
                 WHERE aa.album_id = al.id ORDER BY aa.position LIMIT 1), al.sort_title, al.id",
            );
        }
        AlbumSort::Random => {
            builder.push("RANDOM()");
        }
        AlbumSort::Highest { user } => {
            builder
                .push("(SELECT r.rating FROM user_ratings r WHERE r.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND r.entity_kind = 'album' AND r.entity_id = al.id) DESC, al.sort_title, al.id");
        }
        AlbumSort::Frequent { user } => {
            builder
                .push("(SELECT count(*) FROM plays p JOIN tracks t ON t.id = p.track_id WHERE p.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND t.album_id = al.id) DESC, al.sort_title, al.id");
        }
        AlbumSort::Recent { user } => {
            builder
                .push("(SELECT max(p.started_at) FROM plays p JOIN tracks t ON t.id = p.track_id WHERE p.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND t.album_id = al.id) DESC, al.sort_title, al.id");
        }
        AlbumSort::Starred { user } => {
            builder
                .push("(SELECT f.at FROM user_feedback f WHERE f.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND f.entity_kind = 'album' AND f.value = 'like' AND f.entity_id = al.id) DESC, al.sort_title, al.id");
        }
        AlbumSort::ByYear { from, to } => {
            if from <= to {
                builder.push("al.year, al.sort_title, al.id");
            } else {
                builder.push("al.year DESC, al.sort_title, al.id");
            }
        }
        AlbumSort::ByGenre(_) => {
            builder.push("al.sort_title, al.id");
        }
    }
    builder
        .push(" LIMIT ")
        .push_bind(size)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows = builder
        .build_query_as::<albums::AlbumRow>()
        .fetch_all(&mut *conn)
        .await?;
    rows.into_iter().map(|row| row.into_album()).collect()
}

/// Pushes the `scope_tracks` common table expression an album query reads.
///
/// The CTE is the set of tracks one scope can see: the caller's personal library
/// for a personal scope, and the tracks at the browsable availabilities for the
/// server-wide one. The listing, its count and the genre counts all build on it,
/// so they always agree on what the scope holds.
///
/// # Parameters
///
/// - `builder`: the query being built.
/// - `scope`: the scope to filter by.
fn push_album_scope(builder: &mut QueryBuilder<Sqlite>, scope: Scope) {
    match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().to_vec();
            builder
                .push("SELECT entity_id FROM user_library WHERE user_id = ")
                .push_bind(user.clone())
                .push(" AND entity_kind = 'track' UNION SELECT entity_id FROM user_feedback WHERE user_id = ")
                .push_bind(user.clone())
                .push(" AND entity_kind = 'track' AND value = 'like' UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id WHERE p.owner_user_id = ")
                .push_bind(user.clone())
                .push(" AND p.is_temporary = 0 UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id WHERE mf.storage = 'folder' AND lf.owner_user_id = ")
                .push_bind(user)
                .push(") ");
        }
        Scope::Server(view) => {
            let (available, cached) = view.availability_pair();
            builder
                .push("SELECT id FROM tracks WHERE availability IN (")
                .push_bind(available)
                .push(", ")
                .push_bind(cached)
                .push(")) ");
        }
    }
}

/// Pushes the `WHERE` clause of an album query.
///
/// # Parameters
///
/// - `builder`: the query being built, after its `FROM` and `WHERE`.
/// - `scope`: the scope to filter by.
/// - `sort`: the order the query will be read in, or `None` for a query that
///   counts the scope without narrowing it to an activity.
fn push_album_filter(builder: &mut QueryBuilder<Sqlite>, scope: Scope, sort: Option<&AlbumSort>) {
    match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().to_vec();
            builder
                .push("(al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ")
                .push_bind(user.clone())
                .push(" AND entity_kind = 'album' AND entity_id = al.id) OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ")
                .push_bind(user)
                .push(" AND entity_kind = 'album' AND value = 'like' AND entity_id = al.id))");
        }
        Scope::Server(_) => {
            builder.push(
                "al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)",
            );
        }
    }
    let Some(sort) = sort else {
        return;
    };
    // The lists that read what the user did carry only the albums that have
    // that activity: an album never played is not recently played, and an
    // unrated album has no place in a rating order.
    match sort {
        AlbumSort::Highest { user } => {
            builder
                .push(" AND EXISTS (SELECT 1 FROM user_ratings r WHERE r.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND r.entity_kind = 'album' AND r.entity_id = al.id AND r.rating > 0)");
        }
        AlbumSort::Frequent { user } => {
            builder
                .push(" AND EXISTS (SELECT 1 FROM plays p JOIN tracks t ON t.id = p.track_id WHERE p.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND t.album_id = al.id)");
        }
        AlbumSort::Recent { user } => {
            builder
                .push(" AND EXISTS (SELECT 1 FROM plays p JOIN tracks t ON t.id = p.track_id WHERE p.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND t.album_id = al.id)");
        }
        AlbumSort::Starred { user } => {
            builder
                .push(" AND EXISTS (SELECT 1 FROM user_feedback f WHERE f.user_id = ")
                .push_bind(user.as_bytes().to_vec())
                .push(" AND f.entity_kind = 'album' AND f.value = 'like' AND f.entity_id = al.id)");
        }
        AlbumSort::ByYear { from, to } => {
            let (low, high) = if from <= to {
                (*from, *to)
            } else {
                (*to, *from)
            };
            builder
                .push(" AND al.year BETWEEN ")
                .push_bind(low)
                .push(" AND ")
                .push_bind(high);
        }
        AlbumSort::ByGenre(genre) => {
            builder
                .push(
                    " AND EXISTS (SELECT 1 FROM tracks t JOIN track_genres tg ON tg.track_id = t.id \
                     JOIN genres g ON g.id = tg.genre_id WHERE t.album_id = al.id AND g.name = ",
                )
                .push_bind(fold_genre(genre))
                .push(")");
        }
        _ => {}
    }
}

/// Counts the albums in scope.
///
/// The count covers exactly the albums [`list_albums`] returns for a listing
/// that narrows nothing, so a client can size a page of placeholders before it
/// has read any album. It reads no `ORDER BY`, `LIMIT` or `OFFSET`: the same
/// rows, counted.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
///
/// # Returns
///
/// The number of albums in the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn count_albums(conn: &mut SqliteConnection, scope: Scope) -> Result<i64, LibraryError> {
    if scope == Scope::Server(ServerBrowse::Disabled) {
        return Ok(0);
    }
    let mut builder = QueryBuilder::<Sqlite>::new("WITH scope_tracks(track_id) AS (");
    push_album_scope(&mut builder, scope);
    builder.push("SELECT count(*) AS counted FROM albums al WHERE ");
    push_album_filter(&mut builder, scope, None);
    let counted: i64 = builder.build_query_scalar().fetch_one(&mut *conn).await?;
    Ok(counted)
}

/// A genre with how many songs and albums of it are in scope.
///
/// # Examples
///
/// ```
/// use hearthsound_library::query::GenreCount;
///
/// let genre = GenreCount {
///     name: "ambient".to_owned(),
///     song_count: 3,
///     album_count: 1,
/// };
/// assert_eq!(genre.name, "ambient");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenreCount {
    /// The genre's name, as it is stored.
    pub name: String,
    /// How many songs in scope carry it.
    pub song_count: i64,
    /// How many albums in scope hold one of those songs.
    pub album_count: i64,
}

/// Appends the scope's track clause to a statement.
///
/// The clause is a `WITH scope_tracks(track_id) AS (...)` common table
/// expression naming every track the scope holds, which the statement's own
/// `SELECT` joins against. It comes first in the statement, so a caller appends
/// its own text after it, and every bind the caller adds follows the ones this
/// pushes.
///
/// # Parameters
///
/// - `builder`: the builder to append to.
/// - `scope`: the scope to name.
fn push_scope_tracks(builder: &mut QueryBuilder<Sqlite>, scope: Scope) {
    builder.push("WITH scope_tracks(track_id) AS (");
    match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().to_vec();
            builder
                .push("SELECT entity_id FROM user_library WHERE user_id = ")
                .push_bind(user.clone())
                .push(" AND entity_kind = 'track' UNION SELECT entity_id FROM user_feedback WHERE user_id = ")
                .push_bind(user.clone())
                .push(" AND entity_kind = 'track' AND value = 'like' UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id WHERE p.owner_user_id = ")
                .push_bind(user.clone())
                .push(" AND p.is_temporary = 0 UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id WHERE mf.storage = 'folder' AND lf.owner_user_id = ")
                .push_bind(user);
        }
        Scope::Server(view) => {
            let (available, cached) = view.availability_pair();
            builder
                .push("SELECT id FROM tracks WHERE availability IN (")
                .push_bind(available)
                .push(", ")
                .push_bind(cached)
                .push(")");
        }
    }
    builder.push(") ");
}

/// Lists the genres of the tracks in scope.
///
/// The names are the stored ones: a scan folds a tag's spelling into its
/// lowercase form, so `Ambient` and `ambient` are one genre called `ambient`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to count in.
///
/// # Returns
///
/// The genres, by name. A genre no track in scope carries is left out, so a
/// genre only the server's cached tracks have does not appear in a personal
/// listing.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn list_genres(
    conn: &mut SqliteConnection,
    scope: Scope,
) -> Result<Vec<GenreCount>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("");
    push_scope_tracks(&mut builder, scope);
    builder.push(
        "SELECT g.name AS name, count(DISTINCT t.id) AS song_count, \
                count(DISTINCT t.album_id) AS album_count \
         FROM genres g \
         JOIN track_genres tg ON tg.genre_id = g.id \
         JOIN tracks t ON t.id = tg.track_id \
         JOIN scope_tracks st ON st.track_id = t.id \
         GROUP BY g.id \
         ORDER BY g.name",
    );
    let rows = builder
        .build_query_as::<GenreCountRow>()
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| GenreCount {
            name: row.name,
            song_count: row.song_count,
            album_count: row.album_count,
        })
        .collect())
}

/// One stored genre count.
#[derive(sqlx::FromRow)]
struct GenreCountRow {
    /// The genre's name.
    name: String,
    /// How many songs carry it.
    song_count: i64,
    /// How many albums hold one of them.
    album_count: i64,
}

/// Lists a random sample of the songs in scope.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to sample from.
/// - `size`: how many songs to return.
/// - `genre`: only songs of this genre, when given. The name is compared to the
///   stored, folded form.
/// - `from_year`: the earliest release year of the song's album, when given.
/// - `to_year`: the latest release year, when given.
///
/// # Returns
///
/// The songs, in no order. A song without an album is left out when either year
/// is given, because its release year is unknown.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn random_tracks(
    conn: &mut SqliteConnection,
    scope: Scope,
    size: i64,
    genre: Option<&str>,
    from_year: Option<i64>,
    to_year: Option<i64>,
) -> Result<Vec<TrackView>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("");
    push_scope_tracks(&mut builder, scope);
    builder.push("SELECT t.id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id ");
    if genre.is_some() || from_year.is_some() || to_year.is_some() {
        builder.push("JOIN albums al ON al.id = t.album_id ");
    }
    if let Some(genre) = genre {
        builder
            .push(
                "JOIN track_genres tg ON tg.track_id = t.id \
                 JOIN genres g ON g.id = tg.genre_id AND g.name = ",
            )
            .push_bind(fold_genre(genre));
    }
    builder.push("WHERE 1 = 1");
    if let Some(from) = from_year {
        builder.push(" AND al.year >= ").push_bind(from);
    }
    if let Some(to) = to_year {
        builder.push(" AND al.year <= ").push_bind(to);
    }
    builder
        .push(" ORDER BY RANDOM() LIMIT ")
        .push_bind(size.max(0));
    let rows = builder
        .build_query_scalar::<Vec<u8>>()
        .fetch_all(&mut *conn)
        .await?;
    track_views(conn, rows).await
}

/// Lists the songs in scope that carry a genre.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `genre`: the genre to match, compared to the stored, folded form.
/// - `count`: the maximum number of songs.
/// - `offset`: how many songs to skip.
///
/// # Returns
///
/// The songs, by title and then id, which is the order a track listing uses.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn tracks_by_genre(
    conn: &mut SqliteConnection,
    scope: Scope,
    genre: &str,
    count: i64,
    offset: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("");
    push_scope_tracks(&mut builder, scope);
    builder
        .push(
            "SELECT t.id FROM tracks t \
             JOIN scope_tracks st ON st.track_id = t.id \
             JOIN track_genres tg ON tg.track_id = t.id \
             JOIN genres g ON g.id = tg.genre_id AND g.name = ",
        )
        .push_bind(fold_genre(genre))
        .push(" ORDER BY t.sort_title, t.id LIMIT ")
        .push_bind(count)
        .push(" OFFSET ")
        .push_bind(offset);
    let rows = builder
        .build_query_scalar::<Vec<u8>>()
        .fetch_all(&mut *conn)
        .await?;
    track_views(conn, rows).await
}

/// Folds a genre name the way a scan stores it.
///
/// # Parameters
///
/// - `genre`: the name a client asked for.
///
/// # Returns
///
/// The trimmed, lowercase name.
fn fold_genre(genre: &str) -> String {
    genre.trim().to_lowercase()
}

/// Loads the views of the tracks the given rows name.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `rows`: the stored track ids.
///
/// # Returns
///
/// The views, in the order of the rows.
///
/// # Errors
///
/// Returns [`LibraryError`] when a row cannot be read or an id does not decode.
async fn track_views(
    conn: &mut SqliteConnection,
    rows: Vec<Vec<u8>>,
) -> Result<Vec<TrackView>, LibraryError> {
    let mut views = Vec::with_capacity(rows.len());
    for bytes in rows {
        let id = decode_entity(IdPrefix::Track, &bytes).map_err(LibraryError::from)?;
        if let Some(track) = tracks::get_track(conn, id).await? {
            views.push(track_view(conn, track).await?);
        }
    }
    Ok(views)
}

/// Lists the most played songs that credit an artist.
///
/// The counts are the server's own: every play any user recorded, then how many
/// users liked the song, which is what makes this a *top* listing rather than a
/// personal one.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `artist_id`: the artist the songs credit, in any position.
/// - `count`: the maximum number of songs.
///
/// # Returns
///
/// The songs, most played first, then most liked, then the oldest album, disc,
/// and track; a song with no plays still appears, after the played ones.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn top_tracks_for_artist(
    conn: &mut SqliteConnection,
    scope: Scope,
    artist_id: EntityId,
    count: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let mut builder = QueryBuilder::<Sqlite>::new("");
    push_scope_tracks(&mut builder, scope);
    builder
        .push(
            "SELECT t.id FROM tracks t \
             JOIN scope_tracks st ON st.track_id = t.id \
             JOIN track_artists ta ON ta.track_id = t.id \
             LEFT JOIN albums al ON al.id = t.album_id \
             WHERE ta.artist_id = ",
        )
        .push_bind(entity_key(artist_id))
        .push(
            " ORDER BY (SELECT count(*) FROM plays p WHERE p.track_id = t.id) DESC, \
             (SELECT count(*) FROM user_feedback f WHERE f.entity_kind = 'track' \
                AND f.entity_id = t.id AND f.value = 'like') DESC, \
             al.year, t.disc_number, t.track_number, t.id LIMIT ",
        )
        .push_bind(count);
    let rows = builder
        .build_query_scalar::<Vec<u8>>()
        .fetch_all(&mut *conn)
        .await?;
    track_views(conn, rows).await
}

/// Finds an artist by name.
///
/// The name is folded the way the scanner folds it before it stores the sort
/// name, so a caller may pass the spelling a client sent.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `name`: the artist's name.
///
/// # Returns
///
/// The artist, or `None` when no artist has that name.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn find_artist_by_name(
    conn: &mut SqliteConnection,
    name: &str,
) -> Result<Option<Artist>, LibraryError> {
    let row = sqlx::query_as!(
        artists::ArtistRow,
        "SELECT id, name, sort_name, mb_artist_id, origin, created_at, updated_at \
         FROM artists WHERE sort_name = ? ORDER BY sort_name, id LIMIT 1",
        crate::sort_key(name),
    )
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| row.into_artist()).transpose()
}

/// The entities one user liked.
///
/// # Examples
///
/// ```
/// use hearthsound_library::query::Starred;
///
/// let none = Starred::default();
/// assert!(none.tracks.is_empty());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Starred {
    /// The liked artists, most recently liked first.
    pub artists: Vec<Artist>,
    /// The liked albums, most recently liked first.
    pub albums: Vec<Album>,
    /// The liked tracks, most recently liked first.
    pub tracks: Vec<TrackView>,
}

/// Lists the entities a user liked.
///
/// A like is the only thing this reads: an entity is listed whatever its
/// availability, so a liked track that is not stored still appears, and one
/// user's likes never appear in another user's listing.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose likes are read.
///
/// # Returns
///
/// The liked artists, albums, and tracks, each most recently liked first.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn starred(conn: &mut SqliteConnection, user_id: Uuid) -> Result<Starred, LibraryError> {
    let mut starred = Starred::default();
    let rows = sqlx::query_scalar!(
        "SELECT entity_id FROM user_feedback \
         WHERE user_id = ? AND value = 'like' AND entity_kind = 'artist' \
         ORDER BY at DESC, entity_id",
        user_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;
    for bytes in rows {
        let id = decode_entity(IdPrefix::Artist, &bytes).map_err(LibraryError::from)?;
        if let Some(artist) = artists::get_artist(conn, id).await? {
            starred.artists.push(artist);
        }
    }
    let rows = sqlx::query_scalar!(
        "SELECT entity_id FROM user_feedback \
         WHERE user_id = ? AND value = 'like' AND entity_kind = 'album' \
         ORDER BY at DESC, entity_id",
        user_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;
    for bytes in rows {
        let id = decode_entity(IdPrefix::Album, &bytes).map_err(LibraryError::from)?;
        if let Some(album) = albums::get_album(conn, id).await? {
            starred.albums.push(album);
        }
    }
    let rows = sqlx::query_scalar!(
        "SELECT entity_id FROM user_feedback \
         WHERE user_id = ? AND value = 'like' AND entity_kind = 'track' \
         ORDER BY at DESC, entity_id",
        user_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;
    for bytes in rows {
        let id = decode_entity(IdPrefix::Track, &bytes).map_err(LibraryError::from)?;
        if let Some(track) = tracks::get_track(conn, id).await? {
            starred.tracks.push(track_view(conn, track).await?);
        }
    }
    Ok(starred)
}

/// How many entities one account liked, by kind.
///
/// A client reads this to size the placeholders of its likes view before the
/// listing itself arrives.
///
/// # Examples
///
/// ```
/// use hearthsound_library::query::StarredCounts;
///
/// let counts = StarredCounts {
///     artists: 2,
///     albums: 3,
///     tracks: 40,
/// };
/// assert_eq!(counts.albums, 3);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StarredCounts {
    /// The liked artists.
    pub artists: i64,
    /// The liked albums.
    pub albums: i64,
    /// The liked tracks.
    pub tracks: i64,
}

/// Counts the entities a user liked.
///
/// The counts cover exactly the entities [`starred`] returns: a like whose row
/// has since been removed from the library is not counted, so the counts and the
/// listing never disagree.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose likes are counted.
///
/// # Returns
///
/// The liked artists, albums, and tracks.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn starred_counts(
    conn: &mut SqliteConnection,
    user_id: Uuid,
) -> Result<StarredCounts, LibraryError> {
    let user = user_id.as_bytes().as_slice();
    let artists: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS counted FROM user_feedback f \
         WHERE f.user_id = ? AND f.value = 'like' AND f.entity_kind = 'artist' \
           AND f.entity_id IN (SELECT id FROM artists)",
        user,
    )
    .fetch_one(&mut *conn)
    .await?;
    let albums: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS counted FROM user_feedback f \
         WHERE f.user_id = ? AND f.value = 'like' AND f.entity_kind = 'album' \
           AND f.entity_id IN (SELECT id FROM albums)",
        user,
    )
    .fetch_one(&mut *conn)
    .await?;
    let tracks: i64 = sqlx::query_scalar!(
        "SELECT count(*) AS counted FROM user_feedback f \
         WHERE f.user_id = ? AND f.value = 'like' AND f.entity_kind = 'track' \
           AND f.entity_id IN (SELECT id FROM tracks)",
        user,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(StarredCounts {
        artists,
        albums,
        tracks,
    })
}

/// Counts the media files the scanned folders hold.
///
/// A scan reports how many files it has dealt with while it runs; between
/// scans this is the number of files the library holds, which is what a client
/// shows for the library's size.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// The number of folder media files.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn count_folder_files(conn: &mut SqliteConnection) -> Result<i64, LibraryError> {
    let counted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_files WHERE storage = 'folder'")
            .fetch_one(&mut *conn)
            .await?;
    Ok(counted)
}

/// Returns when the library folders were last scanned.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// The latest `last_scan_at` of the folders, in unix milliseconds, or `None`
/// when no folder has been scanned.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn last_scan(conn: &mut SqliteConnection) -> Result<Option<i64>, LibraryError> {
    let scanned: Option<i64> = sqlx::query_scalar("SELECT max(last_scan_at) FROM library_folders")
        .fetch_one(&mut *conn)
        .await?;
    Ok(scanned)
}

/// Counts the library folders.
///
/// The folders a scan walks are the real ones, not the two virtual music
/// folders a Subsonic client browses.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// The number of library folders.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn count_folders(conn: &mut SqliteConnection) -> Result<i64, LibraryError> {
    let counted: i64 = sqlx::query_scalar("SELECT count(*) FROM library_folders")
        .fetch_one(&mut *conn)
        .await?;
    Ok(counted)
}

/// Returns when the browse index last changed.
///
/// A client that remembers this instant can ask for the indexes and be told
/// nothing has changed since, which is what `getIndexes`' `ifModifiedSince`
/// does.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
///
/// # Returns
///
/// The latest of the artists', albums', and tracks' `updated_at` and the
/// folders' `last_scan_at`, in unix milliseconds, or `0` when the library is
/// empty.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
pub async fn library_last_modified(conn: &mut SqliteConnection) -> Result<i64, LibraryError> {
    let modified: Option<i64> = sqlx::query_scalar(
        "SELECT max(updated_at) FROM ( \
             SELECT max(updated_at) AS updated_at FROM artists \
             UNION ALL SELECT max(updated_at) FROM albums \
             UNION ALL SELECT max(updated_at) FROM tracks \
             UNION ALL SELECT max(last_scan_at) FROM library_folders \
         )",
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(modified.unwrap_or(0))
}

/// Lists the albums in scope that an artist is credited on.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `artist_id`: the artist to list albums for.
///
/// # Returns
///
/// The albums, oldest first, with the sort title as a tie breaker.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn list_albums_for_artist(
    conn: &mut SqliteConnection,
    scope: Scope,
    artist_id: EntityId,
) -> Result<Vec<Album>, LibraryError> {
    let rows = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_as!(
                albums::AlbumRow,
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT al.id, al.title, al.sort_title, al.display_artist, al.year, al.release_date, \
                        al.album_type, al.mb_release_id, al.mb_release_group_id, al.upc, al.origin, \
                        al.created_at, al.updated_at \
                 FROM albums al JOIN album_artists aa ON aa.album_id = al.id \
                 WHERE aa.artist_id = ? \
                   AND (al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) \
                     OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'album' AND entity_id = al.id) \
                     OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'album' \
                                  AND value = 'like' AND entity_id = al.id)) \
                 ORDER BY al.year IS NULL, al.year, al.sort_title, al.id",
                user,
                user,
                user,
                user,
                entity_key(artist_id),
                user,
                user,
            )
            .fetch_all(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(Vec::new());
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_as!(
                albums::AlbumRow,
                "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                 SELECT al.id, al.title, al.sort_title, al.display_artist, al.year, al.release_date, \
                        al.album_type, al.mb_release_id, al.mb_release_group_id, al.upc, al.origin, \
                        al.created_at, al.updated_at \
                 FROM albums al JOIN album_artists aa ON aa.album_id = al.id \
                 WHERE aa.artist_id = ? \
                   AND al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) \
                 ORDER BY al.year IS NULL, al.year, al.sort_title, al.id",
                available,
                cached,
                entity_key(artist_id),
            )
            .fetch_all(&mut *conn)
            .await?
        }
    };
    rows.into_iter().map(|row| row.into_album()).collect()
}

/// Lists the tracks in scope.
///
/// This is the listing an empty `search3` query answers with: the OpenSubsonic
/// protocol requires a server to return all its data for an empty query, and an
/// FTS5 query with no token matches nothing, so the browse layer lists the
/// scope directly instead. The tracks are ordered by sort title, with the id as
/// a tie breaker, so the listing is stable across pages.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `limit`: the maximum number of tracks; a negative limit is unbounded.
/// - `offset`: how many tracks to skip.
///
/// # Returns
///
/// The tracks in scope, each with its album, credited artists, genres, and
/// preferred file.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn list_tracks(
    conn: &mut SqliteConnection,
    scope: Scope,
    limit: i64,
    offset: i64,
) -> Result<Vec<TrackView>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let rows: Vec<Vec<u8>> = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT t.id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id \
                 ORDER BY t.sort_title, t.id LIMIT ? OFFSET ?",
                user,
                user,
                user,
                user,
                limit,
                offset,
            )
            .fetch_all(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "SELECT t.id FROM tracks t WHERE t.availability IN (?, ?) \
                 ORDER BY t.sort_title, t.id LIMIT ? OFFSET ?",
                available,
                cached,
                limit,
                offset,
            )
            .fetch_all(&mut *conn)
            .await?
        }
    };
    track_views(conn, rows).await
}

/// Counts the tracks in scope.
///
/// The count covers exactly the tracks [`list_tracks`] returns for a listing
/// that narrows nothing, which is what the songs view and an empty `search3`
/// read. It applies the same scope and drops only the `ORDER BY`, `LIMIT` and
/// `OFFSET`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
///
/// # Returns
///
/// The number of tracks in the scope.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn count_tracks(conn: &mut SqliteConnection, scope: Scope) -> Result<i64, LibraryError> {
    let counted: i64 = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT count(*) AS counted FROM tracks t JOIN scope_tracks st ON st.track_id = t.id",
                user,
                user,
                user,
                user,
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(0);
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "SELECT count(*) AS counted FROM tracks t WHERE t.availability IN (?, ?)",
                available,
                cached,
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(counted)
}

/// One track of a collection queue.
///
/// The row carries everything a client needs to enqueue the track and draw it
/// without a second request: the title, the credited artist, the album, the
/// length, the availability, and the caller's rating and feedback.
///
/// # Examples
///
/// ```
/// use hearthsound_core::{Availability, EntityId, IdPrefix};
/// use hearthsound_library::query::QueueItem;
///
/// let item = QueueItem {
///     track_id: EntityId::new(IdPrefix::Track),
///     title: "Example".to_owned(),
///     artist: None,
///     album_id: None,
///     artist_id: None,
///     album: None,
///     duration_ms: Some(1_000),
///     availability: Availability::Local,
///     rating: None,
///     liked: false,
///     disliked: false,
/// };
/// assert_eq!(item.duration_ms, Some(1_000));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    /// The track's client id.
    pub track_id: EntityId,
    /// The track's title.
    pub title: String,
    /// The credited artists, comma separated, or the display artist the tags
    /// carried; `None` when neither is known.
    pub artist: Option<String>,
    /// The album the track belongs to, when it has one.
    pub album_id: Option<EntityId>,
    /// The primary credited artist, the one in credit position zero, when the
    /// track has a credit.
    pub artist_id: Option<EntityId>,
    /// The album's title, when the track belongs to one.
    pub album: Option<String>,
    /// The track's length in milliseconds, when it is known.
    pub duration_ms: Option<i64>,
    /// The track's availability on this server.
    pub availability: Availability,
    /// The caller's rating, from 1 to 5; `None` when the track is unrated.
    pub rating: Option<i64>,
    /// Whether the caller liked the track.
    pub liked: bool,
    /// Whether the caller disliked the track.
    pub disliked: bool,
}

/// The collection a queue is read for.
///
/// Every variant names the caller's own view: the personal library, the
/// caller's likes and uploads, a playlist the caller may see, or an album and
/// an artist inside the caller's library. The server scope is not offered here,
/// so a queue can never include a track the caller's library does not hold
/// (except the remaining tracks of an album the caller's library contains,
/// which every client may already stream).
///
/// # Examples
///
/// ```
/// use hearthsound_core::{EntityId, IdPrefix};
/// use hearthsound_library::query::QueueCollection;
///
/// let album = EntityId::new(IdPrefix::Album);
/// assert_ne!(QueueCollection::Album(album), QueueCollection::Artist(album));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueCollection {
    /// The caller's whole personal library, in the order the Songs view uses.
    Songs,
    /// The tracks the caller liked, most recently liked first.
    Liked,
    /// The tracks the caller uploaded, newest first.
    Uploads,
    /// One playlist the caller owns or that is public, in its own order.
    Playlist(EntityId),
    /// One album inside the caller's library, in disc and track order.
    Album(EntityId),
    /// One artist's tracks: the tracks with plays first, by play count, then
    /// the rest in album order.
    Artist(EntityId),
    /// One artist's played tracks only, by play count.
    ArtistTop(EntityId),
}

/// Lists every track of one collection, in the collection's own order.
///
/// The rows are read in one statement, after a one-row visibility check for a
/// playlist, album or artist: they carry the album, the artist, the
/// availability and the caller's annotations, so a queue never costs a query
/// per track. The scope is the caller's personal library, exactly as
/// [`list_tracks`] defines it, so another user's playlist and unowned folder
/// content never appear.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose library, likes, uploads and annotations are
///   read.
/// - `collection`: the collection to read.
///
/// # Returns
///
/// The tracks in the collection's order, or `None` when the collection is a
/// playlist, album or artist the caller may not see.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails or a stored literal or id is
/// malformed.
pub async fn queue(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    collection: QueueCollection,
) -> Result<Option<Vec<QueueItem>>, LibraryError> {
    let user = user_id.as_bytes().to_vec();
    let visible = match collection {
        QueueCollection::Songs | QueueCollection::Liked | QueueCollection::Uploads => true,
        QueueCollection::Playlist(id) => playlist_visible(conn, user_id, id).await?,
        QueueCollection::Album(id) => album_in_scope(conn, Scope::Personal { user_id }, id).await?,
        QueueCollection::Artist(id) | QueueCollection::ArtistTop(id) => {
            artist_in_scope(conn, Scope::Personal { user_id }, id).await?
        }
    };
    if !visible {
        return Ok(None);
    }

    // The projection and the personal scope are shared by every source and
    // bind the user id, so the statement is assembled rather than written as
    // one compile-checked literal; `top_tracks_for_artist` builds its query the
    // same way for the same reason.
    let mut builder = QueryBuilder::<Sqlite>::new("");
    match collection {
        QueueCollection::Songs => {
            push_personal_scope(&mut builder, &user);
            push_queue_projection(&mut builder, &user);
            builder.push(
                " FROM tracks t JOIN scope_tracks st ON st.track_id = t.id \
                 LEFT JOIN albums al ON al.id = t.album_id \
                 ORDER BY t.sort_title, t.id",
            );
        }
        QueueCollection::Liked => {
            push_queue_projection(&mut builder, &user);
            builder
                .push(
                    " FROM user_feedback f JOIN tracks t ON t.id = f.entity_id \
                     LEFT JOIN albums al ON al.id = t.album_id \
                     WHERE f.user_id = ",
                )
                .push_bind(user.clone())
                .push(
                    " AND f.entity_kind = 'track' AND f.value = 'like' \
                     ORDER BY f.at DESC, f.entity_id",
                );
        }
        QueueCollection::Uploads => {
            push_queue_projection(&mut builder, &user);
            builder
                .push(
                    " FROM user_library ul JOIN tracks t ON t.id = ul.entity_id \
                     LEFT JOIN albums al ON al.id = t.album_id \
                     WHERE ul.user_id = ",
                )
                .push_bind(user.clone())
                .push(
                    " AND ul.entity_kind = 'track' AND ul.origin = 'upload' \
                     ORDER BY ul.added_at DESC, ul.entity_id ASC",
                );
        }
        QueueCollection::Playlist(playlist_id) => {
            push_queue_projection(&mut builder, &user);
            builder
                .push(
                    " FROM playlist_tracks pt JOIN tracks t ON t.id = pt.track_id \
                     LEFT JOIN albums al ON al.id = t.album_id \
                     WHERE pt.playlist_id = ",
                )
                .push_bind(entity_key(playlist_id))
                .push(" ORDER BY pt.position");
        }
        QueueCollection::Album(album_id) => {
            push_queue_projection(&mut builder, &user);
            builder
                .push(
                    " FROM tracks t LEFT JOIN albums al ON al.id = t.album_id \
                     WHERE t.album_id = ",
                )
                .push_bind(entity_key(album_id))
                .push(
                    " ORDER BY t.disc_number IS NULL, t.disc_number, \
                     t.track_number IS NULL, t.track_number, t.sort_title, t.id",
                );
        }
        QueueCollection::Artist(artist_id) | QueueCollection::ArtistTop(artist_id) => {
            push_personal_scope(&mut builder, &user);
            push_queue_projection(&mut builder, &user);
            builder
                .push(
                    " FROM tracks t JOIN scope_tracks st ON st.track_id = t.id \
                     LEFT JOIN albums al ON al.id = t.album_id \
                     WHERE t.id IN (SELECT track_id FROM track_artists WHERE artist_id = ",
                )
                .push_bind(entity_key(artist_id))
                .push(")");
            if matches!(collection, QueueCollection::ArtistTop(_)) {
                builder.push(" AND EXISTS (SELECT 1 FROM plays p WHERE p.track_id = t.id)");
            }
            builder.push(
                " ORDER BY (SELECT count(*) FROM plays p WHERE p.track_id = t.id) DESC, \
                 al.year, t.disc_number, t.track_number, t.id",
            );
        }
    }

    let rows = builder
        .build_query_as::<QueueRow>()
        .fetch_all(&mut *conn)
        .await?;
    let items = rows
        .into_iter()
        .map(QueueRow::into_item)
        .collect::<Result<Vec<_>, LibraryError>>()?;
    Ok(Some(items))
}

/// Appends the columns every queue row projects.
///
/// The caller writes the statement up to `SELECT` (and, for the scopes that
/// need one, its `WITH` clause); this writes the select list and the three
/// binds of the caller's annotations. The album's title is joined as `al` by
/// every source.
///
/// # Parameters
///
/// - `builder`: the builder to append to.
/// - `user`: the 16 raw bytes of the account whose annotations are read.
fn push_queue_projection(builder: &mut QueryBuilder<Sqlite>, user: &[u8]) {
    builder
        .push(
            "SELECT t.id AS id, t.title AS title, \
             COALESCE(NULLIF(t.display_artist, ''), \
                 (SELECT group_concat(ar.name, ', ') FROM track_artists ta \
                  JOIN artists ar ON ar.id = ta.artist_id WHERE ta.track_id = t.id)) AS artist, \
             t.album_id AS album_id, al.title AS album, t.duration_ms AS duration_ms, \
             (SELECT ta.artist_id FROM track_artists ta WHERE ta.track_id = t.id \
              ORDER BY ta.position LIMIT 1) AS artist_id, \
             t.availability AS availability, \
             (SELECT r.rating FROM user_ratings r WHERE r.user_id = ",
        )
        .push_bind(user.to_vec())
        .push(
            " AND r.entity_kind = 'track' AND r.entity_id = t.id) AS rating, \
             EXISTS (SELECT 1 FROM user_feedback f WHERE f.user_id = ",
        )
        .push_bind(user.to_vec())
        .push(
            " AND f.entity_kind = 'track' AND f.value = 'like' \
             AND f.entity_id = t.id) AS liked, \
             EXISTS (SELECT 1 FROM user_feedback f WHERE f.user_id = ",
        )
        .push_bind(user.to_vec())
        .push(
            " AND f.entity_kind = 'track' AND f.value = 'dislike' \
             AND f.entity_id = t.id) AS disliked",
        );
}

/// Reports whether an account may see a playlist.
///
/// The rule is the one playlist listings apply: the owner sees it, and
/// everybody sees a public one.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account reading the playlist.
/// - `playlist_id`: the playlist to test.
///
/// # Returns
///
/// `true` when the playlist exists and the account may see it.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails.
async fn playlist_visible(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    playlist_id: EntityId,
) -> Result<bool, LibraryError> {
    let visible: i64 = sqlx::query_scalar!(
        "SELECT EXISTS (SELECT 1 FROM playlists WHERE id = ? \
                        AND (owner_user_id = ? OR is_public = 1))",
        entity_key(playlist_id),
        user_id.as_bytes().as_slice(),
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(visible != 0)
}

/// One stored queue row.
#[derive(sqlx::FromRow)]
struct QueueRow {
    /// The track's primary key.
    id: Vec<u8>,
    /// The track's title.
    title: String,
    /// The credited artists, comma separated.
    artist: Option<String>,
    /// The album's primary key, when the track has an album.
    album_id: Option<Vec<u8>>,
    /// The album's title, when the track has an album.
    album: Option<String>,
    /// The primary artist's primary key, when the track has a credit.
    artist_id: Option<Vec<u8>>,
    /// The track's length in milliseconds.
    duration_ms: Option<i64>,
    /// The stored availability literal.
    availability: String,
    /// The caller's rating, when the track is rated.
    rating: Option<i64>,
    /// `1` when the caller liked the track.
    liked: i64,
    /// `1` when the caller disliked the track.
    disliked: i64,
}

impl QueueRow {
    /// Converts a stored row into a queue item.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when a stored id is not a UUID or the
    /// availability literal is unknown.
    fn into_item(self) -> Result<QueueItem, LibraryError> {
        let availability = self.availability.parse::<Availability>().map_err(|_| {
            LibraryError::UnknownLiteral {
                kind: "availability",
                value: self.availability,
            }
        })?;
        let album_id = self
            .album_id
            .as_deref()
            .map(|bytes| decode_entity(IdPrefix::Album, bytes))
            .transpose()
            .map_err(LibraryError::from)?;
        let artist_id = self
            .artist_id
            .as_deref()
            .map(|bytes| decode_entity(IdPrefix::Artist, bytes))
            .transpose()
            .map_err(LibraryError::from)?;
        Ok(QueueItem {
            track_id: decode_entity(IdPrefix::Track, &self.id).map_err(LibraryError::from)?,
            title: self.title,
            artist: self.artist.filter(|name| !name.is_empty()),
            album_id,
            artist_id,
            album: self.album,
            duration_ms: self.duration_ms,
            availability,
            rating: self.rating,
            liked: self.liked != 0,
            disliked: self.disliked != 0,
        })
    }
}

/// Searches the local full-text index.
///
/// The query text is sanitized into an FTS5 prefix query by [`sanitize_match`],
/// and each requested entity kind is searched separately, so the caller gets the
/// three lists a `search3` response needs. Only entities the scope contains are
/// returned.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `text`: the raw search text.
/// - `kinds`: which of [`EntityKind::Artist`], [`EntityKind::Album`], and
///   [`EntityKind::Track`] to search; a kind outside those three is ignored.
/// - `limit`: the maximum number of results per kind.
/// - `offset`: how many results per kind to skip.
///
/// # Returns
///
/// The matching artists, albums, and tracks, each in relevance order.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn search(
    conn: &mut SqliteConnection,
    scope: Scope,
    text: &str,
    kinds: &[EntityKind],
    limit: i64,
    offset: i64,
) -> Result<SearchResults, LibraryError> {
    let Some(matched) = sanitize_match(text) else {
        return Ok(SearchResults::default());
    };
    let mut results = SearchResults::default();
    if kinds.contains(&EntityKind::Artist) {
        for id in search_kind(conn, scope, &matched, EntityKind::Artist, limit, offset).await? {
            if let Some(artist) = artists::get_artist(conn, id).await? {
                results.artists.push(artist);
            }
        }
    }
    if kinds.contains(&EntityKind::Album) {
        for id in search_kind(conn, scope, &matched, EntityKind::Album, limit, offset).await? {
            if let Some(album) = albums::get_album(conn, id).await? {
                results.albums.push(album);
            }
        }
    }
    if kinds.contains(&EntityKind::Track) {
        for id in search_kind(conn, scope, &matched, EntityKind::Track, limit, offset).await? {
            if let Some(track) = tracks::get_track(conn, id).await? {
                results.tracks.push(track_view(conn, track).await?);
            }
        }
    }
    Ok(results)
}

/// Reads the track's preferred file with its absolute path.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `roots`: the managed storage roots a `library` or `cache` file resolves
///   against.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The preferred file and the absolute path it resolves to, or `None` when the
/// track has no file.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn preferred_media_file(
    conn: &mut SqliteConnection,
    roots: &StorageRoots,
    track_id: EntityId,
) -> Result<Option<PreferredFile>, LibraryError> {
    let Some(media_file) = media_files::preferred_media_file(conn, track_id).await? else {
        return Ok(None);
    };
    let folder_path = match (media_file.storage, media_file.folder_id) {
        (StorageClass::Folder, Some(folder_id)) => folders::get_library_folder(conn, folder_id)
            .await?
            .map(|folder| folder.path),
        _ => None,
    };
    let absolute_path = media_file.absolute_path(folder_path.as_deref(), roots);
    Ok(Some(PreferredFile {
        media_file,
        absolute_path,
    }))
}

/// Records that a media file was just streamed.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `media_file_id`: the file that was served.
///
/// # Returns
///
/// `true` when a row was updated, `false` when the file does not exist.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the update fails.
pub async fn touch_last_accessed(
    conn: &mut SqliteConnection,
    media_file_id: Uuid,
) -> Result<bool, LibraryError> {
    let affected = sqlx::query!(
        "UPDATE media_files SET last_accessed_at = ? WHERE id = ?",
        crate::now_millis(),
        media_file_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

/// One artist row with the number of albums in scope.
#[derive(sqlx::FromRow)]
struct ArtistSummaryRow {
    id: Vec<u8>,
    name: String,
    sort_name: String,
    mb_artist_id: Option<String>,
    origin: String,
    created_at: i64,
    updated_at: i64,
    album_count: i64,
}

impl ArtistSummaryRow {
    /// Converts a stored row into an artist.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError`] when the primary key or the origin literal is
    /// malformed.
    fn into_artist(self) -> Result<Artist, LibraryError> {
        Ok(Artist {
            id: decode_entity(IdPrefix::Artist, &self.id)?,
            name: self.name,
            sort_name: self.sort_name,
            mb_artist_id: self.mb_artist_id,
            origin: self.origin.parse()?,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Builds the view of one track.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track`: the track row.
///
/// # Returns
///
/// The track with its album, artists, genres, and preferred file.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn track_view(conn: &mut SqliteConnection, track: Track) -> Result<TrackView, LibraryError> {
    let album = match track.album_id {
        Some(album_id) => albums::get_album(conn, album_id).await?,
        None => None,
    };
    track_view_in_album(conn, track, album).await
}

/// Builds the view of one track whose album is already known.
///
/// A caller that reads an album with all its tracks has the album row in hand,
/// so passing it down saves one row read per track: an album of twenty tracks
/// reads its own row once instead of twenty-one times.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track`: the track row.
/// - `album`: the track's album, or `None` when it has none.
///
/// # Returns
///
/// The track with its album, artists, genres, and preferred file.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn track_view_in_album(
    conn: &mut SqliteConnection,
    track: Track,
    album: Option<Album>,
) -> Result<TrackView, LibraryError> {
    let credited = artists::list_track_artists(conn, track.id).await?;
    let genres = tracks::list_track_genres(conn, track.id)
        .await?
        .into_iter()
        .map(|genre| genre.name)
        .collect();
    let file = media_files::preferred_media_file(conn, track.id)
        .await?
        .map(|file| projection(&file, track.duration_ms));
    Ok(TrackView {
        track,
        album,
        artists: credited,
        genres,
        file,
    })
}

/// Projects the fields of a media file a browse response needs.
///
/// # Parameters
///
/// - `file`: the media file row.
/// - `track_duration_ms`: the track's duration, used when the file records none.
///
/// # Returns
///
/// The projection, whose `path` is the file's path relative to its root.
fn projection(file: &MediaFile, track_duration_ms: Option<i64>) -> MediaFileProjection {
    let mut suffix = file.suffix();
    if suffix.is_empty() {
        suffix = file.format.clone().unwrap_or_default();
    }
    MediaFileProjection {
        content_type: content_type_for(&suffix).to_owned(),
        suffix,
        size: file.size,
        bitrate_kbps: file.bitrate_kbps,
        duration_ms: file.duration_ms.or(track_duration_ms),
        path: file.rel_path.clone(),
    }
}

/// Runs the full-text query for one entity kind.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to filter by.
/// - `matched`: the sanitized FTS5 query.
/// - `kind`: the entity kind to search.
/// - `limit`: the maximum number of results.
/// - `offset`: how many results to skip.
///
/// # Returns
///
/// The matching entity ids, in relevance order.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn search_kind(
    conn: &mut SqliteConnection,
    scope: Scope,
    matched: &str,
    kind: EntityKind,
    limit: i64,
    offset: i64,
) -> Result<Vec<EntityId>, LibraryError> {
    if let Scope::Server(ServerBrowse::Disabled) = scope {
        return Ok(Vec::new());
    }
    let prefix = match kind {
        EntityKind::Track => IdPrefix::Track,
        EntityKind::Album => IdPrefix::Album,
        EntityKind::Artist => IdPrefix::Artist,
        // Playlists are not in the local index.
        EntityKind::Playlist => return Ok(Vec::new()),
    };
    let rows: Vec<Vec<u8>> = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            match kind {
                EntityKind::Track => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS ( \
                         SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                         UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                         UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                             WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                         UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                             WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                     ) \
                     SELECT t.id FROM search_fts JOIN tracks t ON t.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 1 \
                       AND t.id IN (SELECT track_id FROM scope_tracks) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    user, user, user, user, matched, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Album => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS ( \
                         SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                         UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                         UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                             WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                         UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                             WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                     ) \
                     SELECT al.id FROM search_fts JOIN albums al ON al.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 2 \
                       AND (al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) \
                         OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'album' AND entity_id = al.id) \
                         OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'album' \
                                      AND value = 'like' AND entity_id = al.id)) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    user, user, user, user, matched, user, user, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Artist => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS ( \
                         SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                         UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                         UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                             WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                         UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                             WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                     ) \
                     SELECT ar.id FROM search_fts JOIN artists ar ON ar.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 3 \
                       AND (ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                        JOIN scope_tracks st ON st.track_id = ta.track_id) \
                         OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                                SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)) \
                         OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'artist' \
                                      AND entity_id = ar.id) \
                         OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'artist' \
                                      AND value = 'like' AND entity_id = ar.id)) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    user, user, user, user, matched, user, user, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Playlist => Vec::new(),
            }
        }
        Scope::Server(view) => {
            let (available, cached) = view.availability_pair();
            match kind {
                EntityKind::Track => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                     SELECT t.id FROM search_fts JOIN tracks t ON t.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 1 \
                       AND t.id IN (SELECT track_id FROM scope_tracks) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    available, cached, matched, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Album => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                     SELECT al.id FROM search_fts JOIN albums al ON al.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 2 \
                       AND al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    available, cached, matched, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Artist => sqlx::query_scalar!(
                    "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                     SELECT ar.id FROM search_fts JOIN artists ar ON ar.rowid = search_fts.rowid / 4 \
                     WHERE search_fts MATCH ? AND search_fts.rowid % 4 = 3 \
                       AND (ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                        JOIN scope_tracks st ON st.track_id = ta.track_id) \
                         OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                                SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id))) \
                     ORDER BY rank LIMIT ? OFFSET ?",
                    available, cached, matched, limit, offset,
                )
                .fetch_all(&mut *conn)
                .await?,
                EntityKind::Playlist => Vec::new(),
            }
        }
    };
    rows.into_iter()
        .map(|bytes| decode_entity(prefix, &bytes).map_err(LibraryError::from))
        .collect()
}

/// Reports whether a track is inside a scope.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to test.
/// - `track_id`: the track to test.
///
/// # Returns
///
/// `true` when the scope contains the track.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn track_in_scope(
    conn: &mut SqliteConnection,
    scope: Scope,
    track_id: EntityId,
) -> Result<bool, LibraryError> {
    let present: i64 = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT EXISTS (SELECT 1 FROM scope_tracks WHERE track_id = ?)",
                user,
                user,
                user,
                user,
                entity_key(track_id),
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(false);
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                 SELECT EXISTS (SELECT 1 FROM scope_tracks WHERE track_id = ?)",
                available,
                cached,
                entity_key(track_id),
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(present != 0)
}

/// Reports whether an album is inside a scope.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to test.
/// - `album_id`: the album to test.
///
/// # Returns
///
/// `true` when the scope contains the album.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn album_in_scope(
    conn: &mut SqliteConnection,
    scope: Scope,
    album_id: EntityId,
) -> Result<bool, LibraryError> {
    let present: i64 = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT EXISTS ( \
                     SELECT 1 FROM albums al WHERE al.id = ? \
                       AND (al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id) \
                         OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'album' AND entity_id = al.id) \
                         OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'album' \
                                      AND value = 'like' AND entity_id = al.id)))",
                user,
                user,
                user,
                user,
                entity_key(album_id),
                user,
                user,
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(false);
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                 SELECT EXISTS ( \
                     SELECT 1 FROM albums al WHERE al.id = ? \
                       AND al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id))",
                available,
                cached,
                entity_key(album_id),
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(present != 0)
}

/// Reports whether an artist is inside a scope.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `scope`: the scope to test.
/// - `artist_id`: the artist to test.
///
/// # Returns
///
/// `true` when the scope contains the artist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
async fn artist_in_scope(
    conn: &mut SqliteConnection,
    scope: Scope,
    artist_id: EntityId,
) -> Result<bool, LibraryError> {
    let present: i64 = match scope {
        Scope::Personal { user_id } => {
            let user = user_id.as_bytes().as_slice();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS ( \
                     SELECT entity_id FROM user_library WHERE user_id = ? AND entity_kind = 'track' \
                     UNION SELECT entity_id FROM user_feedback WHERE user_id = ? AND entity_kind = 'track' AND value = 'like' \
                     UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id \
                         WHERE p.owner_user_id = ? AND p.is_temporary = 0 \
                     UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id \
                         WHERE mf.storage = 'folder' AND lf.owner_user_id = ? \
                 ) \
                 SELECT EXISTS ( \
                     SELECT 1 FROM artists ar WHERE ar.id = ? \
                       AND (ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                        JOIN scope_tracks st ON st.track_id = ta.track_id) \
                         OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                                SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)) \
                         OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ? AND entity_kind = 'artist' \
                                      AND entity_id = ar.id) \
                         OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ? AND entity_kind = 'artist' \
                                      AND value = 'like' AND entity_id = ar.id)))",
                user,
                user,
                user,
                user,
                entity_key(artist_id),
                user,
                user,
            )
            .fetch_one(&mut *conn)
            .await?
        }
        Scope::Server(view) => {
            if view == ServerBrowse::Disabled {
                return Ok(false);
            }
            let (available, cached) = view.availability_pair();
            sqlx::query_scalar!(
                "WITH scope_tracks(track_id) AS (SELECT id FROM tracks WHERE availability IN (?, ?)) \
                 SELECT EXISTS ( \
                     SELECT 1 FROM artists ar WHERE ar.id = ? \
                       AND (ar.id IN (SELECT ta.artist_id FROM track_artists ta \
                                        JOIN scope_tracks st ON st.track_id = ta.track_id) \
                         OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN ( \
                                SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id))))",
                available,
                cached,
                entity_key(artist_id),
            )
            .fetch_one(&mut *conn)
            .await?
        }
    };
    Ok(present != 0)
}

/// How many ids one batch statement binds.
///
/// Batch reads chunk their ids at this size, which keeps every statement well
/// under SQLite's bound-parameter limit.
pub const MAX_IDS_PER_QUERY: usize = 500;

/// Reports which of the given tracks are inside a user's personal library.
///
/// The membership test is the one [`track_in_scope`] applies to a single track,
/// answered for a list of tracks in chunks of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose library is tested.
/// - `track_ids`: the tracks to test.
///
/// # Returns
///
/// The tracks of `track_ids` that the library contains.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn personal_track_ids(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    track_ids: &[EntityId],
) -> Result<HashSet<EntityId>, LibraryError> {
    let user = user_id.as_bytes().to_vec();
    let mut found = HashSet::new();
    for chunk in track_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_personal_scope(&mut builder, &user);
        builder.push("SELECT track_id FROM scope_tracks WHERE track_id IN (");
        push_key_list(&mut builder, chunk);
        let keys: Vec<Vec<u8>> = builder.build_query_scalar().fetch_all(&mut *conn).await?;
        found.extend(matched_ids(chunk, keys));
    }
    Ok(found)
}

/// One track an account uploaded into its own library.
///
/// This is the row the uploads listing reports: the track, the album it landed
/// in when it has one, and when the upload was added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedTrack {
    /// The track's id.
    pub track_id: EntityId,
    /// The track's title.
    pub title: String,
    /// The credited artist text, when the file carried one.
    pub artist: Option<String>,
    /// The album's id, when the track belongs to one.
    pub album_id: Option<EntityId>,
    /// The album's title, when the track belongs to one.
    pub album: Option<String>,
    /// The track's length in milliseconds, when it is known.
    pub duration_ms: Option<i64>,
    /// When the upload was added, in unix milliseconds.
    pub added_at: i64,
}

/// Lists the tracks one account uploaded, newest first.
///
/// Only uploads count: a track that entered the library by being saved or liked
/// carries another `origin` and is not listed here. The order is `added_at`
/// descending, with the track id ascending as the tie-breaker, which is the
/// order a cursor over that pair walks.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose uploads are listed.
///
/// # Returns
///
/// Every uploaded track of the account, newest first.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails and [`LibraryError::CorruptId`]
/// when a stored identifier is not a UUID.
pub async fn uploaded_tracks(
    conn: &mut SqliteConnection,
    user_id: Uuid,
) -> Result<Vec<UploadedTrack>, LibraryError> {
    let rows = sqlx::query!(
        "SELECT ul.entity_id AS track_id, t.title AS title, t.display_artist AS artist, \
                t.album_id AS album_id, al.title AS album, t.duration_ms AS duration_ms, \
                ul.added_at AS added_at \
         FROM user_library ul \
         JOIN tracks t ON t.id = ul.entity_id \
         LEFT JOIN albums al ON al.id = t.album_id \
         WHERE ul.user_id = ? AND ul.entity_kind = 'track' AND ul.origin = 'upload' \
         ORDER BY ul.added_at DESC, ul.entity_id ASC",
        user_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(UploadedTrack {
                track_id: decode_entity(IdPrefix::Track, &row.track_id)?,
                title: row.title,
                artist: row.artist,
                album_id: row
                    .album_id
                    .as_deref()
                    .map(|bytes| decode_entity(IdPrefix::Album, bytes))
                    .transpose()?,
                album: row.album,
                duration_ms: row.duration_ms,
                added_at: row.added_at,
            })
        })
        .collect()
}

/// Reports which of the given albums are inside a user's personal library.
///
/// An album is in the library when it holds a library track, is saved, or is
/// liked, as [`album_in_scope`] decides for one album.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose library is tested.
/// - `album_ids`: the albums to test.
///
/// # Returns
///
/// The albums of `album_ids` that the library contains.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn personal_album_ids(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    album_ids: &[EntityId],
) -> Result<HashSet<EntityId>, LibraryError> {
    let user = user_id.as_bytes().to_vec();
    let mut found = HashSet::new();
    for chunk in album_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_personal_scope(&mut builder, &user);
        builder.push("SELECT al.id FROM albums al WHERE al.id IN (");
        push_key_list(&mut builder, chunk);
        builder
            .push(" AND (al.id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id)")
            .push(" OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ")
            .push_bind(user.clone())
            .push(" AND entity_kind = 'album' AND entity_id = al.id)")
            .push(" OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ")
            .push_bind(user.clone())
            .push(" AND entity_kind = 'album' AND value = 'like' AND entity_id = al.id))");
        let keys: Vec<Vec<u8>> = builder.build_query_scalar().fetch_all(&mut *conn).await?;
        found.extend(matched_ids(chunk, keys));
    }
    Ok(found)
}

/// Reports which of the given artists are inside a user's personal library.
///
/// An artist is in the library when it is saved or liked, is credited on a
/// library track, or is credited on an album the library contains, as
/// [`artist_in_scope`] decides for one artist.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose library is tested.
/// - `artist_ids`: the artists to test.
///
/// # Returns
///
/// The artists of `artist_ids` that the library contains.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn personal_artist_ids(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    artist_ids: &[EntityId],
) -> Result<HashSet<EntityId>, LibraryError> {
    let user = user_id.as_bytes().to_vec();
    let mut found = HashSet::new();
    for chunk in artist_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new("");
        push_personal_scope(&mut builder, &user);
        builder.push("SELECT ar.id FROM artists ar WHERE ar.id IN (");
        push_key_list(&mut builder, chunk);
        builder
            .push(" AND (ar.id IN (SELECT ta.artist_id FROM track_artists ta JOIN scope_tracks st ON st.track_id = ta.track_id)")
            .push(" OR ar.id IN (SELECT aa.artist_id FROM album_artists aa WHERE aa.album_id IN (SELECT t.album_id FROM tracks t JOIN scope_tracks st ON st.track_id = t.id))")
            .push(" OR EXISTS (SELECT 1 FROM user_library WHERE user_id = ")
            .push_bind(user.clone())
            .push(" AND entity_kind = 'artist' AND entity_id = ar.id)")
            .push(" OR EXISTS (SELECT 1 FROM user_feedback WHERE user_id = ")
            .push_bind(user.clone())
            .push(" AND entity_kind = 'artist' AND value = 'like' AND entity_id = ar.id))");
        let keys: Vec<Vec<u8>> = builder.build_query_scalar().fetch_all(&mut *conn).await?;
        found.extend(matched_ids(chunk, keys));
    }
    Ok(found)
}

/// Reads the stored availability of the given tracks.
///
/// The read is chunked in batches of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_ids`: the tracks to read.
///
/// # Returns
///
/// The availability of each requested track that is stored. A track that is
/// not stored is absent from the map.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails or a stored literal is unknown.
pub async fn track_availabilities(
    conn: &mut SqliteConnection,
    track_ids: &[EntityId],
) -> Result<HashMap<EntityId, Availability>, LibraryError> {
    let mut found = HashMap::new();
    for chunk in track_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder =
            QueryBuilder::<Sqlite>::new("SELECT id, availability FROM tracks WHERE id IN (");
        push_key_list(&mut builder, chunk);
        let rows: Vec<(Vec<u8>, String)> = builder.build_query_as().fetch_all(&mut *conn).await?;
        let by_key = keyed_ids(chunk);
        for (key, literal) in rows {
            let Some(id) = by_key.get(&key) else {
                continue;
            };
            let availability =
                literal
                    .parse::<Availability>()
                    .map_err(|_| LibraryError::UnknownLiteral {
                        kind: "availability",
                        value: literal.clone(),
                    })?;
            found.insert(*id, availability);
        }
    }
    Ok(found)
}

/// Reads the tracks of each album in the given list.
///
/// The read is chunked in batches of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album_ids`: the albums to read.
///
/// # Returns
///
/// Each requested album that has tracks, with its distinct track ids.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn album_member_tracks(
    conn: &mut SqliteConnection,
    album_ids: &[EntityId],
) -> Result<HashMap<EntityId, Vec<EntityId>>, LibraryError> {
    member_tracks(
        conn,
        album_ids,
        "SELECT DISTINCT album_id, id FROM tracks WHERE album_id IN (",
    )
    .await
}

/// Reads the credited tracks of each artist in the given list.
///
/// The read is chunked in batches of [`MAX_IDS_PER_QUERY`].
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `artist_ids`: the artists to read.
///
/// # Returns
///
/// Each requested artist that is credited on tracks, with its distinct track
/// ids.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails.
pub async fn artist_member_tracks(
    conn: &mut SqliteConnection,
    artist_ids: &[EntityId],
) -> Result<HashMap<EntityId, Vec<EntityId>>, LibraryError> {
    member_tracks(
        conn,
        artist_ids,
        "SELECT DISTINCT artist_id, track_id FROM track_artists WHERE artist_id IN (",
    )
    .await
}

/// Runs a `(owner, track)` batch read and groups the tracks by owner.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `owner_ids`: the owners to read, bound to the statement's `IN` list.
/// - `select`: the statement up to and including its opening parenthesis; it
///   returns the owner key and the track key of each row.
///
/// # Returns
///
/// The tracks of each owner that has any.
///
/// # Errors
///
/// Returns [`LibraryError`] when a query fails or a stored key is corrupt.
async fn member_tracks(
    conn: &mut SqliteConnection,
    owner_ids: &[EntityId],
    select: &'static str,
) -> Result<HashMap<EntityId, Vec<EntityId>>, LibraryError> {
    let mut found: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    for chunk in owner_ids.chunks(MAX_IDS_PER_QUERY) {
        let mut builder = QueryBuilder::<Sqlite>::new(select);
        push_key_list(&mut builder, chunk);
        let rows: Vec<(Vec<u8>, Vec<u8>)> = builder.build_query_as().fetch_all(&mut *conn).await?;
        let owners = keyed_ids(chunk);
        for (owner_key, track_key) in rows {
            let Some(owner) = owners.get(&owner_key) else {
                continue;
            };
            let track = decode_entity(IdPrefix::Track, &track_key)?;
            found.entry(*owner).or_default().push(track);
        }
    }
    Ok(found)
}

/// Indexes requested ids by their stored key.
///
/// # Parameters
///
/// - `requested`: the ids to index.
///
/// # Returns
///
/// A map from each id's stored key back to the id.
fn keyed_ids(requested: &[EntityId]) -> HashMap<Vec<u8>, EntityId> {
    requested.iter().map(|id| (entity_key(*id), *id)).collect()
}

/// Writes the personal-scope `scope_tracks` CTE of one user into a builder.
///
/// The CTE is the union [`track_in_scope`] reads, bound to `user`; the caller
/// appends the statement that selects from it.
///
/// # Parameters
///
/// - `builder`: the builder to append to.
/// - `user`: the 16 raw bytes of the user id.
fn push_personal_scope(builder: &mut QueryBuilder<Sqlite>, user: &[u8]) {
    builder
        .push("WITH scope_tracks(track_id) AS (SELECT entity_id FROM user_library WHERE user_id = ")
        .push_bind(user.to_vec())
        .push(" AND entity_kind = 'track'")
        .push(" UNION SELECT entity_id FROM user_feedback WHERE user_id = ")
        .push_bind(user.to_vec())
        .push(" AND entity_kind = 'track' AND value = 'like'")
        .push(" UNION SELECT pt.track_id FROM playlist_tracks pt JOIN playlists p ON p.id = pt.playlist_id WHERE p.owner_user_id = ")
        .push_bind(user.to_vec())
        .push(" AND p.is_temporary = 0")
        .push(" UNION SELECT mf.track_id FROM media_files mf JOIN library_folders lf ON lf.id = mf.folder_id WHERE mf.storage = 'folder' AND lf.owner_user_id = ")
        .push_bind(user.to_vec())
        .push(") ");
}

/// Appends a comma-separated bind list of entity keys and closes it.
///
/// The caller opens the parenthesis; this writes the ids and the closing one.
///
/// # Parameters
///
/// - `builder`: the builder to append to.
/// - `ids`: the ids to bind.
fn push_key_list(builder: &mut QueryBuilder<Sqlite>, ids: &[EntityId]) {
    let mut separated = builder.separated(", ");
    for id in ids {
        separated.push_bind(entity_key(*id));
    }
    separated.push_unseparated(")");
}

/// Maps the stored keys a batch read returned back to the requested ids.
///
/// # Parameters
///
/// - `requested`: the ids the statement was bound with.
/// - `keys`: the stored keys the statement returned.
///
/// # Returns
///
/// The requested ids whose keys were returned.
fn matched_ids(requested: &[EntityId], keys: Vec<Vec<u8>>) -> HashSet<EntityId> {
    let by_key: HashMap<Vec<u8>, EntityId> =
        requested.iter().map(|id| (entity_key(*id), *id)).collect();
    keys.iter()
        .filter_map(|key| by_key.get(key).copied())
        .collect()
}
