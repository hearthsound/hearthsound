//! Tests for the folder scanner: counting, incremental rescans, pruning,
//! folder-owned pins, and cover art lookup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_core::{Availability, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::artwork::{CoverArt, cover_for_file, track_cover};
use hearthsound_library::scan::{LibraryScanner, ScanJobHandler, ScanRequest};
use hearthsound_library::{LibraryRepository, Origin, ReplayGain, StorageRoots};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds a scanner over a database, with the built-in settings.
async fn scanner_for(db: &Database, events: Arc<EventBus>) -> LibraryScanner {
    let settings = Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();
    LibraryScanner::new(db, events, settings)
}

/// Writes a short FLAC named after `title` into `dir`.
async fn write_song(dir: &Path, title: &str, artist: &str, album: &str) -> std::path::PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name(format!("{title}.flac"))
        .tags(
            TrackTags::new(title, artist)
                .album(album)
                .album_artist(artist)
                .track(1, 2)
                .year(1993)
                .genres(["Trance"]),
        )
        .write_into(dir)
        .await
        .unwrap()
}

/// Inserts a bare user row and returns its id.
async fn insert_user(db: &Database) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, 'User', 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Returns the single track of the only album by `artist`.
async fn only_track(library: &LibraryRepository, artist: &str) -> hearthsound_core::EntityId {
    let artists = library.list_artists().await.unwrap();
    let artist = artists
        .iter()
        .find(|candidate| candidate.name == artist)
        .unwrap();
    let albums = library.list_albums_for_artist(artist.id).await.unwrap();
    library.list_tracks_for_album(albums[0].id).await.unwrap()[0].id
}

#[tokio::test]
async fn scan_indexes_every_audio_file_and_skips_unchanged_ones() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("nested")).unwrap();

    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&music.join("nested"), "Sunset", "Someone Else", "Evening").await;
    std::fs::write(music.join("notes.txt"), b"not audio").unwrap();

    let events = Arc::new(EventBus::new());
    let mut subscriber = events.subscribe();
    let scanner = scanner_for(&db, events.clone()).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_seen, 2, "the text file is not audio");
    assert_eq!(report.files_indexed, 2);
    assert_eq!(report.files_skipped, 0);
    assert_eq!(report.files_removed, 0);
    assert_eq!(library.list_artists().await.unwrap().len(), 2);

    let track = only_track(&library, "Energy 52").await;
    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(stored.origin, Origin::Local);
    assert_eq!(stored.availability, Availability::Local);
    assert!(stored.duration_ms.unwrap() > 0);
    assert_eq!(stored.track_number, Some(1));

    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].storage, StorageClass::Folder);
    assert_eq!(files[0].folder_id, Some(folder.id));
    assert!(files[0].is_preferred);
    assert_eq!(files[0].format.as_deref(), Some("flac"));
    assert_eq!(files[0].codec.as_deref(), Some("flac"));
    assert_eq!(files[0].lossless, Some(true));
    assert_eq!(files[0].sample_rate, Some(44_100));
    assert_eq!(files[0].bit_depth, Some(16));
    assert_eq!(files[0].channels, Some(2));
    assert_eq!(
        library.list_track_genres(track).await.unwrap()[0].name,
        "trance"
    );
    assert!(
        library
            .get_library_folder(folder.id)
            .await
            .unwrap()
            .unwrap()
            .last_scan_at
            .is_some()
    );

    // The bus saw progress and the library changes.
    let mut kinds = Vec::new();
    while let Ok(envelope) = subscriber.try_recv() {
        kinds.push(envelope.event.type_str());
    }
    assert!(kinds.contains(&"scan.progress"));
    assert!(kinds.contains(&"library.changed"));

    // A rescan of an unchanged folder probes nothing.
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_seen, 2);
    assert_eq!(report.files_indexed, 0);
    assert_eq!(report.files_skipped, 2);
    assert_eq!(library.list_artists().await.unwrap().len(), 2);
}

#[tokio::test]
async fn hidden_metadata_files_are_ignored_and_unreadable_files_do_not_stop_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let album = dir.path().join("music").join("Album");
    std::fs::create_dir_all(album.join(".AppleDouble")).unwrap();

    write_song(&album, "02 - Your Imagination", "Band", "Album").await;
    // macOS AppleDouble companions carry the audio extension but hold metadata.
    let apple_double = b"\x00\x05\x16\x07\x00\x02\x00\x00Mac OS X        ";
    std::fs::write(album.join("._02 - Your Imagination.flac"), apple_double).unwrap();
    std::fs::write(album.join(".AppleDouble").join("song.flac"), apple_double).unwrap();
    // A visible file whose content is not audio is reported, not fatal.
    std::fs::write(album.join("03 - Broken.flac"), b"this is not a flac file").unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library
        .add_library_folder(&dir.path().join("music"), None)
        .await
        .unwrap();

    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_seen, 2, "hidden entries are not walked");
    assert_eq!(report.files_indexed, 1);
    assert_eq!(report.files_failed, 1);
    assert_eq!(library.list_artists().await.unwrap().len(), 1);
    assert!(
        library
            .get_library_folder(folder.id)
            .await
            .unwrap()
            .unwrap()
            .last_scan_at
            .is_some(),
        "the scan ran to completion"
    );
}

#[tokio::test]
async fn a_changed_file_is_reindexed_without_duplicating_the_track() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let path = write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;

    // Rewrite the same path with a new title and a different length.
    tokio::fs::remove_file(&path).await.unwrap();
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.6)
        .file_name("Cafe del Mar.flac")
        .tags(
            TrackTags::new("Sunset", "Energy 52")
                .album("Cafe del Mar")
                .album_artist("Energy 52"),
        )
        .write_into(&music)
        .await
        .unwrap();

    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_indexed, 1);
    assert_eq!(report.files_skipped, 0);
    assert_eq!(report.files_removed, 0);

    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(
        stored.title, "Sunset",
        "the track row is refreshed in place"
    );
    assert_eq!(
        library
            .list_media_files_for_track(track)
            .await
            .unwrap()
            .len(),
        1,
        "the media file is refreshed, not duplicated"
    );
}

#[tokio::test]
async fn vanished_files_drop_their_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    let doomed = write_song(&music, "Sunset", "Someone Else", "Evening").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(library.list_artists().await.unwrap().len(), 2);
    let doomed_track = only_track(&library, "Someone Else").await;

    tokio::fs::remove_file(&doomed).await.unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_removed, 1);
    assert_eq!(
        report.tracks_removed, 1,
        "a local track with no file is deleted"
    );
    assert_eq!(report.files_seen, 1);
    assert!(library.get_track(doomed_track).await.unwrap().is_none());
    assert!(
        library
            .list_media_files_for_track(doomed_track)
            .await
            .unwrap()
            .is_empty()
    );
    // Artists and albums are shared rows, so they are kept even when the last
    // track that referenced them is gone; the browse queries hide the ones that
    // have nothing in scope.
    assert_eq!(library.list_artists().await.unwrap().len(), 2);
}

#[tokio::test]
async fn an_owned_folder_pins_its_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let owner = insert_user(&db).await;
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library
        .add_library_folder(&music, Some(owner))
        .await
        .unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;

    let pinned: i64 = sqlx::query_scalar("SELECT pinned FROM track_retention WHERE track_id = ?")
        .bind(track.uuid().as_bytes().as_slice())
        .fetch_one(db.writer())
        .await
        .unwrap();
    assert_eq!(pinned, 1);
    let reasons: String =
        sqlx::query_scalar("SELECT pin_reasons FROM track_retention WHERE track_id = ?")
            .bind(track.uuid().as_bytes().as_slice())
            .fetch_one(db.writer())
            .await
            .unwrap();
    assert_eq!(reasons, "[\"folder\"]");

    // A generic folder does not pin.
    let other = dir.path().join("other");
    std::fs::create_dir(&other).unwrap();
    write_song(&other, "Sunset", "Someone Else", "Evening").await;
    let generic = library.add_library_folder(&other, None).await.unwrap();
    scanner.scan_folder(generic.id).await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM track_retention")
        .fetch_one(db.writer())
        .await
        .unwrap();
    assert_eq!(count, 1, "only the owned folder's track is pinned");
}

#[tokio::test]
async fn the_walk_stays_inside_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&outside).unwrap();
    write_song(&outside, "Hidden", "Nobody", "Nowhere").await;
    std::os::unix::fs::symlink(&outside, music.join("link")).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_seen, 1, "the symlink is not followed");
    assert_eq!(library.list_artists().await.unwrap()[0].name, "Energy 52");
}

#[tokio::test]
async fn cover_art_comes_from_the_embedded_picture_or_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    let embedded = AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("Cafe del Mar.flac")
        .tags(
            TrackTags::new("Cafe del Mar", "Energy 52")
                .album("Cafe del Mar")
                .album_artist("Energy 52"),
        )
        .cover_png(8, 0x336699)
        .write_into(&music)
        .await
        .unwrap();
    let bare = write_song(&music, "Sunset", "Someone Else", "Evening").await;
    std::fs::write(music.join("cover.jpg"), b"jpeg-bytes").unwrap();

    match cover_for_file(&embedded, Some(true), "cover.jpg")
        .await
        .unwrap()
    {
        Some(CoverArt::Embedded { mime_type, data }) => {
            assert_eq!(mime_type, "image/png");
            assert!(!data.is_empty());
        }
        other => panic!("expected an embedded picture, got {other:?}"),
    }
    match cover_for_file(&bare, Some(false), "cover.jpg")
        .await
        .unwrap()
    {
        Some(CoverArt::Sidecar { path, mime_type }) => {
            assert_eq!(path.file_name().unwrap(), "cover.jpg");
            assert_eq!(mime_type, "image/jpeg");
        }
        other => panic!("expected a sidecar image, got {other:?}"),
    }

    // Nothing to find in an empty directory.
    let empty = music.join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(
        cover_for_file(&empty.join("track.flac"), Some(false), "cover.jpg")
            .await
            .unwrap()
            .is_none()
    );

    // An unknown `has_embedded_art` still falls back to the sidecar.
    assert!(matches!(
        cover_for_file(&bare, None, "cover.jpg").await.unwrap(),
        Some(CoverArt::Sidecar { .. })
    ));

    // The configured cover filename is honoured when it is not a standard name.
    write_song(&empty, "Third", "Third Artist", "Third Album").await;
    std::fs::write(empty.join("art.png"), b"png-bytes").unwrap();
    assert!(matches!(
        cover_for_file(&empty.join("Third.flac"), Some(false), "art.png")
            .await
            .unwrap(),
        Some(CoverArt::Sidecar { .. })
    ));

    // Through the repository: the preferred file of a scanned track.
    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;
    let settings = SettingsStore::defaults(&SettingsRegistry::builtin());
    let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));
    match track_cover(&library, &settings, &roots, track)
        .await
        .unwrap()
    {
        Some(CoverArt::Embedded { mime_type, .. }) => assert_eq!(mime_type, "image/png"),
        other => panic!("expected the embedded picture, got {other:?}"),
    }
}

#[tokio::test]
async fn a_scan_stores_replay_gain_and_artist_music_brainz_ids() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("split.flac")
        .tags(
            TrackTags::new("Split Credit", "Nova Reyes")
                .artists(["Ida Frost"])
                .album("Nightfall")
                .album_artist("Nova Reyes")
                .track(1, 2)
                .mb_ids(
                    "1f2d7c50-4a4f-4c58-9d0c-1a4b1f2c3d4e",
                    "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d",
                    "3b4c5d6e-7f8a-4b9c-8d0e-1f2a3b4c5d6e",
                    ["mb-nova", "mb-ida"],
                    ["mb-nova"],
                )
                .replaygain("-3.21 dB", "0.988861", "-3.05 dB", "1.000000"),
        )
        .write_into(&music)
        .await
        .unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();

    let track = only_track(&library, "Nova Reyes").await;
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(
        files[0].replay_gain(),
        Some(ReplayGain {
            track_gain: Some(-3.21),
            track_peak: Some(0.988861),
            album_gain: Some(-3.05),
            album_peak: Some(1.0),
        })
    );

    let artists = library.list_artists().await.unwrap();
    let mbid = |name: &str| {
        artists
            .iter()
            .find(|artist| artist.name == name)
            .and_then(|artist| artist.mb_artist_id.clone())
    };
    assert_eq!(mbid("Nova Reyes").as_deref(), Some("mb-nova"));
    assert_eq!(mbid("Ida Frost").as_deref(), Some("mb-ida"));
}

#[tokio::test]
async fn a_single_valued_artist_tag_splits_on_the_shipped_separators() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("credit.flac")
        .tags(
            TrackTags::new("Split Credit", "Nova Reyes feat. Ida Frost")
                .album("Nightfall")
                .album_artist("Nova Reyes")
                .track(1, 1),
        )
        .write_into(&music)
        .await
        .unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();

    let names = |artists: Vec<hearthsound_library::Artist>| {
        let mut names: Vec<String> = artists.into_iter().map(|artist| artist.name).collect();
        names.sort();
        names
    };
    assert_eq!(
        names(library.list_artists().await.unwrap()),
        ["Ida Frost", "Nova Reyes"]
    );

    let artist = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .find(|artist| artist.name == "Nova Reyes")
        .unwrap();
    let album = library.list_albums_for_artist(artist.id).await.unwrap()[0].clone();
    assert_eq!(
        names(library.list_album_artists(album.id).await.unwrap()),
        ["Nova Reyes"],
        "the album is credited to its album artist alone"
    );

    let track = library.list_tracks_for_album(album.id).await.unwrap()[0].id;
    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(
        stored.display_artist.as_deref(),
        Some("Nova Reyes, Ida Frost")
    );
}

#[tokio::test]
async fn a_single_valued_artist_tag_respects_a_changed_separator_list() {
    use hearthsound_config::keys;

    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("ampersand.flac")
        .tags(
            TrackTags::new("Ampersand", "A & B")
                .album("Together")
                .album_artist("A & B")
                .track(1, 1),
        )
        .write_into(&music)
        .await
        .unwrap();

    let settings = Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();
    // The ampersand is not a shipped separator, so only this list splits on it.
    settings
        .set(
            keys::LIBRARY_SCAN_ARTIST_SEPARATORS.name(),
            hearthsound_config::SettingValue::StringList(vec!["&".to_owned()]),
            None,
        )
        .await
        .unwrap();

    let scanner = LibraryScanner::new(&db, Arc::new(EventBus::new()), settings);
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();

    let mut names: Vec<String> = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .map(|artist| artist.name)
        .collect();
    names.sort();
    assert_eq!(names, ["A", "B"]);
}

#[tokio::test]
async fn the_scan_job_handler_runs_a_scan() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let scanner = Arc::new(scanner_for(&db, Arc::new(EventBus::new())).await);
    let handler = ScanJobHandler::new(scanner);
    let job = |payload: Option<String>| hearthsound_jobs::Job {
        id: uuid::Uuid::now_v7(),
        kind: "library.scan".into(),
        payload,
        state: hearthsound_jobs::JobState::Running,
        priority: hearthsound_core::Priority::Background,
        attempts: 1,
        run_at: 0,
        locked_until: None,
        last_error: None,
        created_at: 0,
        updated_at: 0,
    };

    let payload = serde_json::to_string(&ScanRequest::folder(folder.id)).unwrap();
    hearthsound_jobs::JobHandler::handle(&handler, job(Some(payload)))
        .await
        .unwrap();
    assert_eq!(library.list_artists().await.unwrap().len(), 1);

    // A payload-less job scans every folder.
    hearthsound_jobs::JobHandler::handle(&handler, job(None))
        .await
        .unwrap();
    assert_eq!(library.list_artists().await.unwrap().len(), 1);
}
