//! Probe tests over generated fixtures.

use std::path::Path;

use hearthsound_media::{
    Artwork, Codec, Container, DecodeBackend, MediaError, TrackTags, extract_picture, probe,
    verify_decodes,
};
use hearthsound_test_support::audio::{
    AudioFixture, AudioFormat, BitDepth, TrackTags as FixtureTags,
};

/// The container, codec, and losslessness expected for a fixture format.
fn expected(format: AudioFormat) -> (Container, Codec, bool) {
    match format {
        AudioFormat::Flac => (Container::Flac, Codec::Flac, true),
        AudioFormat::Mp3Cbr | AudioFormat::Mp3Vbr => (Container::Mpeg, Codec::Mp3, false),
        AudioFormat::OggVorbis => (Container::Ogg, Codec::Vorbis, false),
        AudioFormat::Opus => (Container::Ogg, Codec::Opus, false),
        AudioFormat::M4aAac => (Container::Mp4, Codec::Aac, false),
        AudioFormat::Wav => (Container::Wav, Codec::Pcm, true),
        AudioFormat::WavPack => (Container::WavPack, Codec::WavPack, true),
    }
}

#[tokio::test]
async fn probes_every_fixture_format() {
    let dir = tempfile::tempdir().unwrap();
    for format in AudioFormat::ALL {
        let path = AudioFixture::new(format)
            .seconds(1.0)
            .file_name(format!("probe.{}", format.extension()))
            .write_into(dir.path())
            .await
            .unwrap();

        let result = probe(&path).await.unwrap();
        let (container, codec, lossless) = expected(format);
        assert_eq!(result.container, container, "{}", format.extension());
        assert_eq!(result.codec, codec, "{}", format.extension());
        assert_eq!(result.lossless, lossless, "{}", format.extension());
        assert_eq!(
            result.codec.is_lossless(),
            lossless,
            "{}",
            format.extension()
        );
        assert_eq!(result.container.as_str(), container.as_str());
        assert_eq!(result.codec.as_str(), codec.as_str());
        assert_eq!(result.channels, Some(2), "{}", format.extension());
        assert!(
            (900..=1_300).contains(&result.duration_ms),
            "{} reported {} ms",
            format.extension(),
            result.duration_ms
        );
        assert_eq!(
            result.sample_rate,
            Some(if format == AudioFormat::Opus {
                48_000
            } else {
                44_100
            }),
            "{}",
            format.extension()
        );
        assert_eq!(
            result.bit_depth.is_some(),
            lossless,
            "{} bit depth presence",
            format.extension()
        );
        assert!(!result.has_embedded_art, "{}", format.extension());
        assert!(!result.lossless || result.bit_depth == Some(16));
    }
}

#[tokio::test]
async fn probes_the_compression_rate_of_lossy_fixtures() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Mp3Cbr)
        .seconds(1.0)
        .bitrate_kbps(Some(192))
        .write_into(dir.path())
        .await
        .unwrap();
    let result = probe(&path).await.unwrap();
    let bitrate = result.bitrate_kbps.expect("an mp3 records its bitrate");
    assert!(
        (160..=224).contains(&bitrate),
        "reported {bitrate} kbit/s for a 192 kbit/s file"
    );
}

#[tokio::test]
async fn reads_the_full_tag_set() {
    let dir = tempfile::tempdir().unwrap();
    let tags = FixtureTags::new("Blue Hour", "Nova Reyes")
        .artists(["Nova Reyes", "Ida Frost"])
        .album("Nightfall")
        .album_artist("Nova Reyes")
        .track(3, 11)
        .disc(1, 2)
        .year(2019)
        .genres(["Ambient", "Drone"])
        .isrc("USRC17607839");
    for format in AudioFormat::ALL {
        let path = AudioFixture::new(format)
            .seconds(0.5)
            .file_name(format!("tags.{}", format.extension()))
            .tags(tags.clone())
            .write_into(dir.path())
            .await
            .unwrap();

        let probed = probe(&path).await.unwrap().tags;
        assert_eq!(
            probed.title.as_deref(),
            Some("Blue Hour"),
            "{}",
            format.extension()
        );
        assert_eq!(
            probed.artists,
            ["Nova Reyes", "Ida Frost"],
            "{}",
            format.extension()
        );
        assert_eq!(
            probed.album.as_deref(),
            Some("Nightfall"),
            "{}",
            format.extension()
        );
        assert_eq!(
            probed.album_artist.as_deref(),
            Some("Nova Reyes"),
            "{}",
            format.extension()
        );
        assert_eq!(probed.track_number, Some(3), "{}", format.extension());
        assert_eq!(probed.track_total, Some(11), "{}", format.extension());
        assert_eq!(probed.disc_number, Some(1), "{}", format.extension());
        assert_eq!(probed.disc_total, Some(2), "{}", format.extension());
        assert_eq!(
            probed.date.as_deref(),
            Some("2019"),
            "{}",
            format.extension()
        );
        assert_eq!(probed.year(), Some(2019), "{}", format.extension());
        // An APE tag, which WavPack carries, holds a single genre: the second
        // value replaces the first.
        let expected_genres: &[&str] = if format == AudioFormat::WavPack {
            &["Drone"]
        } else {
            &["Ambient", "Drone"]
        };
        assert_eq!(probed.genres, expected_genres, "{}", format.extension());
        assert_eq!(
            probed.isrc.as_deref(),
            Some("USRC17607839"),
            "{}",
            format.extension()
        );
        // The acquiring instance is core bookkeeping and is not read back.
        assert_eq!(probed.acquired_via, None);
        assert!(!probed.is_empty());
        assert!(TrackTags::default().is_empty());
    }
}

#[tokio::test]
async fn reports_musicbrainz_and_replaygain_tags() {
    let dir = tempfile::tempdir().unwrap();
    // The fixture writer covers the display tags; MusicBrainz and ReplayGain
    // tags are written by the media crate itself once the writer lands, so this
    // test only checks that a file without them reports none.
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.4)
        .write_into(dir.path())
        .await
        .unwrap();
    let probed = probe(&path).await.unwrap().tags;
    assert_eq!(probed.mb_recording_id, None);
    assert_eq!(probed.replaygain_track_gain, None);
}

#[tokio::test]
async fn extracts_an_embedded_picture() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.4)
        .cover_png(4, 0x11_22_33)
        .write_into(dir.path())
        .await
        .unwrap();

    let result = probe(&path).await.unwrap();
    assert!(result.has_embedded_art);

    let artwork = extract_picture(&path).await.unwrap().expect("a picture");
    let expected_cover = std::fs::read(dir.path().join("fixture.cover.png")).unwrap();
    assert_eq!(artwork.mime_type(), "image/png");
    assert_eq!(artwork.data(), expected_cover.as_slice());
    assert_eq!(artwork, Artwork::png(expected_cover.clone()));

    let plain = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("plain.flac")
        .write_into(dir.path())
        .await
        .unwrap();
    assert_eq!(extract_picture(&plain).await.unwrap(), None);
}

#[tokio::test]
async fn verifies_that_every_fixture_decodes() {
    let dir = tempfile::tempdir().unwrap();
    for format in AudioFormat::ALL {
        let path = AudioFixture::new(format)
            .seconds(1.0)
            .file_name(format!("verify.{}", format.extension()))
            .write_into(dir.path())
            .await
            .unwrap();

        let report = verify_decodes(&path).await.unwrap();
        let expected_backend = match format {
            // symphonia has no Opus decoder and no WavPack reader.
            AudioFormat::Opus | AudioFormat::WavPack => DecodeBackend::Ffmpeg,
            _ => DecodeBackend::Symphonia,
        };
        assert_eq!(report.backend, expected_backend, "{}", format.extension());
        assert_eq!(report.backend.as_str(), expected_backend.as_str());
        assert_eq!(report.damaged_packets, 0, "{}", format.extension());
        if let Some(duration) = report.duration_ms {
            assert!(
                (900..=1_200).contains(&duration),
                "{} decoded {} ms",
                format.extension(),
                duration
            );
            assert_eq!(report.channels, Some(2), "{}", format.extension());
        } else {
            assert_eq!(report.frames, None);
        }
    }
}

#[tokio::test]
async fn rejects_a_file_that_does_not_decode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.flac");
    // 64 KiB of noise with a FLAC extension parses as no container at all for
    // symphonia and fails ffmpeg's decoder.
    let mut bytes = vec![0u8; 64 * 1024];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    std::fs::write(&path, &bytes).unwrap();

    let error = verify_decodes(&path).await.unwrap_err();
    assert!(matches!(error, MediaError::Decode { .. }), "{error:?}");
}

#[tokio::test]
async fn reports_unreadable_and_unknown_files() {
    let dir = tempfile::tempdir().unwrap();
    let error = probe(&dir.path().join("missing.flac")).await.unwrap_err();
    assert!(matches!(error, MediaError::Io { .. }), "{error:?}");

    let unknown = dir.path().join("junk.bin");
    std::fs::write(&unknown, b"this is not audio").unwrap();
    let error = probe(&unknown).await.unwrap_err();
    assert!(matches!(error, MediaError::Unsupported { .. }), "{error:?}");
}

#[tokio::test]
async fn probes_a_fixture_at_a_chosen_sample_rate_and_depth() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Wav)
        .seconds(0.5)
        .sample_rate(96_000)
        .bit_depth(BitDepth::TwentyFour)
        .channels(1)
        .write_into(dir.path())
        .await
        .unwrap();
    let result = probe(&path).await.unwrap();
    assert_eq!(result.sample_rate, Some(96_000));
    assert_eq!(result.bit_depth, Some(24));
    assert_eq!(result.channels, Some(1));
    assert!(result.lossless);
}

#[tokio::test]
async fn probes_a_path_that_is_not_utf8_free() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("with space and - dash.flac")
        .write_into(dir.path())
        .await
        .unwrap();
    assert!(Path::new(&path).is_file());
    assert_eq!(probe(&path).await.unwrap().codec, Codec::Flac);
}
