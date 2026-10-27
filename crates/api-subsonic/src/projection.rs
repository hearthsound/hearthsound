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
//! - `coverArt` is the album's id when the entity belongs to one, and the
//!   entity's own id otherwise; `getCoverArt` answers whether the server holds
//!   an image for it.
//! - The file-derived fields (`suffix`, `contentType`, `size`, `bitRate`,
//!   `bitDepth`, `samplingRate`, `channelCount`, `duration`, `path`) come from
//!   the media file row rather than from the query projection, because only the
//!   row carries the bit depth, the sample rate, and the channel count. A row
//!   without a file - an entity the server knows but has not stored - emits
//!   those fields empty, which is what tells a client it has to acquire it.
//!
//! The canonical identifiers (`musicBrainzId`, `isrc`) live in `external_ids`
//! rather than in the entity row, so every projection that reports them takes
//! them as a parameter.

use std::collections::HashMap;

use hearthsound_activity::Annotation;
use hearthsound_core::providers::{ExternalId, ISRC, MB_ARTIST, MB_RECORDING, MB_RELEASE};
use hearthsound_library::query::ArtistSummary;
use hearthsound_library::query::{TrackView, content_type_for};
use hearthsound_library::{Album, Artist, MediaFile, ReplayGain};
use uuid::Uuid;

use crate::catalog::{identifier, identifiers};
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
/// - `annotations`: the user's annotations for those artists, keyed by id.
/// - `ids`: the stored identifiers of those artists, keyed by id.
///
/// # Returns
///
/// The `ArtistsID3` member, with `ignoredArticles` first and the index second.
pub(crate) fn artists_id3(
    summaries: &[ArtistSummary],
    annotations: &HashMap<Uuid, Annotation>,
    ids: &HashMap<Uuid, Vec<ExternalId>>,
) -> Object {
    Object::new().set("ignoredArticles", IGNORED_ARTICLES).set(
        "index",
        Array::new(artist_index(summaries, annotations, ids)),
    )
}

/// Builds the index groups of an artist listing.
///
/// The artists are ordered by their article-stripped sort name and grouped by
/// the letter that name starts with, in one pass, so the letters come out in
/// order and each artist is projected once.
///
/// # Parameters
///
/// - `summaries`: the artists, each with its album count.
/// - `annotations`: the user's annotations for them, keyed by id.
/// - `ids`: the stored identifiers of them, keyed by id.
///
/// # Returns
///
/// One `index` member per letter, each with its artists.
pub(crate) fn artist_index(
    summaries: &[ArtistSummary],
    annotations: &HashMap<Uuid, Annotation>,
    ids: &HashMap<Uuid, Vec<ExternalId>>,
) -> Vec<Object> {
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
        let artist = artist_id3(
            &summary.artist,
            summary.album_count,
            annotations
                .get(&summary.artist.id.uuid())
                .unwrap_or(&Annotation::default()),
            row_ids(ids, &summary.artist.id.uuid()),
        );
        if let Some((group, artists)) = groups.last_mut()
            && *group == name
        {
            artists.push(artist);
            continue;
        }
        groups.push((name, vec![artist]));
    }

    groups
        .into_iter()
        .map(|(name, artists)| {
            Object::new()
                .set("name", name)
                .set("artist", Array::new(artists))
        })
        .collect()
}

/// Builds the `artists` member of `getArtists` in the order the artists came
/// in.
///
/// The default listing is the alphabetical index, which files and sorts its
/// artists by the article-stripped sort name at projection time. A listing in
/// another order - by when an artist was stored, or by how often the caller
/// played it - has no alphabetical meaning, so this keeps the order the query
/// returned and starts a new `index` group where the letter an artist is filed
/// under changes. A client that flattens the index therefore reads the order
/// the request asked for.
///
/// # Parameters
///
/// - `summaries`: the artists in the requested order, each with its album
///   count.
/// - `annotations`: the user's annotations for those artists, keyed by id.
/// - `ids`: the stored identifiers of those artists, keyed by id.
///
/// # Returns
///
/// The `ArtistsID3` member, with `ignoredArticles` first and the index second.
pub(crate) fn artists_id3_ordered(
    summaries: &[ArtistSummary],
    annotations: &HashMap<Uuid, Annotation>,
    ids: &HashMap<Uuid, Vec<ExternalId>>,
) -> Object {
    Object::new().set("ignoredArticles", IGNORED_ARTICLES).set(
        "index",
        Array::new(artist_index_in_order(summaries, annotations, ids)),
    )
}

/// Builds the index groups of an artist listing in the given order.
///
/// # Parameters
///
/// - `summaries`: the artists in the order to keep.
/// - `annotations`: the user's annotations for them, keyed by id.
/// - `ids`: the stored identifiers of them, keyed by id.
///
/// # Returns
///
/// One `index` member per run of artists filed under the same letter, in the
/// order they came in.
fn artist_index_in_order(
    summaries: &[ArtistSummary],
    annotations: &HashMap<Uuid, Annotation>,
    ids: &HashMap<Uuid, Vec<ExternalId>>,
) -> Vec<Object> {
    let mut groups: Vec<(String, Vec<Object>)> = Vec::new();
    for summary in summaries {
        let name = index_name(stripped_sort_name(&summary.artist.sort_name));
        let artist = artist_id3(
            &summary.artist,
            summary.album_count,
            annotations
                .get(&summary.artist.id.uuid())
                .unwrap_or(&Annotation::default()),
            row_ids(ids, &summary.artist.id.uuid()),
        );
        if let Some((group, artists)) = groups.last_mut()
            && *group == name
        {
            artists.push(artist);
            continue;
        }
        groups.push((name, vec![artist]));
    }

    groups
        .into_iter()
        .map(|(name, artists)| {
            Object::new()
                .set("name", name)
                .set("artist", Array::new(artists))
        })
        .collect()
}

/// Builds the `indexes` member of `getIndexes`.
///
/// # Parameters
///
/// - `summaries`: the artists, each with its album count.
/// - `annotations`: the user's annotations for them, keyed by id.
/// - `ids`: the stored identifiers of them, keyed by id.
/// - `last_modified`: when the browse index last changed, in unix milliseconds.
///
/// # Returns
///
/// The member, with `lastModified` first, so a client that remembers the
/// instant can skip the listing.
pub(crate) fn indexes(
    summaries: &[ArtistSummary],
    annotations: &HashMap<Uuid, Annotation>,
    ids: &HashMap<Uuid, Vec<ExternalId>>,
    last_modified: i64,
) -> Object {
    Object::new()
        .set("lastModified", last_modified)
        .set("ignoredArticles", IGNORED_ARTICLES)
        .set(
            "index",
            Array::new(artist_index(summaries, annotations, ids)),
        )
}

/// Returns the identifiers stored for one row.
///
/// # Parameters
///
/// - `ids`: the stored identifiers, keyed by the row's primary key.
/// - `id`: the row's primary key.
///
/// # Returns
///
/// The identifiers, or an empty list when the row has none.
fn row_ids<'a>(ids: &'a HashMap<Uuid, Vec<ExternalId>>, id: &Uuid) -> &'a [ExternalId] {
    ids.get(id).map_or(&[], Vec::as_slice)
}

/// Builds the `directory` member of `getMusicDirectory` for an artist or album.
///
/// # Parameters
///
/// - `id`: the directory's own client id.
/// - `parent`: the id of the entity the directory is listed under.
/// - `name`: the directory's name.
/// - `annotation`: the user's stars, rating, and play counts for the entity,
///   when the directory is an artist or an album.
/// - `children`: the projected children.
///
/// # Returns
///
/// The directory, with its children last.
pub(crate) fn directory(
    id: &str,
    parent: &str,
    name: &str,
    annotation: Option<&Annotation>,
    children: Vec<Object>,
) -> Object {
    let mut object = Object::new()
        .set("id", id.to_owned())
        .set("parent", parent.to_owned())
        .set("name", name.to_owned());
    if let Some(annotation) = annotation {
        object = set_annotation(object, annotation);
        object = object.set("playCount", annotation.play_count);
    }
    object.set("child", Array::new(children))
}

/// Builds one `ArtistID3` member.
///
/// # Parameters
///
/// - `artist`: the artist row.
/// - `album_count`: how many albums in scope the artist is credited on.
/// - `annotation`: the user's stars, rating, and play counts for the artist.
/// - `ids`: the artist's stored identifiers.
///
/// # Returns
///
/// The artist, with the base protocol's `id`, `name`, and `albumCount` first
/// and the OpenSubsonic `musicBrainzId` and `sortName` after them. `coverArt`,
/// `roles`, and `disambiguation` are not supported yet and are omitted.
pub(crate) fn artist_id3(
    artist: &Artist,
    album_count: i64,
    annotation: &Annotation,
    ids: &[ExternalId],
) -> Object {
    let mut object = Object::new()
        .set("id", artist.id.to_string())
        .set("name", artist.name.clone())
        .set("coverArt", artist.id.to_string())
        .set("albumCount", album_count);
    object = set_annotation(object, annotation);
    object
        .set("musicBrainzId", identifier(ids, MB_ARTIST))
        .set("sortName", artist.sort_name.clone())
}

/// Adds the annotation members an entity carries when the user set them.
///
/// `starred` is the instant of the like in ISO form and `userRating` is the
/// rating from 1 to 5; both are left out when the user has none, which is what
/// a client reads as "not starred" and "not rated". Play counts are not set
/// here because the member order differs: an entity that reports them sets
/// `playCount` and `played` first.
///
/// # Parameters
///
/// - `object`: the projection so far.
/// - `annotation`: the user's annotation for the entity.
///
/// # Returns
///
/// The object with the members the annotation sets.
fn set_annotation(object: Object, annotation: &Annotation) -> Object {
    let mut object = object;
    if let Some(at) = annotation.starred_at {
        object = object.set("starred", timestamp(at));
    }
    if let Some(rating) = annotation.rating {
        object = object.set("userRating", i64::from(rating));
    }
    object
}

/// Adds the play count and the last-play instant of an entity.
///
/// Both are always emitted, because the protocol says so and a client reads
/// `played` as empty for a track that was never played.
///
/// # Parameters
///
/// - `object`: the projection so far.
/// - `annotation`: the user's annotation for the entity.
///
/// # Returns
///
/// The object with `playCount`, `played`, `starred`, and `userRating` set.
fn set_plays(object: Object, annotation: &Annotation) -> Object {
    let object = object.set("playCount", annotation.play_count).set(
        "played",
        annotation.last_played.map(timestamp).unwrap_or_default(),
    );
    set_annotation(object, annotation)
}

/// Builds one directory `Child` member for an album.
///
/// The base protocol's `getAlbumList` reports an album as a folder rather than
/// as an `AlbumID3`, so its songs are a listing below it rather than a member
/// of it.
///
/// # Parameters
///
/// - `album`: the album row.
/// - `artists`: the album's credited artists, in credit order.
/// - `tracks`: the album's tracks in scope, in playing order.
/// - `annotation`: the user's stars, rating, and play counts for the album.
///
/// # Returns
///
/// The directory child, with `isDir` set and the album's own id as its
/// `coverArt`.
pub(crate) fn album_dir(
    album: &Album,
    artists: &[Artist],
    tracks: &[TrackView],
    annotation: &Annotation,
) -> Object {
    let genres = album_genres(tracks);
    let duration_secs = tracks.iter().map(track_duration_ms).sum::<i64>() / 1000;
    let display_artist = display_name(album.display_artist.as_deref(), artists);
    let artist_id = artists
        .first()
        .map(|artist| artist.id.to_string())
        .unwrap_or_default();
    let object = Object::new()
        .set("id", album.id.to_string())
        .set("parent", artist_id.clone())
        .set("isDir", true)
        .set("title", album.title.clone())
        .set("name", album.title.clone())
        .set("album", album.title.clone())
        .set("artist", display_artist)
        .set("year", album.year.unwrap_or(0))
        .set("genre", genres.first().cloned().unwrap_or_default())
        .set("coverArt", album.id.to_string())
        .set("duration", duration_secs)
        .set("playCount", annotation.play_count)
        .set(
            "played",
            annotation.last_played.map(timestamp).unwrap_or_default(),
        )
        .set("created", timestamp(album.created_at))
        .set("artistId", artist_id)
        .set("songCount", tracks.len() as i64);
    set_annotation(object, annotation)
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

/// Builds the artist reference an older listing carries.
///
/// `search2` and `getStarred` list an artist as a bare reference with the
/// user's star, and nothing else, which is what a client of those responses
/// reads.
///
/// # Parameters
///
/// - `artist`: the artist row.
/// - `annotation`: the user's stars for the artist.
///
/// # Returns
///
/// The reference, with `starred` only when the user starred the artist.
pub(crate) fn starred_artist_ref(artist: &Artist, annotation: &Annotation) -> Object {
    let object = artist_ref(artist);
    match annotation.starred_at {
        Some(at) => object.set("starred", timestamp(at)),
        None => object,
    }
}

/// Builds one `AlbumID3` member.
///
/// # Parameters
///
/// - `album`: the album row.
/// - `artists`: the album's credited artists, in credit order.
/// - `tracks`: the album's tracks in scope, in playing order; their genres,
///   durations, and explicitness are what the album reports.
/// - `annotation`: the user's stars, rating, and play counts for the album.
/// - `ids`: the album's stored identifiers.
///
/// # Returns
///
/// The album. The `song` member is added by the endpoint that returns songs,
/// so an album list entry and an album detail share one projection.
pub(crate) fn album_id3(
    album: &Album,
    artists: &[Artist],
    tracks: &[TrackView],
    annotation: &Annotation,
    ids: &[ExternalId],
) -> Object {
    let genres = album_genres(tracks);
    let duration_secs = tracks.iter().map(track_duration_ms).sum::<i64>() / 1000;
    let display_artist = display_name(album.display_artist.as_deref(), artists);

    let object = Object::new()
        .set("id", album.id.to_string())
        .set("name", album.title.clone())
        .set("artist", display_artist.clone())
        .set("year", album.year.unwrap_or(0))
        .set("coverArt", album.id.to_string())
        .set("duration", duration_secs)
        .set("genre", genres.first().cloned().unwrap_or_default())
        .set("created", timestamp(album.created_at))
        .set(
            "artistId",
            artists
                .first()
                .map(|artist| artist.id.to_string())
                .unwrap_or_default(),
        )
        .set("songCount", tracks.len() as i64);
    let object = set_plays(object, annotation);
    object
        .set("version", album.edition.clone().unwrap_or_default())
        .set("musicBrainzId", identifier(ids, MB_RELEASE))
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
/// - `annotation`: the user's stars, rating, and play counts for the track.
/// - `ids`: the track's stored identifiers.
///
/// # Returns
///
/// The child. The base protocol's fields come first, in the order the protocol
/// lists them, and the OpenSubsonic additions follow; `coverArt` always names
/// the album the track belongs to, or the track itself when it has none, so a
/// client asks for the image and is told when there is none.
pub(crate) fn child(
    view: &TrackView,
    file: Option<&MediaFile>,
    album_artists: &[Artist],
    annotation: &Annotation,
    ids: &[ExternalId],
) -> Object {
    let track = &view.track;
    let album = view.album.as_ref();
    let cover_art = album
        .map(|album| album.id.to_string())
        .unwrap_or_else(|| track.id.to_string());
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

    let object = Object::new()
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
        .set("musicBrainzId", identifier(ids, MB_RECORDING))
        .set("isrc", Array::new(identifiers(ids, ISRC)))
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
        .set("explicitStatus", explicit_status(track.explicit));
    let object = set_plays(object, annotation);
    object
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
pub(crate) fn timestamp(millis: i64) -> String {
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
