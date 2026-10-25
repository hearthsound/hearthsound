//! The typed rows and write inputs of the library tables.
//!
//! Every table the library owns has one row type here, mirroring the columns of
//! `crates/db/migrations/0001_initial.sql`, and one input type carrying the
//! fields a writer supplies. The row types are read back through
//! [`crate::repository`]; the input types are what a scanner, an importer, or
//! the catalog layer hands to a write function.
//!
//! Identifiers are [`EntityId`]s for the three catalog entity tables (`artists`,
//! `albums`, `tracks`) because those are the rows a client can name. Library
//! folders and media files have no client-facing id, so they use a plain
//! [`Uuid`] primary key.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use hearthsound_core::{Availability, EntityId, IdPrefix, QualityTier, StorageClass};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Folds text into the ordering key stored in the `sort_*` columns.
///
/// The key is the trimmed, Unicode-lowercased, whitespace-collapsed form of the
/// text, so `The  Beatles` and `the beatles` share one key. The key is also what
/// the artist and album dedupe compares, so it must stay stable for a given
/// name.
///
/// # Parameters
///
/// - `text`: the display text.
///
/// # Returns
///
/// The folded key, which is empty when the text holds only whitespace.
///
/// # Examples
///
/// ```
/// use hearthsound_library::sort_key;
///
/// assert_eq!(sort_key("  Caf\u{e9}   del Mar "), "caf\u{e9} del mar");
/// ```
pub fn sort_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.trim().chars() {
        for folded in ch.to_lowercase() {
            if folded.is_whitespace() {
                pending_space = !out.is_empty();
            } else {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                out.push(folded);
            }
        }
    }
    out
}

/// Where an entity row came from.
///
/// The literals are the values stored in `artists.origin`, `albums.origin`, and
/// `tracks.origin`, so they must not be renamed.
///
/// # Examples
///
/// ```
/// use hearthsound_library::Origin;
///
/// assert_eq!(Origin::Catalog.as_str(), "catalog");
/// assert_eq!("local".parse::<Origin>().unwrap(), Origin::Local);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// Created by scanning a folder or ingesting an upload.
    Local,
    /// Materialized from a catalog provider.
    Catalog,
}

impl Origin {
    /// Every origin, in a fixed order.
    pub const ALL: [Origin; 2] = [Origin::Local, Origin::Catalog];

    /// Returns the literal stored in the `origin` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Origin::Local => "local",
            Origin::Catalog => "catalog",
        }
    }

    /// Parses an origin from its literal.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownOrigin`] when `value` is not one of the literals
    /// produced by [`Origin::as_str`].
    pub fn from_literal(value: &str) -> Result<Origin, UnknownOrigin> {
        Origin::ALL
            .iter()
            .copied()
            .find(|origin| origin.as_str() == value)
            .ok_or_else(|| UnknownOrigin(value.to_owned()))
    }
}

impl fmt::Display for Origin {
    /// Writes the literal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`Origin`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown origin `{0}`")]
pub struct UnknownOrigin(pub String);

impl FromStr for Origin {
    type Err = UnknownOrigin;

    /// Parses an origin from its literal.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownOrigin`] when `s` is not a known origin.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Origin::from_literal(s)
    }
}

/// The managed storage roots a media file's path is resolved against.
///
/// A `library` or `cache` media file stores a path relative to one of these
/// roots; a `folder` media file stores a path relative to its library folder.
///
/// # Examples
///
/// ```
/// use std::path::{Path, PathBuf};
///
/// use hearthsound_library::StorageRoots;
///
/// let roots = StorageRoots::new(Path::new("/srv/library"), Path::new("/srv/cache"));
/// assert_eq!(roots.library, PathBuf::from("/srv/library"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoots {
    /// The root of permanent managed storage, `<storage>/library`.
    pub library: PathBuf,
    /// The root of evictable managed storage, `<storage>/cache`.
    pub cache: PathBuf,
}

impl StorageRoots {
    /// Builds the roots from the two directories.
    ///
    /// # Parameters
    ///
    /// - `library`: the permanent managed storage root.
    /// - `cache`: the evictable managed storage root.
    pub fn new(library: impl Into<PathBuf>, cache: impl Into<PathBuf>) -> StorageRoots {
        StorageRoots {
            library: library.into(),
            cache: cache.into(),
        }
    }

    /// Returns the root a media file of `storage` lives under.
    ///
    /// # Parameters
    ///
    /// - `storage`: the media file's storage class.
    ///
    /// # Returns
    ///
    /// The `library` root for [`StorageClass::Library`] and the `cache` root for
    /// [`StorageClass::Cache`]. A [`StorageClass::Folder`] file has no managed
    /// root; it resolves against its library folder instead, so this returns the
    /// `library` root only as a harmless fallback.
    pub fn root_for(&self, storage: StorageClass) -> &Path {
        match storage {
            StorageClass::Cache => &self.cache,
            StorageClass::Folder | StorageClass::Library => &self.library,
        }
    }
}

/// One row of the `artists` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artist {
    /// The artist's client id.
    pub id: EntityId,
    /// The artist's display name.
    pub name: String,
    /// The ordering key derived from the name.
    pub sort_name: String,
    /// The MusicBrainz artist id, when known.
    pub mb_artist_id: Option<String>,
    /// Where the row came from.
    pub origin: Origin,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

/// One row of the `albums` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Album {
    /// The album's client id.
    pub id: EntityId,
    /// The album's title.
    pub title: String,
    /// The ordering key derived from the title.
    pub sort_title: String,
    /// The album artist as it is displayed.
    pub display_artist: Option<String>,
    /// The release year, when known.
    pub year: Option<i64>,
    /// The release date as the catalog reported it, when known.
    pub release_date: Option<String>,
    /// The release type, for example `album` or `single`, when known.
    pub album_type: Option<String>,
    /// The MusicBrainz release id, when known.
    pub mb_release_id: Option<String>,
    /// The MusicBrainz release-group id, when known.
    pub mb_release_group_id: Option<String>,
    /// The release UPC, when known.
    pub upc: Option<String>,
    /// Where the row came from.
    pub origin: Origin,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

/// One row of the `tracks` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The track's client id.
    pub id: EntityId,
    /// The album the track belongs to, when it has one.
    pub album_id: Option<EntityId>,
    /// The track title.
    pub title: String,
    /// The ordering key derived from the title.
    pub sort_title: String,
    /// The track artist as it is displayed.
    pub display_artist: Option<String>,
    /// The disc number within the release.
    pub disc_number: Option<i64>,
    /// The track number within its disc.
    pub track_number: Option<i64>,
    /// The duration in milliseconds, when known.
    pub duration_ms: Option<i64>,
    /// The International Standard Recording Code, when known.
    pub isrc: Option<String>,
    /// The MusicBrainz recording id, when known.
    pub mb_recording_id: Option<String>,
    /// Whether the track is explicit, when the tags say.
    pub explicit: Option<bool>,
    /// Where the row came from.
    pub origin: Origin,
    /// The track's availability on this server.
    pub availability: Availability,
    /// The instant before which an `unavailable` track is not retried, in unix
    /// milliseconds.
    pub unavailable_until: Option<i64>,
    /// When the row was created, in unix milliseconds.
    pub created_at: i64,
    /// When the row was last written, in unix milliseconds.
    pub updated_at: i64,
}

/// One row of the `genres` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genre {
    /// The genre's id.
    pub id: Uuid,
    /// The case-folded genre name, unique across the table.
    pub name: String,
}

/// One row of the `library_folders` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFolder {
    /// The folder's id.
    pub id: Uuid,
    /// The canonical absolute path of the folder on disk.
    pub path: PathBuf,
    /// The folder's display name.
    pub name: String,
    /// The user whose personal library the folder's content belongs to, or
    /// `None` when the folder is generic.
    pub owner_user_id: Option<Uuid>,
    /// Whether a watcher should watch the folder for changes.
    pub watch: bool,
    /// When the folder was added, in unix milliseconds.
    pub created_at: i64,
    /// When the folder was last scanned, in unix milliseconds. A scan that
    /// failed leaves it unchanged.
    pub last_scan_at: Option<i64>,
    /// Why the folder's last scan failed or ended with a warning, when it did.
    /// `None` after a clean scan.
    pub last_scan_error: Option<String>,
}

/// What removing a library folder would take with it.
///
/// The counts describe the delete before it happens: `remove_library_folder`
/// deletes the tracks that only a folder's files held, and a track's likes, saves,
/// ratings, plays and playlist entries go with it. The counts come from the same
/// predicate the delete uses, so a preview cannot disagree with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderRemovalImpact {
    /// How many tracks would be deleted from the library.
    pub tracks: i64,
    /// How many likes of those tracks would be lost.
    pub likes: i64,
    /// How many playlist entries of those tracks would be lost.
    pub playlist_entries: i64,
}

/// One row of the `media_files` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    /// The media file's id.
    pub id: Uuid,
    /// The track the file belongs to.
    pub track_id: EntityId,
    /// Where the file lives.
    pub storage: StorageClass,
    /// The library folder the file lives in, for a `folder` file.
    pub folder_id: Option<Uuid>,
    /// The path relative to the folder root or the managed storage root.
    pub rel_path: String,
    /// The file size in bytes.
    pub size: i64,
    /// The file modification time, in unix milliseconds.
    pub mtime_ms: i64,
    /// The container literal the probe found.
    pub format: Option<String>,
    /// The codec literal the probe found.
    pub codec: Option<String>,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: Option<i64>,
    /// The sample rate in hertz.
    pub sample_rate: Option<i64>,
    /// The sample bit depth.
    pub bit_depth: Option<i64>,
    /// The channel count.
    pub channels: Option<i64>,
    /// The duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Whether the codec stores samples without loss.
    pub lossless: Option<bool>,
    /// The quality tier scoring assigned the file.
    pub quality_tier: Option<QualityTier>,
    /// The ReplayGain values as a JSON object.
    pub replaygain: Option<String>,
    /// Whether the file carries an embedded picture.
    pub has_embedded_art: Option<bool>,
    /// When the row was added, in unix milliseconds.
    pub added_at: i64,
    /// When the file was last streamed, in unix milliseconds.
    pub last_accessed_at: Option<i64>,
    /// The user that uploaded the file, for an upload.
    pub uploaded_by: Option<Uuid>,
    /// The acquisition that produced the file, for an acquired file.
    pub acquisition_id: Option<Uuid>,
    /// Whether this is the file the stream service serves for the track.
    pub is_preferred: bool,
}

/// The ReplayGain values a media file carries.
///
/// A file may carry any subset of the four tags, so every value is optional. The
/// values are stored as a JSON object in the `media_files.replaygain` column,
/// which is `NULL` when the file carries none.
///
/// # Examples
///
/// ```
/// use hearthsound_library::ReplayGain;
///
/// let gain = ReplayGain {
///     track_gain: Some(-6.54),
///     track_peak: Some(0.988861),
///     album_gain: None,
///     album_peak: None,
/// };
/// assert_eq!(gain.to_json().as_deref(), Some("{\"trackGain\":-6.54,\"trackPeak\":0.988861}"));
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGain {
    /// The track gain in decibels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_gain: Option<f64>,
    /// The track peak, a linear sample value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_peak: Option<f64>,
    /// The album gain in decibels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_gain: Option<f64>,
    /// The album peak, a linear sample value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_peak: Option<f64>,
}

impl ReplayGain {
    /// Reads the ReplayGain values out of a file's tags.
    ///
    /// Each tag is read as the float it starts with, so `-6.54 dB` and
    /// `0.988861` both parse and a value written some other way is left out.
    /// Non-finite values are rejected, so the result always serializes.
    ///
    /// # Parameters
    ///
    /// - `tags`: the tags the probe read.
    ///
    /// # Returns
    ///
    /// The values, or `None` when no tag carried a readable one.
    pub fn from_tags(tags: &hearthsound_media::TrackTags) -> Option<ReplayGain> {
        let gain = ReplayGain {
            track_gain: leading_float(tags.replaygain_track_gain.as_deref()),
            track_peak: leading_float(tags.replaygain_track_peak.as_deref()),
            album_gain: leading_float(tags.replaygain_album_gain.as_deref()),
            album_peak: leading_float(tags.replaygain_album_peak.as_deref()),
        };
        if gain.is_empty() { None } else { Some(gain) }
    }

    /// Reports whether no value is set.
    ///
    /// # Returns
    ///
    /// `true` when all four values are `None`.
    pub fn is_empty(&self) -> bool {
        self.track_gain.is_none()
            && self.track_peak.is_none()
            && self.album_gain.is_none()
            && self.album_peak.is_none()
    }

    /// Serializes the values into the JSON object the `media_files` column
    /// holds.
    ///
    /// # Returns
    ///
    /// The JSON object, or `None` when no value is set.
    pub fn to_json(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        serde_json::to_string(self).ok()
    }

    /// Parses the JSON object stored in `media_files.replaygain`.
    ///
    /// # Parameters
    ///
    /// - `json`: the stored object.
    ///
    /// # Returns
    ///
    /// The values, or `None` when the object holds none or cannot be parsed.
    pub fn from_json(json: &str) -> Option<ReplayGain> {
        let gain: ReplayGain = serde_json::from_str(json).ok()?;
        if gain.is_empty() { None } else { Some(gain) }
    }
}

/// Reads the float a ReplayGain tag starts with.
///
/// # Parameters
///
/// - `value`: the tag value, for example `-6.54 dB`.
///
/// # Returns
///
/// The leading finite float, or `None` when the value does not start with one.
fn leading_float(value: Option<&str>) -> Option<f64> {
    let value = value?.trim();
    let end = value
        .char_indices()
        .find(|(_, ch)| !(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+')))
        .map(|(index, _)| index)
        .unwrap_or(value.len());
    value
        .get(..end)?
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

/// One artist credit of an album or a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    /// The credited artist's client id.
    pub artist_id: EntityId,
    /// The zero-based credit position.
    pub position: i64,
    /// The credit role, for example `main` or `featured`.
    pub role: String,
}

/// The fields a writer supplies for an artist.
///
/// # Examples
///
/// ```
/// use hearthsound_library::{ArtistInput, Origin};
///
/// let input = ArtistInput::new("Energy 52");
/// assert_eq!(input.name, "Energy 52");
///
/// let catalog = ArtistInput::new("Energy 52").origin(Origin::Catalog);
/// assert_eq!(catalog.origin, Origin::Catalog);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistInput {
    /// The artist's display name.
    pub name: String,
    /// The ordering key, derived from the name when omitted.
    pub sort_name: Option<String>,
    /// The MusicBrainz artist id, when known.
    pub mb_artist_id: Option<String>,
    /// Where the row comes from.
    pub origin: Origin,
}

impl ArtistInput {
    /// Builds an input with only a name.
    ///
    /// The origin is [`Origin::Local`], which is what a scan or an upload
    /// writes; a caller materializing a catalog entity sets
    /// [`ArtistInput::origin`] instead.
    ///
    /// # Parameters
    ///
    /// - `name`: the artist's display name.
    pub fn new(name: impl Into<String>) -> ArtistInput {
        ArtistInput {
            name: name.into(),
            sort_name: None,
            mb_artist_id: None,
            origin: Origin::Local,
        }
    }

    /// Sets the MusicBrainz artist id.
    ///
    /// # Parameters
    ///
    /// - `mb_artist_id`: the MBID, when known.
    pub fn mb_artist_id(mut self, mb_artist_id: impl Into<String>) -> ArtistInput {
        self.mb_artist_id = Some(mb_artist_id.into());
        self
    }

    /// Sets where the row comes from.
    ///
    /// # Parameters
    ///
    /// - `origin`: [`Origin::Local`] for a scanned or uploaded artist,
    ///   [`Origin::Catalog`] for one materialized from a catalog provider.
    pub fn origin(mut self, origin: Origin) -> ArtistInput {
        self.origin = origin;
        self
    }
}

/// An artist input with the role it is credited under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCredit {
    /// The artist to upsert.
    pub artist: ArtistInput,
    /// The role the artist is credited under, for example `main`.
    pub role: String,
}

impl ArtistCredit {
    /// Builds a credit for an artist name.
    ///
    /// # Parameters
    ///
    /// - `name`: the credited artist's name.
    /// - `role`: the credit role.
    pub fn new(name: impl Into<String>, role: impl Into<String>) -> ArtistCredit {
        ArtistCredit {
            artist: ArtistInput::new(name),
            role: role.into(),
        }
    }
}

/// The fields a writer supplies for an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumInput {
    /// The album's title.
    pub title: String,
    /// The ordering key, derived from the title when omitted.
    pub sort_title: Option<String>,
    /// The album artist as it is displayed.
    pub display_artist: Option<String>,
    /// The release year, when known.
    pub year: Option<i64>,
    /// The release date as the catalog reported it, when known.
    pub release_date: Option<String>,
    /// The release type, when known.
    pub album_type: Option<String>,
    /// The MusicBrainz release id, when known.
    pub mb_release_id: Option<String>,
    /// The MusicBrainz release-group id, when known.
    pub mb_release_group_id: Option<String>,
    /// The release UPC, when known.
    pub upc: Option<String>,
    /// Where the row comes from.
    pub origin: Origin,
}

impl AlbumInput {
    /// Builds an input for a title.
    ///
    /// # Parameters
    ///
    /// - `title`: the album title.
    /// - `origin`: where the row comes from.
    pub fn new(title: impl Into<String>, origin: Origin) -> AlbumInput {
        AlbumInput {
            title: title.into(),
            sort_title: None,
            display_artist: None,
            year: None,
            release_date: None,
            album_type: None,
            mb_release_id: None,
            mb_release_group_id: None,
            upc: None,
            origin,
        }
    }
}

/// The fields a writer supplies for a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInput {
    /// The album the track belongs to, when it has one.
    pub album_id: Option<EntityId>,
    /// The track title.
    pub title: String,
    /// The ordering key, derived from the title when omitted.
    pub sort_title: Option<String>,
    /// The track artist as it is displayed.
    pub display_artist: Option<String>,
    /// The disc number within the release.
    pub disc_number: Option<i64>,
    /// The track number within its disc.
    pub track_number: Option<i64>,
    /// The duration in milliseconds, when known.
    pub duration_ms: Option<i64>,
    /// The International Standard Recording Code, when known.
    pub isrc: Option<String>,
    /// The MusicBrainz recording id, when known.
    pub mb_recording_id: Option<String>,
    /// Whether the track is explicit, when the tags say.
    pub explicit: Option<bool>,
    /// Where the row comes from.
    pub origin: Origin,
    /// The track's availability on this server.
    pub availability: Availability,
    /// The instant before which an `unavailable` track is not retried, in unix
    /// milliseconds.
    pub unavailable_until: Option<i64>,
}

impl TrackInput {
    /// Builds an input for a title and an origin.
    ///
    /// # Parameters
    ///
    /// - `title`: the track title.
    /// - `origin`: where the row comes from.
    /// - `availability`: the track's availability.
    pub fn new(title: impl Into<String>, origin: Origin, availability: Availability) -> TrackInput {
        TrackInput {
            album_id: None,
            title: title.into(),
            sort_title: None,
            display_artist: None,
            disc_number: None,
            track_number: None,
            duration_ms: None,
            isrc: None,
            mb_recording_id: None,
            explicit: None,
            origin,
            availability,
            unavailable_until: None,
        }
    }
}

/// The fields a writer supplies for a media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFileInput {
    /// The track the file belongs to.
    pub track_id: EntityId,
    /// Where the file lives.
    pub storage: StorageClass,
    /// The library folder the file lives in, for a `folder` file.
    pub folder_id: Option<Uuid>,
    /// The path relative to the folder root or the managed storage root.
    pub rel_path: String,
    /// The file size in bytes.
    pub size: i64,
    /// The file modification time, in unix milliseconds.
    pub mtime_ms: i64,
    /// The container literal the probe found.
    pub format: Option<String>,
    /// The codec literal the probe found.
    pub codec: Option<String>,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: Option<i64>,
    /// The sample rate in hertz.
    pub sample_rate: Option<i64>,
    /// The sample bit depth.
    pub bit_depth: Option<i64>,
    /// The channel count.
    pub channels: Option<i64>,
    /// The duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Whether the codec stores samples without loss.
    pub lossless: Option<bool>,
    /// The quality tier scoring assigned the file.
    pub quality_tier: Option<QualityTier>,
    /// The ReplayGain values as a JSON object.
    pub replaygain: Option<String>,
    /// Whether the file carries an embedded picture.
    pub has_embedded_art: Option<bool>,
    /// The user that uploaded the file, for an upload.
    pub uploaded_by: Option<Uuid>,
    /// The acquisition that produced the file, for an acquired file.
    pub acquisition_id: Option<Uuid>,
}

/// A `folder` media file found by a scan.
///
/// The scanner builds one per recognised audio file it probes and hands it to
/// [`crate::media_files::upsert_folder_media_file`], which inserts the row the
/// first time and refreshes it when the file changed.
#[derive(Debug, Clone, PartialEq)]
pub struct ScannedMediaFile {
    /// The library folder the file lives in.
    pub folder_id: Uuid,
    /// The path relative to the folder root.
    pub rel_path: String,
    /// The file size in bytes.
    pub size: i64,
    /// The file modification time, in unix milliseconds.
    pub mtime_ms: i64,
    /// The container literal the probe found.
    pub format: Option<String>,
    /// The codec literal the probe found.
    pub codec: Option<String>,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: Option<i64>,
    /// The sample rate in hertz.
    pub sample_rate: Option<i64>,
    /// The sample bit depth.
    pub bit_depth: Option<i64>,
    /// The channel count.
    pub channels: Option<i64>,
    /// The duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Whether the codec stores samples without loss.
    pub lossless: Option<bool>,
    /// The ReplayGain values the file's tags carried.
    pub replaygain: Option<ReplayGain>,
    /// Whether the file carries an embedded picture.
    pub has_embedded_art: Option<bool>,
}

/// A managed (`library` or `cache`) media file to insert.
///
/// This is the shape the import pipeline writes: the path is relative to
/// [`StorageRoots::library`] or [`StorageRoots::cache`], and the folder is
/// always `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedMediaFile {
    /// The track the file belongs to.
    pub track_id: EntityId,
    /// Whether the file is permanent (`library`) or evictable (`cache`).
    pub storage: StorageClass,
    /// The path relative to the managed storage root.
    pub rel_path: String,
    /// The file size in bytes.
    pub size: i64,
    /// The file modification time, in unix milliseconds.
    pub mtime_ms: i64,
    /// The container literal the probe found.
    pub format: Option<String>,
    /// The codec literal the probe found.
    pub codec: Option<String>,
    /// The bitrate in kbit/s.
    pub bitrate_kbps: Option<i64>,
    /// The sample rate in hertz.
    pub sample_rate: Option<i64>,
    /// The sample bit depth.
    pub bit_depth: Option<i64>,
    /// The channel count.
    pub channels: Option<i64>,
    /// The duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// Whether the codec stores samples without loss.
    pub lossless: Option<bool>,
    /// The quality tier scoring assigned the file.
    pub quality_tier: Option<QualityTier>,
    /// The ReplayGain values as a JSON object.
    pub replaygain: Option<String>,
    /// Whether the file carries an embedded picture.
    pub has_embedded_art: Option<bool>,
    /// The user that uploaded the file, for an upload.
    pub uploaded_by: Option<Uuid>,
    /// The acquisition that produced the file, for an acquired file.
    pub acquisition_id: Option<Uuid>,
}

/// Where a media file's path is resolved from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaRoot {
    /// The file is relative to a library folder's path.
    Folder,
    /// The file is relative to [`StorageRoots::library`].
    Library,
    /// The file is relative to [`StorageRoots::cache`].
    Cache,
}

impl From<StorageClass> for MediaRoot {
    /// Maps a storage class onto the root it resolves against.
    fn from(storage: StorageClass) -> MediaRoot {
        match storage {
            StorageClass::Folder => MediaRoot::Folder,
            StorageClass::Library => MediaRoot::Library,
            StorageClass::Cache => MediaRoot::Cache,
        }
    }
}

impl MediaFile {
    /// Resolves the file's absolute path.
    ///
    /// # Parameters
    ///
    /// - `folder_path`: the path of the library folder, for a `folder` file.
    /// - `roots`: the managed storage roots.
    ///
    /// # Returns
    ///
    /// The absolute path: `folder_path` joined with the relative path for a
    /// `folder` file, otherwise the matching managed root joined with it. A
    /// `folder` file with no `folder_path` falls back to the library root so the
    /// function is total; the caller that resolves a scanned file always has the
    /// folder path.
    pub fn absolute_path(&self, folder_path: Option<&Path>, roots: &StorageRoots) -> PathBuf {
        let base = match (self.storage, folder_path) {
            (StorageClass::Folder, Some(path)) => path,
            (StorageClass::Folder, None) => roots.library.as_path(),
            (storage, _) => roots.root_for(storage),
        };
        base.join(Path::new(&self.rel_path))
    }

    /// Returns the file's extension, lowercased, without the leading dot.
    ///
    /// # Returns
    ///
    /// The extension of the relative path, or an empty string when it has none.
    pub fn suffix(&self) -> String {
        Path::new(&self.rel_path)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default()
    }

    /// Parses the ReplayGain object stored for the file.
    ///
    /// # Returns
    ///
    /// The values, or `None` when the file carries none.
    pub fn replay_gain(&self) -> Option<ReplayGain> {
        self.replaygain.as_deref().and_then(ReplayGain::from_json)
    }
}

/// Decodes a 16-byte BLOB primary key.
///
/// # Parameters
///
/// - `bytes`: the stored bytes.
///
/// # Returns
///
/// The UUID.
///
/// # Errors
///
/// Returns [`uuid::Error`] when the slice is not exactly 16 bytes.
pub(crate) fn decode_uuid(bytes: &[u8]) -> Result<Uuid, uuid::Error> {
    Uuid::from_slice(bytes)
}

/// Returns an entity's primary key bytes as an owned vector for a query bind.
///
/// Binding an owned vector rather than a slice of a temporary keeps the
/// compile-checked query macros free of borrow-lifetime noise.
///
/// # Parameters
///
/// - `id`: the entity's client id.
///
/// # Returns
///
/// The 16 raw bytes of the id's UUIDv7.
pub(crate) fn entity_key(id: EntityId) -> Vec<u8> {
    id.uuid().as_bytes().to_vec()
}

/// Wraps a stored BLOB as an [`EntityId`] of `prefix`.
///
/// # Parameters
///
/// - `prefix`: the record kind the id names.
/// - `bytes`: the stored 16-byte primary key.
///
/// # Returns
///
/// The client id.
///
/// # Errors
///
/// Returns [`uuid::Error`] when the slice is not exactly 16 bytes.
pub(crate) fn decode_entity(prefix: IdPrefix, bytes: &[u8]) -> Result<EntityId, uuid::Error> {
    Ok(EntityId::from_uuid(prefix, decode_uuid(bytes)?))
}
