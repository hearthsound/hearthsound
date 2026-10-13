//! Heuristics that recover the tags a file name and its parent directories
//! imply.
//!
//! Soulseek results usually carry no tags: the remote peer exposes a path and
//! nothing else. [`derive_tags`] therefore parses the file name and walks the
//! parent directories to fill in the title, artist, album and track number the
//! scorer compares against the wanted track.
//!
//! The parser recognizes the layouts a shared library actually uses:
//!
//! - `NN - Title.ext`, `NN. Title.ext`, `NN.Artist - Title.ext`,
//!   `NN Title.ext` (leading zero only, so `15 Step.flac` stays a title);
//! - `Artist - Title.ext`, with the artist taken from the stem;
//! - `Artist - Album (Year)/` and `Artist - Album/` release folders, which name
//!   both the artist and the album;
//! - `Artist/Album/`, where the directory nearest the file is the album and the
//!   one above it is the artist;
//! - a single directory before the file (`Artist/01 Title.flac`), which is read
//!   as the artist because a lone directory names an act more often than an
//!   album, and a wrong artist costs more than a missing one;
//! - backslash and forward-slash separators, so a Windows peer path parses like
//!   a Unix one.
//!
//! Directories that never name music (`Music`, `Soulseek Downloads`,
//! `Incomplete`), disc directories (`CD1`, `Disc 2`), host-like segments
//! (`192.168.1.5`) and bracketed suffixes (`[FLAC]`) are skipped. Everything
//! else is left as it is: the parser does not guess, and the scorer renormalizes
//! the fields it could not recover.

use crate::brackets;

/// File extensions that are stripped from a file name.
///
/// A name is only split at its last dot when the part behind it is one of
/// these, so a title that contains a dot (`Dr. Feelgood.flac`) keeps its
/// punctuation and a name without an extension (`01 - Title`) is left intact.
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aifc", "aiff", "alac", "ape", "dff", "dsf", "flac", "m4a", "mka", "mp3", "mp4",
    "mpc", "oga", "ogg", "opus", "tta", "wav", "wave", "wma", "wv",
];

/// Directory names that carry no artist or album information.
///
/// The comparison is case-insensitive and applies to the cleaned segment. The
/// list holds the roots, share markers and format folders real libraries and
/// Soulseek shares use.
const NOISE_SEGMENTS: &[&str] = &[
    "albums",
    "complete",
    "download",
    "downloads",
    "flac",
    "incomplete",
    "lossless",
    "lossy",
    "mp3",
    "music",
    "music library",
    "my music",
    "shared",
    "soulseek",
    "soulseek downloads",
    "various",
    "various artists",
];

/// What a candidate path implies about the track behind it.
///
/// Every field is optional: a file name alone rarely names an album, and a
/// share root rarely names an artist.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::path::derive_tags;
///
/// let tags = derive_tags(r"Music\Air\Moon Safari\01 - La Femme d'Argent.flac");
/// assert_eq!(tags.artist.as_deref(), Some("Air"));
/// assert_eq!(tags.album.as_deref(), Some("Moon Safari"));
/// assert_eq!(tags.title.as_deref(), Some("La Femme d'Argent"));
/// assert_eq!(tags.track_number, Some(1));
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DerivedTags {
    /// The track title, when the file name carries one.
    pub title: Option<String>,
    /// The artist named by the file name or a parent directory.
    pub artist: Option<String>,
    /// The album named by a parent directory.
    pub album: Option<String>,
    /// The track number from the file name.
    pub track_number: Option<u16>,
}

/// Derives the tags a backend path implies.
///
/// The path is split on `/` and `\`, the last segment is parsed as a file name
/// and the segments above it are read as release folders and artist or album
/// directories. Bracketed suffixes are dropped from directory names but kept in
/// the file name, because the scorer needs to see `(Live)` there.
///
/// # Parameters
///
/// - `path`: a backend path such as
///   `Music\Air\Moon Safari\01 - La Femme d'Argent.flac`. An empty path yields
///   no tags.
///
/// # Returns
///
/// The recovered fields; each stays `None` when the heuristic cannot tell.
///
/// # Examples
///
/// ```
/// use hearthsound_acquire::path::derive_tags;
///
/// let tags = derive_tags("/srv/share/Radiohead - In Rainbows (2007) [FLAC]/01 - 15 Step.flac");
/// assert_eq!(tags.artist.as_deref(), Some("Radiohead"));
/// assert_eq!(tags.album.as_deref(), Some("In Rainbows"));
/// assert_eq!(tags.title.as_deref(), Some("15 Step"));
/// assert_eq!(tags.track_number, Some(1));
/// ```
pub fn derive_tags(path: &str) -> DerivedTags {
    let segments: Vec<&str> = path
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();
    let Some((file, directories)) = segments.split_last() else {
        return DerivedTags::default();
    };
    let stem = strip_extension(file);
    let (track_number, rest) = split_track_number(stem);
    let (stem_artist, title) = split_artist_title(&rest);
    let (parent_artist, album) = parent_tags(directories, stem_artist.is_some());
    DerivedTags {
        title: non_empty(title.trim()),
        artist: stem_artist.or(parent_artist),
        album,
        track_number,
    }
}

/// The artist and album one parent directory names.
///
/// A release folder (`Artist - Album`) names both; a bare directory names one,
/// and where it belongs depends on whether another directory is above it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParentName {
    /// The artist, for a release folder.
    artist: Option<String>,
    /// The album, or the whole bare directory name.
    album: Option<String>,
}

/// Returns the artist and album the directories above a file name imply.
///
/// The directories are examined from the file upwards. The first two that name
/// something decide: a release folder somewhere above the file supplies both
/// fields, otherwise the directory nearest the file is the album and the one
/// above it is the artist. When only one directory names anything it is read as
/// the artist, unless the file name already named the artist, in which case it
/// is the album.
///
/// # Parameters
///
/// - `directories`: the path segments above the file, in file order.
/// - `artist_known`: whether the file name already supplied an artist.
fn parent_tags(directories: &[&str], artist_known: bool) -> (Option<String>, Option<String>) {
    let names: Vec<ParentName> = directories
        .iter()
        .rev()
        .filter_map(|segment| parent_name(segment))
        .take(2)
        .collect();
    match names.as_slice() {
        [first, second, ..] => match second.artist.clone() {
            Some(artist) => (Some(artist), second.album.clone()),
            None => {
                let artist = first
                    .artist
                    .clone()
                    .or_else(|| second.artist.clone())
                    .or_else(|| second.album.clone());
                (artist, first.album.clone())
            }
        },
        [only] => {
            if artist_known {
                (None, only.album.clone().or_else(|| only.artist.clone()))
            } else if only.artist.is_some() {
                (only.artist.clone(), only.album.clone())
            } else {
                (only.album.clone(), None)
            }
        }
        [] => (None, None),
    }
}

/// Parses one directory name, or returns `None` when it names nothing usable.
fn parent_name(segment: &str) -> Option<ParentName> {
    let cleaned = collapse_whitespace(&brackets::remove_groups(segment));
    let cleaned = cleaned.trim();
    if cleaned.is_empty()
        || is_noise(cleaned)
        || is_disc_directory(cleaned)
        || is_host_like(cleaned)
    {
        return None;
    }
    match split_artist_title(cleaned) {
        (Some(artist), album) => Some(ParentName {
            artist: Some(artist),
            album: non_empty(&album),
        }),
        (None, name) => Some(ParentName {
            artist: None,
            album: non_empty(&name),
        }),
    }
}

/// Splits `text` into an artist and a title around the first `" - "`.
///
/// A single hyphen without surrounding spaces is part of a name (`Jay-Z`), so
/// only `" - "` separates the two fields.
fn split_artist_title(text: &str) -> (Option<String>, String) {
    match text.find(" - ") {
        Some(index) => {
            let left = text[..index].trim();
            let right = text[index + 3..].trim();
            (non_empty(left), right.to_owned())
        }
        None => (None, text.trim().to_owned()),
    }
}

/// Splits a leading track number from a file stem.
///
/// Only a number that is separated from what follows is a track number: `01 -
/// Title`, `01. Title`, `03.Title`, `1) Title`, `1 - Title` and `01 Title`
/// count, while `15 Step` and `1979` stay titles because neither a separator
/// nor a leading zero says they are track numbers. A stem that is nothing but
/// digits is a track number with no title.
///
/// # Returns
///
/// The track number and the remaining text, or `None` and the trimmed stem when
/// there is no track number.
fn split_track_number(stem: &str) -> (Option<u16>, String) {
    let digits: String = stem.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 3 {
        return (None, stem.trim().to_owned());
    }
    let rest = &stem[digits.len()..];
    if rest.trim().is_empty() {
        return (digits.parse::<u16>().ok(), String::new());
    }
    let immediate = rest.chars().next();
    let after_spaces = rest.trim_start().chars().next();
    let separated = match (immediate, after_spaces) {
        (Some('.' | '-' | '_' | ')'), _) => true,
        (Some(' '), Some('.' | '-' | '_' | ')')) => true,
        (Some(' '), _) => digits.len() > 1 && digits.starts_with('0'),
        _ => false,
    };
    if !separated {
        return (None, stem.trim().to_owned());
    }
    let title = rest.trim_start_matches([' ', '.', '-', '_', ')']);
    (digits.parse::<u16>().ok(), title.trim().to_owned())
}

/// Returns `file` without its extension when the extension is a known audio
/// one.
fn strip_extension(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && AUDIO_EXTENSIONS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(extension)) =>
        {
            stem
        }
        _ => file,
    }
}

/// Returns the trimmed text as a string, or `None` when it is empty.
fn non_empty(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// Returns `text` with its whitespace runs collapsed into single spaces.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Returns whether a cleaned directory name is a share root or format folder.
fn is_noise(segment: &str) -> bool {
    let lowered = segment.to_lowercase();
    NOISE_SEGMENTS.contains(&lowered.as_str())
}

/// Returns whether a cleaned directory name is a disc directory such as `CD1`
/// or `Disc 2`.
fn is_disc_directory(segment: &str) -> bool {
    let lowered = segment.to_lowercase();
    ["cd", "disc", "disk"].iter().any(|prefix| {
        lowered.strip_prefix(prefix).is_some_and(|rest| {
            let rest = rest.trim_start_matches([' ', '-', '_', '.']);
            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
        })
    })
}

/// Returns whether a cleaned directory name looks like a host address or a
/// year rather than a name.
fn is_host_like(segment: &str) -> bool {
    let mut has_digit = false;
    let all_address = segment.chars().all(|c| {
        if c.is_ascii_digit() {
            has_digit = true;
            true
        } else {
            c == '.' || c == ':'
        }
    });
    all_address && has_digit
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the four fields of `derive_tags(path)`.
    fn assert_tags(
        path: &str,
        artist: Option<&str>,
        album: Option<&str>,
        title: Option<&str>,
        track_number: Option<u16>,
    ) {
        let tags = derive_tags(path);
        assert_eq!(tags.artist.as_deref(), artist, "artist of {path}");
        assert_eq!(tags.album.as_deref(), album, "album of {path}");
        assert_eq!(tags.title.as_deref(), title, "title of {path}");
        assert_eq!(tags.track_number, track_number, "track number of {path}");
    }

    #[test]
    fn parses_unix_share_paths() {
        assert_tags(
            "/srv/soulseek/downloads/Music/Air/Moon Safari/01 - La Femme d'Argent.flac",
            Some("Air"),
            Some("Moon Safari"),
            Some("La Femme d'Argent"),
            Some(1),
        );
        assert_tags(
            "Music/Radiohead/In Rainbows/01 - 15 Step.flac",
            Some("Radiohead"),
            Some("In Rainbows"),
            Some("15 Step"),
            Some(1),
        );
        assert_tags(
            "Music/The Beatles/Abbey Road/CD1/07 - Come Together.mp3",
            Some("The Beatles"),
            Some("Abbey Road"),
            Some("Come Together"),
            Some(7),
        );
    }

    #[test]
    fn parses_windows_share_paths() {
        assert_tags(
            r"D:\Soulseek Downloads\complete\Jeff Buckley\Grace\01 - Mojo Pin.flac",
            Some("Jeff Buckley"),
            Some("Grace"),
            Some("Mojo Pin"),
            Some(1),
        );
        assert_tags(
            r"\\192.168.1.5\music\Portishead\Dummy\1 - Mysterons.mp3",
            Some("Portishead"),
            Some("Dummy"),
            Some("Mysterons"),
            Some(1),
        );
    }

    #[test]
    fn parses_release_folders() {
        assert_tags(
            "Music/Radiohead - In Rainbows (2007) [FLAC]/01 - 15 Step.flac",
            Some("Radiohead"),
            Some("In Rainbows"),
            Some("15 Step"),
            Some(1),
        );
        assert_tags(
            "Music/Artist - Album/02. Second Song.mp3",
            Some("Artist"),
            Some("Album"),
            Some("Second Song"),
            Some(2),
        );
        assert_tags(
            "Downloads/Nine Inch Nails - The Downward Spiral/09 - Closer.flac",
            Some("Nine Inch Nails"),
            Some("The Downward Spiral"),
            Some("Closer"),
            Some(9),
        );
    }

    #[test]
    fn parses_flat_file_names() {
        assert_tags(
            "01. Artist - Title.mp3",
            Some("Artist"),
            None,
            Some("Title"),
            Some(1),
        );
        assert_tags(
            "Artist - Title.mp3",
            Some("Artist"),
            None,
            Some("Title"),
            None,
        );
        assert_tags("Title.flac", None, None, Some("Title"), None);
        assert_tags("01 - Title", None, None, Some("Title"), Some(1));
        assert_tags("03.Title.flac", None, None, Some("Title"), Some(3));
        assert_tags("7) Seventh.mp3", None, None, Some("Seventh"), Some(7));
    }

    #[test]
    fn keeps_numbers_that_are_titles() {
        assert_tags("15 Step.flac", None, None, Some("15 Step"), None);
        assert_tags(
            "Music/Radiohead/In Rainbows/15 Step.flac",
            Some("Radiohead"),
            Some("In Rainbows"),
            Some("15 Step"),
            None,
        );
        assert_tags("1979.flac", None, None, Some("1979"), None);
        assert_tags("24K Magic.mp3", None, None, Some("24K Magic"), None);
        assert_tags(
            "Jay-Z - 99 Problems.mp3",
            Some("Jay-Z"),
            None,
            Some("99 Problems"),
            None,
        );
    }

    #[test]
    fn keeps_hyphens_inside_names() {
        assert_tags(
            "Jay-Z/01 - 99 Problems.flac",
            Some("Jay-Z"),
            None,
            Some("99 Problems"),
            Some(1),
        );
        assert_tags(
            "Music/Jay-Z/The Blueprint/01 - The Ruler's Back.mp3",
            Some("Jay-Z"),
            Some("The Blueprint"),
            Some("The Ruler's Back"),
            Some(1),
        );
    }

    #[test]
    fn takes_a_lone_directory_as_the_artist() {
        assert_tags(
            "Music/Jeff Buckley/01 Mojo Pin.flac",
            Some("Jeff Buckley"),
            None,
            Some("Mojo Pin"),
            Some(1),
        );
        assert_tags(
            "Music/Grace/01 Mojo Pin.flac",
            Some("Grace"),
            None,
            Some("Mojo Pin"),
            Some(1),
        );
    }

    #[test]
    fn takes_the_album_from_the_directory_when_the_stem_names_the_artist() {
        assert_tags(
            "Music/Portishead/Dummy/01 - Mysterons.flac",
            Some("Portishead"),
            Some("Dummy"),
            Some("Mysterons"),
            Some(1),
        );
        assert_tags(
            "Music/Portishead/Dummy/Portishead - Mysterons.flac",
            Some("Portishead"),
            Some("Dummy"),
            Some("Mysterons"),
            None,
        );
    }

    #[test]
    fn skips_noise_and_disc_directories() {
        assert_tags(
            "Music/My Music/Soulseek Downloads/Incomplete/Artist/Album/CD 2/01 - Song.mp3",
            Some("Artist"),
            Some("Album"),
            Some("Song"),
            Some(1),
        );
        assert_tags(
            "Albums/Artist - Album/01 - Song.flac",
            Some("Artist"),
            Some("Album"),
            Some("Song"),
            Some(1),
        );
    }

    #[test]
    fn skips_host_segments() {
        assert_tags(
            "192.168.1.5/Music/Artist - Album/01 - Song.flac",
            Some("Artist"),
            Some("Album"),
            Some("Song"),
            Some(1),
        );
        assert_tags("2011/01 - Song.flac", None, None, Some("Song"), Some(1));
    }

    #[test]
    fn strips_extensions_and_keeps_dotted_titles() {
        assert_tags("Dr. Feelgood.flac", None, None, Some("Dr. Feelgood"), None);
        assert_tags("Dr. Feelgood.mp3", None, None, Some("Dr. Feelgood"), None);
        assert_tags("Title.unknown", None, None, Some("Title.unknown"), None);
        assert_tags("", None, None, None, None);
    }

    #[test]
    fn drops_brackets_from_directories_only() {
        assert_tags(
            "Music/Artist - Album (2011) [FLAC 24-96]/01 - Song (Live).flac",
            Some("Artist"),
            Some("Album"),
            Some("Song (Live)"),
            Some(1),
        );
    }

    #[test]
    fn parses_brackets_in_share_markers() {
        assert_tags(
            "Music/@#!$abc (share)/Artist/Album/01 - Song.mp3",
            Some("Artist"),
            Some("Album"),
            Some("Song"),
            Some(1),
        );
    }

    #[test]
    fn track_number_edges() {
        assert_eq!(
            split_track_number("01 - Title"),
            (Some(1), "Title".to_owned())
        );
        assert_eq!(
            split_track_number("1. Title"),
            (Some(1), "Title".to_owned())
        );
        assert_eq!(
            split_track_number("100 Title"),
            (None, "100 Title".to_owned())
        );
        assert_eq!(
            split_track_number("001 - Title"),
            (Some(1), "Title".to_owned())
        );
        assert_eq!(
            split_track_number("1000 - Title"),
            (None, "1000 - Title".to_owned())
        );
        assert_eq!(split_track_number("02"), (Some(2), String::new()));
        assert_eq!(split_track_number("Song"), (None, "Song".to_owned()));
    }

    #[test]
    fn extension_edges() {
        assert_eq!(strip_extension("Song.FLAC"), "Song");
        assert_eq!(strip_extension("Song"), "Song");
        assert_eq!(strip_extension("Song.txt"), "Song.txt");
        assert_eq!(strip_extension(".flac"), ".flac");
    }
}
