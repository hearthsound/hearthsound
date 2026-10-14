//! Probing media files for their format, duration, losslessness, and tags.
//!
//! A probe reads only the headers and the tags, never the samples, so it stays
//! cheap enough to run over a whole library. Tag reading and format detection
//! go through `lofty`, which knows every container the server imports; the
//! full-decode check in [`verify_decodes`] goes through `symphonia` and falls
//! back to an `ffmpeg` decode for the codecs `symphonia` cannot decode, which is
//! currently Opus.
//!
//! Every function is asynchronous and does its blocking work on a blocking
//! thread.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use std::path::Path;
//!
//! let result = hearthsound_media::probe(Path::new("/music/track.flac")).await?;
//! assert!(result.lossless);
//! # Ok(())
//! # }
//! ```

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use lofty::config::ParseOptions;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::prelude::Accessor;
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

use crate::error::MediaError;
use crate::ffmpeg::FfmpegRunner;
use crate::tags::{Artwork, TrackTags};

/// The container a media file is stored in.
///
/// The variants are the containers the server imports, not every container
/// `lofty` knows. Vorbis, Opus, and Speex files all live in Ogg, so the
/// container alone does not name the codec; [`Codec`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Container {
    /// A raw AAC stream, as found in `.aac` ADTS files.
    Aac,
    /// Audio Interchange File Format.
    Aiff,
    /// Monkey's Audio.
    Ape,
    /// Free Lossless Audio Codec.
    Flac,
    /// MPEG audio (MP1, MP2, MP3).
    Mpeg,
    /// MPEG-4 Audio, as found in `.m4a` files.
    Mp4,
    /// Musepack.
    Musepack,
    /// Ogg, carrying Vorbis, Opus, or Speex.
    Ogg,
    /// WavPack.
    WavPack,
    /// RIFF/WAVE.
    Wav,
    /// A container the probe could not identify.
    Unknown,
}

impl Container {
    /// Returns the literal stored in `media_files.format`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Container::Aac => "aac",
            Container::Aiff => "aiff",
            Container::Ape => "ape",
            Container::Flac => "flac",
            Container::Mpeg => "mpeg",
            Container::Mp4 => "mp4",
            Container::Musepack => "musepack",
            Container::Ogg => "ogg",
            Container::WavPack => "wavpack",
            Container::Wav => "wav",
            Container::Unknown => "unknown",
        }
    }

    /// Maps a `lofty` file type onto a container.
    ///
    /// # Parameters
    ///
    /// - `file_type`: the file type `lofty` detected.
    fn from_file_type(file_type: FileType) -> Container {
        match file_type {
            FileType::Aac => Container::Aac,
            FileType::Aiff => Container::Aiff,
            FileType::Ape => Container::Ape,
            FileType::Flac => Container::Flac,
            FileType::Mpeg => Container::Mpeg,
            FileType::Mp4 => Container::Mp4,
            FileType::Mpc => Container::Musepack,
            FileType::Opus | FileType::Speex | FileType::Vorbis => Container::Ogg,
            FileType::Wav => Container::Wav,
            FileType::WavPack => Container::WavPack,
            _ => Container::Unknown,
        }
    }
}

/// The codec the samples are encoded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    /// Advanced Audio Coding.
    Aac,
    /// Apple Lossless Audio Codec.
    Alac,
    /// Monkey's Audio.
    Ape,
    /// Free Lossless Audio Codec.
    Flac,
    /// MPEG audio layer 3.
    Mp3,
    /// Musepack.
    Musepack,
    /// Opus.
    Opus,
    /// Uncompressed pulse-code modulation.
    Pcm,
    /// Speex.
    Speex,
    /// Ogg Vorbis.
    Vorbis,
    /// WavPack.
    WavPack,
    /// A codec the probe could not identify.
    Unknown,
}

impl Codec {
    /// Returns the literal stored in `media_files.codec`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Codec::Aac => "aac",
            Codec::Alac => "alac",
            Codec::Ape => "ape",
            Codec::Flac => "flac",
            Codec::Mp3 => "mp3",
            Codec::Musepack => "musepack",
            Codec::Opus => "opus",
            Codec::Pcm => "pcm",
            Codec::Speex => "speex",
            Codec::Vorbis => "vorbis",
            Codec::WavPack => "wavpack",
            Codec::Unknown => "unknown",
        }
    }

    /// Reports whether the codec stores samples without loss.
    ///
    /// # Returns
    ///
    /// `true` for FLAC, ALAC, WavPack, Monkey's Audio, and PCM. The acquisition
    /// verification uses this to reject a lossy file that claimed a lossless
    /// quality tier.
    pub const fn is_lossless(self) -> bool {
        matches!(
            self,
            Codec::Alac | Codec::Ape | Codec::Flac | Codec::Pcm | Codec::WavPack
        )
    }
}

/// What a probe found in a media file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    /// The container the file is stored in.
    pub container: Container,
    /// The codec the samples are encoded with.
    pub codec: Codec,
    /// Whether the codec stores samples without loss.
    pub lossless: bool,
    /// The audio bitrate in kbit/s, when the container records one.
    pub bitrate_kbps: Option<u32>,
    /// The sample rate in hertz, when the container records one.
    pub sample_rate: Option<u32>,
    /// The sample bit depth, when the codec has one.
    pub bit_depth: Option<u8>,
    /// The number of channels, when the container records it.
    pub channels: Option<u8>,
    /// The duration in milliseconds as the container reports it.
    pub duration_ms: u64,
    /// The tags found in the file's primary tag.
    pub tags: TrackTags,
    /// Whether the file carries an embedded picture.
    pub has_embedded_art: bool,
}

/// Which decoder verified a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeBackend {
    /// The pure-Rust `symphonia` decoder.
    Symphonia,
    /// The `ffmpeg` decoder, used for the codecs `symphonia` lacks.
    Ffmpeg,
}

impl DecodeBackend {
    /// Returns a lowercase name for the backend.
    pub const fn as_str(self) -> &'static str {
        match self {
            DecodeBackend::Symphonia => "symphonia",
            DecodeBackend::Ffmpeg => "ffmpeg",
        }
    }
}

/// What a full-decode verification found.
///
/// The duration is only known when the decoder reports it: `symphonia` sums the
/// frames it decoded, while the `ffmpeg` fallback decodes to a null sink and
/// reports no frame count. Verification compares the decoded duration against
/// the duration a catalog provider claims when it is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeReport {
    /// Which decoder ran the check.
    pub backend: DecodeBackend,
    /// The decoded duration in milliseconds, when the decoder counted frames.
    pub duration_ms: Option<u64>,
    /// The sample rate of the decoded audio, when the decoder reported one.
    pub sample_rate: Option<u32>,
    /// The number of channels of the decoded audio, when the decoder reported
    /// them.
    pub channels: Option<u32>,
    /// The number of frames the decoder decoded.
    pub frames: Option<u64>,
    /// How many packets the decoder could not decode and skipped.
    ///
    /// A file can still be considered decoded when a few packets were damaged;
    /// the verification layer decides how many are too many.
    pub damaged_packets: u32,
}

/// Probes a media file.
///
/// The file is opened once for the tags and the container properties, and a
/// second time when the container is MP4, which is the only container whose
/// codec is not implied by its file type.
///
/// # Parameters
///
/// - `path`: the file to probe.
///
/// # Returns
///
/// The container, codec, technical properties, tags, and whether the file has
/// embedded art.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read,
/// [`MediaError::Tag`] when it cannot be parsed, and
/// [`MediaError::Unsupported`] when its format is unknown.
pub async fn probe(path: &Path) -> Result<ProbeResult, MediaError> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || probe_blocking(&owned))
        .await
        .map_err(MediaError::task_failed)?
}

/// Probes a media file on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to probe.
///
/// # Errors
///
/// Returns the same errors as [`probe`].
fn probe_blocking(path: &Path) -> Result<ProbeResult, MediaError> {
    let tagged = read_tagged_file(path)?;
    let file_type = tagged.file_type();
    let codec = codec_of(file_type, path)?;
    let properties = tagged.properties();
    Ok(ProbeResult {
        container: Container::from_file_type(file_type),
        codec,
        lossless: codec.is_lossless(),
        bitrate_kbps: properties
            .audio_bitrate()
            .or_else(|| properties.overall_bitrate()),
        sample_rate: properties.sample_rate(),
        bit_depth: properties.bit_depth(),
        channels: properties.channels(),
        duration_ms: u64::try_from(properties.duration().as_millis()).unwrap_or(u64::MAX),
        tags: read_tags(&tagged),
        has_embedded_art: tagged.tags().iter().any(|tag| !tag.pictures().is_empty()),
    })
}

/// Extracts the first embedded picture from a media file.
///
/// # Parameters
///
/// - `path`: the file to read.
///
/// # Returns
///
/// The first picture of the file's first tag that carries one, or `None` when
/// the file has no embedded art.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read and
/// [`MediaError::Tag`] when it cannot be parsed.
pub async fn extract_picture(path: &Path) -> Result<Option<Artwork>, MediaError> {
    let owned = path.to_path_buf();
    tokio::task::spawn_blocking(move || extract_picture_blocking(&owned))
        .await
        .map_err(MediaError::task_failed)?
}

/// Extracts the first embedded picture on the calling thread.
///
/// # Parameters
///
/// - `path`: the file to read.
///
/// # Errors
///
/// Returns the same errors as [`extract_picture`].
fn extract_picture_blocking(path: &Path) -> Result<Option<Artwork>, MediaError> {
    let tagged = read_tagged_file(path)?;
    for tag in tagged.tags() {
        if let Some(picture) = tag.pictures().first() {
            let mime_type = picture.mime_type().map_or_else(
                || "application/octet-stream".to_owned(),
                |mime| mime.as_str().to_owned(),
            );
            return Ok(Some(Artwork::new(mime_type, picture.data().to_vec())));
        }
    }
    Ok(None)
}

/// Decodes a media file from end to end.
///
/// The file is decoded with `symphonia` when `symphonia` has a decoder for its
/// codec. When it does not, which is the case for Opus and for containers
/// `symphonia` cannot read, the file is decoded by `ffmpeg` instead, which
/// proves that the file is decodable but reports no frame count. The fallback
/// runs through the runner, so it counts against the process budget.
///
/// # Parameters
///
/// - `path`: the file to decode.
/// - `runner`: the ffmpeg runner used for the fallback decode.
///
/// # Returns
///
/// The backend that verified the file and what it measured.
///
/// # Errors
///
/// Returns [`MediaError::Decode`] when the file cannot be decoded from start to
/// finish, and [`MediaError::Io`] when it cannot be read at all.
pub async fn verify_decodes(
    path: &Path,
    runner: &FfmpegRunner,
) -> Result<DecodeReport, MediaError> {
    let owned = path.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || symphonia_decode(&owned))
        .await
        .map_err(MediaError::task_failed)?;
    match outcome {
        Ok(report) => Ok(report),
        Err(SymphoniaFailure::Unsupported(message)) => ffmpeg_decode(path, runner, &message).await,
        Err(SymphoniaFailure::Failed(message)) => Err(MediaError::decode(path, message)),
    }
}

/// Why `symphonia` could not verify a file.
#[derive(Debug)]
enum SymphoniaFailure {
    /// No `symphonia` reader applies to the file, so `ffmpeg` has to try: the
    /// codec or the container is not registered, or the probe found only a
    /// container whose stream ends before its first block.
    Unsupported(String),
    /// The file is unreadable or does not decode, so it must be rejected.
    Failed(String),
}

/// Decodes a file with `symphonia` and sums the decoded frames.
///
/// # Parameters
///
/// - `path`: the file to decode.
///
/// # Errors
///
/// Returns [`SymphoniaFailure`], which says whether the failure is a missing
/// decoder or a broken file.
fn symphonia_decode(path: &Path) -> Result<DecodeReport, SymphoniaFailure> {
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let file = File::open(path).map_err(|error| SymphoniaFailure::Failed(error.to_string()))?;
    let source = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(classify_probe_error)?;

    let (track_id, decoder) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| SymphoniaFailure::Unsupported("no audio track".to_owned()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| SymphoniaFailure::Unsupported("no audio codec parameters".to_owned()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(classify_symphonia_error)?;
        (track.id, decoder)
    };

    let mut decoder = decoder;
    let mut frames: u64 = 0;
    let mut sample_rate = None;
    let mut channels = None;
    let mut damaged_packets: u32 = 0;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            // A chained stream changed its track list. Everything before the
            // chain boundary decoded, so the file is not treated as broken.
            Err(SymphoniaError::ResetRequired) => {
                damaged_packets += 1;
                break;
            }
            Err(error) => return Err(classify_symphonia_error(error)),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buffer) => {
                frames += buffer.frames() as u64;
                sample_rate = Some(buffer.spec().rate());
                channels = Some(buffer.spec().channels().count() as u32);
            }
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {
                damaged_packets += 1;
            }
            Err(error) => return Err(classify_symphonia_error(error)),
        }
    }

    let duration_ms = match (sample_rate, frames) {
        (Some(rate), frames) if rate > 0 => Some(frames * 1000 / u64::from(rate)),
        _ => None,
    };
    Ok(DecodeReport {
        backend: DecodeBackend::Symphonia,
        duration_ms,
        sample_rate,
        channels,
        frames: Some(frames),
        damaged_packets,
    })
}

/// Maps a `symphonia` probe error onto a verification failure.
///
/// The probe scans the stream for the marker of a registered container, and a
/// marker can occur by chance inside the data of a codec `symphonia` cannot
/// read. The reader that matched the chance marker then walks the stream
/// looking for a structure it never finds, and reports an unexpected end of
/// file when the stream runs out. Nothing about that says the file is broken,
/// so the probe falls back to `ffmpeg` exactly as it does for a missing reader.
/// Every other probe error keeps the file out of `ffmpeg`'s hands and is
/// reported as a failure.
///
/// # Parameters
///
/// - `error`: the error the probe returned.
///
/// # Returns
///
/// [`SymphoniaFailure::Unsupported`] for a missing reader and for an unexpected
/// end of file, and [`SymphoniaFailure::Failed`] for everything else.
fn classify_probe_error(error: symphonia::core::errors::Error) -> SymphoniaFailure {
    match &error {
        symphonia::core::errors::Error::Unsupported(message) => {
            SymphoniaFailure::Unsupported((*message).to_owned())
        }
        symphonia::core::errors::Error::IoError(source)
            if source.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            SymphoniaFailure::Unsupported(error.to_string())
        }
        _ => SymphoniaFailure::Failed(error.to_string()),
    }
}

/// Maps a `symphonia` error onto a verification failure.
///
/// # Parameters
///
/// - `error`: the error to classify.
///
/// # Returns
///
/// [`SymphoniaFailure::Unsupported`] for a missing format reader or codec, so
/// the caller falls back to `ffmpeg`, and [`SymphoniaFailure::Failed`] for
/// everything else.
fn classify_symphonia_error(error: symphonia::core::errors::Error) -> SymphoniaFailure {
    match error {
        symphonia::core::errors::Error::Unsupported(message) => {
            SymphoniaFailure::Unsupported(message.to_owned())
        }
        other => SymphoniaFailure::Failed(other.to_string()),
    }
}

/// Decodes a file with `ffmpeg` into a null sink.
///
/// `ffmpeg -v error -xerror -i <file> -f null -` decodes every packet and
/// discards the samples; a non-zero exit status means the file does not decode.
/// `-xerror` turns a decode error into a failure exit status: without it a
/// stream that ends inside a block only prints the error and ffmpeg still exits
/// successfully, which would accept a truncated file.
///
/// # Parameters
///
/// - `path`: the file to decode.
/// - `runner`: the runner the decode counts against.
/// - `reason`: why `symphonia` was skipped, recorded at debug level.
///
/// # Errors
///
/// Returns [`MediaError::Decode`] when ffmpeg rejects the file and
/// [`MediaError::Process`] when ffmpeg cannot be started.
async fn ffmpeg_decode(
    path: &Path,
    runner: &FfmpegRunner,
    reason: &str,
) -> Result<DecodeReport, MediaError> {
    let args: Vec<std::ffi::OsString> = vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-xerror".into(),
        "-i".into(),
        path.as_os_str().to_owned(),
        "-f".into(),
        "null".into(),
        "-".into(),
    ];
    tracing::debug!(
        path = %path.display(),
        reason,
        "decoding with ffmpeg"
    );
    match runner.run(args).await {
        Ok(()) => {}
        Err(MediaError::Process { message, .. }) => {
            return Err(MediaError::decode(path, message));
        }
        Err(other) => return Err(other),
    }
    Ok(DecodeReport {
        backend: DecodeBackend::Ffmpeg,
        duration_ms: None,
        sample_rate: None,
        channels: None,
        frames: None,
        damaged_packets: 0,
    })
}

/// Reads the tagged file for a path, detecting the format from the content.
///
/// # Parameters
///
/// - `path`: the file to read.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read,
/// [`MediaError::Tag`] when it cannot be parsed, and
/// [`MediaError::Unsupported`] when no format matches.
fn read_tagged_file(path: &Path) -> Result<TaggedFile, MediaError> {
    open_probe(path)?.read().map_err(|source| {
        if source.is_unknown_format() {
            MediaError::unsupported(path, "no known container was detected")
        } else {
            MediaError::tag(path, source)
        }
    })
}

/// Opens an audio file and detects its container.
///
/// The file is opened explicitly so that a missing or unreadable file is
/// reported as an IO error rather than as a parse error, and the container is
/// detected from the file's content, not from its extension.
///
/// # Parameters
///
/// - `path`: the file to open.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read and
/// [`MediaError::Unsupported`] when no known container was detected.
pub(crate) fn open_probe(path: &Path) -> Result<Probe<BufReader<File>>, MediaError> {
    let file = File::open(path).map_err(|source| MediaError::io(path, source))?;
    let probe = Probe::new(BufReader::new(file))
        .guess_file_type()
        .map_err(|source| MediaError::io(path, source))?;
    if probe.file_type().is_none() {
        return Err(MediaError::unsupported(
            path,
            "no known container was detected",
        ));
    }
    Ok(probe)
}

/// Detects the container of an audio file.
///
/// # Parameters
///
/// - `path`: the file to inspect.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read and
/// [`MediaError::Unsupported`] when no known container was detected.
pub(crate) fn detect_file_type(path: &Path) -> Result<FileType, MediaError> {
    open_probe(path)?
        .file_type()
        .ok_or_else(|| MediaError::unsupported(path, "no known container was detected"))
}

/// Resolves the codec of a file.
///
/// # Parameters
///
/// - `file_type`: the file type `lofty` detected.
/// - `path`: the file, read a second time for MP4 files.
///
/// # Errors
///
/// Returns [`MediaError::Io`] or [`MediaError::Tag`] when an MP4 file cannot be
/// parsed to find its codec.
fn codec_of(file_type: FileType, path: &Path) -> Result<Codec, MediaError> {
    Ok(match file_type {
        FileType::Aac => Codec::Aac,
        FileType::Aiff | FileType::Wav => Codec::Pcm,
        FileType::Ape => Codec::Ape,
        FileType::Flac => Codec::Flac,
        // MPEG audio layers 1 and 2 are rare enough that the probe does not
        // distinguish them from layer 3.
        FileType::Mpeg => Codec::Mp3,
        FileType::Mp4 => match mp4_codec(path)? {
            Some(Mp4Codec::AAC) => Codec::Aac,
            Some(Mp4Codec::ALAC) => Codec::Alac,
            Some(Mp4Codec::MP3) => Codec::Mp3,
            Some(Mp4Codec::FLAC) => Codec::Flac,
            _ => Codec::Unknown,
        },
        FileType::Mpc => Codec::Musepack,
        FileType::Opus => Codec::Opus,
        FileType::Speex => Codec::Speex,
        FileType::Vorbis => Codec::Vorbis,
        FileType::WavPack => Codec::WavPack,
        _ => Codec::Unknown,
    })
}

/// Reads the codec out of an MP4 file.
///
/// MP4 is the only container the probe cannot classify from its file type,
/// because it carries both AAC and ALAC.
///
/// # Parameters
///
/// - `path`: the MP4 file.
///
/// # Errors
///
/// Returns [`MediaError::Io`] when the file cannot be read and
/// [`MediaError::Tag`] when it cannot be parsed.
fn mp4_codec(path: &Path) -> Result<Option<Mp4Codec>, MediaError> {
    let file = File::open(path).map_err(|source| MediaError::io(path, source))?;
    let mut reader = BufReader::new(file);
    let mp4 = Mp4File::read_from(&mut reader, ParseOptions::new())
        .map_err(|source| MediaError::tag(path, source))?;
    Ok(mp4.properties().codec())
}

/// Reads the tag set out of a tagged file.
///
/// The primary tag is used, falling back to the first tag present, which is how
/// an MP3 with only an APE or ID3v1 tag is still read.
///
/// # Parameters
///
/// - `tagged`: the file whose tags to read.
///
/// # Returns
///
/// The tags, with repeated artist and genre values preserved.
fn read_tags(tagged: &TaggedFile) -> TrackTags {
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return TrackTags::default();
    };
    let mut artists: Vec<String> = Vec::new();
    if let Some(artist) = tag.get_string(ItemKey::TrackArtist) {
        artists.push(artist.to_owned());
    }
    for artist in tag.get_strings(ItemKey::TrackArtists) {
        if !artists.iter().any(|known| known == artist) {
            artists.push(artist.to_owned());
        }
    }
    TrackTags {
        title: text(tag, ItemKey::TrackTitle),
        artists,
        album: text(tag, ItemKey::AlbumTitle),
        album_artist: text(tag, ItemKey::AlbumArtist),
        track_number: tag.track(),
        track_total: tag.track_total(),
        disc_number: tag.disk(),
        disc_total: tag.disk_total(),
        date: text(tag, ItemKey::RecordingDate).or_else(|| text(tag, ItemKey::Year)),
        genres: non_empty(tag.get_strings(ItemKey::Genre)),
        isrc: text(tag, ItemKey::Isrc),
        mb_recording_id: text(tag, ItemKey::MusicBrainzRecordingId),
        mb_release_id: text(tag, ItemKey::MusicBrainzReleaseId),
        mb_release_group_id: text(tag, ItemKey::MusicBrainzReleaseGroupId),
        mb_artist_id: text(tag, ItemKey::MusicBrainzArtistId),
        mb_album_artist_id: text(tag, ItemKey::MusicBrainzReleaseArtistId),
        replaygain_track_gain: text(tag, ItemKey::ReplayGainTrackGain),
        replaygain_track_peak: text(tag, ItemKey::ReplayGainTrackPeak),
        replaygain_album_gain: text(tag, ItemKey::ReplayGainAlbumGain),
        replaygain_album_peak: text(tag, ItemKey::ReplayGainAlbumPeak),
        acquired_via: None,
    }
}

/// Reads one text tag.
///
/// # Parameters
///
/// - `tag`: the tag to read from.
/// - `key`: the tag key.
///
/// # Returns
///
/// The value, or `None` when the key is absent or empty.
fn text(tag: &Tag, key: ItemKey) -> Option<String> {
    tag.get_string(key)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Collects the non-empty values of a repeated tag.
///
/// # Parameters
///
/// - `values`: the values to filter.
fn non_empty<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::errors::Error as SymphoniaError;

    #[test]
    fn a_probe_that_runs_out_of_stream_falls_back_to_ffmpeg() {
        let chance_marker = SymphoniaError::IoError(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "out of bounds",
        ));
        assert!(matches!(
            classify_probe_error(chance_marker),
            SymphoniaFailure::Unsupported(_)
        ));

        let missing_reader = SymphoniaError::Unsupported("core (probe): no suitable format reader");
        assert!(matches!(
            classify_probe_error(missing_reader),
            SymphoniaFailure::Unsupported(_)
        ));
    }

    #[test]
    fn a_probe_that_cannot_read_the_file_is_reported_as_a_failure() {
        let unreadable = SymphoniaError::IoError(std::io::Error::other("input/output error"));
        assert!(matches!(
            classify_probe_error(unreadable),
            SymphoniaFailure::Failed(_)
        ));

        let malformed = SymphoniaError::DecodeError("flac: bad stream info");
        assert!(matches!(
            classify_probe_error(malformed),
            SymphoniaFailure::Failed(_)
        ));
    }
}
