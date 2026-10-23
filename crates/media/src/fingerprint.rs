//! Chromaprint fingerprints of audio files.
//!
//! A fingerprint is what a verification plugin matches against a service such as
//! AcoustID: it compresses the first two minutes of a decoded file into a string
//! that is stable across the formats and bitrates the same recording can be
//! stored in.
//!
//! The decode runs through [`FfmpegRunner`], so fingerprinting spends the same
//! process budget as every other ffmpeg invocation in the server: the first 120
//! seconds are decoded to 44.1 kHz stereo 16-bit little-endian samples on the
//! child's standard output and handed to `rusty-chromaprint` on a blocking
//! thread. The compressed fingerprint is encoded as URL-safe base64 without
//! padding, exactly as `fpcalc` prints it, so a plugin can send it to a service
//! unchanged.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use std::path::Path;
//!
//! use hearthsound_media::{FfmpegRunner, fingerprint};
//!
//! let runner = FfmpegRunner::new(None, 4);
//! let print = fingerprint(Path::new("/music/track.flac"), &runner).await?;
//! println!("{} lasts {} s", print.value, print.duration_secs);
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use data_encoding::BASE64URL_NOPAD;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use tokio::io::AsyncReadExt;

use crate::error::MediaError;
use crate::ffmpeg::FfmpegRunner;
use crate::probe::probe;

/// The sample rate the decoded audio is requested at.
const DECODE_SAMPLE_RATE: u32 = 44_100;

/// The number of channels the decoded audio is requested with.
const DECODE_CHANNELS: u32 = 2;

/// The number of seconds a fingerprint covers, counted from the start.
const FINGERPRINT_SECS: u32 = 120;

/// A Chromaprint fingerprint of a file's first two minutes.
///
/// The value is the compressed fingerprint itself; the duration is the whole
/// file's length, which the service needs to narrow its search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// The compressed fingerprint, URL-safe base64 without padding.
    pub value: String,
    /// The file's full duration in whole seconds.
    pub duration_secs: u32,
}

/// Computes the Chromaprint fingerprint of a file.
///
/// The file is probed first, because the fingerprint carries the whole file's
/// duration rather than the length of the decoded excerpt. The first
/// [`FINGERPRINT_SECS`] seconds are then decoded with ffmpeg under `runner`'s
/// process budget and fingerprinted with `rusty-chromaprint`'s test2 preset,
/// which is the preset `fpcalc` uses. A file shorter than the excerpt is
/// fingerprinted in full.
///
/// # Parameters
///
/// - `path`: the audio file to fingerprint.
/// - `runner`: the ffmpeg runner that decodes it.
///
/// # Returns
///
/// The compressed fingerprint and the file's duration in whole seconds.
///
/// # Errors
///
/// Returns the probe's error when the file cannot be read or parsed,
/// [`MediaError::Process`] when ffmpeg cannot be started or exits with a
/// failure, [`MediaError::Stream`] when its standard output cannot be read, and
/// [`MediaError::Unsupported`] when the fingerprinter refuses the decoded
/// stream.
///
/// # Examples
///
/// ```no_run
/// # async fn example() -> Result<(), hearthsound_media::MediaError> {
/// use std::path::Path;
///
/// use hearthsound_media::{FfmpegRunner, fingerprint};
///
/// let runner = FfmpegRunner::new(None, 4);
/// let print = fingerprint(Path::new("/music/track.flac"), &runner).await?;
/// assert!(!print.value.is_empty());
/// # Ok(())
/// # }
/// ```
pub async fn fingerprint(path: &Path, runner: &FfmpegRunner) -> Result<Fingerprint, MediaError> {
    let probed = probe(path).await?;
    let pcm = decode(path, runner).await?;
    let samples: Vec<i16> = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| i16::from_le_bytes(*pair))
        .collect();
    let owned = path.to_path_buf();
    let value = tokio::task::spawn_blocking(move || {
        let config = Configuration::preset_test2();
        let mut fingerprinter = Fingerprinter::new(&config);
        fingerprinter
            .start(DECODE_SAMPLE_RATE, DECODE_CHANNELS)
            .map_err(|error| MediaError::Unsupported {
                path: owned.clone(),
                message: error.to_string(),
            })?;
        fingerprinter.consume(&samples);
        fingerprinter.finish();
        let compressed = FingerprintCompressor::from(&config).compress(fingerprinter.fingerprint());
        Ok::<String, MediaError>(BASE64URL_NOPAD.encode(&compressed))
    })
    .await
    .map_err(MediaError::task_failed)??;

    let duration_secs = u32::try_from(probed.duration_ms.div_ceil(1000)).unwrap_or(u32::MAX);
    Ok(Fingerprint {
        value,
        duration_secs,
    })
}

/// Decodes the first [`FINGERPRINT_SECS`] seconds of a file to raw samples.
///
/// # Parameters
///
/// - `path`: the file to decode.
/// - `runner`: the ffmpeg runner the child runs under.
///
/// # Returns
///
/// The samples as interleaved little-endian 16-bit pairs.
///
/// # Errors
///
/// Returns [`MediaError::Process`] when ffmpeg cannot be started or exits with a
/// failure, and [`MediaError::Stream`] when its standard output cannot be read.
async fn decode(path: &Path, runner: &FfmpegRunner) -> Result<Vec<u8>, MediaError> {
    let args = [
        "-v".to_owned(),
        "error".to_owned(),
        "-i".to_owned(),
        path.to_string_lossy().into_owned(),
        "-t".to_owned(),
        FINGERPRINT_SECS.to_string(),
        "-ac".to_owned(),
        DECODE_CHANNELS.to_string(),
        "-ar".to_owned(),
        DECODE_SAMPLE_RATE.to_string(),
        "-f".to_owned(),
        "s16le".to_owned(),
        "-".to_owned(),
    ];
    let mut process = runner.spawn(args.map(Into::into)).await?;
    let mut pcm = Vec::new();
    if let Some(mut stdout) = process.take_stdout()
        && let Err(error) = stdout.read_to_end(&mut pcm).await
    {
        process.kill()?;
        return Err(MediaError::Stream {
            message: error.to_string(),
        });
    }
    let status = process.wait().await?;
    if !status.success() {
        return Err(MediaError::Process {
            program: process.program().to_owned(),
            args: path.display().to_string(),
            message: status.to_string(),
        });
    }
    Ok(pcm)
}
