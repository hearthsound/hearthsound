//! Tests for the album edition label: the folder rule, the tag rule, and the
//! one-time backfill.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{Job, JobHandler};
use hearthsound_jobs::{JobQueue, QueueOptions};
use hearthsound_library::editions::{
    EDITIONS_JOB_KIND, EDITIONS_PASS, EditionsJobHandler, enqueue_if_pending,
};
use hearthsound_library::identity::maintenance_pending;
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin};
use hearthsound_test_support::audio::{
    AudioFixture, AudioFormat, TrackTags, write_release_comment,
};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds a claimed `library.editions` job.
fn editions_job() -> Job {
    Job {
        id: uuid::Uuid::now_v7(),
        kind: EDITIONS_JOB_KIND.into(),
        payload: None,
        state: hearthsound_jobs::JobState::Running,
        priority: hearthsound_core::Priority::Background,
        attempts: 1,
        run_at: 0,
        locked_until: None,
        last_error: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// Runs the edition backfill over a database.
async fn run_pass(db: &Arc<Database>) {
    let handler = EditionsJobHandler::new(Arc::clone(db));
    JobHandler::handle(&handler, editions_job()).await.unwrap();
}

async fn scanner_for(db: &Database) -> LibraryScanner {
    let settings = Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();
    LibraryScanner::new(
        db,
        std::sync::Arc::new(EventBus::new()),
        settings,
        std::sync::Arc::new(FolderLocks::new()),
    )
}

/// Writes a short FLAC of one album into `dir`, under the given MusicBrainz
/// release id so two releases of one title stay apart.
async fn write_release(dir: &Path, title: &str, album: &str, release: &str) {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name(format!("{title}.flac"))
        .tags(
            TrackTags::new(title, "Lorde")
                .album(album)
                .album_artist("Lorde")
                .track(1, 2)
                .year(2021)
                .mb_ids(release, release, "group", ["lorde"], ["lorde"]),
        )
        .write_into(dir)
        .await
        .unwrap();
}

/// Returns the edition label of the album titled `title`.
async fn edition_of(db: &Database, title: &str) -> Option<String> {
    let editions: Vec<Option<String>> =
        sqlx::query_scalar("SELECT edition FROM albums WHERE title = ?")
            .bind(title)
            .fetch_all(db.readers())
            .await
            .unwrap();
    assert_eq!(editions.len(), 1, "one album titled {title}");
    editions[0].clone()
}

#[tokio::test]
async fn the_scan_labels_a_release_after_its_folder() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir_all(music.join("Lorde/Solar Power")).unwrap();
    std::fs::create_dir_all(music.join("Lorde/Solar Power (Deluxe Edition)")).unwrap();

    write_release(
        &music.join("Lorde/Solar Power"),
        "The Path",
        "Solar Power",
        "r1",
    )
    .await;
    write_release(
        &music.join("Lorde/Solar Power (Deluxe Edition)"),
        "Hold No Grudge",
        "Solar Power",
        "r2",
    )
    .await;

    let folder = library.add_library_folder(&music, None).await.unwrap();
    let scanner = scanner_for(&db).await;
    scanner.scan_folder(folder.id).await.unwrap();

    // Two releases of one title stay two albums, and only the deluxe one states
    // an edition: that is what tells the two cards apart.
    let editions: Vec<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT mb_release_id, edition FROM albums ORDER BY mb_release_id")
            .fetch_all(db.readers())
            .await
            .unwrap();
    assert_eq!(
        editions,
        [
            (Some("r1".to_owned()), None),
            (Some("r2".to_owned()), Some("Deluxe Edition".to_owned())),
        ]
    );
}

#[tokio::test]
async fn a_release_comment_wins_over_the_folder() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let album_dir = music.join("Solar Power (Deluxe Edition)");
    std::fs::create_dir_all(&album_dir).unwrap();
    write_release(&album_dir, "The Path", "Solar Power", "r1").await;
    let path = album_dir.join("The Path.flac");
    write_release_comment(&path, "Tour Edition").await.unwrap();

    let folder = library.add_library_folder(&music, None).await.unwrap();
    let scanner = scanner_for(&db).await;
    scanner.scan_folder(folder.id).await.unwrap();

    assert_eq!(
        edition_of(&db, "Solar Power").await.as_deref(),
        Some("Tour Edition")
    );
}

#[tokio::test]
async fn the_folder_states_an_edition_and_a_match_states_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let matching = music.join("Car Seat Headrest/Twin Fantasy (Face to Face)");
    let unrelated = music.join("Alex G/House of Sugar");
    std::fs::create_dir_all(&matching).unwrap();
    std::fs::create_dir_all(&unrelated).unwrap();
    write_release(&matching, "Beach Life-in-Death", "Twin Fantasy", "r1").await;
    write_release(&unrelated, "Judge", "House of Sugar", "r2").await;

    let folder = library.add_library_folder(&music, None).await.unwrap();
    let scanner = scanner_for(&db).await;
    scanner.scan_folder(folder.id).await.unwrap();

    assert_eq!(
        edition_of(&db, "Twin Fantasy").await.as_deref(),
        Some("Face to Face")
    );
    assert_eq!(edition_of(&db, "House of Sugar").await, None);
}

#[tokio::test]
async fn the_backfill_labels_the_albums_a_library_already_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let album_dir = music.join("Lorde/Solar Power (Deluxe Edition)");
    std::fs::create_dir_all(&album_dir).unwrap();
    write_release(&album_dir, "The Path", "Solar Power", "r1").await;

    let folder = library.add_library_folder(&music, None).await.unwrap();
    let scanner = scanner_for(&db).await;
    scanner.scan_folder(folder.id).await.unwrap();
    // The state a library that predates the label looks like: the row is there
    // and the column holds nothing.
    sqlx::query("UPDATE albums SET edition = NULL")
        .execute(db.writer())
        .await
        .unwrap();
    assert_eq!(edition_of(&db, "Solar Power").await, None);

    let queue = JobQueue::new(&db, QueueOptions::default());
    assert!(enqueue_if_pending(&db, &queue).await.unwrap());
    run_pass(&db).await;

    assert_eq!(
        edition_of(&db, "Solar Power").await.as_deref(),
        Some("Deluxe Edition")
    );
    assert!(!maintenance_pending(&db, EDITIONS_PASS).await.unwrap());
}

#[tokio::test]
async fn an_album_without_files_keeps_the_label_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let album = library
        .upsert_album(
            &AlbumInput::new("Solar Power", Origin::Catalog),
            &[ArtistCredit::new("Lorde", "main")],
        )
        .await
        .unwrap();
    sqlx::query("UPDATE albums SET edition = 'Deluxe Edition' WHERE id = ?")
        .bind(album.uuid().as_bytes().as_slice())
        .execute(db.writer())
        .await
        .unwrap();

    let queue = JobQueue::new(&db, QueueOptions::default());
    enqueue_if_pending(&db, &queue).await.unwrap();
    run_pass(&db).await;

    let stored: Option<String> = sqlx::query_scalar("SELECT edition FROM albums WHERE id = ?")
        .bind(album.uuid().as_bytes().as_slice())
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(stored.as_deref(), Some("Deluxe Edition"));
}
