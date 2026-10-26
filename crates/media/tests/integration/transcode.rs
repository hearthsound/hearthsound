//! Tests for transcoding into mp3, Opus, and AAC streams.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use hearthsound_media::{
    Codec, Container, FfmpegRunner, MediaError, TranscodeFormat, TranscodeRequest, probe,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};

/// How long a freshly spawned ffmpeg may take to deliver the first bytes.
///
/// A spawn returns as soon as the child process exists, so the first chunk of a
/// stream arrives only after the child has started up, read its input, and
/// filled the pipe. That takes about a tenth of a second on an idle machine,
/// but a loaded machine can stretch it past a second, so the deadline is the
/// ten seconds a client waits for a stream before giving up.
const FIRST_BYTES_DEADLINE: Duration = Duration::from_secs(10);

/// The container and codec a transcode format produces.
fn expected(format: TranscodeFormat) -> (Container, Codec) {
    match format {
        TranscodeFormat::Mp3 => (Container::Mpeg, Codec::Mp3),
        TranscodeFormat::Opus => (Container::Ogg, Codec::Opus),
        TranscodeFormat::Aac => (Container::Aac, Codec::Aac),
    }
}

/// Reads one ffprobe field of a stream or its container.
///
/// # Parameters
///
/// - `path`: the file to inspect.
/// - `section`: `format` or `stream`.
/// - `field`: the field name, for example `duration`.
async fn ffprobe(path: &Path, section: &str, field: &str) -> Option<String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            &format!("{section}={field}"),
            "-of",
            "default=nw=1",
        ])
        .arg(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{field}=")))
        .map(str::to_owned)
}

/// Drains a stream into one byte vector.
///
/// # Parameters
///
/// - `stream`: the stream to drain.
async fn drain(
    mut stream: impl futures_util::Stream<Item = Result<bytes::Bytes, MediaError>> + Unpin,
) -> Result<Vec<u8>, MediaError> {
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk?);
    }
    Ok(collected)
}

/// Writes encoded bytes beside the fixtures and returns the path.
///
/// # Parameters
///
/// - `dir`: the directory to write into.
/// - `name`: the file name.
/// - `bytes`: the encoded audio.
fn write_encoded(dir: &Path, name: &str, bytes: &[u8]) -> Option<PathBuf> {
    let path = dir.join(name);
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

#[tokio::test]
async fn transcodes_to_every_format() {
    let dir = tempfile::tempdir().unwrap();
    let source = AudioFixture::new(AudioFormat::Flac)
        .seconds(3.0)
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 4);

    for format in TranscodeFormat::ALL {
        let request = TranscodeRequest::new(&source, format);
        let mut stream = runner.transcode(&request).await.unwrap();
        assert_eq!(stream.format(), format);
        assert_eq!(stream.content_type(), format.content_type());

        // The first bytes arrive promptly: clients wait about ten seconds for a
        // stream before giving up.
        let first = tokio::time::timeout(FIRST_BYTES_DEADLINE, stream.next())
            .await
            .expect("the first chunk arrives before the client gives up")
            .expect("the stream has data")
            .unwrap();
        assert!(!first.is_empty(), "{} sent an empty chunk", format);

        let mut collected = first.to_vec();
        collected.extend(drain(stream).await.unwrap());
        let path = write_encoded(
            dir.path(),
            &format!("transcoded.{}", format.suffix()),
            &collected,
        )
        .unwrap();

        let probed = probe(&path).await.unwrap();
        let (container, codec) = expected(format);
        assert_eq!(probed.container, container, "{format}");
        assert_eq!(probed.codec, codec, "{format}");

        let duration: f64 = ffprobe(&path, "format", "duration")
            .await
            .expect("ffprobe reports a duration")
            .parse()
            .unwrap();
        assert!(
            (2.6..=3.4).contains(&duration),
            "{format} produced {duration}s from a three second file"
        );
    }
    assert_eq!(runner.in_flight(), 0);
}

#[tokio::test]
async fn honours_the_bitrate_cap() {
    let dir = tempfile::tempdir().unwrap();
    let source = AudioFixture::new(AudioFormat::Flac)
        .seconds(3.0)
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 4);

    let request = TranscodeRequest::new(&source, TranscodeFormat::Mp3).max_bitrate_kbps(Some(64));
    assert_eq!(request.bitrate_kbps(), 64);
    let stream = runner.transcode(&request).await.unwrap();
    let collected = drain(stream).await.unwrap();
    let path = write_encoded(dir.path(), "capped.mp3", &collected).unwrap();

    let bitrate: f64 = ffprobe(&path, "format", "bit_rate")
        .await
        .expect("ffprobe reports a bitrate")
        .parse()
        .unwrap();
    assert!(
        (56_000.0..=72_000.0).contains(&bitrate),
        "a 64 kbit/s request produced {bitrate} bit/s"
    );

    // A request without a cap uses the format default.
    let default = TranscodeRequest::new(&source, TranscodeFormat::Mp3);
    assert_eq!(default.bitrate_kbps(), 320);
}

#[tokio::test]
async fn starts_at_the_requested_offset() {
    let dir = tempfile::tempdir().unwrap();
    let source = AudioFixture::new(AudioFormat::Flac)
        .seconds(3.0)
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 4);

    let request =
        TranscodeRequest::new(&source, TranscodeFormat::Mp3).offset(Duration::from_secs(1));
    let stream = runner.transcode(&request).await.unwrap();
    let collected = drain(stream).await.unwrap();
    let path = write_encoded(dir.path(), "offset.mp3", &collected).unwrap();

    let duration: f64 = ffprobe(&path, "format", "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    assert!(
        (1.6..=2.4).contains(&duration),
        "an offset of one second left {duration}s of a three second file"
    );
}

#[tokio::test]
async fn kills_ffmpeg_when_the_stream_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    // A long file keeps ffmpeg busy while the test drops the stream.
    let source = AudioFixture::new(AudioFormat::Wav)
        .seconds(600.0)
        .file_name("long.wav")
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 2);

    let mut stream = runner
        .transcode(&TranscodeRequest::new(&source, TranscodeFormat::Mp3))
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    assert_eq!(runner.in_flight(), 1);

    drop(stream);
    let released = tokio::time::timeout(Duration::from_secs(5), async {
        while runner.in_flight() != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(released.is_ok(), "the transcode permit was not released");
}

#[tokio::test]
async fn reports_a_transcode_that_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FfmpegRunner::new(None, 2);
    let request = TranscodeRequest::new(dir.path().join("not-a-file.flac"), TranscodeFormat::Mp3);
    let mut stream = runner.transcode(&request).await.unwrap();
    let first = stream.next().await.expect("the stream reports the failure");
    assert!(
        matches!(first, Err(MediaError::Process { .. })),
        "{first:?}"
    );
    assert_eq!(runner.in_flight(), 0);
}

#[tokio::test]
async fn parses_the_format_names() {
    assert_eq!(
        "mp3".parse::<TranscodeFormat>().unwrap(),
        TranscodeFormat::Mp3
    );
    assert_eq!(
        "opus".parse::<TranscodeFormat>().unwrap(),
        TranscodeFormat::Opus
    );
    assert_eq!(
        "aac".parse::<TranscodeFormat>().unwrap(),
        TranscodeFormat::Aac
    );
    assert_eq!(TranscodeFormat::Mp3.to_string(), "mp3");
    assert_eq!(TranscodeFormat::Opus.content_type(), "audio/ogg");
    assert_eq!(TranscodeFormat::Aac.suffix(), "aac");
    assert!("flac".parse::<TranscodeFormat>().is_err());
    assert!("MP3".parse::<TranscodeFormat>().is_err());

    // The stream type is usable through the trait object the crate exports.
    let dir = tempfile::tempdir().unwrap();
    let source = AudioFixture::new(AudioFormat::Flac)
        .seconds(1.0)
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 2);
    let stream = runner
        .transcode(&TranscodeRequest::new(&source, TranscodeFormat::Aac))
        .await
        .unwrap();
    let boxed: hearthsound_media::AudioStream = Box::pin(stream);
    let chunks = drain(boxed).await.unwrap();
    assert!(!chunks.is_empty());
}

/// A channel cap reaches the encoder as `-ac`.
#[tokio::test]
async fn honours_the_channel_cap() {
    let dir = tempfile::tempdir().unwrap();
    let source = AudioFixture::new(AudioFormat::Flac)
        .seconds(2.0)
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 4);

    let request = TranscodeRequest::new(&source, TranscodeFormat::Mp3).max_channels(Some(1));
    let stream = runner.transcode(&request).await.unwrap();
    let collected = drain(stream).await.unwrap();
    let path = write_encoded(dir.path(), "mono.mp3", &collected).unwrap();

    let channels: u32 = ffprobe(&path, "stream", "channels")
        .await
        .expect("ffprobe reports a channel count")
        .parse()
        .unwrap();
    assert_eq!(channels, 1, "a one-channel request produced {channels}");

    // Zero is the protocol's "any", so it is no cap at all.
    let request = TranscodeRequest::new(&source, TranscodeFormat::Mp3).max_channels(Some(0));
    assert_eq!(request.max_channels, None);
}

/// A stream dropped while ffmpeg has produced nothing still kills the child at
/// once: a client that walks away must not hold a transcode slot until the
/// source wakes up.
#[cfg(unix)]
#[tokio::test]
async fn dropping_a_stalled_stream_kills_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    // A FIFO nobody writes to: ffmpeg blocks opening it, so it never emits a
    // byte - the worst case a disconnect meets.
    let source = dir.path().join("stalled.flac");
    let created = std::process::Command::new("mkfifo")
        .arg(&source)
        .status()
        .expect("mkfifo runs");
    assert!(created.success(), "the fifo was created");

    let runner = FfmpegRunner::new(None, 1);
    let stream = runner
        .transcode(&TranscodeRequest::new(&source, TranscodeFormat::Mp3))
        .await
        .unwrap();
    // Let the reader task block on the silent child.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(runner.in_flight(), 1, "the transcode holds its slot");

    drop(stream);

    // The permit comes back only once the child has exited, so a count of zero
    // says the stalled process is gone.
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while runner.in_flight() > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a dropped stalled stream kept its ffmpeg child alive"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
