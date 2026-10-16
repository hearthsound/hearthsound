//! Tests for the folder watcher: a change on disk becomes an indexed track.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{JobQueue, JobRunner, JobRunnerOptions, QueueOptions, Shutdown};
use hearthsound_library::LibraryRepository;
use hearthsound_library::scan::{LibraryScanner, SCAN_JOB_KIND, ScanJobHandler};
use hearthsound_library::watch::LibraryWatcher;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};

/// Opens a migrated database in `dir`.
async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds the settings service over a database.
async fn settings(db: &Database) -> Settings {
    Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap()
}

/// Writes the three tracks of one album into `dir`.
async fn write_album(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for index in 1..=3 {
        AudioFixture::new(AudioFormat::Flac)
            .seconds(0.3)
            .file_name(format!("0{index} - Track {index}.flac"))
            .tags(
                TrackTags::new(format!("Track {index}"), "Watched Artist")
                    .album("Watched Album")
                    .album_artist("Watched Artist")
                    .track(index, 3),
            )
            .write_into(dir)
            .await
            .unwrap();
    }
}

/// Copies a directory tree, which is what a user does when they drop an album
/// into a watched folder.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Polls `check` until it holds or the deadline passes.
///
/// # Returns
///
/// `true` when the check held before the deadline.
async fn wait_for<F, Fut>(deadline: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let started = Instant::now();
    while started.elapsed() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    check().await
}

/// An album dropped into a watched folder is indexed without a scan being asked
/// for.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_album_is_indexed_when_it_appears() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let settings = settings(&db).await;
    let music = dir.path().join("music");
    let staged = dir.path().join("staged");
    std::fs::create_dir(&music).unwrap();
    library.add_library_folder(&music, None).await.unwrap();

    let queue = Arc::new(JobQueue::new(&db, QueueOptions::default()));
    let scanner = Arc::new(LibraryScanner::new(
        &db,
        Arc::new(EventBus::new()),
        settings.clone(),
    ));
    let watcher = LibraryWatcher::new(&db, Arc::clone(&scanner), Arc::clone(&queue), settings);

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

    watcher.reconcile().await.unwrap();
    assert_eq!(watcher.watched_folders(), 1);

    // The album is written outside the watched folder and then moved in, which
    // is one burst of filesystem events.
    write_album(&staged.join("Watched Album")).await;
    copy_tree(&staged.join("Watched Album"), &music.join("Watched Album"));

    let library = LibraryRepository::new(&db);
    let indexed = wait_for(Duration::from_secs(10), || async {
        library
            .list_artists()
            .await
            .is_ok_and(|artists| !artists.is_empty())
    })
    .await;
    assert!(indexed, "the watcher indexed the new album");

    let tracks = library.list_artists().await.unwrap();
    assert_eq!(tracks.len(), 1, "one artist came from the new album");
    let artist = tracks[0].id;
    let albums = library.list_albums_for_artist(artist).await.unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(
        library
            .list_tracks_for_album(albums[0].id)
            .await
            .unwrap()
            .len(),
        3
    );

    watcher.stop();
    shutdown.trigger();
    running.await.unwrap();
}
