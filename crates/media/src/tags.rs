//! The track tag set the server reads from and writes into media files.
//!
//! The fields are exactly the tags the import pipeline writes (see
//! `06-acquisition-and-playback.md` section 10.1): the display tags, the
//! Picard MusicBrainz identifiers, the ReplayGain values, and the
//! `HEARTHSOUND_ACQUIRED_VIA` marker. One type serves both directions, so a
//! probe result can be written back to another file without translation, and a
//! round trip through a file is directly comparable.

use std::path::{Path, PathBuf};

use crate::error::MediaError;
use hearthsound_core::Slug;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};

/// The tags of a track.
///
/// Every field is optional because tag formats differ: a file may carry a title
/// and nothing else. A field is only written when it holds a value, and a
/// missing field means "not present in the file" when read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackTags {
    /// The track title.
    pub title: Option<String>,
    /// The performing artists, in order.
    pub artists: Vec<String>,
    /// The album title.
    pub album: Option<String>,
    /// The album artist.
    pub album_artist: Option<String>,
    /// The track number within its disc.
    pub track_number: Option<u32>,
    /// The number of tracks on the disc.
    pub track_total: Option<u32>,
    /// The disc number.
    pub disc_number: Option<u32>,
    /// The number of discs.
    pub disc_total: Option<u32>,
    /// The release date as stored in the file, for example `2019` or
    /// `2019-04-12`.
    pub date: Option<String>,
    /// The genres, in order.
    pub genres: Vec<String>,
    /// The International Standard Recording Code.
    pub isrc: Option<String>,
    /// The MusicBrainz recording id, written as `MUSICBRAINZ_TRACKID`.
    pub mb_recording_id: Option<String>,
    /// The MusicBrainz release id, written as `MUSICBRAINZ_ALBUMID`.
    pub mb_release_id: Option<String>,
    /// The MusicBrainz release group id, written as
    /// `MUSICBRAINZ_RELEASEGROUPID`.
    pub mb_release_group_id: Option<String>,
    /// The MusicBrainz artist ids, written as `MUSICBRAINZ_ARTISTID`, one per
    /// credited artist in the same order as [`artists`].
    ///
    /// [`artists`]: TrackTags::artists
    pub mb_artist_ids: Vec<String>,
    /// The MusicBrainz album artist ids, written as
    /// `MUSICBRAINZ_ALBUMARTISTID`, one per credited album artist in the same
    /// order as [`album_artist`].
    ///
    /// [`album_artist`]: TrackTags::album_artist
    pub mb_album_artist_ids: Vec<String>,
    /// Whether the track belongs to a compilation, written as the
    /// `COMPILATION` flag (`TCMP` for ID3v2, `cpil` for MP4).
    pub compilation: Option<bool>,
    /// The ReplayGain track gain, for example `-3.21 dB`.
    pub replaygain_track_gain: Option<String>,
    /// The ReplayGain track peak.
    pub replaygain_track_peak: Option<String>,
    /// The ReplayGain album gain.
    pub replaygain_album_gain: Option<String>,
    /// The ReplayGain album peak.
    pub replaygain_album_peak: Option<String>,
    /// The slug of the plugin instance the file was acquired through, written
    /// as the `HEARTHSOUND_ACQUIRED_VIA` tag.
    ///
    /// The tag writer writes it; the probe does not read it back, because the
    /// acquiring instance is core bookkeeping rather than library metadata.
    pub acquired_via: Option<Slug>,
}

impl TrackTags {
    /// Returns the release year parsed from the beginning of [`date`].
    ///
    /// # Returns
    ///
    /// The first four ASCII digits of the date, when it starts with four of
    /// them.
    ///
    /// [`date`]: TrackTags::date
    pub fn year(&self) -> Option<u32> {
        let date = self.date.as_deref()?;
        let digits = date.get(..4)?;
        if digits.len() == 4 && digits.bytes().all(|byte| byte.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    }

    /// Reports whether no tag holds a value.
    ///
    /// # Returns
    ///
    /// `true` when every field is empty, so writing the tags would not change
    /// the file.
    pub fn is_empty(&self) -> bool {
        self == &TrackTags::default()
    }
}

/// An image associated with a track or an album.
///
/// The bytes are the encoded image file, exactly as they were read from disk or
/// from an embedded picture, so a caller can write them to a sidecar file or
/// embed them without re-encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    /// The image MIME type, for example `image/jpeg`.
    mime_type: String,
    /// The encoded image bytes.
    data: Vec<u8>,
}

impl Artwork {
    /// Builds an image from its MIME type and bytes.
    ///
    /// # Parameters
    ///
    /// - `mime_type`: the image MIME type, for example `image/png`.
    /// - `data`: the encoded image bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_media::Artwork;
    ///
    /// let cover = Artwork::new("image/jpeg", vec![0xff, 0xd8, 0xff]);
    /// assert_eq!(cover.mime_type(), "image/jpeg");
    /// assert_eq!(cover.data(), [0xff, 0xd8, 0xff]);
    /// ```
    pub fn new(mime_type: impl Into<String>, data: impl Into<Vec<u8>>) -> Artwork {
        Artwork {
            mime_type: mime_type.into(),
            data: data.into(),
        }
    }

    /// Builds a PNG image.
    ///
    /// # Parameters
    ///
    /// - `data`: the encoded PNG bytes.
    pub fn png(data: impl Into<Vec<u8>>) -> Artwork {
        Artwork::new("image/png", data)
    }

    /// Builds a JPEG image.
    ///
    /// # Parameters
    ///
    /// - `data`: the encoded JPEG bytes.
    pub fn jpeg(data: impl Into<Vec<u8>>) -> Artwork {
        Artwork::new("image/jpeg", data)
    }

    /// Returns the image MIME type.
    pub fn mime_type(&self) -> &str {
        &self.mime_type
    }

    /// Returns the encoded image bytes.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consumes the image and returns its bytes.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// The tag key the acquiring instance slug is written under.
///
/// The name is the server's own marker, so it is written as a custom key: a
/// Vorbis comment for FLAC and Ogg, a `TXXX` frame with this description for
/// ID3v2, a `----:com.apple.iTunes:HEARTHSOUND_ACQUIRED_VIA` freeform atom for
/// MP4, and an APE item for WavPack.
pub const ACQUIRED_VIA_KEY: &str = "HEARTHSOUND_ACQUIRED_VIA";

/// The MP4 freeform mean the custom keys are written under.
///
/// MP4 has no place for arbitrary atoms, so taggers agree on the
/// `com.apple.iTunes` mean; every reader that understands freeform atoms
/// (including `lofty`) looks there.
const MP4_FREEFORM_MEAN: &str = "com.apple.iTunes";

/// Writes the tag set into an audio file.
///
/// The file's container decides where the tags land: Vorbis comments for FLAC
/// and Ogg, ID3v2 for MPEG, AAC, WAV and AIFF, `ilst` for MP4, and an APE tag
/// for WavPack, Monkey's Audio and Musepack. Writing replaces the values of the
/// keys the set carries and leaves every other tag type in the file alone.
///
/// The work is blocking (`lofty` rewrites the file) and runs on a blocking
/// thread.
///
/// # Parameters
///
/// - `path`: the file to tag; it must already exist in a supported container.
/// - `tags`: the tags to write; empty fields are skipped.
/// - `cover`: an optional cover to embed as a front cover picture.
///
/// # Returns
///
/// `Ok(())` once the file was rewritten.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read or written,
/// [`MediaError::Tag`] when a tag cannot be encoded, and
/// [`MediaError::Unsupported`] for a container the server does not tag.
pub async fn write_tags(
    path: &Path,
    tags: &TrackTags,
    cover: Option<&Artwork>,
) -> Result<(), MediaError> {
    let path = path.to_path_buf();
    let tags = tags.clone();
    let cover = cover.cloned();
    tokio::task::spawn_blocking(move || write_tags_blocking(&path, &tags, cover.as_ref()))
        .await
        .map_err(MediaError::task_failed)?
}

/// Writes the tag set on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to tag.
/// - `tags`: the tags to write.
/// - `cover`: an optional cover to embed.
///
/// # Errors
///
/// Returns the same errors as [`write_tags`].
fn write_tags_blocking(
    path: &Path,
    tags: &TrackTags,
    cover: Option<&Artwork>,
) -> Result<(), MediaError> {
    use lofty::ape::{ApeItem, ApeTag};
    use lofty::config::WriteOptions;
    use lofty::file::FileType;
    use lofty::id3::v2::Id3v2Tag;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::ogg::tag::VorbisComments;
    use lofty::prelude::TagExt;

    let options = WriteOptions::default();
    let file_type = crate::probe::detect_file_type(path)?;
    match file_type {
        FileType::Flac | FileType::Opus | FileType::Speex | FileType::Vorbis => {
            let mut tag = VorbisComments::from(generic_tag(TagType::VorbisComments, tags, cover));
            if let Some(slug) = &tags.acquired_via {
                tag.insert(ACQUIRED_VIA_KEY.to_owned(), slug.as_str().to_owned());
            }
            tag.save_to_path(path, options)
                .map_err(|source| MediaError::tag(path, source))
        }
        FileType::Mpeg | FileType::Aac | FileType::Wav | FileType::Aiff => {
            let mut tag = Id3v2Tag::from(generic_tag(TagType::Id3v2, tags, cover));
            if let Some(slug) = &tags.acquired_via {
                tag.insert_user_text(ACQUIRED_VIA_KEY.to_owned(), slug.as_str().to_owned());
            }
            tag.save_to_path(path, options)
                .map_err(|source| MediaError::tag(path, source))
        }
        FileType::Mp4 => {
            let mut tag = Ilst::from(generic_tag(TagType::Mp4Ilst, tags, cover));
            if let Some(slug) = &tags.acquired_via {
                tag.insert(Atom::new(
                    AtomIdent::Freeform {
                        mean: MP4_FREEFORM_MEAN.into(),
                        name: ACQUIRED_VIA_KEY.into(),
                    },
                    AtomData::UTF8(slug.as_str().to_owned()),
                ));
            }
            tag.save_to_path(path, options)
                .map_err(|source| MediaError::tag(path, source))
        }
        FileType::Ape | FileType::Mpc | FileType::WavPack => {
            let mut tag = ApeTag::from(generic_tag(TagType::Ape, tags, cover));
            if let Some(slug) = &tags.acquired_via {
                let item = ApeItem::new(
                    ACQUIRED_VIA_KEY.to_owned(),
                    ItemValue::Text(slug.as_str().to_owned()),
                )
                .map_err(|source| MediaError::unsupported(path, source.to_string()))?;
                tag.insert(item);
            }
            tag.save_to_path(path, options)
                .map_err(|source| MediaError::tag(path, source))
        }
        _ => Err(MediaError::unsupported(
            path,
            format!("{file_type:?} files cannot be tagged"),
        )),
    }
}

/// Writes a cover image beside an audio file.
///
/// The import pipeline calls this with `library.cover_filename` (default
/// `cover.jpg`) and the directory the audio file lands in.
///
/// # Parameters
///
/// - `dir`: the directory to write into; it must already exist.
/// - `file_name`: the sidecar file name, including its extension.
/// - `cover`: the image to write.
///
/// # Returns
///
/// The path of the written cover, which is `dir` joined with `file_name`.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be written.
pub async fn write_sidecar_cover(
    dir: &Path,
    file_name: &str,
    cover: &Artwork,
) -> Result<PathBuf, MediaError> {
    let path = dir.join(file_name);
    tokio::fs::write(&path, cover.data())
        .await
        .map_err(|source| MediaError::io(&path, source))?;
    Ok(path)
}

/// Builds the generic tag set for a target tag type.
///
/// The generic [`Tag`] carries every field through `lofty`'s per-format key
/// mapping, which is what turns the same `ItemKey`s into Vorbis comments, ID3v2
/// frames, MP4 atoms, or APE items.
///
/// # Parameters
///
/// - `tag_type`: the tag type the file's container uses.
/// - `tags`: the tags to write.
/// - `cover`: an optional cover to embed.
///
/// # Returns
///
/// A tag holding one item per non-empty field, plus the cover.
fn generic_tag(tag_type: TagType, tags: &TrackTags, cover: Option<&Artwork>) -> Tag {
    let mut tag = Tag::new(tag_type);
    set(&mut tag, ItemKey::TrackTitle, tags.title.as_deref());
    if let Some(artist) = tags.artists.first() {
        tag.insert_text(ItemKey::TrackArtist, artist.clone());
    }
    append(&mut tag, ItemKey::TrackArtists, &tags.artists);
    set(&mut tag, ItemKey::AlbumTitle, tags.album.as_deref());
    set(&mut tag, ItemKey::AlbumArtist, tags.album_artist.as_deref());
    set_text(&mut tag, ItemKey::TrackNumber, tags.track_number);
    set_text(&mut tag, ItemKey::TrackTotal, tags.track_total);
    set_text(&mut tag, ItemKey::DiscNumber, tags.disc_number);
    set_text(&mut tag, ItemKey::DiscTotal, tags.disc_total);
    set(&mut tag, ItemKey::RecordingDate, tags.date.as_deref());
    append(&mut tag, ItemKey::Genre, &tags.genres);
    set(&mut tag, ItemKey::Isrc, tags.isrc.as_deref());
    set(
        &mut tag,
        ItemKey::MusicBrainzRecordingId,
        tags.mb_recording_id.as_deref(),
    );
    set(
        &mut tag,
        ItemKey::MusicBrainzReleaseId,
        tags.mb_release_id.as_deref(),
    );
    set(
        &mut tag,
        ItemKey::MusicBrainzReleaseGroupId,
        tags.mb_release_group_id.as_deref(),
    );
    append(&mut tag, ItemKey::MusicBrainzArtistId, &tags.mb_artist_ids);
    append(
        &mut tag,
        ItemKey::MusicBrainzReleaseArtistId,
        &tags.mb_album_artist_ids,
    );
    if let Some(compilation) = tags.compilation {
        set(
            &mut tag,
            ItemKey::FlagCompilation,
            Some(if compilation { "1" } else { "0" }),
        );
    }
    set(
        &mut tag,
        ItemKey::ReplayGainTrackGain,
        tags.replaygain_track_gain.as_deref(),
    );
    set(
        &mut tag,
        ItemKey::ReplayGainTrackPeak,
        tags.replaygain_track_peak.as_deref(),
    );
    set(
        &mut tag,
        ItemKey::ReplayGainAlbumGain,
        tags.replaygain_album_gain.as_deref(),
    );
    set(
        &mut tag,
        ItemKey::ReplayGainAlbumPeak,
        tags.replaygain_album_peak.as_deref(),
    );
    if let Some(cover) = cover {
        tag.push_picture(
            Picture::unchecked(cover.data().to_vec())
                .mime_type(MimeType::from_str(cover.mime_type()))
                .pic_type(PictureType::CoverFront)
                .build(),
        );
    }
    tag
}

/// Sets one single-valued text tag, replacing any previous value.
///
/// # Parameters
///
/// - `tag`: the tag to write into.
/// - `key`: the tag key.
/// - `value`: the value, or `None` to leave the key untouched.
fn set(tag: &mut Tag, key: ItemKey, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        tag.insert_text(key, value.to_owned());
    }
}

/// Sets one single-valued numeric tag.
///
/// # Parameters
///
/// - `tag`: the tag to write into.
/// - `key`: the tag key.
/// - `value`: the value, or `None` to leave the key untouched.
fn set_text(tag: &mut Tag, key: ItemKey, value: Option<u32>) {
    if let Some(value) = value {
        tag.insert_text(key, value.to_string());
    }
}

/// Sets a multi-valued text tag, replacing any previous values.
///
/// # Parameters
///
/// - `tag`: the tag to write into.
/// - `key`: the tag key.
/// - `values`: the values; empty strings are skipped.
fn append(tag: &mut Tag, key: ItemKey, values: &[String]) {
    let values: Vec<&String> = values.iter().filter(|value| !value.is_empty()).collect();
    if values.is_empty() {
        return;
    }
    tag.remove_key(key);
    for value in values {
        tag.push(TagItem::new(key, ItemValue::Text(value.clone())));
    }
}
