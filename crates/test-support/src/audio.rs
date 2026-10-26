//! Generating audio and cover fixtures at test time.
//!
//! Nothing here is committed to the repository: every file is synthesized when
//! a test needs it, so the repository never contains copyrighted audio. The
//! samples come from the `ffmpeg` binary on `PATH`, invoked with an argument
//! vector and never through a shell, and the tags are written with `lofty` on a
//! blocking thread.
//!
//! [`AudioFixture`] is the convenient entry point: it combines a format, a
//! signal, a set of tags and an optional cover, and writes one tagged file into
//! a directory the caller owns.
//!
//! # Examples
//!
//! ```
//! # async fn example() -> Result<(), hearthsound_test_support::audio::AudioFixtureError> {
//! use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
//!
//! let dir = tempfile::tempdir().unwrap();
//! let path = AudioFixture::new(AudioFormat::Flac)
//!     .seconds(1.0)
//!     .tags(TrackTags::new("Blue Hour", "Test Artist").album("Nightfall"))
//!     .write_into(dir.path())
//!     .await?;
//! assert!(path.is_file());
//! # Ok(())
//! # }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::TagExt;
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem};
use tokio::process::Command;

/// The error a fixture helper returns.
#[derive(Debug, thiserror::Error)]
pub enum AudioFixtureError {
    /// A fixture file could not be read or written.
    #[error("fixture io error at `{path}`: {source}")]
    Io {
        /// The path the operation used.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The `ffmpeg` process failed or could not be started.
    #[error("ffmpeg {args} failed: {message}")]
    Ffmpeg {
        /// The argument vector, joined for the message.
        args: String,
        /// The status line plus captured stderr.
        message: String,
    },
    /// The format does not support the requested bit depth.
    #[error("{format} does not support {bits}-bit samples")]
    UnsupportedBitDepth {
        /// The requested format, as its extension.
        format: &'static str,
        /// The requested bit depth, in bits.
        bits: u8,
    },
    /// The format does not support the requested sample rate.
    #[error("{format} does not support a {rate} Hz sample rate")]
    UnsupportedSampleRate {
        /// The requested format, as its extension.
        format: &'static str,
        /// The requested sample rate, in hertz.
        rate: u32,
    },
    /// The tags could not be read or written.
    #[error("tag error at `{path}`: {source}")]
    Tag {
        /// The path the operation used.
        path: PathBuf,
        /// The underlying `lofty` error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl AudioFixtureError {
    /// Builds a tag error for a path.
    ///
    /// # Parameters
    ///
    /// - `path`: the file the tag operation used.
    /// - `source`: the lofty error.
    fn tag(
        path: &Path,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> AudioFixtureError {
        AudioFixtureError::Tag {
            path: path.to_path_buf(),
            source: Box::new(source),
        }
    }

    /// Builds a [`AudioFixtureError::Ffmpeg`] from a failed process.
    ///
    /// # Parameters
    ///
    /// - `args`: the argument vector that ran.
    /// - `status`: the exit status.
    /// - `stderr`: the captured standard error.
    fn ffmpeg_failed(args: &[OsString], status: std::process::ExitStatus, stderr: &[u8]) -> Self {
        AudioFixtureError::Ffmpeg {
            args: join_args(args),
            message: format!(
                "{status}: {}",
                String::from_utf8_lossy(stderr).trim().to_owned()
            ),
        }
    }
}

/// The Vorbis comment key the release disambiguation is written under.
const VORBIS_RELEASE_COMMENT_KEY: &str = "MUSICBRAINZ_ALBUMCOMMENT";

/// The description the release disambiguation is written under elsewhere.
const RELEASE_COMMENT_NAME: &str = "MusicBrainz Album Comment";

/// The mean of the MP4 freeform atoms Picard writes into.
const ITUNES_MEAN: &str = "com.apple.iTunes";

/// The sample bit depth of a generated file.
///
/// The depth only means something for the lossless formats that store samples
/// verbatim ([`AudioFormat::Flac`], [`AudioFormat::Wav`] and
/// [`AudioFormat::WavPack`]); the lossy formats in [`AudioFormat::ALL`] ignore
/// it. FLAC supports 16 and 24 bits, WAV all three, and WavPack 16 and 24.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    /// 16-bit samples.
    #[default]
    Sixteen,
    /// 24-bit samples.
    TwentyFour,
    /// 32-bit samples.
    ThirtyTwo,
}

impl BitDepth {
    /// Returns the bit depth in bits.
    pub const fn bits(self) -> u8 {
        match self {
            BitDepth::Sixteen => 16,
            BitDepth::TwentyFour => 24,
            BitDepth::ThirtyTwo => 32,
        }
    }
}

/// The signal a generated file carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// A sine tone at the given frequency in hertz.
    Sine {
        /// The tone frequency in hertz.
        hz: f32,
    },
    /// Pink noise, which is less compressible than a tone and therefore gives
    /// lossy encoders something real to do.
    Noise,
}

impl Default for Signal {
    /// Returns a 440 Hz sine tone.
    fn default() -> Signal {
        Signal::Sine { hz: 440.0 }
    }
}

impl Signal {
    /// Returns a sine tone at the given frequency.
    ///
    /// # Parameters
    ///
    /// - `hz`: the tone frequency in hertz.
    pub const fn sine(hz: f32) -> Signal {
        Signal::Sine { hz }
    }

    /// Returns the ffmpeg `lavfi` source that produces this signal.
    ///
    /// # Parameters
    ///
    /// - `sample_rate`: the sample rate in hertz.
    /// - `duration`: the requested duration.
    fn lavfi_source(self, sample_rate: u32, duration: Duration) -> String {
        let seconds = duration.as_secs_f64();
        match self {
            Signal::Sine { hz } => {
                format!("sine=frequency={hz}:duration={seconds}:sample_rate={sample_rate}")
            }
            Signal::Noise => format!("anoisesrc=d={seconds}:c=pink:r={sample_rate}:a=0.5"),
        }
    }
}

/// A container and codec combination a fixture can be generated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// FLAC, lossless.
    Flac,
    /// MP3 encoded by `libmp3lame` at a constant bitrate.
    Mp3Cbr,
    /// MP3 encoded by `libmp3lame` in VBR mode at `-q:a 0`.
    Mp3Vbr,
    /// Ogg Vorbis at a constant bitrate.
    OggVorbis,
    /// Opus in Ogg at a constant bitrate.
    Opus,
    /// AAC in an MP4 container at a constant bitrate.
    M4aAac,
    /// RIFF/WAVE with little-endian integer samples.
    Wav,
    /// WavPack, lossless, carrying an APE tag.
    WavPack,
}

impl AudioFormat {
    /// Every format, so tests can cover all of them in one loop.
    pub const ALL: [AudioFormat; 8] = [
        AudioFormat::Flac,
        AudioFormat::Mp3Cbr,
        AudioFormat::Mp3Vbr,
        AudioFormat::OggVorbis,
        AudioFormat::Opus,
        AudioFormat::M4aAac,
        AudioFormat::Wav,
        AudioFormat::WavPack,
    ];

    /// Returns the file extension, without the leading dot.
    pub const fn extension(self) -> &'static str {
        match self {
            AudioFormat::Flac => "flac",
            AudioFormat::Mp3Cbr | AudioFormat::Mp3Vbr => "mp3",
            AudioFormat::OggVorbis => "ogg",
            AudioFormat::Opus => "opus",
            AudioFormat::M4aAac => "m4a",
            AudioFormat::Wav => "wav",
            AudioFormat::WavPack => "wv",
        }
    }

    /// Reports whether the format stores samples losslessly.
    pub const fn is_lossless(self) -> bool {
        matches!(
            self,
            AudioFormat::Flac | AudioFormat::Wav | AudioFormat::WavPack
        )
    }

    /// Returns the bitrate a fixture uses when the spec does not name one.
    ///
    /// # Returns
    ///
    /// `None` for the formats without a bitrate: FLAC, WAV and WavPack are
    /// lossless, and VBR MP3 picks its own bitrate from the quality target.
    pub const fn default_bitrate_kbps(self) -> Option<u32> {
        match self {
            AudioFormat::Flac | AudioFormat::Wav | AudioFormat::WavPack | AudioFormat::Mp3Vbr => {
                None
            }
            AudioFormat::Mp3Cbr => Some(320),
            AudioFormat::OggVorbis => Some(256),
            AudioFormat::Opus => Some(160),
            AudioFormat::M4aAac => Some(256),
        }
    }

    /// Reports whether the format can encode the given sample rate.
    ///
    /// # Parameters
    ///
    /// - `sample_rate`: the rate in hertz.
    ///
    /// # Returns
    ///
    /// `true` for every rate except the ones Opus cannot represent, which are
    /// everything outside 8, 12, 16, 24 and 48 kHz.
    pub const fn supports_sample_rate(self, sample_rate: u32) -> bool {
        match self {
            AudioFormat::Opus => matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000),
            _ => true,
        }
    }

    /// Returns the ffmpeg encoder arguments for the format.
    ///
    /// # Parameters
    ///
    /// - `spec`: the spec being encoded.
    ///
    /// # Errors
    ///
    /// Returns [`AudioFixtureError::UnsupportedBitDepth`] for a combination the
    /// encoder cannot represent.
    fn encoder_args(self, spec: &AudioSpec) -> Result<Vec<OsString>, AudioFixtureError> {
        let bitrate = spec.bitrate_kbps.map(|kbps| format!("{kbps}k"));
        let unsupported = || AudioFixtureError::UnsupportedBitDepth {
            format: self.extension(),
            bits: spec.bit_depth.bits(),
        };
        let args: Vec<OsString> = match self {
            AudioFormat::Flac => {
                if spec.bit_depth == BitDepth::ThirtyTwo {
                    return Err(unsupported());
                }
                let mut args: Vec<OsString> = vec!["-c:a".into(), "flac".into()];
                args.extend(sample_format_args("s16", "s32", spec.bit_depth));
                args
            }
            AudioFormat::Wav => vec![
                "-c:a".into(),
                match spec.bit_depth {
                    BitDepth::Sixteen => "pcm_s16le",
                    BitDepth::TwentyFour => "pcm_s24le",
                    BitDepth::ThirtyTwo => "pcm_s32le",
                }
                .into(),
            ],
            AudioFormat::WavPack => {
                if spec.bit_depth == BitDepth::ThirtyTwo {
                    return Err(unsupported());
                }
                let mut args: Vec<OsString> = vec!["-c:a".into(), "wavpack".into()];
                args.extend(sample_format_args("s16p", "s32p", spec.bit_depth));
                args
            }
            AudioFormat::Mp3Cbr => vec![
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                bitrate.unwrap_or_else(|| "320k".to_owned()).into(),
            ],
            AudioFormat::Mp3Vbr => vec![
                "-c:a".into(),
                "libmp3lame".into(),
                "-q:a".into(),
                "0".into(),
            ],
            AudioFormat::OggVorbis => vec![
                "-c:a".into(),
                "libvorbis".into(),
                "-b:a".into(),
                bitrate.unwrap_or_else(|| "256k".to_owned()).into(),
            ],
            AudioFormat::Opus => vec![
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                bitrate.unwrap_or_else(|| "160k".to_owned()).into(),
            ],
            AudioFormat::M4aAac => vec![
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                bitrate.unwrap_or_else(|| "256k".to_owned()).into(),
            ],
        };
        Ok(args)
    }
}

/// Returns the ffmpeg `-sample_fmt` and raw-depth arguments for a depth.
///
/// # Parameters
///
/// - `sixteen`: the encoder's 16-bit sample format.
/// - `wide`: the encoder's sample format that carries 24 bits.
/// - `bit_depth`: the requested depth.
fn sample_format_args(sixteen: &str, wide: &str, bit_depth: BitDepth) -> Vec<OsString> {
    match bit_depth {
        BitDepth::Sixteen => vec!["-sample_fmt".into(), sixteen.into()],
        BitDepth::TwentyFour => vec![
            "-sample_fmt".into(),
            wide.into(),
            "-bits_per_raw_sample".into(),
            "24".into(),
        ],
        BitDepth::ThirtyTwo => vec!["-sample_fmt".into(), wide.into()],
    }
}

/// Everything a generated audio file needs beyond its format.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioSpec {
    /// The container and codec.
    pub format: AudioFormat,
    /// How long the file is.
    pub duration: Duration,
    /// The sample rate in hertz.
    pub sample_rate: u32,
    /// The sample bit depth; only the lossless formats honour it.
    pub bit_depth: BitDepth,
    /// The number of channels: 1 for mono, 2 for stereo.
    pub channels: u16,
    /// The target bitrate for the formats that take one.
    pub bitrate_kbps: Option<u32>,
    /// The signal inside the file.
    pub signal: Signal,
}

impl AudioSpec {
    /// Returns a two second, 44.1 kHz, stereo spec for the given format.
    ///
    /// # Parameters
    ///
    /// - `format`: the container and codec to generate.
    pub fn new(format: AudioFormat) -> AudioSpec {
        AudioSpec {
            format,
            duration: Duration::from_secs(2),
            sample_rate: match format {
                AudioFormat::Opus => 48_000,
                _ => 44_100,
            },
            bit_depth: BitDepth::default(),
            channels: 2,
            bitrate_kbps: format.default_bitrate_kbps(),
            signal: Signal::default(),
        }
    }

    /// Sets the duration in seconds.
    ///
    /// # Parameters
    ///
    /// - `seconds`: the duration; fractions are honoured.
    pub fn seconds(mut self, seconds: f64) -> Self {
        self.duration = Duration::from_secs_f64(seconds);
        self
    }

    /// Sets the duration.
    ///
    /// # Parameters
    ///
    /// - `duration`: the duration.
    pub fn duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Sets the sample rate.
    ///
    /// Opus only encodes 8, 12, 16, 24 and 48 kHz; any other rate makes
    /// [`generate`] fail with [`AudioFixtureError::UnsupportedSampleRate`].
    ///
    /// # Parameters
    ///
    /// - `sample_rate`: the rate in hertz.
    pub fn sample_rate(mut self, sample_rate: u32) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Sets the sample bit depth.
    ///
    /// # Parameters
    ///
    /// - `bit_depth`: the depth; ignored by the lossy formats.
    pub fn bit_depth(mut self, bit_depth: BitDepth) -> Self {
        self.bit_depth = bit_depth;
        self
    }

    /// Sets the number of channels.
    ///
    /// # Parameters
    ///
    /// - `channels`: 1 for mono, 2 for stereo.
    pub fn channels(mut self, channels: u16) -> Self {
        self.channels = channels;
        self
    }

    /// Sets the target bitrate.
    ///
    /// # Parameters
    ///
    /// - `bitrate_kbps`: the bitrate in kbit/s, or `None` to let the encoder
    ///   choose (the default for FLAC, WAV, WavPack and VBR MP3).
    pub fn bitrate_kbps(mut self, bitrate_kbps: Option<u32>) -> Self {
        self.bitrate_kbps = bitrate_kbps;
        self
    }

    /// Sets the signal.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal to write.
    pub fn signal(mut self, signal: Signal) -> Self {
        self.signal = signal;
        self
    }

    /// Builds the ffmpeg argument vector that writes this spec to `path`.
    ///
    /// # Parameters
    ///
    /// - `path`: the output file.
    ///
    /// # Errors
    ///
    /// Returns [`AudioFixtureError::UnsupportedBitDepth`] when a format cannot
    /// store the requested depth.
    fn ffmpeg_args(&self, path: &Path) -> Result<Vec<OsString>, AudioFixtureError> {
        if !self.format.supports_sample_rate(self.sample_rate) {
            return Err(AudioFixtureError::UnsupportedSampleRate {
                format: self.format.extension(),
                rate: self.sample_rate,
            });
        }
        let mut args: Vec<OsString> = vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-y".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            self.signal
                .lavfi_source(self.sample_rate, self.duration)
                .into(),
        ];
        if self.signal == Signal::Noise {
            args.push("-t".into());
            args.push(format!("{:.6}", self.duration.as_secs_f64()).into());
        }
        args.push("-ar".into());
        args.push(self.sample_rate.to_string().into());
        args.push("-ac".into());
        args.push(self.channels.to_string().into());
        args.extend(self.format.encoder_args(self)?);
        args.push(path.as_os_str().to_owned());
        Ok(args)
    }
}

/// An image embedded in, or written beside, a generated file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverImage {
    /// The image MIME type, for example `image/png`.
    pub mime_type: String,
    /// The encoded image bytes.
    pub data: Vec<u8>,
}

impl CoverImage {
    /// Builds a PNG cover.
    ///
    /// # Parameters
    ///
    /// - `data`: the encoded PNG bytes.
    pub fn png(data: Vec<u8>) -> CoverImage {
        CoverImage {
            mime_type: "image/png".to_owned(),
            data,
        }
    }

    /// Builds a JPEG cover.
    ///
    /// # Parameters
    ///
    /// - `data`: the encoded JPEG bytes.
    pub fn jpeg(data: Vec<u8>) -> CoverImage {
        CoverImage {
            mime_type: "image/jpeg".to_owned(),
            data,
        }
    }
}

/// The image format of a generated cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverFormat {
    /// Portable Network Graphics.
    Png,
    /// JPEG.
    Jpeg,
}

impl CoverFormat {
    /// Returns the file extension, without the leading dot.
    pub const fn extension(self) -> &'static str {
        match self {
            CoverFormat::Png => "png",
            CoverFormat::Jpeg => "jpg",
        }
    }

    /// Returns the MIME type.
    pub const fn mime_type(self) -> &'static str {
        match self {
            CoverFormat::Png => "image/png",
            CoverFormat::Jpeg => "image/jpeg",
        }
    }
}

/// A cover the fixture generates and embeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverSpec {
    /// The image format.
    pub format: CoverFormat,
    /// The width and height in pixels.
    pub size: u32,
    /// The colour as 24-bit RGB.
    pub rgb: u32,
}

/// The tags written into a generated file.
///
/// Every field is optional so a test can write exactly the tags it cares about.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackTags {
    /// The track title.
    pub title: Option<String>,
    /// The performing artists, in order; the first becomes the single-valued
    /// artist tag and all of them the multi-valued one.
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
    /// The release year.
    pub year: Option<u32>,
    /// The genres.
    ///
    /// An APE tag, which is what [`AudioFormat::WavPack`] carries, keeps a
    /// single genre: writing a second one replaces the first.
    pub genres: Vec<String>,
    /// The International Standard Recording Code.
    pub isrc: Option<String>,
    /// The MusicBrainz recording id.
    pub mb_recording_id: Option<String>,
    /// The MusicBrainz release id.
    pub mb_release_id: Option<String>,
    /// The MusicBrainz release group id.
    pub mb_release_group_id: Option<String>,
    /// The MusicBrainz artist ids, one per credited artist.
    pub mb_artist_ids: Vec<String>,
    /// The MusicBrainz album artist ids, one per credited album artist.
    pub mb_album_artist_ids: Vec<String>,
    /// The ReplayGain track gain, for example `-3.21 dB`.
    pub replaygain_track_gain: Option<String>,
    /// The ReplayGain track peak.
    pub replaygain_track_peak: Option<String>,
    /// The ReplayGain album gain.
    pub replaygain_album_gain: Option<String>,
    /// The ReplayGain album peak.
    pub replaygain_album_peak: Option<String>,
    /// The compilation flag.
    pub compilation: Option<bool>,
}

impl TrackTags {
    /// Returns tags with a title and one artist.
    ///
    /// # Parameters
    ///
    /// - `title`: the track title.
    /// - `artist`: the performing artist.
    pub fn new(title: impl Into<String>, artist: impl Into<String>) -> TrackTags {
        TrackTags {
            title: Some(title.into()),
            artists: vec![artist.into()],
            ..TrackTags::default()
        }
    }

    /// Sets the title.
    ///
    /// # Parameters
    ///
    /// - `title`: the track title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Appends performing artists.
    ///
    /// # Parameters
    ///
    /// - `artists`: the artists to append, in order.
    pub fn artists<I, S>(mut self, artists: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.artists.extend(artists.into_iter().map(Into::into));
        self
    }

    /// Sets the album title.
    ///
    /// # Parameters
    ///
    /// - `album`: the album title.
    pub fn album(mut self, album: impl Into<String>) -> Self {
        self.album = Some(album.into());
        self
    }

    /// Sets the album artist.
    ///
    /// # Parameters
    ///
    /// - `album_artist`: the album artist.
    pub fn album_artist(mut self, album_artist: impl Into<String>) -> Self {
        self.album_artist = Some(album_artist.into());
        self
    }

    /// Sets the track number and the track total.
    ///
    /// # Parameters
    ///
    /// - `number`: the track number.
    /// - `total`: the number of tracks on the disc.
    pub fn track(mut self, number: u32, total: u32) -> Self {
        self.track_number = Some(number);
        self.track_total = Some(total);
        self
    }

    /// Sets the disc number and the disc total.
    ///
    /// # Parameters
    ///
    /// - `number`: the disc number.
    /// - `total`: the number of discs.
    pub fn disc(mut self, number: u32, total: u32) -> Self {
        self.disc_number = Some(number);
        self.disc_total = Some(total);
        self
    }

    /// Sets the release year.
    ///
    /// # Parameters
    ///
    /// - `year`: the year, for example `1994`.
    pub fn year(mut self, year: u32) -> Self {
        self.year = Some(year);
        self
    }

    /// Appends genres.
    ///
    /// # Parameters
    ///
    /// - `genres`: the genres to append.
    pub fn genres<I, S>(mut self, genres: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.genres.extend(genres.into_iter().map(Into::into));
        self
    }

    /// Sets the ISRC.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the recording code.
    pub fn isrc(mut self, isrc: impl Into<String>) -> Self {
        self.isrc = Some(isrc.into());
        self
    }

    /// Sets the MusicBrainz identifiers.
    ///
    /// # Parameters
    ///
    /// - `recording`: the recording id.
    /// - `release`: the release id.
    /// - `release_group`: the release group id.
    /// - `artists`: the performing artist ids, one per credited artist.
    /// - `album_artists`: the album artist ids, one per credited album artist.
    pub fn mb_ids<I, S, J, T>(
        mut self,
        recording: impl Into<String>,
        release: impl Into<String>,
        release_group: impl Into<String>,
        artists: I,
        album_artists: J,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        J: IntoIterator<Item = T>,
        T: Into<String>,
    {
        self.mb_recording_id = Some(recording.into());
        self.mb_release_id = Some(release.into());
        self.mb_release_group_id = Some(release_group.into());
        self.mb_artist_ids = artists.into_iter().map(Into::into).collect();
        self.mb_album_artist_ids = album_artists.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the ReplayGain values.
    ///
    /// # Parameters
    ///
    /// - `track_gain`: the track gain, for example `-3.21 dB`.
    /// - `track_peak`: the track peak.
    /// - `album_gain`: the album gain.
    /// - `album_peak`: the album peak.
    pub fn replaygain(
        mut self,
        track_gain: impl Into<String>,
        track_peak: impl Into<String>,
        album_gain: impl Into<String>,
        album_peak: impl Into<String>,
    ) -> Self {
        self.replaygain_track_gain = Some(track_gain.into());
        self.replaygain_track_peak = Some(track_peak.into());
        self.replaygain_album_gain = Some(album_gain.into());
        self.replaygain_album_peak = Some(album_peak.into());
        self
    }

    /// Sets the compilation flag.
    ///
    /// # Parameters
    ///
    /// - `compilation`: whether the track belongs to a compilation.
    pub fn compilation(mut self, compilation: bool) -> Self {
        self.compilation = Some(compilation);
        self
    }

    /// Returns the pending tag items, without the cover.
    ///
    /// # Returns
    ///
    /// One `(key, value)` pair per tag, in the order they are written. Repeated
    /// keys carry multi-valued tags.
    fn items(&self) -> Vec<(ItemKey, String)> {
        let mut items = Vec::new();
        if let Some(title) = &self.title {
            items.push((ItemKey::TrackTitle, title.clone()));
        }
        if let Some(artist) = self.artists.first() {
            items.push((ItemKey::TrackArtist, artist.clone()));
        }
        items.extend(
            self.artists
                .iter()
                .map(|artist| (ItemKey::TrackArtists, artist.clone())),
        );
        if let Some(album) = &self.album {
            items.push((ItemKey::AlbumTitle, album.clone()));
        }
        if let Some(album_artist) = &self.album_artist {
            items.push((ItemKey::AlbumArtist, album_artist.clone()));
        }
        if let Some(number) = self.track_number {
            items.push((ItemKey::TrackNumber, number.to_string()));
        }
        if let Some(total) = self.track_total {
            items.push((ItemKey::TrackTotal, total.to_string()));
        }
        if let Some(number) = self.disc_number {
            items.push((ItemKey::DiscNumber, number.to_string()));
        }
        if let Some(total) = self.disc_total {
            items.push((ItemKey::DiscTotal, total.to_string()));
        }
        if let Some(year) = self.year {
            items.push((ItemKey::RecordingDate, year.to_string()));
        }
        items.extend(
            self.genres
                .iter()
                .map(|genre| (ItemKey::Genre, genre.clone())),
        );
        if let Some(isrc) = &self.isrc {
            items.push((ItemKey::Isrc, isrc.clone()));
        }
        if let Some(recording) = &self.mb_recording_id {
            items.push((ItemKey::MusicBrainzRecordingId, recording.clone()));
        }
        if let Some(release) = &self.mb_release_id {
            items.push((ItemKey::MusicBrainzReleaseId, release.clone()));
        }
        if let Some(release_group) = &self.mb_release_group_id {
            items.push((ItemKey::MusicBrainzReleaseGroupId, release_group.clone()));
        }
        items.extend(
            self.mb_artist_ids
                .iter()
                .map(|id| (ItemKey::MusicBrainzArtistId, id.clone())),
        );
        items.extend(
            self.mb_album_artist_ids
                .iter()
                .map(|id| (ItemKey::MusicBrainzReleaseArtistId, id.clone())),
        );
        if let Some(gain) = &self.replaygain_track_gain {
            items.push((ItemKey::ReplayGainTrackGain, gain.clone()));
        }
        if let Some(peak) = &self.replaygain_track_peak {
            items.push((ItemKey::ReplayGainTrackPeak, peak.clone()));
        }
        if let Some(gain) = &self.replaygain_album_gain {
            items.push((ItemKey::ReplayGainAlbumGain, gain.clone()));
        }
        if let Some(peak) = &self.replaygain_album_peak {
            items.push((ItemKey::ReplayGainAlbumPeak, peak.clone()));
        }
        if let Some(compilation) = self.compilation {
            items.push((
                ItemKey::FlagCompilation,
                if compilation { "1" } else { "0" }.to_owned(),
            ));
        }
        items
    }
}

/// Writes a generated audio file.
///
/// The parent directory of `path` must exist. The file is written by a fresh
/// `ffmpeg` process that is awaited to completion.
///
/// # Parameters
///
/// - `spec`: the format, signal and encoding parameters.
/// - `path`: where to write the file; its extension is not checked.
///
/// # Returns
///
/// `Ok(())` once ffmpeg exited successfully.
///
/// # Errors
///
/// Returns [`AudioFixtureError::UnsupportedBitDepth`] when the format cannot
/// store the requested depth, and [`AudioFixtureError::Ffmpeg`] when ffmpeg
/// cannot be started or exits with a failure.
pub async fn generate(spec: &AudioSpec, path: &Path) -> Result<(), AudioFixtureError> {
    let args = spec.ffmpeg_args(path)?;
    run_ffmpeg(&args).await
}

/// Writes tags into an existing audio file.
///
/// The tags land in the file's primary tag format: Vorbis comments for FLAC and
/// Ogg, ID3v2 for MPEG, AAC, WAV and AIFF, `ilst` for MP4, and an APE tag for
/// WavPack. The cover, when given, is embedded as a front cover picture.
///
/// The tag write is a blocking `lofty` operation and runs on a blocking thread.
///
/// # Parameters
///
/// - `path`: the file to tag; it must already exist in a supported format.
/// - `tags`: the tags to write.
/// - `cover`: an optional embedded cover image.
///
/// # Returns
///
/// `Ok(())` once the file was rewritten with the new tags.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Tag`] when the file cannot be parsed or the tag
/// cannot be encoded, and [`AudioFixtureError::Io`] when it cannot be read.
pub async fn write_tags(
    path: &Path,
    tags: &TrackTags,
    cover: Option<CoverImage>,
) -> Result<(), AudioFixtureError> {
    let path = path.to_path_buf();
    let tags = tags.clone();
    tokio::task::spawn_blocking(move || write_tags_blocking(&path, &tags, cover))
        .await
        .map_err(|source| AudioFixtureError::Io {
            path: PathBuf::from("<blocking task>"),
            source: std::io::Error::other(source.to_string()),
        })?
}

/// Writes tags on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to tag.
/// - `tags`: the tags to write.
/// - `cover`: an optional embedded cover image.
///
/// # Errors
///
/// Returns the same errors as [`write_tags`].
fn write_tags_blocking(
    path: &Path,
    tags: &TrackTags,
    cover: Option<CoverImage>,
) -> Result<(), AudioFixtureError> {
    let file_type = lofty::probe::Probe::open(path)
        .map_err(|source| AudioFixtureError::tag(path, source))?
        .guess_file_type()
        .map_err(|source| AudioFixtureError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .file_type()
        .ok_or_else(|| AudioFixtureError::Ffmpeg {
            args: String::new(),
            message: format!("no taggable format for {}", path.display()),
        })?;

    let mut tag = Tag::new(file_type.primary_tag_type());
    for (key, value) in tags.items() {
        tag.push(TagItem::new(key, ItemValue::Text(value)));
    }
    if let Some(cover) = cover {
        let mime = MimeType::from_str(&cover.mime_type);
        tag.push_picture(
            Picture::unchecked(cover.data)
                .mime_type(mime)
                .pic_type(PictureType::CoverFront)
                .build(),
        );
    }

    tag.save_to_path(path, WriteOptions::default())
        .map_err(|source| AudioFixtureError::tag(path, source))
}

/// Writes a small solid-colour cover image.
///
/// # Parameters
///
/// - `path`: where to write the image.
/// - `spec`: the image format, size and colour.
///
/// # Returns
///
/// `Ok(())` once ffmpeg exited successfully.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Ffmpeg`] when ffmpeg cannot be started or exits
/// with a failure.
pub async fn write_cover(path: &Path, spec: CoverSpec) -> Result<(), AudioFixtureError> {
    let (codec, pixel_format) = match spec.format {
        CoverFormat::Png => ("png", "rgb24"),
        CoverFormat::Jpeg => ("mjpeg", "yuvj420p"),
    };
    let source = format!(
        "color=c=0x{:06x}:s={}x{}:d=0.04,format={pixel_format}",
        spec.rgb, spec.size, spec.size
    );
    let args: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        source.into(),
        "-frames:v".into(),
        "1".into(),
        "-c:v".into(),
        codec.into(),
        path.as_os_str().to_owned(),
    ];
    run_ffmpeg(&args).await
}

/// A generated, tagged audio file.
///
/// The builder collects a format, a signal, tags and an optional cover, and
/// writes them as one file into a directory the caller owns.
///
/// # Examples
///
/// ```
/// # async fn example() -> Result<(), hearthsound_test_support::audio::AudioFixtureError> {
/// use hearthsound_test_support::audio::{AudioFixture, AudioFormat, Signal};
///
/// let dir = tempfile::tempdir().unwrap();
/// let path = AudioFixture::new(AudioFormat::Mp3Cbr)
///     .seconds(0.5)
///     .signal(Signal::Noise)
///     .write_into(dir.path())
///     .await?;
/// assert_eq!(path.extension().unwrap(), "mp3");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct AudioFixture {
    spec: AudioSpec,
    tags: TrackTags,
    cover: Option<CoverSpec>,
    file_name: Option<String>,
}

impl AudioFixture {
    /// Returns an untagged fixture for the given format.
    ///
    /// # Parameters
    ///
    /// - `format`: the container and codec to generate.
    pub fn new(format: AudioFormat) -> AudioFixture {
        AudioFixture {
            spec: AudioSpec::new(format),
            tags: TrackTags::default(),
            cover: None,
            file_name: None,
        }
    }

    /// Replaces the whole spec.
    ///
    /// # Parameters
    ///
    /// - `spec`: the spec to use.
    pub fn spec(mut self, spec: AudioSpec) -> Self {
        self.spec = spec;
        self
    }

    /// Sets the duration in seconds.
    ///
    /// # Parameters
    ///
    /// - `seconds`: the duration; fractions are honoured.
    pub fn seconds(mut self, seconds: f64) -> Self {
        self.spec = self.spec.seconds(seconds);
        self
    }

    /// Sets the sample rate.
    ///
    /// Opus only encodes 8, 12, 16, 24 and 48 kHz; any other rate makes
    /// [`generate`] fail with [`AudioFixtureError::UnsupportedSampleRate`].
    ///
    /// # Parameters
    ///
    /// - `sample_rate`: the rate in hertz.
    pub fn sample_rate(mut self, sample_rate: u32) -> Self {
        self.spec = self.spec.sample_rate(sample_rate);
        self
    }

    /// Sets the sample bit depth.
    ///
    /// # Parameters
    ///
    /// - `bit_depth`: the depth; ignored by the lossy formats.
    pub fn bit_depth(mut self, bit_depth: BitDepth) -> Self {
        self.spec = self.spec.bit_depth(bit_depth);
        self
    }

    /// Sets the number of channels.
    ///
    /// # Parameters
    ///
    /// - `channels`: 1 for mono, 2 for stereo.
    pub fn channels(mut self, channels: u16) -> Self {
        self.spec = self.spec.channels(channels);
        self
    }

    /// Sets the target bitrate.
    ///
    /// # Parameters
    ///
    /// - `bitrate_kbps`: the bitrate in kbit/s, or `None` to let the encoder
    ///   choose.
    pub fn bitrate_kbps(mut self, bitrate_kbps: Option<u32>) -> Self {
        self.spec = self.spec.bitrate_kbps(bitrate_kbps);
        self
    }

    /// Sets the signal.
    ///
    /// # Parameters
    ///
    /// - `signal`: the signal to write.
    pub fn signal(mut self, signal: Signal) -> Self {
        self.spec = self.spec.signal(signal);
        self
    }

    /// Sets the tags.
    ///
    /// # Parameters
    ///
    /// - `tags`: the tags to write.
    pub fn tags(mut self, tags: TrackTags) -> Self {
        self.tags = tags;
        self
    }

    /// Generates and embeds a PNG cover beside the file.
    ///
    /// # Parameters
    ///
    /// - `size`: the width and height in pixels.
    /// - `rgb`: the colour as 24-bit RGB.
    pub fn cover_png(mut self, size: u32, rgb: u32) -> Self {
        self.cover = Some(CoverSpec {
            format: CoverFormat::Png,
            size,
            rgb,
        });
        self
    }

    /// Generates and embeds a JPEG cover beside the file.
    ///
    /// # Parameters
    ///
    /// - `size`: the width and height in pixels.
    /// - `rgb`: the colour as 24-bit RGB.
    pub fn cover_jpeg(mut self, size: u32, rgb: u32) -> Self {
        self.cover = Some(CoverSpec {
            format: CoverFormat::Jpeg,
            size,
            rgb,
        });
        self
    }

    /// Overrides the file name.
    ///
    /// # Parameters
    ///
    /// - `file_name`: the name to write under the target directory, including
    ///   the extension.
    pub fn file_name(mut self, file_name: impl Into<String>) -> Self {
        self.file_name = Some(file_name.into());
        self
    }

    /// Returns the file name this fixture writes.
    ///
    /// # Returns
    ///
    /// The override, or `fixture.<extension>`.
    pub fn file_name_of(&self) -> String {
        self.file_name
            .clone()
            .unwrap_or_else(|| format!("fixture.{}", self.spec.format.extension()))
    }

    /// Writes the file into a directory.
    ///
    /// # Parameters
    ///
    /// - `dir`: the existing directory to write into.
    ///
    /// # Returns
    ///
    /// The path of the written audio file. A cover, when requested, is written
    /// beside it as `<stem>.cover.<ext>` and embedded in the file.
    ///
    /// # Errors
    ///
    /// Returns [`AudioFixtureError::Ffmpeg`] when ffmpeg fails,
    /// [`AudioFixtureError::Io`] when the directory cannot be written, and
    /// [`AudioFixtureError::Tag`] when the tags cannot be written.
    pub async fn write_into(self, dir: &Path) -> Result<PathBuf, AudioFixtureError> {
        let path = dir.join(self.file_name_of());
        generate(&self.spec, &path).await?;
        let cover = match self.cover {
            Some(spec) => {
                let stem = path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "fixture".to_owned());
                let cover_path = dir.join(format!("{stem}.cover.{}", spec.format.extension()));
                write_cover(&cover_path, spec).await?;
                let data =
                    tokio::fs::read(&cover_path)
                        .await
                        .map_err(|source| AudioFixtureError::Io {
                            path: cover_path.clone(),
                            source,
                        })?;
                Some(CoverImage {
                    mime_type: spec.format.mime_type().to_owned(),
                    data,
                })
            }
            None => None,
        };
        write_tags(&path, &self.tags, cover).await?;
        Ok(path)
    }

    /// Writes the file into a fresh temporary directory.
    ///
    /// # Returns
    ///
    /// The temporary directory, which the caller must keep alive, and the path
    /// of the audio file inside it.
    ///
    /// # Errors
    ///
    /// Returns [`AudioFixtureError::Io`] when the temporary directory cannot be
    /// created, and the same errors as [`AudioFixture::write_into`].
    pub async fn write_in_temp(self) -> Result<(tempfile::TempDir, PathBuf), AudioFixtureError> {
        let dir = tempfile::tempdir().map_err(|source| AudioFixtureError::Io {
            path: PathBuf::from("<tempdir>"),
            source,
        })?;
        let path = self.write_into(dir.path()).await?;
        Ok((dir, path))
    }
}

/// Writes the MusicBrainz release disambiguation comment into a file's tag.
///
/// The field has no `lofty` [`ItemKey`], so it is written into the container's
/// own tag type, exactly as MusicBrainz Picard writes it: the
/// `MUSICBRAINZ_ALBUMCOMMENT` Vorbis comment, the `MusicBrainz Album Comment`
/// APE item, the `TXXX:MusicBrainz Album Comment` ID3v2 frame, or the
/// `----:com.apple.iTunes:MusicBrainz Album Comment` freeform atom of an MP4
/// file. A fixture that needs the tag rule of the edition label is tagged with
/// this after [`AudioFixture::write_into`].
///
/// The write is a blocking `lofty` operation and runs on a blocking thread.
///
/// # Parameters
///
/// - `path`: the file to tag; it must already exist in a supported format.
/// - `comment`: the comment to write.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Tag`] when the file cannot be parsed or written,
/// [`AudioFixtureError::Io`] when it cannot be opened, and
/// [`AudioFixtureError::Ffmpeg`] when the container is one this helper does not
/// write.
pub async fn write_release_comment(path: &Path, comment: &str) -> Result<(), AudioFixtureError> {
    let path = path.to_path_buf();
    let comment = comment.to_owned();
    tokio::task::spawn_blocking(move || write_release_comment_blocking(&path, &comment))
        .await
        .map_err(|source| AudioFixtureError::Io {
            path: PathBuf::from("<blocking task>"),
            source: std::io::Error::other(source.to_string()),
        })?
}

/// Writes the release comment on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to tag.
/// - `comment`: the comment to write.
///
/// # Errors
///
/// Returns the same errors as [`write_release_comment`].
fn write_release_comment_blocking(path: &Path, comment: &str) -> Result<(), AudioFixtureError> {
    let file_type = lofty::probe::Probe::open(path)
        .map_err(|source| AudioFixtureError::tag(path, source))?
        .guess_file_type()
        .map_err(|source| AudioFixtureError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .file_type()
        .ok_or_else(|| AudioFixtureError::Ffmpeg {
            args: String::new(),
            message: format!("no taggable format for {}", path.display()),
        })?;
    let options = lofty::config::ParseOptions::new();
    match file_type {
        lofty::file::FileType::Flac => rewrite::<lofty::flac::FlacFile>(path, options, |file| {
            match file.vorbis_comments_mut() {
                Some(tag) => vorbis_insert(tag, comment),
                None => {
                    let mut tag = lofty::ogg::tag::VorbisComments::default();
                    vorbis_insert(&mut tag, comment);
                    file.set_vorbis_comments(tag);
                }
            }
        }),
        lofty::file::FileType::Vorbis => rewrite::<lofty::ogg::VorbisFile>(path, options, |file| {
            vorbis_insert(file.vorbis_comments_mut(), comment);
        }),
        lofty::file::FileType::Opus => rewrite::<lofty::ogg::OpusFile>(path, options, |file| {
            vorbis_insert(file.vorbis_comments_mut(), comment);
        }),
        lofty::file::FileType::Mpeg => {
            rewrite::<lofty::mpeg::MpegFile>(path, options, |file| match file.id3v2_mut() {
                Some(tag) => user_text(tag, comment),
                None => {
                    let mut tag = lofty::id3::v2::Id3v2Tag::default();
                    user_text(&mut tag, comment);
                    file.set_id3v2(tag);
                }
            })
        }
        lofty::file::FileType::Wav => {
            rewrite::<lofty::iff::wav::WavFile>(path, options, |file| match file.id3v2_mut() {
                Some(tag) => user_text(tag, comment),
                None => {
                    let mut tag = lofty::id3::v2::Id3v2Tag::default();
                    user_text(&mut tag, comment);
                    file.set_id3v2(tag);
                }
            })
        }
        lofty::file::FileType::WavPack => {
            rewrite::<lofty::wavpack::WavPackFile>(path, options, |file| match file.ape_mut() {
                Some(tag) => ape_insert(tag, comment),
                None => {
                    let mut tag = lofty::ape::ApeTag::default();
                    ape_insert(&mut tag, comment);
                    file.set_ape(tag);
                }
            })
        }
        lofty::file::FileType::Mp4 => rewrite::<lofty::mp4::Mp4File>(path, options, |file| {
            let atom = lofty::mp4::Atom::new(
                lofty::mp4::AtomIdent::Freeform {
                    mean: std::borrow::Cow::Borrowed(ITUNES_MEAN),
                    name: std::borrow::Cow::Borrowed(RELEASE_COMMENT_NAME),
                },
                lofty::mp4::AtomData::UTF8(comment.to_owned()),
            );
            match file.ilst_mut() {
                Some(ilst) => ilst.insert(atom),
                None => {
                    let mut ilst = lofty::mp4::Ilst::default();
                    ilst.insert(atom);
                    file.set_ilst(ilst);
                }
            }
        }),
        other => Err(AudioFixtureError::Ffmpeg {
            args: String::new(),
            message: format!(
                "no release comment writer for {other:?} in {}",
                path.display()
            ),
        }),
    }
}

/// Reads a file into its concrete container type, changes it, and saves it back.
///
/// # Parameters
///
/// - `path`: the file to rewrite.
/// - `options`: the parse options.
/// - `change`: the change to apply to the parsed file.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Io`] when the file cannot be opened and
/// [`AudioFixtureError::Tag`] when it cannot be parsed or written.
fn rewrite<T: lofty::file::AudioFile>(
    path: &Path,
    options: lofty::config::ParseOptions,
    change: impl FnOnce(&mut T),
) -> Result<(), AudioFixtureError> {
    let mut file = read_blocking::<T>(path, options)?;
    change(&mut file);
    file.save_to_path(path, WriteOptions::default())
        .map_err(|source| AudioFixtureError::tag(path, source))
}

/// Writes the release comment into a Vorbis comment block.
///
/// # Parameters
///
/// - `tag`: the block to write into.
/// - `comment`: the comment to write.
fn vorbis_insert(tag: &mut lofty::ogg::tag::VorbisComments, comment: &str) {
    tag.insert(VORBIS_RELEASE_COMMENT_KEY.to_owned(), comment.to_owned());
}

/// Writes the release comment into an ID3v2 tag as a `TXXX` frame.
///
/// # Parameters
///
/// - `tag`: the tag to write into.
/// - `comment`: the comment to write.
fn user_text(tag: &mut lofty::id3::v2::Id3v2Tag, comment: &str) {
    tag.insert_user_text(RELEASE_COMMENT_NAME.to_owned(), comment.to_owned());
}

/// Writes the release comment into an APE tag.
///
/// # Parameters
///
/// - `tag`: the tag to write into.
/// - `comment`: the comment to write.
fn ape_insert(tag: &mut lofty::ape::ApeTag, comment: &str) {
    if let Ok(item) = lofty::ape::ApeItem::new(
        RELEASE_COMMENT_NAME.to_owned(),
        lofty::tag::ItemValue::Text(comment.to_owned()),
    ) {
        tag.insert(item);
    }
}

/// Reads a file's concrete container type on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to read.
/// - `options`: the parse options.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Io`] when the file cannot be opened and
/// [`AudioFixtureError::Tag`] when it cannot be parsed.
fn read_blocking<T: lofty::file::AudioFile>(
    path: &Path,
    options: lofty::config::ParseOptions,
) -> Result<T, AudioFixtureError> {
    let file = std::fs::File::open(path).map_err(|source| AudioFixtureError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    T::read_from(&mut std::io::BufReader::new(file), options)
        .map_err(|source| AudioFixtureError::tag(path, source))
}

/// Runs one ffmpeg command to completion.
///
/// # Parameters
///
/// - `args`: the argument vector, without the binary itself.
///
/// # Errors
///
/// Returns [`AudioFixtureError::Ffmpeg`] with the captured stderr when ffmpeg
/// is missing or exits non-zero.
async fn run_ffmpeg(args: &[OsString]) -> Result<(), AudioFixtureError> {
    let mut command = Command::new("ffmpeg");
    command.args(args).kill_on_drop(true);
    let output = command
        .output()
        .await
        .map_err(|source| AudioFixtureError::Ffmpeg {
            args: join_args(args),
            message: source.to_string(),
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(AudioFixtureError::ffmpeg_failed(
        args,
        output.status,
        &output.stderr,
    ))
}

/// Joins an argument vector for a log line.
///
/// # Parameters
///
/// - `args`: the argument vector.
fn join_args(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;

    /// Reads a file back with lofty.
    ///
    /// # Parameters
    ///
    /// - `path`: the file to read.
    fn read(path: &Path) -> lofty::file::TaggedFile {
        lofty::read_from_path(path).expect("fixture must be readable")
    }

    #[tokio::test]
    async fn generates_every_format_with_readable_properties() {
        let dir = tempfile::tempdir().unwrap();
        for format in AudioFormat::ALL {
            let path = AudioFixture::new(format)
                .seconds(1.0)
                .file_name(format!("{}.{}", format.extension(), format.extension()))
                .write_into(dir.path())
                .await
                .unwrap();
            assert!(path.is_file(), "{} was not written", format.extension());

            let tagged = read(&path);
            let duration = tagged.properties().duration().as_secs_f64();
            assert!(
                (0.8..1.4).contains(&duration),
                "{} reported {duration}s",
                format.extension()
            );
            assert!(
                tagged.properties().sample_rate().is_some(),
                "{} reported no sample rate",
                format.extension()
            );
            assert_eq!(
                tagged.properties().channels(),
                Some(2),
                "{} reported the wrong channel count",
                format.extension()
            );
            assert_eq!(
                tagged.properties().bit_depth().is_some(),
                format.is_lossless(),
                "{} reported the wrong bit depth presence",
                format.extension()
            );
        }
    }

    #[tokio::test]
    async fn honours_the_requested_sample_rate_and_depth() {
        let dir = tempfile::tempdir().unwrap();
        let path = AudioFixture::new(AudioFormat::Flac)
            .seconds(0.5)
            .sample_rate(48_000)
            .bit_depth(BitDepth::TwentyFour)
            .write_into(dir.path())
            .await
            .unwrap();
        let tagged = read(&path);
        assert_eq!(tagged.properties().sample_rate(), Some(48_000));
        assert_eq!(tagged.properties().bit_depth(), Some(24));

        let path = AudioFixture::new(AudioFormat::Wav)
            .seconds(0.5)
            .sample_rate(22_050)
            .bit_depth(BitDepth::ThirtyTwo)
            .channels(1)
            .write_into(dir.path())
            .await
            .unwrap();
        let tagged = read(&path);
        assert_eq!(tagged.properties().sample_rate(), Some(22_050));
        assert_eq!(tagged.properties().bit_depth(), Some(32));
        assert_eq!(tagged.properties().channels(), Some(1));
    }

    #[tokio::test]
    async fn rejects_a_bit_depth_the_encoder_cannot_store() {
        let dir = tempfile::tempdir().unwrap();
        let error = AudioFixture::new(AudioFormat::Flac)
            .seconds(0.2)
            .bit_depth(BitDepth::ThirtyTwo)
            .write_into(dir.path())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AudioFixtureError::UnsupportedBitDepth { bits: 32, .. }
        ));
    }

    #[tokio::test]
    async fn writes_the_full_tag_set() {
        let dir = tempfile::tempdir().unwrap();
        let tags = TrackTags::new("Blue Hour", "Nova Reyes")
            .artists(["Nova Reyes", "Ida Frost"])
            .album("Nightfall")
            .album_artist("Nova Reyes")
            .track(3, 11)
            .disc(1, 2)
            .year(2019)
            .genres(["Ambient"])
            .isrc("USRC17607839");
        for format in AudioFormat::ALL {
            let path = AudioFixture::new(format)
                .seconds(0.6)
                .file_name(format!("tagged.{}", format.extension()))
                .tags(tags.clone())
                .write_into(dir.path())
                .await
                .unwrap();

            let tagged = read(&path);
            let tag = tagged
                .primary_tag()
                .or_else(|| tagged.first_tag())
                .unwrap_or_else(|| panic!("{} has no tag", format.extension()));
            assert_eq!(
                tag.get_string(ItemKey::TrackTitle),
                Some("Blue Hour"),
                "{} title",
                format.extension()
            );
            assert_eq!(
                tag.get_string(ItemKey::TrackArtist),
                Some("Nova Reyes"),
                "{} artist",
                format.extension()
            );
            let mut artists: Vec<&str> = tag
                .get_strings(ItemKey::TrackArtist)
                .chain(tag.get_strings(ItemKey::TrackArtists))
                .collect();
            artists.sort_unstable();
            artists.dedup();
            assert_eq!(
                artists,
                ["Ida Frost", "Nova Reyes"],
                "{} artists",
                format.extension()
            );
            assert_eq!(
                tag.get_string(ItemKey::AlbumTitle),
                Some("Nightfall"),
                "{} album",
                format.extension()
            );
            assert_eq!(
                tag.get_string(ItemKey::AlbumArtist),
                Some("Nova Reyes"),
                "{} album artist",
                format.extension()
            );
            assert_eq!(tag.track(), Some(3), "{} track", format.extension());
            assert_eq!(
                tag.track_total(),
                Some(11),
                "{} track total",
                format.extension()
            );
            assert_eq!(tag.disk(), Some(1), "{} disc", format.extension());
            assert_eq!(
                tag.disk_total(),
                Some(2),
                "{} disc total",
                format.extension()
            );
            assert_eq!(
                tag.get_string(ItemKey::RecordingDate),
                Some("2019"),
                "{} date",
                format.extension()
            );
            let genres: Vec<&str> = tag.get_strings(ItemKey::Genre).collect();
            assert_eq!(genres, ["Ambient"], "{} genres", format.extension());
            assert_eq!(
                tag.get_string(ItemKey::Isrc),
                Some("USRC17607839"),
                "{} isrc",
                format.extension()
            );
        }
    }

    #[tokio::test]
    async fn keeps_repeated_values_where_the_tag_format_has_room() {
        let dir = tempfile::tempdir().unwrap();
        let tags = TrackTags::new("T", "A").genres(["Ambient", "Drone"]);
        for format in AudioFormat::ALL {
            let path = AudioFixture::new(format)
                .seconds(0.3)
                .file_name(format!("repeated.{}", format.extension()))
                .tags(tags.clone())
                .write_into(dir.path())
                .await
                .unwrap();
            let tagged = read(&path);
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag()).unwrap();
            let genres: Vec<&str> = tag.get_strings(ItemKey::Genre).collect();
            let expected: &[&str] = if format == AudioFormat::WavPack {
                &["Drone"]
            } else {
                &["Ambient", "Drone"]
            };
            assert_eq!(genres, expected, "{} genres", format.extension());
        }
    }

    #[tokio::test]
    async fn embeds_the_generated_cover() {
        let dir = tempfile::tempdir().unwrap();
        for (format, jpeg) in [
            (AudioFormat::Flac, false),
            (AudioFormat::Mp3Cbr, true),
            (AudioFormat::M4aAac, false),
        ] {
            let cover_format = if jpeg {
                CoverFormat::Jpeg
            } else {
                CoverFormat::Png
            };
            let fixture = AudioFixture::new(format)
                .seconds(0.4)
                .file_name(format!("covered.{}", format.extension()));
            let fixture = if jpeg {
                fixture.cover_jpeg(3, 0x33_66_99)
            } else {
                fixture.cover_png(3, 0x33_66_99)
            };
            let path = fixture.write_into(dir.path()).await.unwrap();

            let cover = dir
                .path()
                .join(format!("covered.cover.{}", cover_format.extension()));
            assert!(
                cover.is_file(),
                "cover for {} is missing",
                format.extension()
            );

            let tagged = read(&path);
            let tag = tagged.primary_tag().or_else(|| tagged.first_tag()).unwrap();
            let pictures = tag.pictures();
            assert_eq!(
                pictures.len(),
                1,
                "{} embedded the wrong picture count",
                format.extension()
            );
            assert_eq!(
                pictures[0].mime_type().map(ToString::to_string),
                Some(cover_format.mime_type().to_owned()),
                "{} picture mime",
                format.extension()
            );
            assert!(!pictures[0].data().is_empty());
        }
    }

    #[tokio::test]
    async fn writes_into_a_temporary_directory() {
        let (_dir, path) = AudioFixture::new(AudioFormat::Opus)
            .seconds(0.3)
            .write_in_temp()
            .await
            .unwrap();
        assert!(path.is_file());
        assert_eq!(path.file_name().unwrap(), "fixture.opus");
    }
}
