//! Transcoding a stored file into a streaming format.
//!
//! One ffmpeg process per stream decodes the file and encodes the requested
//! format, writing it to its standard output, which the returned
//! [`TranscodeStream`] reads in bounded chunks. The process is killed when the
//! stream is dropped, so an abandoned request gives its concurrency slot back
//! immediately.
//!
//! Output is flushed packet by packet (`-flush_packets 1`), so the first bytes
//! reach the client without waiting for the encoder to fill a buffer.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use std::path::PathBuf;
//!
//! use hearthsound_media::{FfmpegRunner, TranscodeFormat, TranscodeRequest};
//!
//! let runner = FfmpegRunner::new(None, 8);
//! let request = TranscodeRequest::new(PathBuf::from("/music/track.flac"), TranscodeFormat::Mp3)
//!     .max_bitrate_kbps(Some(192));
//! let stream = runner.transcode(&request).await?;
//! assert_eq!(stream.content_type(), "audio/mpeg");
//! # Ok(())
//! # }
//! ```

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use crate::error::MediaError;
use crate::ffmpeg::FfmpegRunner;

/// The encoded byte stream a transcode produces.
///
/// The stream is boxed so a caller can hold it in a field and hand it to the
/// HTTP body type without naming the concrete type the ffmpeg pump builds.
pub type AudioStream = Pin<Box<dyn Stream<Item = Result<Bytes, MediaError>> + Send>>;

/// How many encoded chunks may wait for a slow client before ffmpeg is held
/// back. Combined with the pipe buffer this bounds a stalled stream.
const CHUNK_QUEUE: usize = 8;

/// How many bytes are read from ffmpeg in one chunk.
const CHUNK_BYTES: usize = 32 * 1024;

/// An output format the server transcodes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscodeFormat {
    /// MPEG audio layer 3, encoded by `libmp3lame`.
    Mp3,
    /// Opus in Ogg, encoded by `libopus`.
    Opus,
    /// AAC as a raw ADTS stream.
    Aac,
}

impl TranscodeFormat {
    /// Every format, in the order the server prefers them.
    pub const ALL: [TranscodeFormat; 3] = [
        TranscodeFormat::Mp3,
        TranscodeFormat::Opus,
        TranscodeFormat::Aac,
    ];

    /// Returns the format name, which is also the `format` request parameter.
    pub const fn as_str(self) -> &'static str {
        match self {
            TranscodeFormat::Mp3 => "mp3",
            TranscodeFormat::Opus => "opus",
            TranscodeFormat::Aac => "aac",
        }
    }

    /// Returns the MIME type of the encoded stream.
    pub const fn content_type(self) -> &'static str {
        match self {
            TranscodeFormat::Mp3 => "audio/mpeg",
            // Opus is muxed in Ogg.
            TranscodeFormat::Opus => "audio/ogg",
            // ADTS is a raw AAC stream.
            TranscodeFormat::Aac => "audio/aac",
        }
    }

    /// Returns the file suffix of the encoded stream.
    pub const fn suffix(self) -> &'static str {
        match self {
            TranscodeFormat::Mp3 => "mp3",
            TranscodeFormat::Opus => "opus",
            TranscodeFormat::Aac => "aac",
        }
    }

    /// Returns the bitrate used when the request names no cap.
    pub const fn default_bitrate_kbps(self) -> u32 {
        match self {
            TranscodeFormat::Mp3 => 320,
            TranscodeFormat::Opus => 160,
            TranscodeFormat::Aac => 256,
        }
    }

    /// Returns the ffmpeg encoder and muxer arguments for a bitrate.
    ///
    /// # Parameters
    ///
    /// - `bitrate_kbps`: the target bitrate in kbit/s.
    ///
    /// # Returns
    ///
    /// The `-c:a`, `-b:a` and `-f` arguments, without the input or output.
    pub(crate) fn encoder_args(self, bitrate_kbps: u32) -> Vec<OsString> {
        let bitrate = format!("{bitrate_kbps}k");
        match self {
            TranscodeFormat::Mp3 => vec![
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                bitrate.into(),
                "-f".into(),
                "mp3".into(),
            ],
            TranscodeFormat::Opus => vec![
                "-c:a".into(),
                "libopus".into(),
                "-b:a".into(),
                bitrate.into(),
                "-f".into(),
                "ogg".into(),
            ],
            TranscodeFormat::Aac => vec![
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                bitrate.into(),
                "-f".into(),
                "adts".into(),
            ],
        }
    }
}

impl fmt::Display for TranscodeFormat {
    /// Writes the format name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error returned when a string is not a known [`TranscodeFormat`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown transcode format `{0}`")]
pub struct UnknownTranscodeFormat(pub String);

impl FromStr for TranscodeFormat {
    type Err = UnknownTranscodeFormat;

    /// Parses a format from its name.
    ///
    /// The comparison is exact and case-sensitive, because the name comes from
    /// a `format` request parameter.
    ///
    /// # Errors
    ///
    /// Returns [`UnknownTranscodeFormat`] when `s` is not one of the literals
    /// produced by [`TranscodeFormat::as_str`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TranscodeFormat::ALL
            .iter()
            .copied()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| UnknownTranscodeFormat(s.to_owned()))
    }
}

/// What to transcode and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscodeRequest {
    /// The file to transcode.
    pub path: PathBuf,
    /// The output format.
    pub format: TranscodeFormat,
    /// The bitrate cap in kbit/s, or `None` for the format's default.
    pub max_bitrate_kbps: Option<u32>,
    /// How far into the file the stream starts.
    pub offset: Option<Duration>,
    /// How many channels the output carries, or `None` for the source's own.
    pub max_channels: Option<u8>,
}

impl TranscodeRequest {
    /// Returns a request for a whole file at the format's default bitrate.
    ///
    /// # Parameters
    ///
    /// - `path`: the file to transcode.
    /// - `format`: the output format.
    pub fn new(path: impl Into<PathBuf>, format: TranscodeFormat) -> TranscodeRequest {
        TranscodeRequest {
            path: path.into(),
            format,
            max_bitrate_kbps: None,
            offset: None,
            max_channels: None,
        }
    }

    /// Sets the bitrate cap.
    ///
    /// # Parameters
    ///
    /// - `max_bitrate_kbps`: the cap in kbit/s, or `None` to use the format's
    ///   default. Values below 8 are raised to 8, which is the lowest bitrate
    ///   the encoders accept.
    pub fn max_bitrate_kbps(mut self, max_bitrate_kbps: Option<u32>) -> Self {
        self.max_bitrate_kbps = max_bitrate_kbps;
        self
    }

    /// Sets how many channels the output carries.
    ///
    /// # Parameters
    ///
    /// - `max_channels`: the channel count, or `None` for the source's own. A
    ///   count of zero is read as no limit, because the protocol uses zero for
    ///   "any" and an encoder accepts no zero channels.
    pub fn max_channels(mut self, max_channels: Option<u8>) -> Self {
        self.max_channels = max_channels.filter(|channels| *channels > 0);
        self
    }

    /// Sets the start offset.
    ///
    /// # Parameters
    ///
    /// - `offset`: how far into the file the stream starts.
    pub fn offset(mut self, offset: Duration) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Returns the bitrate the encoder is asked for.
    pub fn bitrate_kbps(&self) -> u32 {
        self.max_bitrate_kbps
            .unwrap_or_else(|| self.format.default_bitrate_kbps())
            .max(8)
    }
}

/// An encoded stream produced by ffmpeg.
///
/// The stream yields the encoder's output as it is flushed and ends when ffmpeg
/// exits. Dropping it kills the process and releases its concurrency slot.
pub struct TranscodeStream {
    format: TranscodeFormat,
    stream: AudioStream,
}

impl TranscodeStream {
    /// Returns the format the stream is encoded in.
    pub fn format(&self) -> TranscodeFormat {
        self.format
    }

    /// Returns the MIME type of the stream.
    pub fn content_type(&self) -> &'static str {
        self.format.content_type()
    }
}

impl fmt::Debug for TranscodeStream {
    /// Writes the format and the fact that the stream is opaque.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscodeStream")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Stream for TranscodeStream {
    type Item = Result<Bytes, MediaError>;

    /// Yields the next encoded chunk.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

impl FfmpegRunner {
    /// Starts a transcode of a stored file.
    ///
    /// The child holds a concurrency slot for as long as the returned stream
    /// lives. The stream is fed from a bounded queue, so a client that stops
    /// reading blocks ffmpeg rather than growing a buffer.
    ///
    /// # Parameters
    ///
    /// - `request`: the file, the output format, the bitrate cap, and the start
    ///   offset.
    ///
    /// # Returns
    ///
    /// The encoded stream.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when ffmpeg cannot be started and
    /// [`MediaError::TaskFailed`] when its standard output is unavailable.
    pub async fn transcode(
        &self,
        request: &TranscodeRequest,
    ) -> Result<TranscodeStream, MediaError> {
        let args = transcode_args(request);
        let mut process = self.spawn(args).await?;
        let stdout = process.take_stdout().ok_or_else(|| MediaError::Process {
            program: self.binary().display().to_string(),
            args: "transcode".to_owned(),
            message: "ffmpeg reported no standard output".to_owned(),
        })?;
        Ok(TranscodeStream {
            format: request.format,
            stream: chunk_stream(process, stdout, request.path.clone()),
        })
    }
}

/// Builds the argument vector of a transcode.
///
/// # Parameters
///
/// - `request`: the transcode to run.
///
/// # Returns
///
/// The ffmpeg arguments, with the seek before the input so ffmpeg seeks
/// instead of decoding and discarding.
pub(crate) fn transcode_args(request: &TranscodeRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    if let Some(offset) = request.offset.filter(|offset| !offset.is_zero()) {
        args.push("-ss".into());
        args.push(format!("{:.3}", offset.as_secs_f64()).into());
    }
    args.push("-i".into());
    args.push(request.path.as_os_str().to_owned());
    args.push("-vn".into());
    args.extend(request.format.encoder_args(request.bitrate_kbps()));
    if let Some(channels) = request.max_channels {
        args.push("-ac".into());
        args.push(channels.to_string().into());
    }
    args.push("-flush_packets".into());
    args.push("1".into());
    args.push("pipe:1".into());
    args
}

/// Reads a child's standard output into a bounded channel.
///
/// The child is moved into the task so that dropping the returned stream (and
/// therefore the channel) ends the task, kills the child, and releases its
/// permit. A read error is forwarded to the client as a stream error.
///
/// # Parameters
///
/// - `process`: the running child, which the task owns.
/// - `stdout`: the child's standard output.
/// - `path`: the file being transcoded, used in error messages.
///
/// # Returns
///
/// The boxed stream of encoded chunks.
pub(crate) fn chunk_stream(
    mut process: crate::ffmpeg::FfmpegProcess,
    mut stdout: tokio::process::ChildStdout,
    label: PathBuf,
) -> AudioStream {
    let (sender, receiver) = mpsc::channel::<Result<Bytes, MediaError>>(CHUNK_QUEUE);
    let program = process.program().to_owned();
    tokio::spawn(async move {
        let mut buffer = vec![0u8; CHUNK_BYTES];
        loop {
            match stdout.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = Bytes::copy_from_slice(&buffer[..read]);
                    if sender.send(Ok(chunk)).await.is_err() {
                        // The client went away: kill ffmpeg instead of letting
                        // it fill a pipe nobody reads.
                        let _ = process.kill();
                        break;
                    }
                }
                Err(source) => {
                    let _ = sender
                        .send(Err(MediaError::Stream {
                            message: source.to_string(),
                        }))
                        .await;
                    let _ = process.kill();
                    break;
                }
            }
        }
        // A transcode that failed before it produced a byte, or one that died
        // halfway, still has to reach the client as an error rather than as a
        // truncated stream. The standard error itself went to the debug log.
        match process.wait().await {
            Ok(status) if !status.success() => {
                let _ = sender
                    .send(Err(MediaError::Process {
                        program,
                        args: label.display().to_string(),
                        message: format!("ffmpeg exited with {status}"),
                    }))
                    .await;
            }
            Ok(_) => {}
            Err(error) => {
                let _ = sender.send(Err(error)).await;
            }
        }
        // Dropping the handle releases the concurrency permit.
    });
    Box::pin(ChannelStream { receiver })
}

/// A [`Stream`] over the chunks a reader task produces.
struct ChannelStream {
    receiver: mpsc::Receiver<Result<Bytes, MediaError>>,
}

impl Stream for ChannelStream {
    type Item = Result<Bytes, MediaError>;

    /// Receives the next chunk from the reader task.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

/// Estimates the encoded size of a stream.
///
/// The estimate is the transcode's bitrate multiplied by its duration, which is
/// what the Subsonic `estimateContentLength` parameter asks for: clients need a
/// length up front, and an exact length is only known once the encoder has
/// finished.
///
/// # Parameters
///
/// - `duration`: the length of the stream, after any offset was subtracted.
/// - `bitrate_kbps`: the bitrate the encoder was asked for.
///
/// # Returns
///
/// The estimated length in bytes.
///
/// # Examples
///
/// ```
/// use std::time::Duration;
///
/// use hearthsound_media::estimate_content_length;
///
/// // A three minute song at 192 kbit/s is about 4.3 MB.
/// let estimate = estimate_content_length(Duration::from_secs(180), 192);
/// assert_eq!(estimate, 4_320_000);
/// ```
pub fn estimate_content_length(duration: Duration, bitrate_kbps: u32) -> u64 {
    let milliseconds = duration.as_millis();
    let bytes = milliseconds * u128::from(bitrate_kbps) / 8;
    u64::try_from(bytes).unwrap_or(u64::MAX)
}
