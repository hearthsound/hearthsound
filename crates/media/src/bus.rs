//! The pending PCM bus: silence while a track is being acquired, then the file.
//!
//! A pending stream answers a client that asked for a track the server does not
//! have yet. Instead of failing, the server starts playing: one encoder ffmpeg
//! reads raw PCM on its standard input and produces the requested format, and
//! the bus feeds it
//!
//! 1. a two second silence burst, so the client hears something immediately,
//! 2. then silence paced at [`PCM_BYTES_PER_SECOND`], the real-time rate of the
//!    bus format, so the client does not run ahead of the wall clock,
//! 3. then, when the track becomes available, the samples of a decoder ffmpeg
//!    that reads the file faster than real time. The switch happens on the same
//!    PCM pipe, so the client sees one continuous stream.
//!
//! The stream ends when the file finishes, when the caller reports a failure
//! (one second later), or when [`PendingRequest::max_wait`] elapses without a
//! file.
//!
//! The bus is bounded in memory: the burst is a fixed 352800 bytes, pacing is
//! driven by the wall clock, and the encoder's output flows through a bounded
//! queue, so a client that stops reading blocks ffmpeg instead of growing a
//! buffer. Dropping the stream kills the encoder, which ends the feeder, which
//! kills the decoder, and every permit returns to the runner.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example() -> Result<(), hearthsound_media::MediaError> {
//! use std::path::PathBuf;
//! use std::time::Duration;
//!
//! use hearthsound_media::{FfmpegRunner, PendingRequest, TranscodeFormat};
//!
//! let runner = FfmpegRunner::new(None, 8);
//! let request = PendingRequest::new(TranscodeFormat::Mp3, 320, Duration::from_secs(180));
//! let pending = runner.pending_stream(&request).await?;
//! let (stream, control) = pending.into_parts();
//! control.ready(PathBuf::from("/cache/track.flac")).await?;
//! assert!(!control.audio_delivered());
//! # drop(stream);
//! # Ok(())
//! # }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::Stream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use crate::error::MediaError;
use crate::ffmpeg::FfmpegRunner;
use crate::transcode::{AudioStream, TranscodeFormat, chunk_stream};

/// The sample rate of the PCM bus, in hertz.
pub const PCM_SAMPLE_RATE: u32 = 44_100;
/// The number of channels on the PCM bus.
pub const PCM_CHANNELS: u16 = 2;
/// How many bytes one second of PCM audio occupies: 44100 x 2 x 2.
pub const PCM_BYTES_PER_SECOND: u64 = 176_400;

/// How much silence is written before pacing starts.
pub const SILENCE_BURST: Duration = Duration::from_secs(2);

/// The size of the silence burst, in bytes.
const SILENCE_BURST_BYTES: u64 = SILENCE_BURST.as_secs() * PCM_BYTES_PER_SECOND;

/// How often the feeder tops the encoder up while it waits for the file.
const PACING_TICK: Duration = Duration::from_millis(100);

/// How long the stream keeps playing after a failure is reported.
const FAIL_GRACE: Duration = Duration::from_secs(1);

/// How many bytes are moved in one read or write.
const COPY_CHUNK: usize = 64 * 1024;

/// What a pending stream encodes and how long it may wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    /// The format the encoder produces.
    pub format: TranscodeFormat,
    /// The encoder bitrate in kbit/s.
    pub bitrate_kbps: u32,
    /// How long the stream plays before it ends without a file.
    pub max_wait: Duration,
}

impl PendingRequest {
    /// Returns a request for a format and bitrate.
    ///
    /// # Parameters
    ///
    /// - `format`: the output format; the caller resolves this from
    ///   `stream.pending.format`.
    /// - `bitrate_kbps`: the encoder bitrate; values below 8 are raised to 8.
    /// - `max_wait`: how long to play silence before ending the stream; the
    ///   caller resolves this from `stream.pending.max_wait_secs`.
    pub fn new(format: TranscodeFormat, bitrate_kbps: u32, max_wait: Duration) -> PendingRequest {
        PendingRequest {
            format,
            bitrate_kbps: bitrate_kbps.max(8),
            max_wait,
        }
    }
}

/// A pending stream and the handle that steers it.
pub struct PendingStream {
    stream: AudioStream,
    control: PendingControl,
}

impl PendingStream {
    /// Returns the control handle.
    ///
    /// The handle can be used while the stream is being read, which is how the
    /// acquisition reports that the file is ready.
    pub fn control(&self) -> &PendingControl {
        &self.control
    }

    /// Splits the stream into its byte stream and its control handle.
    ///
    /// The response body takes the byte stream; the request task keeps the
    /// handle to report the file or a failure.
    pub fn into_parts(self) -> (AudioStream, PendingControl) {
        (self.stream, self.control)
    }

    /// Reports whether any file audio has been delivered.
    ///
    /// This is the flag the play recorder uses: a pending stream that only ever
    /// played silence is not a play.
    pub fn audio_delivered(&self) -> bool {
        self.control.audio_delivered()
    }
}

impl std::fmt::Debug for PendingStream {
    /// Writes the format and whether file audio has been delivered.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingStream")
            .field("audio_delivered", &self.audio_delivered())
            .finish_non_exhaustive()
    }
}

impl Stream for PendingStream {
    type Item = Result<bytes::Bytes, MediaError>;

    /// Yields the next encoded chunk.
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

/// Steers a running pending stream.
///
/// The handle is cheap to clone and stays usable while the byte stream is
/// consumed.
#[derive(Debug, Clone)]
pub struct PendingControl {
    commands: mpsc::Sender<BusCommand>,
    delivered: Arc<AtomicBool>,
}

impl PendingControl {
    /// Switches the stream from silence to the file.
    ///
    /// The decoder runs faster than real time and pipes into the same encoder,
    /// so the client hears no gap between the padding and the song. The stream
    /// ends when the file ends.
    ///
    /// # Parameters
    ///
    /// - `path`: the file to play.
    ///
    /// # Returns
    ///
    /// `Ok(())` once the switch was handed to the bus, which is not the same as
    /// the file being audible yet.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::PendingEnded`] when the stream already ended, which
    /// means the client is gone or the wait elapsed.
    pub async fn ready(&self, path: impl Into<PathBuf>) -> Result<(), MediaError> {
        self.send(BusCommand::Ready(path.into()), "switch to a file")
            .await
    }

    /// Ends the stream one second from now.
    ///
    /// Silence keeps playing during that second, so the client receives a
    /// well-formed stream that simply stops rather than a gap and a truncated
    /// file.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::PendingEnded`] when the stream already ended.
    pub async fn fail(&self) -> Result<(), MediaError> {
        self.send(BusCommand::Fail, "report a failure").await
    }

    /// Reports whether any file audio has been delivered to the client.
    pub fn audio_delivered(&self) -> bool {
        self.delivered.load(Ordering::SeqCst)
    }

    /// Sends one command into the bus.
    ///
    /// # Parameters
    ///
    /// - `command`: the command to send.
    /// - `action`: what the caller was trying to do, for the error message.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::PendingEnded`] when the feeder is gone.
    async fn send(&self, command: BusCommand, action: &'static str) -> Result<(), MediaError> {
        self.commands
            .send(command)
            .await
            .map_err(|_| MediaError::PendingEnded { action })
    }
}

/// A command from the control handle into the feeder.
#[derive(Debug)]
enum BusCommand {
    /// Play this file instead of silence.
    Ready(PathBuf),
    /// End the stream shortly.
    Fail,
}

impl FfmpegRunner {
    /// Starts a pending stream.
    ///
    /// The encoder takes a concurrency slot immediately, and the decoder that
    /// plays the file takes a second one when [`PendingControl::ready`] is
    /// called, so a pending stream costs two slots while it plays real audio.
    ///
    /// # Parameters
    ///
    /// - `request`: the output format, the bitrate, and how long the stream may
    ///   wait for a file.
    ///
    /// # Returns
    ///
    /// The pending stream and its control handle.
    ///
    /// # Errors
    ///
    /// Returns [`MediaError::Process`] when the encoder cannot be started and
    /// [`MediaError::TaskFailed`] when its pipes are unavailable.
    pub async fn pending_stream(
        &self,
        request: &PendingRequest,
    ) -> Result<PendingStream, MediaError> {
        let mut encoder = self.spawn(encoder_args(request)).await?;
        let stdin = encoder.take_stdin().ok_or_else(|| MediaError::Process {
            program: self.binary().display().to_string(),
            args: "pending encoder".to_owned(),
            message: "ffmpeg reported no standard input".to_owned(),
        })?;
        let stdout = encoder.take_stdout().ok_or_else(|| MediaError::Process {
            program: self.binary().display().to_string(),
            args: "pending encoder".to_owned(),
            message: "ffmpeg reported no standard output".to_owned(),
        })?;

        let (commands, receiver) = mpsc::channel::<BusCommand>(1);
        let delivered = Arc::new(AtomicBool::new(false));
        let stream = chunk_stream(encoder, stdout, PathBuf::from("pending stream"));
        let runner = self.clone();
        let feeder_delivered = Arc::clone(&delivered);
        let max_wait = request.max_wait;
        tokio::spawn(async move {
            feed(stdin, receiver, runner, feeder_delivered, max_wait).await;
        });
        Ok(PendingStream {
            stream,
            control: PendingControl {
                commands,
                delivered,
            },
        })
    }
}

/// Builds the argument vector of the encoder.
///
/// # Parameters
///
/// - `request`: the pending request.
///
/// # Returns
///
/// The ffmpeg arguments: raw PCM on standard input, the requested format on
/// standard output.
fn encoder_args(request: &PendingRequest) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-nostdin".into(),
        "-f".into(),
        "s16le".into(),
        "-ar".into(),
        PCM_SAMPLE_RATE.to_string().into(),
        "-ac".into(),
        PCM_CHANNELS.to_string().into(),
        "-i".into(),
        "pipe:0".into(),
        "-vn".into(),
    ];
    args.extend(request.format.encoder_args(request.bitrate_kbps));
    args.push("-flush_packets".into());
    args.push("1".into());
    args.push("pipe:1".into());
    args
}

/// Builds the argument vector of the decoder that plays a file.
///
/// # Parameters
///
/// - `path`: the file to decode.
///
/// # Returns
///
/// The ffmpeg arguments: the file on disk, PCM on standard output, with no
/// real-time pacing, so the decoder runs as fast as the encoder consumes.
fn decoder_args(path: &Path) -> Vec<OsString> {
    vec![
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-nostdin".into(),
        "-i".into(),
        path.as_os_str().to_owned(),
        "-vn".into(),
        "-f".into(),
        "s16le".into(),
        "-ar".into(),
        PCM_SAMPLE_RATE.to_string().into(),
        "-ac".into(),
        PCM_CHANNELS.to_string().into(),
        "pipe:1".into(),
    ]
}

/// Feeds the encoder until the stream ends.
///
/// The feeder owns the encoder's standard input, so dropping it (when the
/// feeder returns) tells the encoder to flush and exit, which ends the stream.
///
/// # Parameters
///
/// - `stdin`: the encoder's standard input.
/// - `commands`: the control handle's commands.
/// - `runner`: the runner that starts the decoder.
/// - `delivered`: set once file audio reaches the encoder.
/// - `max_wait`: how long to play silence before ending the stream.
async fn feed(
    mut stdin: ChildStdin,
    mut commands: mpsc::Receiver<BusCommand>,
    runner: FfmpegRunner,
    delivered: Arc<AtomicBool>,
    max_wait: Duration,
) {
    let zeros = vec![0u8; COPY_CHUNK];
    let started = Instant::now();
    if write_silence(&mut stdin, &zeros, SILENCE_BURST_BYTES)
        .await
        .is_err()
    {
        return;
    }
    let mut written = SILENCE_BURST_BYTES;

    let mut deadline = Box::pin(tokio::time::sleep(max_wait));
    let mut ticker = tokio::time::interval(PACING_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(BusCommand::Ready(path)) => {
                    play_file(&mut stdin, &runner, &path, &delivered).await;
                    return;
                }
                Some(BusCommand::Fail) => {
                    deadline
                        .as_mut()
                        .reset(tokio::time::Instant::now() + FAIL_GRACE);
                }
                None => break,
            },
            () = &mut deadline => break,
            _ = ticker.tick() => {
                // Keep the silence exactly at the real-time rate: the burst
                // already covers the first two seconds, so the target grows
                // from there.
                let target = SILENCE_BURST_BYTES
                    + (started.elapsed().as_secs_f64() * PCM_BYTES_PER_SECOND as f64) as u64;
                if target > written {
                    let wanted = target - written;
                    if write_silence(&mut stdin, &zeros, wanted).await.is_err() {
                        return;
                    }
                    written += wanted;
                }
            }
        }
    }
    // Dropping `stdin` flushes the encoder and ends the stream.
}

/// Pipes a decoded file into the encoder.
///
/// # Parameters
///
/// - `stdin`: the encoder's standard input.
/// - `runner`: the runner that starts the decoder.
/// - `path`: the file to play.
/// - `delivered`: set once file audio reaches the encoder.
async fn play_file(
    stdin: &mut ChildStdin,
    runner: &FfmpegRunner,
    path: &Path,
    delivered: &AtomicBool,
) {
    let mut decoder = match runner.spawn(decoder_args(path)).await {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::debug!(
                path = %path.display(),
                error = %error,
                "the pending stream could not start its decoder"
            );
            return;
        }
    };
    let Some(mut stdout) = decoder.take_stdout() else {
        return;
    };
    tracing::debug!(path = %path.display(), "the pending stream switched to the file");
    let mut buffer = vec![0u8; COPY_CHUNK];
    let mut complete = false;
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => {
                complete = true;
                break;
            }
            Ok(read) => {
                if stdin.write_all(&buffer[..read]).await.is_err() {
                    // The client is gone, so stop decoding immediately.
                    break;
                }
                delivered.store(true, Ordering::SeqCst);
            }
            Err(error) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %error,
                    "the pending stream stopped reading its decoder"
                );
                break;
            }
        }
    }
    if !complete {
        let _ = decoder.kill();
    }
    let _ = decoder.wait().await;
}

/// Writes silence into the encoder.
///
/// # Parameters
///
/// - `stdin`: the encoder's standard input.
/// - `zeros`: a buffer of zero bytes to write from.
/// - `bytes`: how many bytes of silence to write.
///
/// # Errors
///
/// Returns the write error, which means the encoder is gone.
async fn write_silence(stdin: &mut ChildStdin, zeros: &[u8], bytes: u64) -> std::io::Result<()> {
    let mut remaining = bytes;
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(COPY_CHUNK as u64)).unwrap_or(COPY_CHUNK);
        stdin.write_all(&zeros[..chunk]).await?;
        remaining -= chunk as u64;
    }
    Ok(())
}
