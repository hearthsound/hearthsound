//! Tests for the folder scanner: counting, incremental rescans, pruning,
//! folder-owned pins, and cover art lookup.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_core::{Availability, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{JobQueue, JobRunner, JobRunnerOptions, JobState, QueueOptions, Shutdown};
use hearthsound_library::artwork::{CoverArt, cover_for_file, track_cover};
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::{
    LibraryScanner, SCAN_JOB_KIND, ScanJobHandler, ScanRequest, ScanStatus,
};
use hearthsound_library::{
    LibraryRepository, MediaFileInput, Origin, ReplayGain, StorageRoots, TrackInput,
};
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
    LibraryScanner::new(db, events, settings, Arc::new(FolderLocks::new()))
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

/// Likes a track for a user.
async fn like(db: &Database, user: uuid::Uuid, track: hearthsound_core::EntityId) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, 'track', ?, 'like', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Puts a track in a new playlist of a user and returns the playlist's id.
async fn playlist_entry(
    db: &Database,
    user: uuid::Uuid,
    track: hearthsound_core::EntityId,
) -> uuid::Uuid {
    let playlist = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_public, is_temporary, created_at, \
                                updated_at) VALUES (?, ?, 'Mix', 0, 0, 0, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) \
         VALUES (?, 0, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    playlist
}

/// Counts the likes a track carries.
async fn likes(db: &Database, track: hearthsound_core::EntityId) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM user_feedback \
         WHERE entity_kind = 'track' AND entity_id = ? AND value = 'like'",
    )
    .bind(track.uuid().as_bytes().as_slice())
    .fetch_one(db.readers())
    .await
    .unwrap()
}

/// Counts the entries a playlist has for a track.
async fn playlist_entries(
    db: &Database,
    playlist: uuid::Uuid,
    track: hearthsound_core::EntityId,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM playlist_tracks WHERE playlist_id = ? AND track_id = ?",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .fetch_one(db.readers())
    .await
    .unwrap()
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

/// A link inside a folder is part of the folder, wherever it points.
///
/// The administrator chose the folder, so a link into it is a deliberate
/// decision; a scan and a watcher therefore agree on following it.
#[tokio::test]
async fn a_symlinked_directory_is_followed_outside_the_folder() {
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
    assert_eq!(report.files_seen, 2, "the link is followed");
    let mut names: Vec<String> = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .map(|artist| artist.name)
        .collect();
    names.sort();
    assert_eq!(names, ["Energy 52", "Nobody"]);
}

/// A link cycle is visited once, and the scan finishes.
#[tokio::test]
async fn a_symlink_cycle_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("a")).unwrap();
    std::fs::create_dir_all(music.join("b")).unwrap();
    write_song(
        &music.join("a"),
        "Cafe del Mar",
        "Energy 52",
        "Cafe del Mar",
    )
    .await;
    write_song(&music.join("b"), "Sunset", "Someone Else", "Evening").await;
    std::os::unix::fs::symlink(music.join("a"), music.join("a/loop")).unwrap();
    std::os::unix::fs::symlink(&music, music.join("b/up")).unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let report = tokio::time::timeout(Duration::from_secs(30), scanner.scan_folder(folder.id))
        .await
        .expect("the walk terminates")
        .unwrap();

    assert_eq!(report.files_seen, 2, "each file is walked once");
    for name in ["Energy 52", "Someone Else"] {
        let track = only_track(&library, name).await;
        assert_eq!(
            library
                .list_media_files_for_track(track)
                .await
                .unwrap()
                .len(),
            1,
            "{name} is indexed once"
        );
    }
}

/// A link to an audio file is indexed under its own name.
#[tokio::test]
async fn a_symlinked_audio_file_is_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let target = write_song(&outside, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    std::os::unix::fs::symlink(&target, music.join("linked.flac")).unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_seen, 1);
    assert_eq!(report.files_indexed, 1);

    let track = only_track(&library, "Energy 52").await;
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].rel_path, "linked.flac", "the path stays relative");
}

/// A name that is not valid UTF-8 is skipped, never stored lossily.
#[tokio::test]
async fn a_file_name_that_is_not_utf8_is_skipped() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    std::fs::write(
        music.join(OsString::from_vec(b"bad\xff.flac".to_vec())),
        b"audio",
    )
    .unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();

    assert_eq!(report.files_seen, 1);
    assert_eq!(report.unreadable, 1);
    assert_eq!(library.list_artists().await.unwrap().len(), 1);
    let lossy: i64 = sqlx::query_scalar("SELECT count(*) FROM media_files WHERE rel_path LIKE ?")
        .bind("%\u{fffd}%")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(lossy, 0, "no stored path carries a replacement character");
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

    let scanner = LibraryScanner::new(
        &db,
        Arc::new(EventBus::new()),
        settings,
        Arc::new(FolderLocks::new()),
    );
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
async fn a_compilation_is_credited_to_various_artists() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    for (index, artist) in ["Nova Reyes", "Ida Frost", "Elmar Voss"].iter().enumerate() {
        AudioFixture::new(AudioFormat::Flac)
            .seconds(0.3)
            .file_name(format!("track{index}.flac"))
            .tags(
                TrackTags::new(format!("Song {index}"), *artist)
                    .album("Late Night Mix")
                    .track(index as u32 + 1, 3)
                    .compilation(true),
            )
            .write_into(&music)
            .await
            .unwrap();
    }
    // A second album without the flag and without an album artist keeps the
    // behavior it had: it is credited to its first track's artist.
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("single.flac")
        .tags(
            TrackTags::new("Alone", "Nova Reyes")
                .album("Nightfall")
                .track(1, 1),
        )
        .write_into(&music)
        .await
        .unwrap();

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();

    let various = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .find(|artist| artist.name == "Various Artists")
        .expect("the compilation is credited to Various Artists");
    let compilation = library
        .list_albums_for_artist(various.id)
        .await
        .unwrap()
        .into_iter()
        .find(|album| album.title == "Late Night Mix")
        .expect("the compilation album is credited to Various Artists");
    assert_eq!(compilation.album_type.as_deref(), Some("compilation"));
    assert_eq!(
        library
            .list_tracks_for_album(compilation.id)
            .await
            .unwrap()
            .len(),
        3
    );

    let nightfall = library
        .list_albums_for_artist(
            library
                .list_artists()
                .await
                .unwrap()
                .into_iter()
                .find(|artist| artist.name == "Nova Reyes")
                .unwrap()
                .id,
        )
        .await
        .unwrap()
        .into_iter()
        .find(|album| album.title == "Nightfall")
        .expect("the plain album keeps its first artist");
    assert_eq!(nightfall.album_type, None);
}

#[tokio::test]
async fn a_full_scan_reprobes_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let request = ScanRequest::folder(folder.id);

    let report = scanner.scan(&request).await.unwrap().remove(0);
    assert_eq!(report.files_indexed, 1);

    // An incremental rescan trusts the stored size and modification time.
    let report = scanner.scan(&request).await.unwrap().remove(0);
    assert_eq!(report.files_skipped, 1);
    assert_eq!(report.files_indexed, 0);

    // A full scan probes the file again and keeps one track and one file.
    let report = scanner
        .scan(&request.clone().full())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(report.files_skipped, 0);
    assert_eq!(report.files_indexed, 1);
    assert_eq!(library.list_artists().await.unwrap().len(), 1);

    let track = only_track(&library, "Energy 52").await;
    assert_eq!(
        library
            .list_media_files_for_track(track)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_subtree_scan_only_touches_its_subtree() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("chosen")).unwrap();
    std::fs::create_dir_all(music.join("other")).unwrap();
    write_song(&music.join("other"), "Untouched", "Someone Else", "Other").await;
    write_song(&music.join("chosen"), "Original", "First Artist", "Chosen").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(report.files_indexed, 2);

    let subtree = ScanRequest::folder(folder.id).paths(["chosen"]);
    std::fs::remove_file(music.join("chosen/Original.flac")).unwrap();
    let report = scanner.scan(&subtree).await.unwrap().remove(0);
    assert_eq!(report.files_seen, 0, "only the chosen subtree is walked");
    assert_eq!((report.files_removed, report.tracks_removed), (1, 1));

    // A new album inside the subtree is indexed.
    write_song(&music.join("chosen"), "New", "New Artist", "Chosen").await;
    let report = scanner.scan(&subtree).await.unwrap().remove(0);
    assert_eq!((report.files_seen, report.files_indexed), (1, 1));

    // The sibling subtree was never walked, so its rows are untouched.
    let names: Vec<String> = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .map(|artist| artist.name)
        .collect();
    assert!(names.contains(&"New Artist".to_owned()));
    assert!(names.contains(&"Someone Else".to_owned()));
    let untouched = only_track(&library, "Someone Else").await;
    assert_eq!(
        library.list_media_files_for_track(untouched).await.unwrap()[0].rel_path,
        "other/Untouched.flac"
    );
}

#[tokio::test]
async fn a_scan_never_leaves_the_folder_through_its_subtrees() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&outside).unwrap();
    write_song(&outside, "Hidden", "Nobody", "Nowhere").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let request = ScanRequest::folder(folder.id).paths(["../outside", "/etc", "./"]);
    let report = scanner.scan(&request).await.unwrap().remove(0);
    assert_eq!(report.files_seen, 0);
    assert!(library.list_artists().await.unwrap().is_empty());
}

/// The scanner reports a scan from the moment it is requested until the job
/// finishes, and the files it processed while running.
#[tokio::test]
async fn the_scan_status_tracks_queued_and_running_scans() {
    use std::time::{Duration, Instant};

    use hearthsound_jobs::{JobQueue, JobRunner, JobRunnerOptions, QueueOptions, Shutdown};

    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    for index in 0..60 {
        write_song(
            &music,
            &format!("Song {index}"),
            &format!("Artist {index}"),
            "Status",
        )
        .await;
    }

    let scanner = Arc::new(scanner_for(&db, Arc::new(EventBus::new())).await);
    let folder = library.add_library_folder(&music, None).await.unwrap();
    assert_eq!(scanner.status(), ScanStatus::default());

    let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
    let request = ScanRequest::folder(folder.id);
    scanner.request_scan(&queue, &request).await.unwrap();
    let queued = scanner.status();
    assert!(queued.scanning, "a requested scan counts as scanning");
    assert_eq!(queued.files_processed, 0);

    let mut runner = JobRunner::new(Arc::clone(&queue), JobRunnerOptions::default());
    runner.register(
        SCAN_JOB_KIND,
        Arc::new(ScanJobHandler::new(Arc::clone(&scanner))),
    );
    let runner = Arc::new(runner);
    let shutdown = Shutdown::new();
    let running = {
        let runner = Arc::clone(&runner);
        let shutdown = shutdown.clone();
        tokio::spawn(async move { runner.run(shutdown).await })
    };

    let mut running_with_files = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let status = scanner.status();
        running_with_files |= status.scanning && status.files_processed > 0;
        if running_with_files && !status.scanning {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert!(
        running_with_files,
        "the status reports the files a running scan processed"
    );

    shutdown.trigger();
    running.await.unwrap();
    let idle = scanner.status();
    assert!(!idle.scanning, "nothing is queued and nothing is running");
    assert!(idle.files_processed > 0, "the finished scan kept its count");
    assert_eq!(library.list_artists().await.unwrap().len(), 60);
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

/// A full scan of one folder and several incremental scans of it finish while
/// another writer keeps using the database.
///
/// The scans queue behind one folder lock and the runner extends their claims
/// while a batch's write transaction holds the single writer connection. A claim
/// extension that waited for that connection would leave the transaction open,
/// and every other write would queue behind it until the pool's acquire timeout;
/// this is the shape a large watched folder puts the server in.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_scans_of_one_folder_keep_the_writer_available() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    for album in 0..4 {
        let album_dir = music.join(format!("Album {album}"));
        std::fs::create_dir_all(&album_dir).unwrap();
        for track in 1..=4 {
            write_song(
                &album_dir,
                &format!("Track {track}"),
                "Scan Artist",
                &format!("Album {album}"),
            )
            .await;
        }
    }
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let scanner = Arc::new(scanner_for(&db, Arc::new(EventBus::new())).await);
    let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
    let mut runner = JobRunner::new(
        Arc::clone(&queue),
        JobRunnerOptions {
            workers: 4,
            poll_interval: Duration::from_millis(10),
            heartbeat_interval: Duration::from_millis(5),
            ..JobRunnerOptions::default()
        },
    );
    runner.register(
        SCAN_JOB_KIND,
        Arc::new(ScanJobHandler::new(Arc::clone(&scanner))),
    );
    let shutdown = Shutdown::new();
    let running = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { runner.run(shutdown).await })
    };

    scanner
        .request_scan(&queue, &ScanRequest::folder(folder.id))
        .await
        .unwrap();
    for _ in 0..6 {
        let request = ScanRequest::folder(folder.id).paths(["Album 0".to_owned()]);
        scanner.request_scan(&queue, &request).await.unwrap();
    }

    // Another writer - a session's last-seen stamp, an admin edit - must keep
    // getting the connection while the scans run.
    let writing = {
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            for index in 0..40 {
                let id = uuid::Uuid::now_v7();
                let write = sqlx::query(
                    "INSERT INTO users (id, username, display_name, password_hash, created_at, \
                                        updated_at) VALUES (?, ?, 'User', 'x', 0, 0)",
                )
                .bind(id.as_bytes().as_slice())
                .bind(format!("scan-{index}-{id}"))
                .execute(db.writer());
                tokio::time::timeout(Duration::from_secs(10), write)
                    .await
                    .expect("a write gets the writer while the scans run")
                    .expect("the write succeeds");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let ready = queue.count(JobState::Ready).await.unwrap();
        let claimed = queue.count(JobState::Running).await.unwrap();
        if ready + claimed == 0 {
            break;
        }
        assert!(Instant::now() < deadline, "the scans finish in time");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    writing.await.unwrap();

    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(tracks, 16);

    shutdown.trigger();
    running.await.unwrap();
}

/// The message a scan that refused to prune stores on the folder.
const PRUNE_REFUSED: &str =
    "Nothing was removed: the folder root is empty, so the disk may not be mounted.";

/// Builds a claimed `library.scan` job with a payload.
///
/// # Parameters
///
/// - `payload`: the job's JSON payload.
fn scan_job(payload: Option<String>) -> hearthsound_jobs::Job {
    hearthsound_jobs::Job {
        id: uuid::Uuid::now_v7(),
        kind: SCAN_JOB_KIND.into(),
        payload,
        state: JobState::Running,
        priority: hearthsound_core::Priority::Background,
        attempts: 1,
        run_at: 0,
        locked_until: None,
        last_error: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// A folder whose root holds no entry at all keeps every row it had.
///
/// An unmounted bind mount is an existing directory with nothing in it, which
/// looks exactly like an empty folder. Pruning it would delete a library that is
/// only out of reach, likes and playlist entries included.
#[tokio::test]
async fn an_empty_root_keeps_its_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let first = write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    let second = write_song(&music, "Sunset", "Someone Else", "Evening").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    assert_eq!(library.list_artists().await.unwrap().len(), 2);

    // The music went away, but the mount point is still a directory.
    std::fs::remove_file(&first).unwrap();
    std::fs::remove_file(&second).unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();

    assert!(report.prune_refused, "the scan refused to prune");
    assert_eq!(report.files_removed, 0);
    assert_eq!(report.tracks_removed, 0);
    assert_eq!(library.list_artists().await.unwrap().len(), 2);
    assert_eq!(
        library
            .get_library_folder(folder.id)
            .await
            .unwrap()
            .unwrap()
            .last_scan_error,
        Some(PRUNE_REFUSED.to_owned())
    );

    // Once the root holds content again the next scan prunes normally.
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    let report = scanner.scan_folder(folder.id).await.unwrap();
    assert!(!report.prune_refused);
    assert_eq!(
        library
            .get_library_folder(folder.id)
            .await
            .unwrap()
            .unwrap()
            .last_scan_error,
        None
    );
}

/// A root that holds a non-audio file is mounted and is pruned.
#[tokio::test]
async fn a_root_holding_only_a_non_audio_file_still_prunes() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let song = write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();

    std::fs::remove_file(&song).unwrap();
    std::fs::write(music.join("notes.txt"), b"not audio").unwrap();
    let report = scanner.scan_folder(folder.id).await.unwrap();

    assert!(
        !report.prune_refused,
        "an entry proves the mount is present"
    );
    assert_eq!(report.files_removed, 1);
    assert_eq!(report.tracks_removed, 1);
    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(tracks, 0, "the track row went with its last file");
}

/// One unreadable subdirectory is counted and skipped, not fatal.
#[tokio::test]
async fn an_unreadable_subdirectory_does_not_stop_the_scan() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let locked = music.join("locked");
    std::fs::create_dir(&locked).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();

    // Root reads the directory whatever its mode, so the assertion is skipped
    // there rather than failed.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let enforced = std::fs::read_dir(&locked).is_err();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(report.files_seen, 1);
    assert_eq!(report.files_indexed, 1);
    assert_eq!(library.list_artists().await.unwrap().len(), 1);
    if !enforced {
        return;
    }
    assert_eq!(report.unreadable, 1);
    let problem = report.problem().unwrap();
    assert!(problem.contains("1 entries could not be read"), "{problem}");
    assert!(problem.contains("locked"), "{problem}");
    let stored = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap()
        .last_scan_error
        .unwrap();
    assert!(stored.contains("locked"), "{stored}");
}

/// A folder whose root cannot be read fails its own scan only.
#[tokio::test]
async fn one_unreadable_folder_does_not_stop_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let broken = dir.path().join("broken");
    let music = dir.path().join("music");
    std::fs::create_dir(&broken).unwrap();
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let broken_folder = library.add_library_folder(&broken, None).await.unwrap();
    library.add_library_folder(&music, None).await.unwrap();
    // A path that is a file cannot be walked at all.
    std::fs::remove_dir(&broken).unwrap();
    std::fs::write(&broken, b"not a directory").unwrap();

    let error = scanner.scan(&ScanRequest::all()).await.unwrap_err();
    assert!(matches!(
        error,
        hearthsound_library::LibraryError::Scan { .. }
    ));
    assert_eq!(
        library.list_artists().await.unwrap().len(),
        1,
        "the healthy folder was still indexed"
    );
    let stored = library
        .get_library_folder(broken_folder.id)
        .await
        .unwrap()
        .unwrap()
        .last_scan_error
        .unwrap();
    assert!(stored.starts_with("Scan failed: reading"), "{stored}");
}

/// A scan that could not run is recorded on its folder and keeps the last
/// successful instant.
#[tokio::test]
async fn a_failed_scan_is_recorded_on_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let scanned_at = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap()
        .last_scan_at;
    assert!(scanned_at.is_some());

    // The directory is replaced by a file, which no walk can read.
    std::fs::remove_dir_all(&music).unwrap();
    std::fs::write(&music, b"not a directory").unwrap();
    assert!(scanner.scan_folder(folder.id).await.is_err());

    let folder = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        folder.last_scan_at, scanned_at,
        "a failed scan keeps the instant"
    );
    let error = folder.last_scan_error.unwrap();
    assert!(error.starts_with("Scan failed: reading"), "{error}");
    assert!(error.contains("music"), "{error}");
}

/// A later clean scan clears the recorded problem.
#[tokio::test]
async fn a_clean_scan_clears_the_recorded_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let scanned_at = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap()
        .last_scan_at;

    std::fs::remove_dir_all(&music).unwrap();
    std::fs::write(&music, b"not a directory").unwrap();
    assert!(scanner.scan_folder(folder.id).await.is_err());

    std::fs::remove_file(&music).unwrap();
    std::fs::create_dir(&music).unwrap();
    write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    scanner.scan_folder(folder.id).await.unwrap();

    let folder = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(folder.last_scan_error, None);
    assert!(folder.last_scan_at >= scanned_at);
}

/// A queued scan of a folder that no longer exists finishes without failing.
#[tokio::test]
async fn a_scan_of_a_deleted_folder_finishes_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();

    let scanner = Arc::new(scanner_for(&db, Arc::new(EventBus::new())).await);
    let queue = JobQueue::new(&db, QueueOptions::default());
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner
        .request_scan(&queue, &ScanRequest::folder(folder.id))
        .await
        .unwrap();
    library.remove_library_folder(folder.id).await.unwrap();
    assert!(scanner.status().scanning, "the request is queued");

    let payload = serde_json::to_string(&ScanRequest::folder(folder.id)).unwrap();
    let handler = ScanJobHandler::new(Arc::clone(&scanner));
    hearthsound_jobs::JobHandler::handle(&handler, scan_job(Some(payload)))
        .await
        .unwrap();
    assert!(!scanner.status().scanning, "the counter was released");
}

/// A scan whose payload cannot be parsed still releases the status counter.
#[tokio::test]
async fn a_malformed_scan_payload_releases_the_scan_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let scanner = Arc::new(scanner_for(&db, Arc::new(EventBus::new())).await);
    let queue = JobQueue::new(&db, QueueOptions::default());
    scanner
        .request_scan(&queue, &ScanRequest::folder(folder.id))
        .await
        .unwrap();
    let job = queue.claim().await.unwrap().unwrap();
    sqlx::query("UPDATE jobs SET payload = '{' WHERE id = ?")
        .bind(job.id.as_bytes().as_slice())
        .execute(db.writer())
        .await
        .unwrap();
    let malformed = hearthsound_jobs::Job {
        payload: Some("{".to_owned()),
        ..job
    };

    let handler = ScanJobHandler::new(Arc::clone(&scanner));
    assert!(
        hearthsound_jobs::JobHandler::handle(&handler, malformed)
            .await
            .is_err()
    );
    assert!(!scanner.status().scanning, "the counter was released");
}

/// Overlapping folders are allowed, and each file is indexed once.
///
/// A file belongs to the deepest folder that contains it, so the outer folder's
/// walk never descends into the inner one.
#[tokio::test]
async fn a_nested_folder_indexes_each_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    std::fs::create_dir_all(&inner).unwrap();
    write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&music, "Sunset", "Someone Else", "Evening").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    let nested = library.add_library_folder(&inner, None).await.unwrap();

    let outer_report = scanner.scan_folder(outer.id).await.unwrap();
    assert_eq!(
        outer_report.files_seen, 1,
        "the outer scan skips the inner root"
    );
    let nested_report = scanner.scan_folder(nested.id).await.unwrap();
    assert_eq!(nested_report.files_seen, 1);

    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(tracks, 2, "one track per file");
    let stray: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM media_files WHERE storage = 'folder' AND folder_id = ?",
    )
    .bind(outer.id.as_bytes().as_slice())
    .fetch_one(db.readers())
    .await
    .unwrap();
    assert_eq!(stray, 1, "the outer folder holds only the file it owns");
}

/// A nested folder takes over the track an outer folder indexed for the same
/// file, so the like and the playlist entry on it survive.
///
/// A file belongs to the deepest folder that contains it: the inner folder
/// re-points the outer folder's row instead of indexing the file again, which
/// keeps the track - and everything that points at it - and leaves the outer
/// folder nothing of its own to prune.
#[tokio::test]
async fn a_nested_folder_keeps_the_track_an_outer_folder_indexed() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    std::fs::create_dir_all(&inner).unwrap();
    write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&music, "Sunset", "Someone Else", "Evening").await;
    let user = insert_user(&db).await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(outer.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;
    like(&db, user, track).await;
    let playlist = playlist_entry(&db, user, track).await;

    // The inner folder is added after the outer one indexed its file.
    let nested = library.add_library_folder(&inner, None).await.unwrap();
    let report = scanner.scan_folder(nested.id).await.unwrap();
    assert_eq!(report.files_seen, 1);
    assert_eq!(
        report.files_skipped, 1,
        "the adopted row was not probed again"
    );
    assert_eq!(report.tracks_removed, 0, "the outer track is adopted");

    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(tracks, 2, "one track per file");
    assert_eq!(
        only_track(&library, "Energy 52").await,
        track,
        "the track kept its id"
    );
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 1, "the file was not indexed twice");
    assert_eq!(files[0].folder_id, Some(nested.id));
    assert_eq!(files[0].rel_path, "Cafe del Mar.flac");

    let report = scanner.scan_folder(outer.id).await.unwrap();
    assert_eq!(
        report.files_removed, 0,
        "the outer folder has nothing to prune"
    );
    assert_eq!(report.tracks_removed, 0);
    assert_eq!(only_track(&library, "Energy 52").await, track);
    assert_eq!(likes(&db, track).await, 1, "the like survived");
    assert_eq!(
        playlist_entries(&db, playlist, track).await,
        1,
        "the playlist entry survived"
    );
    let stray: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM media_files WHERE storage = 'folder' AND folder_id = ?",
    )
    .bind(outer.id.as_bytes().as_slice())
    .fetch_one(db.readers())
    .await
    .unwrap();
    assert_eq!(stray, 1, "the outer folder kept only the file it owns");
    assert_eq!(library.list_artists().await.unwrap().len(), 2);
}

/// A scan of the outer folder before the inner folder's first scan keeps the
/// track: the rows under a nested root are not the outer folder's to prune.
#[tokio::test]
async fn an_outer_folder_scan_leaves_the_rows_a_nested_folder_will_take_over() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    std::fs::create_dir_all(&inner).unwrap();
    write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&music, "Sunset", "Someone Else", "Evening").await;
    let user = insert_user(&db).await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(outer.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;
    like(&db, user, track).await;
    let playlist = playlist_entry(&db, user, track).await;

    let nested = library.add_library_folder(&inner, None).await.unwrap();
    let report = scanner.scan_folder(outer.id).await.unwrap();
    assert_eq!(
        report.files_removed, 0,
        "the nested root's rows are the nested folder's to take over"
    );
    assert_eq!(report.tracks_removed, 0);
    assert_eq!(only_track(&library, "Energy 52").await, track);

    let report = scanner.scan_folder(nested.id).await.unwrap();
    assert_eq!(report.files_seen, 1);
    assert_eq!(
        report.files_skipped, 1,
        "the adopted row was not probed again"
    );
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].folder_id, Some(nested.id));
    assert_eq!(only_track(&library, "Energy 52").await, track);
    assert_eq!(likes(&db, track).await, 1);
    assert_eq!(playlist_entries(&db, playlist, track).await, 1);
}

/// A second track for a file that already has one is removed by the nested
/// folder's scan, keeping the older track and its like.
///
/// A database written before a file had one owner holds a row in each folder,
/// and so a track per folder. The nested folder's scan keeps the enclosing
/// folder's track - the older one, the one a like was made against - and
/// removes the duplicate.
#[tokio::test]
async fn a_nested_folder_scan_removes_a_duplicate_track_for_the_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    std::fs::create_dir_all(&inner).unwrap();
    write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&music, "Sunset", "Someone Else", "Evening").await;
    let user = insert_user(&db).await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(outer.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;
    like(&db, user, track).await;

    let nested = library.add_library_folder(&inner, None).await.unwrap();
    let duplicate = library
        .insert_track(
            &TrackInput::new("Cafe del Mar", Origin::Local, Availability::Local),
            &[],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_media_file(&MediaFileInput {
            track_id: duplicate,
            storage: StorageClass::Folder,
            folder_id: Some(nested.id),
            rel_path: "Cafe del Mar.flac".to_owned(),
            size: 0,
            mtime_ms: 0,
            format: None,
            codec: None,
            bitrate_kbps: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            duration_ms: None,
            lossless: None,
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();

    let report = scanner.scan_folder(nested.id).await.unwrap();
    assert_eq!(report.tracks_removed, 1, "the duplicate track went");
    assert!(
        library.get_track(duplicate).await.unwrap().is_none(),
        "the duplicate is gone"
    );
    assert_eq!(only_track(&library, "Energy 52").await, track);
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].folder_id, Some(nested.id));
    assert_eq!(likes(&db, track).await, 1, "the like survived");
}

/// A nested folder whose root is empty keeps the rows an outer folder holds
/// for it: an empty root is what an unmounted disk looks like.
#[tokio::test]
async fn a_nested_folder_with_an_empty_root_keeps_the_rows_an_outer_folder_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    std::fs::create_dir_all(&inner).unwrap();
    let song = write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    let user = insert_user(&db).await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(outer.id).await.unwrap();
    let track = only_track(&library, "Energy 52").await;
    like(&db, user, track).await;
    let nested = library.add_library_folder(&inner, None).await.unwrap();

    std::fs::remove_file(&song).unwrap();
    let report = scanner.scan_folder(nested.id).await.unwrap();
    assert!(report.prune_refused, "an empty root is not pruned");
    assert_eq!(report.files_removed, 0);
    assert_eq!(only_track(&library, "Energy 52").await, track);
    assert_eq!(likes(&db, track).await, 1);
}

/// A scan of one subtree of a nested folder leaves the rows outside it alone.
///
/// The watcher scans the directory a file changed in, so a subtree scan must not
/// treat the files it did not walk as gone: the rows an enclosing folder holds
/// outside the subtree stay until a scan sees that subtree too.
#[tokio::test]
async fn a_nested_folder_subtree_scan_leaves_the_other_rows_alone() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("Alex G");
    let live = inner.join("Live");
    std::fs::create_dir_all(&live).unwrap();
    write_song(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;
    write_song(&live, "Sunset", "Energy 52", "Evening").await;
    let user = insert_user(&db).await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let outer = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(outer.id).await.unwrap();
    let studio = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .find(|artist| artist.name == "Energy 52")
        .unwrap();
    let album_of = |title: &'static str| {
        let library = &library;
        async move {
            library
                .list_albums_for_artist(studio.id)
                .await
                .unwrap()
                .into_iter()
                .find(|album| album.title == title)
                .unwrap()
                .id
        }
    };
    let studio_album = album_of("Cafe del Mar").await;
    let studio_track = library.list_tracks_for_album(studio_album).await.unwrap()[0].id;
    let evening_album = album_of("Evening").await;
    let evening_track = library.list_tracks_for_album(evening_album).await.unwrap()[0].id;
    like(&db, user, studio_track).await;

    let nested = library.add_library_folder(&inner, None).await.unwrap();
    let request = ScanRequest::folder(nested.id).paths(["Live"]);
    let report = scanner.scan(&request).await.unwrap().remove(0);
    assert_eq!(report.files_seen, 1, "only the subtree was walked");

    let files = library
        .list_media_files_for_track(evening_track)
        .await
        .unwrap();
    assert_eq!(files.len(), 1, "the walked file was taken over");
    assert_eq!(files[0].folder_id, Some(nested.id));
    assert_eq!(files[0].rel_path, "Live/Sunset.flac");
    assert!(
        library.get_track(studio_track).await.unwrap().is_some(),
        "the row outside the subtree is left alone"
    );
    let files = library
        .list_media_files_for_track(studio_track)
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].folder_id,
        Some(outer.id),
        "the outer folder still owns it until a scan walks it"
    );
    assert_eq!(likes(&db, studio_track).await, 1);
}

/// Writes a short FLAC with an explicit year and artist id.
///
/// # Parameters
///
/// - `dir`: the directory to write into.
/// - `stem`: the file name stem.
/// - `artist`: the credited artist.
/// - `album`: the album title.
/// - `year`: the release year the tags carry.
/// - `artist_mbid`: the MusicBrainz artist id, when the tags carry one.
async fn write_tagged(
    dir: &Path,
    stem: &str,
    artist: &str,
    album: &str,
    year: u32,
    artist_mbid: Option<&str>,
) -> std::path::PathBuf {
    let mut tags = TrackTags::new(stem, artist)
        .album(album)
        .album_artist(artist)
        .track(1, 1)
        .year(year);
    if let Some(mbid) = artist_mbid {
        tags = tags.mb_ids(
            format!("mb-recording-{stem}"),
            format!("mb-release-{album}-{year}"),
            format!("mb-group-{album}"),
            [mbid.to_owned()],
            [mbid.to_owned()],
        );
    }
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name(format!("{stem}.flac"))
        .tags(tags)
        .write_into(dir)
        .await
        .unwrap()
}

/// An untagged file next to two tagged namesakes gets its own artist row.
///
/// The order is fixed by scanning each act's file first: an untagged file that
/// is scanned before any identified artist of its name adopts that row instead,
/// which is the unambiguous case.
#[tokio::test]
async fn an_untagged_artist_beside_two_tagged_namesakes_gets_its_own_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;

    // Each file is its own folder so the scan order is the test's, not the
    // filesystem's.
    for (name, mbid) in [
        ("one", Some("mb-artist-a")),
        ("two", Some("mb-artist-b")),
        ("three", None),
    ] {
        let folder = dir.path().join(name);
        std::fs::create_dir(&folder).unwrap();
        write_tagged(&folder, &format!("Song {name}"), "Alex G", name, 2001, mbid).await;
        let library_folder = library.add_library_folder(&folder, None).await.unwrap();
        scanner.scan_folder(library_folder.id).await.unwrap();
    }

    let artists = library.list_artists().await.unwrap();
    assert_eq!(artists.len(), 3, "two acts and one untagged row");
    let untagged = artists
        .iter()
        .find(|artist| artist.mb_artist_id.is_none())
        .expect("the untagged file has a row of its own");
    let track = only_track_named(&db, "Song three").await;
    let credits = library.list_track_artists(track).await.unwrap();
    assert_eq!(credits[0].id, untagged.id);
}

/// Two files of one album title and artist but different years are two albums.
#[tokio::test]
async fn two_years_of_one_title_in_one_folder_are_two_albums() {
    for backwards in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = migrated(dir.path()).await;
        let library = LibraryRepository::new(&db);
        let music = dir.path().join("music");
        std::fs::create_dir(&music).unwrap();
        let years = if backwards {
            [2010, 2001]
        } else {
            [2001, 2010]
        };
        for (index, year) in years.iter().enumerate() {
            write_tagged(
                &music,
                &format!("Song {index}"),
                "Greatest Hits Band",
                "Greatest Hits",
                *year,
                None,
            )
            .await;
        }

        let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
        let folder = library.add_library_folder(&music, None).await.unwrap();
        scanner.scan_folder(folder.id).await.unwrap();

        let artist = library
            .list_artists()
            .await
            .unwrap()
            .into_iter()
            .find(|artist| artist.name == "Greatest Hits Band")
            .unwrap();
        let mut albums = library.list_albums_for_artist(artist.id).await.unwrap();
        albums.sort_by_key(|album| album.year);
        assert_eq!(albums.len(), 2, "the year is part of the identity");
        assert_eq!(albums[0].year, Some(2001));
        assert_eq!(albums[1].year, Some(2010));
        for album in &albums {
            assert_eq!(
                library.list_tracks_for_album(album.id).await.unwrap().len(),
                1
            );
        }
    }
}

/// A file re-tagged with a release id stays the album it was.
#[tokio::test]
async fn a_re_tagged_file_keeps_its_album() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let path = write_song(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner.scan_folder(folder.id).await.unwrap();
    let artist = library
        .list_artists()
        .await
        .unwrap()
        .into_iter()
        .find(|artist| artist.name == "Energy 52")
        .unwrap();
    assert_eq!(
        library
            .list_albums_for_artist(artist.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // The same path, re-tagged with a release id and the year it had.
    let tags = TrackTags::new("Cafe del Mar", "Energy 52")
        .album("Cafe del Mar")
        .album_artist("Energy 52")
        .track(1, 2)
        .year(1993)
        .mb_ids(
            "mb-recording-cafe",
            "mb-release-cafe",
            "mb-group-cafe",
            ["mb-artist-energy-52"],
            ["mb-artist-energy-52"],
        );
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name("Cafe del Mar.flac")
        .tags(tags)
        .write_into(&music)
        .await
        .unwrap();
    assert_eq!(path.file_name().unwrap(), "Cafe del Mar.flac");
    scanner.scan_folder(folder.id).await.unwrap();

    let albums = library.list_albums_for_artist(artist.id).await.unwrap();
    assert_eq!(albums.len(), 1, "the album adopted the release id");
    assert_eq!(albums[0].mb_release_id.as_deref(), Some("mb-release-cafe"));
    assert_eq!(albums[0].year, Some(1993), "the first year stayed");
}

/// Two folders with the same artist name share one artist.
///
/// An artist is one act across the library, so a folder boundary never splits
/// it; the folder identity questions live in `04-data-model.md`.
#[tokio::test]
async fn two_folders_with_the_same_artist_name_share_one_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    write_song(&first, "One", "Alex G", "First Album").await;
    write_song(&second, "Two", "Alex G", "Second Album").await;

    let scanner = scanner_for(&db, Arc::new(EventBus::new())).await;
    let one = library.add_library_folder(&first, None).await.unwrap();
    let two = library.add_library_folder(&second, None).await.unwrap();
    scanner.scan_folder(one.id).await.unwrap();
    scanner.scan_folder(two.id).await.unwrap();

    assert_eq!(library.list_artists().await.unwrap().len(), 1);
}

/// Returns the id of the track whose title is `title`.
async fn only_track_named(db: &Database, title: &str) -> hearthsound_core::EntityId {
    let id: Vec<u8> = sqlx::query_scalar("SELECT id FROM tracks WHERE title = ?")
        .bind(title)
        .fetch_one(db.readers())
        .await
        .unwrap();
    let uuid = uuid::Uuid::from_slice(&id).unwrap();
    hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::Track, uuid)
}
