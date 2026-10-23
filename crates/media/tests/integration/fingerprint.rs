//! Fingerprint tests over generated fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use data_encoding::BASE64URL_NOPAD;
use hearthsound_media::{FfmpegRunner, fingerprint};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, Signal};

/// Generates a tone of the given frequency and returns its path.
///
/// # Parameters
///
/// - `directory`: the directory the file is written into.
/// - `hz`: the tone's frequency.
async fn tone(directory: &Path, hz: f32) -> std::path::PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(3.0)
        .signal(Signal::Sine { hz })
        .file_name(format!("tone-{hz}.flac"))
        .write_into(directory)
        .await
        .unwrap()
}

#[tokio::test]
async fn fingerprints_a_generated_tone() {
    let dir = tempfile::tempdir().unwrap();
    let path = tone(dir.path(), 440.0).await;
    let runner = FfmpegRunner::new(None, 2);

    let print = fingerprint(&path, &runner).await.unwrap();

    assert_eq!(print.duration_secs, 3);
    assert!(!print.value.is_empty());
    let decoded = BASE64URL_NOPAD
        .decode(print.value.as_bytes())
        .expect("the value must be URL-safe base64 without padding");
    assert!(
        decoded.len() > 4,
        "a three second tone carries fingerprint bits"
    );
}

#[tokio::test]
async fn different_tones_have_different_fingerprints() {
    let dir = tempfile::tempdir().unwrap();
    let first = tone(dir.path(), 440.0).await;
    let second = tone(dir.path(), 620.0).await;
    let runner = FfmpegRunner::new(None, 2);

    let first_print = fingerprint(&first, &runner).await.unwrap();
    let second_print = fingerprint(&second, &runner).await.unwrap();

    assert_ne!(first_print.value, second_print.value);
}

#[tokio::test]
async fn a_file_shorter_than_the_excerpt_is_fingerprinted_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Wav)
        .seconds(0.2)
        .signal(Signal::Sine { hz: 440.0 })
        .file_name("short.wav")
        .write_into(dir.path())
        .await
        .unwrap();
    let runner = FfmpegRunner::new(None, 2);

    let print = fingerprint(&path, &runner).await.unwrap();

    assert_eq!(print.duration_secs, 1);
}
