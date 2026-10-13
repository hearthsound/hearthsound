//! Tests for the pending PCM bus: silence padding, file handoff, and cleanup.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use hearthsound_media::{
    Codec, Container, FfmpegRunner, MediaError, PendingRequest, TranscodeFormat, probe,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};

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

/// Writes encoded bytes into a directory.
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

/// Reads one ffprobe field of a container.
///
/// # Parameters
///
/// - `path`: the file to inspect.
/// - `field`: the field name, for example `duration`.
async fn ffprobe(path: &Path, field: &str) -> Option<String> {
    let output = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            &format!("format={field}"),
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

/// Returns a fixture with a file of the given length.
///
/// # Parameters
///
/// - `dir`: the directory to write into.
/// - `name`: the file name.
/// - `seconds`: the length in seconds.
async fn fixture(dir: &Path, name: &str, seconds: f64) -> Option<PathBuf> {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(seconds)
        .file_name(name)
        .write_into(dir)
        .await
        .ok()
}

/// Waits until a predicate holds, up to a timeout.
///
/// # Parameters
///
/// - `timeout`: how long to wait.
/// - `predicate`: the condition to wait for.
async fn wait_for(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

#[tokio::test]
async fn pads_with_silence_then_plays_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path(), "song.flac", 2.0).await.unwrap();
    let runner = FfmpegRunner::new(None, 4);

    let request = PendingRequest::new(TranscodeFormat::Mp3, 320, Duration::from_secs(30));
    let mut pending = runner.pending_stream(&request).await.unwrap();

    // The client hears the padding immediately.
    let first = tokio::time::timeout(Duration::from_secs(1), pending.next())
        .await
        .expect("the first bytes arrive within a second")
        .expect("the stream has data")
        .unwrap();
    assert!(!first.is_empty());
    assert!(!pending.audio_delivered());

    // The acquisition finishes a moment later.
    tokio::time::sleep(Duration::from_millis(300)).await;
    pending.control().ready(&source).await.unwrap();

    let mut collected = first.to_vec();
    collected.extend(drain(pending).await.unwrap());
    let path = write_encoded(dir.path(), "pending.mp3", &collected).unwrap();

    let probed = probe(&path).await.unwrap();
    assert_eq!(probed.container, Container::Mpeg);
    assert_eq!(probed.codec, Codec::Mp3);

    // Two seconds of burst, a fraction of a second of paced silence, and the
    // two second song.
    let duration: f64 = ffprobe(&path, "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    assert!(
        (3.8..=5.6).contains(&duration),
        "the pending stream produced {duration}s"
    );
    assert_eq!(runner.in_flight(), 0);
}

#[tokio::test]
async fn encodes_every_pending_format() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path(), "song.flac", 1.0).await.unwrap();
    let runner = FfmpegRunner::new(None, 6);

    for format in TranscodeFormat::ALL {
        let request = PendingRequest::new(
            format,
            format.default_bitrate_kbps(),
            Duration::from_secs(30),
        );
        let mut pending = runner.pending_stream(&request).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), pending.next())
            .await
            .expect("the first bytes arrive within a second")
            .expect("the stream has data")
            .unwrap();
        assert!(!first.is_empty(), "{format} produced nothing");
        pending.control().ready(&source).await.unwrap();

        let mut collected = first.to_vec();
        collected.extend(drain(pending).await.unwrap());
        let path = write_encoded(
            dir.path(),
            &format!("pending.{}", format.suffix()),
            &collected,
        )
        .unwrap();

        let probed = probe(&path).await.unwrap();
        let (container, codec) = match format {
            TranscodeFormat::Mp3 => (Container::Mpeg, Codec::Mp3),
            TranscodeFormat::Opus => (Container::Ogg, Codec::Opus),
            TranscodeFormat::Aac => (Container::Aac, Codec::Aac),
        };
        assert_eq!(probed.container, container, "{format}");
        assert_eq!(probed.codec, codec, "{format}");

        // The padding is silence, so an ADTS container cannot be measured by
        // ffprobe (it divides the size by the bitrate of the silent frames).
        // The decoders count frames instead, which is exact for mp3 and AAC;
        // Opus comes from its Ogg header through ffprobe.
        let seconds = match format {
            TranscodeFormat::Opus => ffprobe(&path, "duration")
                .await
                .expect("ffprobe reports a duration")
                .parse::<f64>()
                .unwrap(),
            _ => {
                let report = hearthsound_media::verify_decodes(&path, &runner)
                    .await
                    .unwrap();
                report.duration_ms.expect("the decoder counted frames") as f64 / 1000.0
            }
        };
        assert!(
            (2.7..=4.0).contains(&seconds),
            "{format} produced {seconds}s"
        );
        assert_eq!(runner.in_flight(), 0, "{format}");
    }
}

#[tokio::test]
async fn ends_one_second_after_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FfmpegRunner::new(None, 4);
    let request = PendingRequest::new(TranscodeFormat::Mp3, 192, Duration::from_secs(30));
    let mut pending = runner.pending_stream(&request).await.unwrap();

    let first = pending.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    pending.control().fail().await.unwrap();

    let mut collected = first.to_vec();
    let started = tokio::time::Instant::now();
    collected.extend(drain(pending).await.unwrap());
    let elapsed = started.elapsed();

    let path = write_encoded(dir.path(), "failed.mp3", &collected).unwrap();
    let duration: f64 = ffprobe(&path, "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    // Two seconds of burst, a fifth of a second of silence, and the one second
    // of grace.
    assert!(
        (2.9..=4.0).contains(&duration),
        "a failed pending stream produced {duration}s"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the stream took {elapsed:?} to end"
    );
}

#[tokio::test]
async fn ends_when_the_wait_elapses_without_a_file() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FfmpegRunner::new(None, 4);
    let request = PendingRequest::new(TranscodeFormat::Mp3, 192, Duration::from_millis(400));
    let pending = runner.pending_stream(&request).await.unwrap();

    let started = tokio::time::Instant::now();
    let collected = drain(pending).await.unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "the wait elapsed in {elapsed:?}"
    );

    let path = write_encoded(dir.path(), "waited.mp3", &collected).unwrap();
    let duration: f64 = ffprobe(&path, "duration")
        .await
        .expect("ffprobe reports a duration")
        .parse()
        .unwrap();
    // Two seconds of burst plus the pacing up to the wait.
    assert!(
        (2.0..=3.0).contains(&duration),
        "the pending stream produced {duration}s"
    );
    assert_eq!(runner.in_flight(), 0);
}

#[tokio::test]
async fn kills_both_processes_when_the_stream_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path(), "long.flac", 600.0).await.unwrap();
    let runner = FfmpegRunner::new(None, 2);

    let request = PendingRequest::new(TranscodeFormat::Mp3, 320, Duration::from_secs(30));
    let mut pending = runner.pending_stream(&request).await.unwrap();
    assert_eq!(runner.in_flight(), 1);

    let first = pending.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    pending.control().ready(&source).await.unwrap();
    assert!(
        wait_for(Duration::from_secs(5), || runner.in_flight() == 2).await,
        "the decoder did not start"
    );

    drop(pending);
    assert!(
        wait_for(Duration::from_secs(10), || runner.in_flight() == 0).await,
        "the pending stream did not release its processes"
    );
}

#[tokio::test]
async fn a_stalled_client_blocks_the_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture(dir.path(), "long.flac", 600.0).await.unwrap();
    let runner = FfmpegRunner::new(None, 4);

    let request = PendingRequest::new(TranscodeFormat::Mp3, 320, Duration::from_secs(30));
    let pending = runner.pending_stream(&request).await.unwrap();
    pending.control().ready(&source).await.unwrap();

    // Read nothing. ffmpeg could encode ten minutes of audio in a couple of
    // seconds, so both processes still running after three seconds of silence
    // proves the client backpressure reached them: the encoder is blocked on a
    // full output pipe and the decoder on a full input pipe, and neither can
    // buffer the file.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            runner.in_flight(),
            2,
            "the pending stream ran ahead of a stalled client"
        );
    }
    assert!(
        pending.audio_delivered(),
        "the switch to the file never delivered audio"
    );

    drop(pending);
    assert!(
        wait_for(Duration::from_secs(10), || runner.in_flight() == 0).await,
        "the stalled stream did not release its processes"
    );
}

#[tokio::test]
async fn reports_a_command_that_arrives_after_the_end() {
    let dir = tempfile::tempdir().unwrap();
    let runner = FfmpegRunner::new(None, 4);
    let request = PendingRequest::new(TranscodeFormat::Mp3, 192, Duration::from_millis(200));
    let pending = runner.pending_stream(&request).await.unwrap();
    let (stream, control) = pending.into_parts();
    assert!(!control.audio_delivered());

    let collected = drain(stream).await.unwrap();
    assert!(!collected.is_empty());
    assert!(
        !control.audio_delivered(),
        "a stream that only played silence did not deliver file audio"
    );

    // The feeder ends with the stream, so a later command has nowhere to go.
    let mut ended = false;
    for _ in 0..100 {
        if let Err(error) = control.ready(dir.path().join("late.flac")).await {
            assert!(
                matches!(error, MediaError::PendingEnded { .. }),
                "{error:?}"
            );
            ended = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(ended, "the pending stream outlived its control handle");
    assert!(matches!(
        control.fail().await.unwrap_err(),
        MediaError::PendingEnded { .. }
    ));
}
