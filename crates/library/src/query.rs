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
//! - the **server** scope, whose content is set by `privacy.server_browse`:
//!   everything stored, only the permanent (`local`) content, or nothing at all
//!   when the view is disabled.
//!
//! Everything is read through one connection so a caller can run a whole browse
//! inside a read transaction. The queries return rows that are ready for the
//! Subsonic projection: a track view carries its album, its credited artists, its
//! genres, and the fields of its preferred file.

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix, StorageClass};
use sqlx::{QueryBuilder, Sqlite, SqliteConnection};
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{Album, Artist, MediaFile, StorageRoots, Track, decode_entity, entity_key};
use crate::{albums, artists, folders, media_files, tracks};

/// The literal `privacy.server_browse` uses for the whole server view.
const SERVER_BROWSE_ALL: &str = "all";
/// The literal `privacy.server_browse` uses for the permanent-only view.
const SERVER_BROWSE_PERMANENT: &str = "permanent_only";
/// The literal `privacy.server_browse` uses for a disabled server view.
const SERVER_BROWSE_DISABLED: &str = "disabled";

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
    /// Builds the server scope from the resolved settings.
    ///
    /// # Parameters
    ///
    /// - `settings`: the resolved settings snapshot.
    ///
    /// # Returns
    ///
    /// The server scope the `privacy.server_browse` setting selects.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Setting`] when the setting cannot be read and
    /// [`LibraryError::UnknownLiteral`] when it holds an unknown value.
    pub fn server_from_settings(settings: &SettingsStore) -> Result<Scope, LibraryError> {
        Ok(Scope::Server(ServerBrowse::from_settings(settings)?))
    }
}

/// The content of the server browse view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerBrowse {
    /// Everything stored, including evictable cache.
    All,
    /// Permanent content only: tracks whose availability is `local`.
    PermanentOnly,
    /// The server view is not offered.
    Disabled,
}

impl ServerBrowse {
    /// Reads the view from `privacy.server_browse`.
    ///
    /// The registry already rejects an unknown value when the settings are
    /// resolved, so the unknown-value branch only guards a store that was built
    /// outside the registry.
    ///
    /// # Parameters
    ///
    /// - `settings`: the resolved settings snapshot.
    ///
    /// # Returns
    ///
    /// The configured view.
    ///
    /// # Errors
    ///
    /// Returns [`LibraryError::Setting`] when the setting cannot be read and
    /// [`LibraryError::UnknownLiteral`] when it holds an unknown value.
    pub fn from_settings(settings: &SettingsStore) -> Result<ServerBrowse, LibraryError> {
        let value = settings.get(keys::PRIVACY_SERVER_BROWSE)?;
        match value.as_str() {
            SERVER_BROWSE_ALL => Ok(ServerBrowse::All),
            SERVER_BROWSE_PERMANENT => Ok(ServerBrowse::PermanentOnly),
            SERVER_BROWSE_DISABLED => Ok(ServerBrowse::Disabled),
            _ => Err(LibraryError::UnknownLiteral {
                kind: "server browse view",
                value,
            }),
        }
    }

    /// Returns the literal the view is stored under.
    pub const fn as_str(self) -> &'static str {
        match self {
            ServerBrowse::All => SERVER_BROWSE_ALL,
            ServerBrowse::PermanentOnly => SERVER_BROWSE_PERMANENT,
            ServerBrowse::Disabled => SERVER_BROWSE_DISABLED,
        }
    }

    /// Returns the two availability literals the view matches.
    ///
    /// # Returns
    ///
    /// `("local", "cached")` for [`ServerBrowse::All`] and `("local", "local")`
    /// for the other two; the pair is bound to an `IN (?, ?)` predicate, so a
    /// repeated literal is the permanent-only form.
    fn availability_pair(self) -> (&'static str, &'static str) {
        match self {
            ServerBrowse::All => (Availability::Local.as_str(), Availability::Cached.as_str()),
            ServerBrowse::PermanentOnly | ServerBrowse::Disabled => {
                (Availability::Local.as_str(), Availability::Local.as_str())
            }
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
        tracks.push(track_view(conn, track).await?);
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
    builder.push(
        "SELECT al.id, al.title, al.sort_title, al.display_artist, al.year, al.release_date, \
         al.album_type, al.mb_release_id, al.mb_release_group_id, al.upc, al.origin, \
         al.created_at, al.updated_at \
         FROM albums al \
         WHERE ",
    );
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
    match &sort {
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
/// The play counts are the server's own: every play any user recorded, which is
/// what makes this a *top* listing rather than a personal one.
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
/// The songs, most played first, then the oldest album, disc, and track; a song
/// with no plays still appears, after the played ones.
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
