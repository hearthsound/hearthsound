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
//!   carries embedded art, and empty otherwise. Later milestones resolve the
//!   named id to bytes; this milestone only names it.
//! - The file-derived fields (`suffix`, `contentType`, `size`, `bitRate`,
//!   `bitDepth`, `samplingRate`, `channelCount`, `duration`, `path`) come from
//!   the media file row rather than from the query projection, because only the
//!   row carries the bit depth, the sample rate, and the channel count.

use hearthsound_library::query::ArtistSummary;
use hearthsound_library::query::{TrackView, content_type_for};
use hearthsound_library::{Album, Artist, MediaFile};

use crate::value::{Array, Object};

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
/// `roles`, and `disambiguation` are not supported by this milestone and are
/// omitted.
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
        .set("replayGain", replay_gain())
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

/// Builds the `replayGain` member this milestone reports.
///
/// # Returns
///
/// Every gain and peak at zero, because the tag reader's ReplayGain values are
/// not projected into the library rows yet. The member is still emitted, so a
/// client knows the server supports the field.
fn replay_gain() -> Object {
    Object::new()
        .set("trackGain", 0.0)
        .set("albumGain", 0.0)
        .set("trackPeak", 0.0)
        .set("albumPeak", 0.0)
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
        assert_eq!(replay_gain().len(), 5);
    }

    #[test]
    fn a_timestamp_is_rendered_as_an_iso_instant() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_621_189_890_000), "2021-05-16T18:31:30Z");
        assert!(timestamp(i64::MAX).is_empty());
    }
}
