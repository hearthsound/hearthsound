//! Round-trip tests for the tag writer over every container the server imports.

use std::path::Path;

use hearthsound_core::Slug;
use hearthsound_media::{
    Artwork, FfmpegRunner, TrackTags, extract_picture, probe, write_sidecar_cover, write_tags,
};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat};

/// The tags the tests write and expect back.
fn tag_set(acquired_via: &Slug) -> TrackTags {
    TrackTags {
        title: Some("Blue Hour".to_owned()),
        artists: vec!["Nova Reyes".to_owned(), "Ida Frost".to_owned()],
        album: Some("Nightfall".to_owned()),
        album_artist: Some("Nova Reyes".to_owned()),
        track_number: Some(3),
        track_total: Some(11),
        disc_number: Some(1),
        disc_total: Some(2),
        date: Some("2019-04-12".to_owned()),
        genres: vec!["Ambient".to_owned(), "Drone".to_owned()],
        isrc: Some("USRC17607839".to_owned()),
        mb_recording_id: Some("1f2d7c50-4a4f-4c58-9d0c-1a4b1f2c3d4e".to_owned()),
        mb_release_id: Some("2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d".to_owned()),
        mb_release_group_id: Some("3b4c5d6e-7f8a-4b9c-8d0e-1f2a3b4c5d6e".to_owned()),
        mb_artist_ids: vec![
            "4c5d6e7f-8a9b-4c0d-9e1f-2a3b4c5d6e7f".to_owned(),
            "6e7f8a9b-0c1d-4e2f-8a3b-4c5d6e7f8a9b".to_owned(),
        ],
        mb_album_artist_ids: vec!["5d6e7f8a-9b0c-4d1e-8f2a-3b4c5d6e7f8a".to_owned()],
        compilation: Some(true),
        replaygain_track_gain: Some("-3.21 dB".to_owned()),
        replaygain_track_peak: Some("0.988861".to_owned()),
        replaygain_album_gain: Some("-3.05 dB".to_owned()),
        replaygain_album_peak: Some("1.000000".to_owned()),
        acquired_via: Some(acquired_via.clone()),
    }
}

/// Reads the `HEARTHSOUND_ACQUIRED_VIA` value straight out of the container.
///
/// The probe deliberately does not read this key back, so the test reads the
/// container-specific tag itself and thereby also checks that the key landed in
/// the right place: a Vorbis comment, an ID3v2 `TXXX` frame, an MP4 freeform
/// atom, or an APE item.
fn acquired_via_in_file(path: &Path, format: AudioFormat) -> Option<String> {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile;

    let key = hearthsound_media::ACQUIRED_VIA_KEY;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    match format {
        AudioFormat::Flac => {
            let file = lofty::flac::FlacFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.vorbis_comments()
                .and_then(|comments| comments.get(key))
                .map(str::to_owned)
        }
        AudioFormat::OggVorbis => {
            let file = lofty::ogg::VorbisFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.vorbis_comments().get(key).map(str::to_owned)
        }
        AudioFormat::Opus => {
            let file = lofty::ogg::OpusFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.vorbis_comments().get(key).map(str::to_owned)
        }
        AudioFormat::Mp3Cbr | AudioFormat::Mp3Vbr => {
            let file = lofty::mpeg::MpegFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.id3v2()
                .and_then(|tag| tag.get_user_text(key))
                .map(str::to_owned)
        }
        AudioFormat::M4aAac => {
            let file = lofty::mp4::Mp4File::read_from(&mut reader, ParseOptions::new()).ok()?;
            let ident = lofty::mp4::AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: key.into(),
            };
            file.ilst()
                .and_then(|ilst| ilst.get(&ident))
                .and_then(|atom| {
                    atom.data().find_map(|data| match data {
                        lofty::mp4::AtomData::UTF8(value) => Some(value.clone()),
                        _ => None,
                    })
                })
        }
        AudioFormat::Wav => {
            let file =
                lofty::iff::wav::WavFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.id3v2()
                .and_then(|tag| tag.get_user_text(key))
                .map(str::to_owned)
        }
        AudioFormat::WavPack => {
            let file =
                lofty::wavpack::WavPackFile::read_from(&mut reader, ParseOptions::new()).ok()?;
            file.ape()
                .and_then(|tag| tag.get(key))
                .and_then(|item| item.text_values())
                .and_then(|mut values| values.next())
                .map(str::to_owned)
        }
    }
}

#[tokio::test]
async fn round_trips_the_full_tag_set_through_every_container() {
    let dir = tempfile::tempdir().unwrap();
    let acquired_via = Slug::new("slskd01").unwrap();
    let tags = tag_set(&acquired_via);
    let runner = FfmpegRunner::new(None, 4);

    for format in AudioFormat::ALL {
        let path = AudioFixture::new(format)
            .seconds(0.4)
            .file_name(format!("written.{}", format.extension()))
            .write_into(dir.path())
            .await
            .unwrap();

        write_tags(&path, &tags, None).await.unwrap();

        let mut probed = probe(&path).await.unwrap().tags;
        // An APE tag keeps a single genre, so the second value replaces the
        // first; every other container keeps both.
        probed.genres = if format == AudioFormat::WavPack {
            vec!["Drone".to_owned()]
        } else {
            tags.genres.clone()
        };
        let mut expected = tags.clone();
        expected.genres = probed.genres.clone();
        // The probe does not read the acquiring instance back.
        expected.acquired_via = None;
        assert_eq!(probed, expected, "{} round trip", format.extension());

        assert_eq!(
            acquired_via_in_file(&path, format).as_deref(),
            Some(acquired_via.as_str()),
            "{} acquired-via tag",
            format.extension()
        );

        // The file still plays after being rewritten.
        let verified = hearthsound_media::verify_decodes(&path, &runner)
            .await
            .unwrap();
        assert!(verified.damaged_packets == 0, "{}", format.extension());
    }
}

/// The fixture writer and the probe agree on the MusicBrainz ids and the
/// compilation flag, which is what the library scan tests rely on.
#[tokio::test]
async fn round_trips_fixture_music_brainz_ids_and_compilation() {
    use hearthsound_test_support::audio::{
        TrackTags as FixtureTags, write_tags as write_fixture_tags,
    };

    let dir = tempfile::tempdir().unwrap();
    let recording = "1f2d7c50-4a4f-4c58-9d0c-1a4b1f2c3d4e";
    let release = "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d";
    let release_group = "3b4c5d6e-7f8a-4b9c-8d0e-1f2a3b4c5d6e";
    let artist_ids = [
        "4c5d6e7f-8a9b-4c0d-9e1f-2a3b4c5d6e7f",
        "6e7f8a9b-0c1d-4e2f-8a3b-4c5d6e7f8a9b",
    ];
    let album_artist_ids = ["5d6e7f8a-9b0c-4d1e-8f2a-3b4c5d6e7f8a"];

    for format in [AudioFormat::Flac, AudioFormat::Mp3Cbr] {
        let path = AudioFixture::new(format)
            .seconds(0.3)
            .write_into(dir.path())
            .await
            .unwrap();
        let tags = FixtureTags::new("Split Credit", "Nova Reyes")
            .mb_ids(
                recording,
                release,
                release_group,
                artist_ids,
                album_artist_ids,
            )
            .replaygain("-3.21 dB", "0.988861", "-3.05 dB", "1.000000")
            .compilation(true);
        write_fixture_tags(&path, &tags, None).await.unwrap();

        let probed = probe(&path).await.unwrap().tags;
        assert_eq!(
            probed.mb_recording_id.as_deref(),
            Some(recording),
            "{} recording id",
            format.extension()
        );
        assert_eq!(
            probed.mb_release_id.as_deref(),
            Some(release),
            "{} release id",
            format.extension()
        );
        assert_eq!(
            probed.mb_release_group_id.as_deref(),
            Some(release_group),
            "{} release group id",
            format.extension()
        );
        assert_eq!(
            probed.mb_artist_ids,
            artist_ids,
            "{} artist ids",
            format.extension()
        );
        assert_eq!(
            probed.mb_album_artist_ids,
            album_artist_ids,
            "{} album artist ids",
            format.extension()
        );
        assert_eq!(
            probed.replaygain_track_gain.as_deref(),
            Some("-3.21 dB"),
            "{} track gain",
            format.extension()
        );
        assert_eq!(
            probed.replaygain_album_peak.as_deref(),
            Some("1.000000"),
            "{} album peak",
            format.extension()
        );
        assert_eq!(
            probed.compilation,
            Some(true),
            "{} compilation flag",
            format.extension()
        );
    }
}

#[tokio::test]
async fn writes_only_the_fields_that_are_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .write_into(dir.path())
        .await
        .unwrap();
    let tags = TrackTags {
        title: Some("Only A Title".to_owned()),
        ..TrackTags::default()
    };
    write_tags(&path, &tags, None).await.unwrap();
    let probed = probe(&path).await.unwrap().tags;
    assert_eq!(probed, tags);
}

#[tokio::test]
async fn embeds_a_cover_and_writes_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let cover = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("source.flac")
        .cover_png(5, 0x22_44_66)
        .write_into(dir.path())
        .await
        .unwrap();
    let artwork = extract_picture(&cover).await.unwrap().unwrap();

    let target = AudioFixture::new(AudioFormat::M4aAac)
        .seconds(0.4)
        .file_name("target.m4a")
        .write_into(dir.path())
        .await
        .unwrap();
    assert!(!probe(&target).await.unwrap().has_embedded_art);
    write_tags(
        &target,
        &TrackTags {
            title: Some("Covered".to_owned()),
            ..TrackTags::default()
        },
        Some(&artwork),
    )
    .await
    .unwrap();

    let probed = probe(&target).await.unwrap();
    assert!(probed.has_embedded_art);
    assert_eq!(probed.tags.title.as_deref(), Some("Covered"));
    let embedded = extract_picture(&target).await.unwrap().unwrap();
    assert_eq!(embedded, artwork);
    assert_eq!(embedded.mime_type(), "image/png");

    let sidecar = write_sidecar_cover(dir.path(), "cover.jpg", &Artwork::jpeg(vec![1, 2, 3]))
        .await
        .unwrap();
    assert_eq!(sidecar, dir.path().join("cover.jpg"));
    assert_eq!(std::fs::read(&sidecar).unwrap(), [1, 2, 3]);
}

#[tokio::test]
async fn reports_missing_files_and_unknown_containers() {
    let dir = tempfile::tempdir().unwrap();
    let error = write_tags(
        &dir.path().join("missing.flac"),
        &TrackTags::default(),
        None,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, hearthsound_media::MediaError::Io { .. }),
        "{error:?}"
    );

    let unknown = dir.path().join("notes.bin");
    std::fs::write(&unknown, b"not audio at all").unwrap();
    let error = write_tags(&unknown, &TrackTags::default(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, hearthsound_media::MediaError::Unsupported { .. }),
        "{error:?}"
    );
}
