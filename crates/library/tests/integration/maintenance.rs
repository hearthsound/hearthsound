//! Tests for storage maintenance: moving a managed file to the root its
//! track's pins imply, demoting it, or deleting it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_config::keys;
use hearthsound_config::{SettingSources, SettingValue, SettingsRegistry, settings::Settings};
use hearthsound_core::events::{EventBus, Replay, ServerEvent};
use hearthsound_core::{Availability, EntityId, PinReason, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::maintenance::StorageMaintainer;
use hearthsound_library::{LibraryRepository, ManagedMediaFile, Origin, StorageRoots, TrackInput};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds the settings service over a database, with the built-in defaults.
async fn settings_for(db: &Database) -> Settings {
    Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap()
}

/// A maintainer over a temporary data directory.
struct Fixture {
    /// Keeps the storage tree alive for the whole test.
    _dir: tempfile::TempDir,
    /// The open database.
    db: Database,
    /// The two managed roots.
    roots: StorageRoots,
    /// The settings service the maintainer reads.
    settings: Settings,
    /// The bus the maintainer publishes on.
    events: Arc<EventBus>,
    /// The repository the rows are read back through.
    library: LibraryRepository,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = migrated(dir.path()).await;
        let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));
        std::fs::create_dir_all(&roots.library).unwrap();
        std::fs::create_dir_all(&roots.cache).unwrap();
        let settings = settings_for(&db).await;
        Fixture {
            _dir: dir,
            library: LibraryRepository::new(&db),
            db,
            roots,
            settings,
            events: Arc::new(EventBus::new()),
        }
    }

    fn maintainer(&self) -> StorageMaintainer {
        StorageMaintainer::new(
            &self.db,
            self.roots.clone(),
            self.settings.clone(),
            Arc::clone(&self.events),
        )
    }

    /// Inserts a track with the given starting availability.
    async fn track(&self, availability: Availability) -> EntityId {
        self.library
            .insert_track(
                &TrackInput::new("Cafe del Mar", Origin::Local, availability),
                &[],
                &[],
            )
            .await
            .unwrap()
    }

    /// Inserts a media file row and writes the file it names.
    async fn file(&self, track: EntityId, storage: StorageClass, rel_path: &str) -> uuid::Uuid {
        let id = self
            .library
            .insert_managed_media_file(&ManagedMediaFile {
                track_id: track,
                storage,
                rel_path: rel_path.to_owned(),
                size: 4,
                mtime_ms: 1,
                format: Some("flac".to_owned()),
                codec: None,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                duration_ms: None,
                lossless: Some(true),
                quality_tier: None,
                replaygain: None,
                has_embedded_art: None,
                uploaded_by: None,
                acquisition_id: None,
            })
            .await
            .unwrap();
        let path = self.roots.root_for(storage).join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"flac").unwrap();
        id
    }

    /// Writes a file that no row names, which is what a collision looks like.
    fn stray_file(&self, storage: StorageClass, rel_path: &str) -> PathBuf {
        let path = self.roots.root_for(storage).join(rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"other").unwrap();
        path
    }

    /// Returns a media file row.
    async fn row(&self, id: uuid::Uuid) -> Option<hearthsound_library::MediaFile> {
        self.library.get_media_file(id).await.unwrap()
    }

    /// Returns the availability events published so far.
    fn availability_events(&self) -> Vec<(String, Availability)> {
        let Replay::Events(events) = self.events.replay_after(0) else {
            panic!("the bus holds only the events of this test");
        };
        events
            .into_iter()
            .filter_map(|envelope| match envelope.event {
                ServerEvent::TrackAvailability(payload) => {
                    Some((payload.track_id, payload.availability))
                }
                _ => None,
            })
            .collect()
    }
}

/// Unpinning a track demotes its file into the cache root.
#[tokio::test]
async fn an_unpin_demotes_the_file_into_the_cache() {
    let fixture = Fixture::new().await;
    let track = fixture.track(Availability::Local).await;
    let rel_path = "Cafe del Mar/1999 - Cafe del Mar/01 - Cafe del Mar.flac";
    let id = fixture.file(track, StorageClass::Library, rel_path).await;
    fixture
        .library
        .add_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();

    // While the track is pinned nothing moves.
    assert_eq!(
        fixture.maintainer().relocate().await.unwrap(),
        Default::default()
    );
    assert!(fixture.roots.library.join(rel_path).is_file());

    fixture
        .library
        .remove_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();
    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report.demoted, 1);
    assert_eq!(report.promoted, 0);
    assert_eq!(report.deleted, 0);

    // The row follows the file, and the track is cached rather than local.
    let row = fixture.row(id).await.unwrap();
    assert_eq!(row.storage, StorageClass::Cache);
    assert_eq!(row.rel_path, rel_path);
    assert!(fixture.roots.cache.join(rel_path).is_file());
    assert!(
        !fixture.roots.library.join(rel_path).exists(),
        "the file left the permanent root"
    );
    assert!(
        !fixture.roots.library.join("Cafe del Mar").exists(),
        "the emptied directories are removed"
    );
    let stored = fixture.library.get_track(track).await.unwrap().unwrap();
    assert_eq!(stored.availability, Availability::Cached);
    assert_eq!(
        fixture.availability_events(),
        [(track.to_string(), Availability::Cached)]
    );
}

/// Pinning a track promotes its cache file into the library root.
#[tokio::test]
async fn a_pin_promotes_the_file_into_the_library() {
    let fixture = Fixture::new().await;
    let track = fixture.track(Availability::Cached).await;
    let rel_path = "Cafe del Mar/1999 - Cafe del Mar/01 - Cafe del Mar.flac";
    let id = fixture.file(track, StorageClass::Cache, rel_path).await;

    fixture
        .library
        .add_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();
    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report.promoted, 1);

    let row = fixture.row(id).await.unwrap();
    assert_eq!(row.storage, StorageClass::Library);
    assert_eq!(row.rel_path, rel_path);
    assert!(fixture.roots.library.join(rel_path).is_file());
    assert!(!fixture.roots.cache.join(rel_path).exists());
    assert!(
        !fixture.roots.cache.join("Cafe del Mar").exists(),
        "the emptied directories are removed"
    );
    assert_eq!(
        fixture
            .library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local
    );
    // The pin already made the track local, so moving its file changes nothing
    // a client could observe and nothing is published.
    assert!(fixture.availability_events().is_empty());
}

/// A promoted file whose name is taken takes the next collision suffix.
#[tokio::test]
async fn a_promoted_file_takes_the_next_free_name() {
    let fixture = Fixture::new().await;
    let track = fixture.track(Availability::Cached).await;
    let rel_path = "Cafe del Mar/01 - Cafe del Mar.flac";
    let id = fixture.file(track, StorageClass::Cache, rel_path).await;
    let occupied = fixture.stray_file(StorageClass::Library, rel_path);

    fixture
        .library
        .add_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();
    assert_eq!(fixture.maintainer().relocate().await.unwrap().promoted, 1);

    assert_eq!(std::fs::read(&occupied).unwrap(), b"other");
    let row = fixture.row(id).await.unwrap();
    assert_eq!(row.rel_path, "Cafe del Mar/01 - Cafe del Mar (2).flac");
    assert_eq!(
        std::fs::read(fixture.roots.library.join(&row.rel_path)).unwrap(),
        b"flac"
    );
}

/// Deleting is what an unpin does when `storage.unpin_action` says so.
#[tokio::test]
async fn an_unpin_deletes_the_file_when_configured() {
    let fixture = Fixture::new().await;
    fixture
        .settings
        .set(
            keys::STORAGE_UNPIN_ACTION.name(),
            SettingValue::String("delete".to_owned()),
            None,
        )
        .await
        .unwrap();
    let track = fixture.track(Availability::Local).await;
    let rel_path = "Cafe del Mar/01 - Cafe del Mar.flac";
    let id = fixture.file(track, StorageClass::Library, rel_path).await;

    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report.deleted, 1);
    assert!(
        !fixture.roots.library.join(rel_path).exists(),
        "the file is gone"
    );
    assert!(fixture.row(id).await.is_none(), "its row is gone too");
    assert_eq!(
        fixture
            .library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Remote
    );
    assert_eq!(
        fixture.availability_events(),
        [(track.to_string(), Availability::Remote)]
    );
}

/// Deleting one file keeps the availability another file gives the track.
#[tokio::test]
async fn a_delete_keeps_the_availability_of_the_files_that_remain() {
    let fixture = Fixture::new().await;
    fixture
        .settings
        .set(
            keys::STORAGE_UNPIN_ACTION.name(),
            SettingValue::String("delete".to_owned()),
            None,
        )
        .await
        .unwrap();
    let track = fixture.track(Availability::Local).await;
    // A second, unpinned managed file: the track has a file left, so it stays
    // served rather than becoming remote.
    fixture
        .file(track, StorageClass::Cache, "Cafe del Mar/other.flac")
        .await;
    let id = fixture
        .file(
            track,
            StorageClass::Library,
            "Cafe del Mar/01 - Cafe del Mar.flac",
        )
        .await;

    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report.deleted, 1);
    assert!(fixture.row(id).await.is_none());
    assert_eq!(
        fixture
            .library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Cached
    );
}

/// A file inside a scanned folder is never a candidate.
#[tokio::test]
async fn folder_files_are_left_alone() {
    let fixture = Fixture::new().await;
    let folder_root = fixture._dir.path().join("music").join("cafe");
    std::fs::create_dir_all(&folder_root).unwrap();
    let folder = fixture
        .library
        .add_library_folder(&folder_root, None)
        .await
        .unwrap();
    let track = fixture.track(Availability::Local).await;
    let id = fixture
        .library
        .insert_media_file(&hearthsound_library::MediaFileInput {
            track_id: track,
            storage: StorageClass::Folder,
            folder_id: Some(folder.id),
            rel_path: "01 - Cafe del Mar.flac".to_owned(),
            size: 4,
            mtime_ms: 1,
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

    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report, Default::default());
    let row = fixture.row(id).await.unwrap();
    assert_eq!(row.storage, StorageClass::Folder);
    assert_eq!(row.folder_id, Some(folder.id));
    assert_eq!(row.rel_path, "01 - Cafe del Mar.flac");
    assert_eq!(
        fixture
            .library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local
    );
}

/// A stored library row whose file is gone does not stop the pass.
#[tokio::test]
async fn a_file_that_is_not_on_disk_is_skipped() {
    let fixture = Fixture::new().await;
    let track = fixture.track(Availability::Cached).await;
    let rel_path = "Cafe del Mar/01 - Cafe del Mar.flac";
    let id = fixture.file(track, StorageClass::Cache, rel_path).await;
    fixture
        .library
        .add_pin_reason(track, PinReason::UserLibrary)
        .await
        .unwrap();
    std::fs::remove_file(fixture.roots.cache.join(rel_path)).unwrap();

    let report = fixture.maintainer().relocate().await.unwrap();
    assert_eq!(report, Default::default(), "the move could not happen");
    let row = fixture.row(id).await.unwrap();
    assert_eq!(row.storage, StorageClass::Cache, "the row is untouched");
}
