//! The Subsonic projections a browse response is built from.
//!
//! A projection turns rows the library crate already read into one member of a
//! response: a `Child` (a song), an `AlbumID3`, an `ArtistID3`, or the
//! `ArtistsID3` index. Every browse endpoint builds its payload from these
//! functions, and `search3` reuses them for its local results, so the members
//! cannot drift between endpoints.
//!
//! The field sets follow the OpenSubsonic reference. The base protocol's fields
//! are always emitted, and the OpenSubsonic additions are emitted with an empty
//! or default value when this server has nothing to put in them, because a
//! client tells a supported-but-unknown field from an unsupported field by its
//! presence (https://opensubsonic.netlify.app/docs/responses/child/). A field
//! this server does not support at all is omitted rather than emitted empty.
//!
//! Two members need data from outside the entity row:
//!
//! - `coverArt` names the album when the track's preferred media file carries
//!   embedded art and the track belongs to an album, and the track itself
//!   otherwise. An album's own `coverArt` is its id when any of its tracks
//!   carries embedded art, and empty otherwise; `getCoverArt` resolves the
//!   named id to bytes.
//! - The file-derived fields (`suffix`, `contentType`, `size`, `bitRate`,
//!   `bitDepth`, `samplingRate`, `channelCount`, `duration`, `path`) come from
//!   the media file row rather than from the query projection, because only the
//!   row carries the bit depth, the sample rate, and the channel count.

use hearthsound_catalog::{CatalogAlbumRefResult, CatalogAlbumResult, CatalogArtistResult};
use hearthsound_core::providers::{ArtistCredit, CatalogTrack};
use hearthsound_core::{CatalogId, EntityKind, Slug};
use hearthsound_library::query::ArtistSummary;
use hearthsound_library::query::{TrackView, content_type_for};
use hearthsound_library::{Album, Artist, MediaFile, ReplayGain};

use crate::catalog::{artist_id, music_brainz_id};
use crate::value::{Array, Object};

/// The `albums.album_type` literal a compilation is stored with.
const COMPILATION_ALBUM_TYPE: &str = "compilation";

/// The ignored articles `getArtists` reports.
///
/// The string is part of the response contract: a client shows it beside the
/// index so a user knows why an artist is filed under a letter that is not the
/// first letter of its name.
pub(crate) const IGNORED_ARTICLES: &str = "The El La Los Las Le Les";

/// The article words stripped from an artist's sort name before indexing.
///
/// The comparison is against the folded sort name, so every entry is lowercase.
const ARTICLES: [&str; 7] = ["the", "el", "la", "los", "las", "le", "les"];

/// Builds the `artists` member of `getArtists`.
///
/// The artists are grouped into an index by the first letter of their
/// article-stripped sort name, and the groups and the artists inside them are
/// ordered by that stripped name, so `The Beatles` is filed and sorted under
/// `B`. An artist whose stripped name starts with anything but a letter is
/// filed under `#`.
///
/// # Parameters
///
/// - `summaries`: the artists in scope, each with its album count.
///
/// # Returns
///
/// The `ArtistsID3` member, with `ignoredArticles` first and the index second.
pub(crate) fn artists_id3(summaries: &[ArtistSummary]) -> Object {
    let mut ordered: Vec<(&str, &ArtistSummary)> = summaries
        .iter()
        .map(|summary| (stripped_sort_name(&summary.artist.sort_name), summary))
        .collect();
    ordered.sort_by(|left, right| {
        left.0.cmp(right.0).then_with(|| {
            left.1
                .artist
                .id
                .to_string()
                .cmp(&right.1.artist.id.to_string())
        })
    });

    let mut groups: Vec<(String, Vec<Object>)> = Vec::new();
    for (sort_name, summary) in ordered {
        let name = index_name(sort_name);
        let artist = artist_id3(&summary.artist, summary.album_count);
        if let Some((group, artists)) = groups.last_mut()
            && *group == name
        {
            artists.push(artist);
            continue;
        }
        groups.push((name, vec![artist]));
    }

    let index: Vec<Object> = groups
        .into_iter()
        .map(|(name, artists)| {
            Object::new()
                .set("name", name)
                .set("artist", Array::new(artists))
        })
        .collect();

    Object::new()
        .set("ignoredArticles", IGNORED_ARTICLES)
        .set("index", Array::new(index))
}

/// Builds one `ArtistID3` member.
///
/// # Parameters
///
/// - `artist`: the artist row.
/// - `album_count`: how many albums in scope the artist is credited on.
///
/// # Returns
///
/// The artist, with the base protocol's `id`, `name`, and `albumCount` first
/// and the OpenSubsonic `musicBrainzId` and `sortName` after them. `coverArt`,
/// `roles`, and `disambiguation` are not supported yet and are omitted.
pub(crate) fn artist_id3(artist: &Artist, album_count: i64) -> Object {
    Object::new()
        .set("id", artist.id.to_string())
        .set("name", artist.name.clone())
        .set("albumCount", album_count)
        .set(
            "musicBrainzId",
            artist.mb_artist_id.clone().unwrap_or_default(),
        )
        .set("sortName", artist.sort_name.clone())
}

/// Builds the minimal `ArtistID3` reference embedded in another member.
///
/// The OpenSubsonic reference asks an embedded artist to carry only the
/// required fields, so this is `id` and `name` and nothing else.
///
/// # Parameters
///
/// - `artist`: the artist row.
///
/// # Returns
///
/// The reference, with the id first.
pub(crate) fn artist_ref(artist: &Artist) -> Object {
    Object::new()
        .set("id", artist.id.to_string())
        .set("name", artist.name.clone())
}

/// Builds one `AlbumID3` member.
///
/// # Parameters
///
/// - `album`: the album row.
/// - `artists`: the album's credited artists, in credit order.
/// - `tracks`: the album's tracks in scope, in playing order; their genres,
///   durations, and explicitness are what the album reports.
/// - `has_art`: whether any of the album's preferred files carries embedded
///   cover art.
///
/// # Returns
///
/// The album. The `song` member is added by the endpoint that returns songs,
/// so an album list entry and an album detail share one projection.
pub(crate) fn album_id3(
    album: &Album,
    artists: &[Artist],
    tracks: &[TrackView],
    has_art: bool,
) -> Object {
    let genres = album_genres(tracks);
    let duration_secs = tracks.iter().map(track_duration_ms).sum::<i64>() / 1000;
    let display_artist = display_name(album.display_artist.as_deref(), artists);

    Object::new()
        .set("id", album.id.to_string())
        .set("name", album.title.clone())
        .set("artist", display_artist.clone())
        .set("year", album.year.unwrap_or(0))
        .set(
            "coverArt",
            if has_art {
                album.id.to_string()
            } else {
                String::new()
            },
        )
        .set("duration", duration_secs)
        .set("playCount", 0)
        .set("genre", genres.first().cloned().unwrap_or_default())
        .set("created", timestamp(album.created_at))
        .set(
            "artistId",
            artists
                .first()
                .map(|artist| artist.id.to_string())
                .unwrap_or_default(),
        )
        .set("songCount", tracks.len() as i64)
        .set("played", "")
        .set(
            "musicBrainzId",
            album.mb_release_id.clone().unwrap_or_default(),
        )
        .set(
            "genres",
            Array::named("genre", genres.iter().map(|name| genre(name))),
        )
        .set(
            "artists",
            Array::named("artist", artists.iter().map(artist_ref)),
        )
        .set("displayArtist", display_artist)
        .set(
            "releaseTypes",
            Array::named("releaseType", album.album_type.iter().cloned()),
        )
        .set(
            "isCompilation",
            album.album_type.as_deref() == Some(COMPILATION_ALBUM_TYPE),
        )
        .set("moods", Array::named("mood", std::iter::empty::<String>()))
        .set("sortName", album.sort_title.clone())
        .set("explicitStatus", album_explicit(tracks))
}

/// Builds one `Child` member (a song).
///
/// # Parameters
///
/// - `view`: the track with its album, credited artists, genres, and preferred
///   file projection.
/// - `file`: the track's preferred media file row, when it has a file. It
///   supplies the fields the query projection does not carry: the bit depth,
///   the sample rate, the channel count, and the embedded-art flag.
/// - `album_artists`: the credited artists of the track's album, empty when the
///   track has no album.
///
/// # Returns
///
/// The child. The base protocol's fields come first, in the order the protocol
/// lists them, and the OpenSubsonic additions follow; see the module
/// documentation for how `coverArt` is chosen.
pub(crate) fn child(
    view: &TrackView,
    file: Option<&MediaFile>,
    album_artists: &[Artist],
) -> Object {
    let track = &view.track;
    let album = view.album.as_ref();
    let has_art = matches!(file.and_then(|file| file.has_embedded_art), Some(true));
    let cover_art = match album {
        Some(album) if has_art => album.id.to_string(),
        _ => track.id.to_string(),
    };
    let (suffix, content_type) = match file {
        Some(file) => {
            let suffix = file_suffix(file);
            let content_type = content_type_for(&suffix).to_owned();
            (suffix, content_type)
        }
        None => (String::new(), String::new()),
    };
    let duration_secs = file
        .and_then(|file| file.duration_ms)
        .or(track.duration_ms)
        .unwrap_or(0)
        / 1000;

    Object::new()
        .set("id", track.id.to_string())
        .set(
            "parent",
            album.map(|album| album.id.to_string()).unwrap_or_default(),
        )
        .set("isDir", false)
        .set("title", track.title.clone())
        .set(
            "album",
            album.map(|album| album.title.clone()).unwrap_or_default(),
        )
        .set(
            "artist",
            display_name(track.display_artist.as_deref(), &view.artists),
        )
        .set("track", track.track_number.unwrap_or(0))
        .set("year", album.and_then(|album| album.year).unwrap_or(0))
        .set("genre", view.genres.first().cloned().unwrap_or_default())
        .set("coverArt", cover_art)
        .set("size", file.map(|file| file.size).unwrap_or(0))
        .set("contentType", content_type)
        .set("suffix", suffix)
        .set("duration", duration_secs)
        .set(
            "bitRate",
            file.and_then(|file| file.bitrate_kbps).unwrap_or(0),
        )
        .set(
            "bitDepth",
            file.and_then(|file| file.bit_depth).unwrap_or(0),
        )
        .set(
            "samplingRate",
            file.and_then(|file| file.sample_rate).unwrap_or(0),
        )
        .set(
            "channelCount",
            file.and_then(|file| file.channels).unwrap_or(0),
        )
        .set(
            "path",
            file.map(|file| file.rel_path.clone()).unwrap_or_default(),
        )
        .set("playCount", 0)
        .set("played", "")
        .set("discNumber", track.disc_number.unwrap_or(0))
        .set("created", timestamp(track.created_at))
        .set(
            "albumId",
            album.map(|album| album.id.to_string()).unwrap_or_default(),
        )
        .set(
            "artistId",
            view.artists
                .first()
                .map(|artist| artist.id.to_string())
                .unwrap_or_default(),
        )
        .set("type", "music")
        .set("mediaType", "song")
        .set("isVideo", false)
        .set("bpm", 0)
        .set("comment", "")
        .set("sortName", track.sort_title.clone())
        .set(
            "musicBrainzId",
            track.mb_recording_id.clone().unwrap_or_default(),
        )
        .set("isrc", Array::new(track.isrc.iter().cloned()))
        .set(
            "genres",
            Array::named("genre", view.genres.iter().map(|name| genre(name))),
        )
        .set(
            "artists",
            Array::named("artist", view.artists.iter().map(artist_ref)),
        )
        .set(
            "displayArtist",
            display_name(track.display_artist.as_deref(), &view.artists),
        )
        .set(
            "albumArtists",
            Array::named("albumArtist", album_artists.iter().map(artist_ref)),
        )
        .set(
            "displayAlbumArtist",
            album
                .and_then(|album| album.display_artist.clone())
                .unwrap_or_default(),
        )
        .set(
            "contributors",
            Array::named("contributor", std::iter::empty::<Object>()),
        )
        .set("displayComposer", "")
        .set("moods", Array::named("mood", std::iter::empty::<String>()))
        .set("explicitStatus", explicit_status(track.explicit))
        .set(
            "replayGain",
            replay_gain(file.and_then(MediaFile::replay_gain)),
        )
        .set("works", Array::named("work", std::iter::empty::<Object>()))
        .set(
            "movements",
            Array::named("movement", std::iter::empty::<Object>()),
        )
        .set(
            "groupings",
            Array::named("grouping", std::iter::empty::<String>()),
        )
}

/// Builds one `ItemGenre` member.
///
/// # Parameters
///
/// - `name`: the genre name.
///
/// # Returns
///
/// The genre, named `name`.
fn genre(name: &str) -> Object {
    Object::new().set("name", name.to_owned())
}

/// Builds the `replayGain` member from the file's stored values.
///
/// # Parameters
///
/// - `gain`: the preferred file's ReplayGain values, or `None` when the track
///   has no file or its file carries none.
///
/// # Returns
///
/// The member, with every unset gain and peak at zero. The member is always
/// emitted, so a client knows the server supports the field.
fn replay_gain(gain: Option<ReplayGain>) -> Object {
    let gain = gain.unwrap_or_default();
    Object::new()
        .set("trackGain", gain.track_gain.unwrap_or(0.0))
        .set("albumGain", gain.album_gain.unwrap_or(0.0))
        .set("trackPeak", gain.track_peak.unwrap_or(0.0))
        .set("albumPeak", gain.album_peak.unwrap_or(0.0))
        .set("baseGain", 0.0)
}

/// Returns the `explicitStatus` literal of one track.
///
/// # Parameters
///
/// - `explicit`: the track's explicit flag, when its tags carry one.
///
/// # Returns
///
/// `explicit`, `clean`, or an empty string when the track says nothing.
fn explicit_status(explicit: Option<bool>) -> &'static str {
    match explicit {
        Some(true) => "explicit",
        Some(false) => "clean",
        None => "",
    }
}

/// Returns the `explicitStatus` literal of an album.
///
/// # Parameters
///
/// - `tracks`: the album's tracks in scope.
///
/// # Returns
///
/// `explicit` when any track is explicit, `clean` when no track is explicit and
/// at least one is clean, and an empty string otherwise, as the protocol
/// prescribes.
fn album_explicit(tracks: &[TrackView]) -> &'static str {
    if tracks.iter().any(|view| view.track.explicit == Some(true)) {
        "explicit"
    } else if tracks.iter().any(|view| view.track.explicit == Some(false)) {
        "clean"
    } else {
        ""
    }
}

/// Collects an album's genres from its tracks.
///
/// # Parameters
///
/// - `tracks`: the album's tracks in scope, in playing order.
///
/// # Returns
///
/// The distinct genre names, in the order they first appear.
fn album_genres(tracks: &[TrackView]) -> Vec<String> {
    let mut genres = Vec::new();
    for view in tracks {
        for genre in &view.genres {
            if !genres.contains(genre) {
                genres.push(genre.clone());
            }
        }
    }
    genres
}

/// Returns a track's duration in milliseconds.
///
/// # Parameters
///
/// - `view`: the track view.
///
/// # Returns
///
/// The preferred file's duration when it records one, the track's own duration
/// otherwise, and zero when neither does.
fn track_duration_ms(view: &TrackView) -> i64 {
    view.file
        .as_ref()
        .and_then(|file| file.duration_ms)
        .or(view.track.duration_ms)
        .unwrap_or(0)
}

/// Returns a media file's suffix.
///
/// # Parameters
///
/// - `file`: the media file row.
///
/// # Returns
///
/// The extension of the relative path, lowercased and without the leading dot,
/// or the probed container literal when the path has no extension.
fn file_suffix(file: &MediaFile) -> String {
    let suffix = file.suffix();
    if suffix.is_empty() {
        file.format.clone().unwrap_or_default()
    } else {
        suffix
    }
}

/// Returns a display artist, falling back to the credited artist names.
///
/// # Parameters
///
/// - `display`: the stored display artist, when the row carries one.
/// - `artists`: the credited artists, in credit order.
///
/// # Returns
///
/// The stored display artist when it is not empty, the credited names joined
/// with `, ` otherwise, and an empty string when neither is available.
fn display_name(display: Option<&str>, artists: &[Artist]) -> String {
    match display {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => artists
            .iter()
            .map(|artist| artist.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Strips a leading ignored article from a sort name.
///
/// # Parameters
///
/// - `sort_name`: the folded sort name.
///
/// # Returns
///
/// The sort name without a leading article word, or the whole sort name when it
/// begins with none. An article is stripped only when it stands alone, so
/// `theatre` keeps its `the`.
fn stripped_sort_name(sort_name: &str) -> &str {
    for article in ARTICLES {
        if let Some(rest) = sort_name.strip_prefix(article)
            && let Some(rest) = rest.strip_prefix(' ')
        {
            return rest;
        }
    }
    sort_name
}

/// Returns the index letter of an article-stripped sort name.
///
/// # Parameters
///
/// - `sort_name`: the article-stripped sort name.
///
/// # Returns
///
/// The first character uppercased when it is a letter, and `#` otherwise
/// (including for an empty name).
fn index_name(sort_name: &str) -> String {
    match sort_name.chars().next() {
        Some(first) if first.is_alphabetic() => first.to_uppercase().collect(),
        _ => "#".to_owned(),
    }
}

/// Formats a stored timestamp as an ISO 8601 instant.
///
/// # Parameters
///
/// - `millis`: milliseconds since the Unix epoch.
///
/// # Returns
///
/// The instant in RFC 3339 form, or an empty string when the stored value is
/// outside the representable range.
fn timestamp(millis: i64) -> String {
    let nanos = i128::from(millis) * 1_000_000;
    match time::OffsetDateTime::from_unix_timestamp_nanos(nanos) {
        Ok(value) => value
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        Err(error) => {
            tracing::warn!(millis, error = %error, "a stored timestamp is out of range");
            String::new()
        }
    }
}

/// Builds one catalog `Child` member (a song the server has not stored).
///
/// The fields follow the catalog result rules (05-api.md, section 3.5): the id
/// is the `x_` id that names the entity, `suffix` and `contentType` are empty
/// because no file exists yet and the container the acquired track will have is
/// unknown, `size` is zero because no file exists yet, `duration` is the
/// provider's duration, `path` is a synthetic `catalog/<slug>/<id>` that can
/// never be a filesystem path, `coverArt` names the entity itself so
/// `getCoverArt` resolves it through the issuing instance, and the title carries
/// the catalog title suffix. Every other OpenSubsonic field is emitted at its
/// default, so a client can tell a supported-but-unknown value from an
/// unsupported field.
///
/// # Parameters
///
/// - `id`: the catalog id that names the track.
/// - `track`: the track the provider returned.
/// - `title_suffix`: the suffix appended to the title.
///
/// # Returns
///
/// The child.
pub(crate) fn catalog_child(id: &CatalogId, track: &CatalogTrack, title_suffix: &str) -> Object {
    let slug = id.slug();
    let album = track.album.as_ref();
    let album_id = album
        .and_then(|album| CatalogId::new(slug.clone(), EntityKind::Album, album.id.clone()).ok());
    let album_artists: &[ArtistCredit] = album
        .map(|album| album.artists.as_slice())
        .unwrap_or_default();
    let duration_secs = track
        .duration_ms
        .map(|duration| i64::from(duration) / 1_000)
        .unwrap_or(0);

    Object::new()
        .set("id", id.to_string())
        .set(
            "parent",
            album_id
                .as_ref()
                .map(|album| album.to_string())
                .unwrap_or_default(),
        )
        .set("isDir", false)
        .set("title", format!("{}{title_suffix}", track.title))
        .set(
            "album",
            album.map(|album| album.title.clone()).unwrap_or_default(),
        )
        .set("artist", display_credits(&track.artists))
        .set("track", i64::from(track.track_number.unwrap_or(0)))
        .set(
            "year",
            album
                .and_then(|album| album.year)
                .map(i64::from)
                .unwrap_or(0),
        )
        .set("genre", "")
        .set("coverArt", id.to_string())
        .set("size", 0)
        .set("contentType", "")
        .set("suffix", "")
        .set("duration", duration_secs)
        .set("bitRate", 0)
        .set("bitDepth", 0)
        .set("samplingRate", 0)
        .set("channelCount", 0)
        .set("path", format!("catalog/{slug}/{}", track.id))
        .set("playCount", 0)
        .set("played", "")
        .set("discNumber", i64::from(track.disc_number.unwrap_or(0)))
        .set("created", "")
        .set(
            "albumId",
            album_id
                .as_ref()
                .map(|album| album.to_string())
                .unwrap_or_default(),
        )
        .set(
            "artistId",
            track
                .artists
                .first()
                .and_then(|credit| artist_id(slug, credit))
                .map(|artist| artist.to_string())
                .unwrap_or_default(),
        )
        .set("type", "music")
        .set("mediaType", "song")
        .set("isVideo", false)
        .set("bpm", 0)
        .set("comment", "")
        .set("sortName", "")
        .set("musicBrainzId", music_brainz_id(&track.ids))
        .set("isrc", Array::new(track.isrc.iter().cloned()))
        .set(
            "genres",
            Array::named("genre", std::iter::empty::<String>()),
        )
        .set(
            "artists",
            Array::named(
                "artist",
                track.artists.iter().map(|credit| credit_ref(slug, credit)),
            ),
        )
        .set("displayArtist", display_credits(&track.artists))
        .set(
            "albumArtists",
            Array::named(
                "albumArtist",
                album_artists.iter().map(|credit| credit_ref(slug, credit)),
            ),
        )
        .set("displayAlbumArtist", display_credits(album_artists))
        .set(
            "contributors",
            Array::named("contributor", std::iter::empty::<Object>()),
        )
        .set("displayComposer", "")
        .set("moods", Array::named("mood", std::iter::empty::<String>()))
        .set("explicitStatus", explicit_status(track.explicit))
        .set("replayGain", replay_gain(None))
        .set("works", Array::named("work", std::iter::empty::<Object>()))
        .set(
            "movements",
            Array::named("movement", std::iter::empty::<Object>()),
        )
        .set(
            "groupings",
            Array::named("grouping", std::iter::empty::<String>()),
        )
}

/// Builds one catalog `AlbumID3` member from a search result.
///
/// A search returns an album reference: a title, the credits, a year and the
/// images, without a tracklist. The projection is the same member
/// [`catalog_album`] produces for a full album, with the fields the reference
/// does not carry left at their default.
///
/// # Parameters
///
/// - `result`: the album reference the provider returned, with its catalog id.
/// - `title_suffix`: the suffix appended to the title.
///
/// # Returns
///
/// The album.
pub(crate) fn catalog_album_ref(result: &CatalogAlbumRefResult, title_suffix: &str) -> Object {
    let id = &result.id;
    let slug = id.slug();
    let album = &result.album;

    Object::new()
        .set("id", id.to_string())
        .set("name", format!("{}{title_suffix}", album.title))
        .set("artist", display_credits(&album.artists))
        .set("year", album.year.map(i64::from).unwrap_or(0))
        .set("coverArt", cover_art(id, !album.images.is_empty()))
        .set("duration", 0)
        .set("playCount", 0)
        .set("genre", "")
        .set("created", "")
        .set(
            "artistId",
            album
                .artists
                .first()
                .and_then(|credit| artist_id(slug, credit))
                .map(|artist| artist.to_string())
                .unwrap_or_default(),
        )
        .set("songCount", 0)
        .set("played", "")
        .set("musicBrainzId", "")
        .set(
            "genres",
            Array::named("genre", std::iter::empty::<String>()),
        )
        .set(
            "artists",
            Array::named(
                "artist",
                album.artists.iter().map(|credit| credit_ref(slug, credit)),
            ),
        )
        .set("displayArtist", display_credits(&album.artists))
        .set(
            "releaseTypes",
            Array::named("releaseType", std::iter::empty::<String>()),
        )
        .set("moods", Array::named("mood", std::iter::empty::<String>()))
        .set("sortName", "")
        .set("explicitStatus", "")
}

/// Builds one catalog `AlbumID3` member from a full album lookup.
///
/// # Parameters
///
/// - `result`: the album the provider returned, with its catalog id.
/// - `title_suffix`: the suffix appended to the title.
///
/// # Returns
///
/// The album, without its `song` children; the endpoint that returns songs adds
/// them with [`catalog_child`].
pub(crate) fn catalog_album(result: &CatalogAlbumResult, title_suffix: &str) -> Object {
    let id = &result.id;
    let slug = id.slug();
    let album = &result.album;
    let genres: Vec<String> = album.genres.clone();
    let duration_secs = album
        .tracks
        .iter()
        .filter_map(|track| track.duration_ms)
        .map(i64::from)
        .sum::<i64>()
        / 1_000;

    Object::new()
        .set("id", id.to_string())
        .set("name", format!("{}{title_suffix}", album.title))
        .set("artist", display_credits(&album.artists))
        .set("year", release_year(album.release_date.as_deref()))
        .set("coverArt", cover_art(id, !album.images.is_empty()))
        .set("duration", duration_secs)
        .set("playCount", 0)
        .set("genre", genres.first().cloned().unwrap_or_default())
        .set("created", "")
        .set(
            "artistId",
            album
                .artists
                .first()
                .and_then(|credit| artist_id(slug, credit))
                .map(|artist| artist.to_string())
                .unwrap_or_default(),
        )
        .set("songCount", album.tracks.len() as i64)
        .set("played", "")
        .set("musicBrainzId", music_brainz_id(&album.ids))
        .set(
            "genres",
            Array::named("genre", genres.iter().map(|name| genre(name))),
        )
        .set(
            "artists",
            Array::named(
                "artist",
                album.artists.iter().map(|credit| credit_ref(slug, credit)),
            ),
        )
        .set("displayArtist", display_credits(&album.artists))
        .set(
            "releaseTypes",
            Array::named("releaseType", album.album_type.iter().cloned()),
        )
        .set("moods", Array::named("mood", std::iter::empty::<String>()))
        .set("sortName", "")
        .set("explicitStatus", "")
}

/// Builds one catalog `ArtistID3` member.
///
/// # Parameters
///
/// - `result`: the artist the provider returned, with its catalog id.
/// - `album_count`: how many albums the artist has, when the caller knows it;
///   a search result does not.
/// - `title_suffix`: the suffix appended to the name.
///
/// # Returns
///
/// The artist.
pub(crate) fn catalog_artist(
    result: &CatalogArtistResult,
    album_count: i64,
    title_suffix: &str,
) -> Object {
    let artist = &result.artist;

    Object::new()
        .set("id", result.id.to_string())
        .set("name", format!("{}{title_suffix}", artist.name))
        .set("albumCount", album_count)
        .set("musicBrainzId", music_brainz_id(&artist.ids))
        .set("sortName", "")
}

/// Builds the minimal `ArtistID3` reference an entity's credits embed.
///
/// # Parameters
///
/// - `slug`: the slug of the instance that issued the entity.
/// - `credit`: the artist credit.
///
/// # Returns
///
/// The reference, named but with an empty id when the credit carries no
/// provider identifier, because a credit the catalog cannot resolve must not
/// point at a fabricated entity.
fn credit_ref(slug: &Slug, credit: &ArtistCredit) -> Object {
    Object::new()
        .set(
            "id",
            artist_id(slug, credit)
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .set("name", credit.name.clone())
}

/// Joins an entity's credits into its display artist.
///
/// # Parameters
///
/// - `credits`: the artist credits, in credit order.
///
/// # Returns
///
/// The credit names joined with `, `, and an empty string when there are none.
fn display_credits(credits: &[ArtistCredit]) -> String {
    credits
        .iter()
        .map(|credit| credit.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns a catalog entity's `coverArt` value.
///
/// # Parameters
///
/// - `id`: the entity's catalog id.
/// - `has_image`: whether the provider returned an image for the entity.
///
/// # Returns
///
/// The entity's own id when it has an image, so `getCoverArt` resolves it
/// through the issuing instance, and an empty string otherwise.
fn cover_art(id: &CatalogId, has_image: bool) -> String {
    if has_image {
        id.to_string()
    } else {
        String::new()
    }
}

/// Returns the year a release date names.
///
/// # Parameters
///
/// - `release_date`: the provider's ISO 8601 date or date-time.
///
/// # Returns
///
/// The first four characters parsed as a year, or zero when the value does not
/// begin with one.
fn release_year(release_date: Option<&str>) -> i64 {
    release_date
        .and_then(|date| date.get(..4))
        .and_then(|year| year.parse::<i64>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_name_is_the_first_letter_of_the_stripped_name() {
        assert_eq!(index_name(stripped_sort_name("the beatles")), "B");
        assert_eq!(index_name(stripped_sort_name("el mar")), "M");
        assert_eq!(index_name(stripped_sort_name("2 mello")), "#");
        assert_eq!(index_name(stripped_sort_name("")), "#");
        assert_eq!(index_name(stripped_sort_name("charlotte")), "C");
    }

    #[test]
    fn an_article_is_stripped_only_when_it_stands_alone() {
        assert_eq!(stripped_sort_name("the cardinals"), "cardinals");
        assert_eq!(stripped_sort_name("las hijas"), "hijas");
        assert_eq!(stripped_sort_name("theatre"), "theatre");
        assert_eq!(stripped_sort_name("the"), "the");
        assert_eq!(stripped_sort_name("le sacre"), "sacre");
        assert_eq!(stripped_sort_name("leon"), "leon");
    }

    #[test]
    fn a_replay_gain_member_defaults_every_gain_to_zero() {
        assert_eq!(replay_gain(None).len(), 5);
    }

    #[test]
    fn a_replay_gain_member_reports_the_stored_values() {
        use crate::value::Value;

        let gain = replay_gain(Some(ReplayGain {
            track_gain: Some(-6.54),
            track_peak: Some(0.988861),
            album_gain: Some(-5.2),
            album_peak: None,
        }));
        assert_eq!(
            gain.members(),
            [
                ("trackGain".into(), Value::Float(-6.54)),
                ("albumGain".into(), Value::Float(-5.2)),
                ("trackPeak".into(), Value::Float(0.988861)),
                ("albumPeak".into(), Value::Float(0.0)),
                ("baseGain".into(), Value::Float(0.0)),
            ]
        );
    }

    #[test]
    fn a_timestamp_is_rendered_as_an_iso_instant() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_621_189_890_000), "2021-05-16T18:31:30Z");
        assert!(timestamp(i64::MAX).is_empty());
    }
}
