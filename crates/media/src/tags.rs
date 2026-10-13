//! The track tag set the server reads from and writes into media files.
//!
//! The fields are exactly the tags the import pipeline writes (see
//! `06-acquisition-and-playback.md` section 10.1): the display tags, the
//! Picard MusicBrainz identifiers, the ReplayGain values, and the
//! `HEARTHSOUND_ACQUIRED_VIA` marker. One type serves both directions, so a
//! probe result can be written back to another file without translation, and a
//! round trip through a file is directly comparable.

use hearthsound_core::Slug;

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
    /// The MusicBrainz artist id, written as `MUSICBRAINZ_ARTISTID`.
    pub mb_artist_id: Option<String>,
    /// The MusicBrainz album artist id, written as
    /// `MUSICBRAINZ_ALBUMARTISTID`.
    pub mb_album_artist_id: Option<String>,
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
