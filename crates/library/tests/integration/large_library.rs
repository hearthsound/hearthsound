//! The large-library benchmark.
//!
//! A scan of 20,000 files indexes every one of them, and a rescan of the
//! unchanged library skips every one of them quickly. These are the acceptance
//! criteria for a library that size, so the test is ignored by default: it
//! writes 20,000 files and takes minutes.
//!
//! ```
//! cargo nextest run -p hearthsound-library --run-ignored only -E 'test(large_library)'
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::LibraryRepository;
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags, write_tags};

/// How many albums the generated library holds.
const ALBUMS: usize = 2_000;

/// How many tracks each generated album holds.
const TRACKS_PER_ALBUM: usize = 10;

/// How many files the generated library holds.
const FILES: usize = ALBUMS * TRACKS_PER_ALBUM;

/// How many artists the generated albums are spread over.
const ARTISTS: usize = 50;

/// How long a no-change rescan of the library may take.
const RESCAN_BUDGET: Duration = Duration::from_secs(30);

/// Opens a migrated database in `dir`.
async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds a scanner with the built-in settings.
async fn scanner(db: &Database) -> LibraryScanner {
    let settings = Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();
    LibraryScanner::new(
        db,
        Arc::new(EventBus::new()),
        settings,
        Arc::new(FolderLocks::new()),
    )
}

/// Counts the tracks the scan wrote.
async fn count_tracks(db: &Database) -> i64 {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    count
}

/// Counts the albums the scan wrote.
async fn count_albums(db: &Database) -> i64 {
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM albums")
        .fetch_one(db.readers())
        .await
        .unwrap();
    count
}

/// Writes one album's tracks by copying a generated file and retagging it.
///
/// # Parameters
///
/// - `music`: the library root.
/// - `template`: the generated file every track is copied from.
/// - `album`: the album index.
async fn write_album(music: &Path, template: &Path, album: usize) {
    let artist = format!("Artist {}", album % ARTISTS);
    let album_title = format!("Album {album}");
    let album_dir = music.join(&album_title);
    std::fs::create_dir_all(&album_dir).unwrap();
    for track in 1..=TRACKS_PER_ALBUM {
        let path = album_dir.join(format!("{track:02} - Track {track}.mp3"));
        std::fs::copy(template, &path).unwrap();
        write_tags(
            &path,
            &TrackTags::new(format!("Track {track}"), artist.clone())
                .album(album_title.clone())
                .album_artist(artist.clone())
                .track(track as u32, TRACKS_PER_ALBUM as u32),
            None,
        )
        .await
        .unwrap();
    }
}

/// A 20,000-file library scans completely, then rescans without probing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "generates 20000 files; run with --run-ignored only"]
async fn a_twenty_thousand_file_library_scans_and_rescans() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();

    let template_dir = dir.path().join("template");
    std::fs::create_dir_all(&template_dir).unwrap();
    let template = AudioFixture::new(AudioFormat::Mp3Cbr)
        .seconds(0.2)
        .file_name("template.mp3")
        .write_into(&template_dir)
        .await
        .unwrap();

    let started = Instant::now();
    for album in 0..ALBUMS {
        write_album(&music, &template, album).await;
    }
    println!("generated {FILES} files in {:?}", started.elapsed());

    let scanner = scanner(&db).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let started = Instant::now();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    let first = started.elapsed();
    println!("first scan: {first:?}");
    assert_eq!(report.files_seen, FILES as u64);
    assert_eq!(report.files_indexed, FILES as u64);
    assert_eq!(report.files_skipped, 0);
    assert_eq!(report.files_failed, 0);
    assert_eq!(count_tracks(&db).await, FILES as i64);
    assert_eq!(count_albums(&db).await, ALBUMS as i64);

    let started = Instant::now();
    let report = scanner.scan_folder(folder.id).await.unwrap();
    let rescan = started.elapsed();
    println!("rescan: {rescan:?}");
    assert_eq!(report.files_seen, FILES as u64);
    assert_eq!(report.files_skipped, FILES as u64);
    assert_eq!(report.files_indexed, 0);
    assert_eq!(count_tracks(&db).await, FILES as i64);
    assert!(
        rescan < RESCAN_BUDGET,
        "a no-change rescan of {FILES} files took {rescan:?}"
    );
}

/// How many files the removal test seeds.
const REMOVAL_FILES: usize = 2_000;

/// How many files the prune test seeds.
const PRUNE_FILES: usize = 1_000;

/// How long one write may wait while a folder is removed.
///
/// The removal commits per batch, so a write from another request waits for one
/// batch; a removal that held the writer for the whole folder would be far over
/// this.
const WRITE_BUDGET: Duration = Duration::from_secs(2);

/// Seeds one folder with `count` local tracks that each have one file row.
///
/// The rows are written straight to the database because these tests are about
/// the write path, not about probing: no audio is needed for a row to be pruned
/// or removed.
///
/// # Parameters
///
/// - `db`: the open database.
/// - `folder`: the folder the file rows belong to.
/// - `count`: how many tracks to seed.
/// - `prefix`: the file name prefix, so two folders do not collide.
async fn seed_rows(db: &Database, folder: uuid::Uuid, count: usize, prefix: &str) {
    let mut tx = db.writer().begin().await.unwrap();
    for index in 0..count {
        let track = uuid::Uuid::now_v7();
        sqlx::query(
            "INSERT INTO tracks (id, title, sort_title, display_artist, origin, availability, \
                                 created_at, updated_at) \
             VALUES (?, ?, ?, 'Seeded Artist', 'local', 'local', 0, 0)",
        )
        .bind(track.as_bytes().as_slice())
        .bind(format!("Track {index}"))
        .bind(format!("track {index}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO media_files (id, track_id, storage, folder_id, rel_path, size, mtime_ms, \
                                      added_at) \
             VALUES (?, ?, 'folder', ?, ?, 1, 0, 0)",
        )
        .bind(uuid::Uuid::now_v7().as_bytes().as_slice())
        .bind(track.as_bytes().as_slice())
        .bind(folder.as_bytes().as_slice())
        .bind(format!("{prefix} {index}.flac"))
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

/// A folder removal of two thousand files never holds the writer for the folder.
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_folder_keeps_the_writer_available() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let other = dir.path().join("other");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&other).unwrap();
    let doomed = library.add_library_folder(&music, None).await.unwrap();
    let survivor = library.add_library_folder(&other, None).await.unwrap();
    seed_rows(&db, doomed.id, REMOVAL_FILES, "Doomed").await;
    seed_rows(&db, survivor.id, 1, "Kept").await;
    let survivor_track: Vec<u8> =
        sqlx::query_scalar("SELECT track_id FROM media_files WHERE folder_id = ? LIMIT 1")
            .bind(survivor.id.as_bytes().as_slice())
            .fetch_one(db.readers())
            .await
            .unwrap();
    let survivor_track = hearthsound_core::EntityId::from_uuid(
        hearthsound_core::IdPrefix::Track,
        uuid::Uuid::from_slice(&survivor_track).unwrap(),
    );

    let removing = {
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            LibraryRepository::new(&db)
                .remove_library_folder(doomed.id)
                .await
        })
    };
    let mut writes = 0_u32;
    while !removing.is_finished() {
        let library = LibraryRepository::new(&db);
        tokio::time::timeout(
            WRITE_BUDGET,
            library.add_pin_reason(survivor_track, hearthsound_core::PinReason::Admin),
        )
        .await
        .expect("a write gets the writer while the folder is removed")
        .expect("the write succeeds");
        writes += 1;
    }
    let removed = removing.await.unwrap().unwrap();
    assert_eq!(removed.len(), REMOVAL_FILES);
    assert!(writes > 0, "the writer stayed available during the removal");

    let folders = LibraryRepository::new(&db)
        .list_library_folders()
        .await
        .unwrap();
    assert_eq!(folders.len(), 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM media_files WHERE folder_id = ?")
        .bind(doomed.id.as_bytes().as_slice())
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

/// A prune of a thousand vanished files removes them all, in batches.
#[tokio::test(flavor = "multi_thread")]
async fn prune_removes_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let other = dir.path().join("other");
    std::fs::create_dir(&music).unwrap();
    std::fs::create_dir(&other).unwrap();
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let untouched = library.add_library_folder(&other, None).await.unwrap();
    seed_rows(&db, folder.id, PRUNE_FILES, "Gone").await;
    seed_rows(&db, untouched.id, 5, "Kept").await;
    // An entry proves the mount is present, which is what lets the scan prune.
    std::fs::write(music.join("notes.txt"), b"not audio").unwrap();

    let scanner = scanner(&db).await;
    let report = scanner.scan_folder(folder.id).await.unwrap();

    assert_eq!(report.files_removed as usize, PRUNE_FILES);
    assert_eq!(report.tracks_removed as usize, PRUNE_FILES);
    let tracks: i64 = sqlx::query_scalar("SELECT count(*) FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(tracks, 5, "the other folder's tracks are untouched");
}
