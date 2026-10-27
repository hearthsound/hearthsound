//! Tests for the album edition label: the folder rule and the tag rule.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::LibraryRepository;
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::LibraryScanner;
use hearthsound_test_support::audio::{
    AudioFixture, AudioFormat, TrackTags, write_release_comment,
};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
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
    let editions: Vec<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT (SELECT e.value FROM external_ids e \
                  WHERE e.entity_kind = 'album' AND e.entity_id = a.id \
                    AND e.namespace = 'mb-release'), \
                a.edition \
         FROM albums a ORDER BY 1",
    )
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
