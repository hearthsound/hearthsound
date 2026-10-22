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

/// Sets a setting the maintainer reads.
async fn set(fixture: &Fixture, key: &str, value: SettingValue) {
    fixture.settings.set(key, value, None).await.unwrap();
}

/// Sets a file's size, addition time, and last stream time.
async fn age(fixture: &Fixture, id: uuid::Uuid, size: i64, added_at: i64, accessed: Option<i64>) {
    sqlx::query("UPDATE media_files SET size = ?, added_at = ?, last_accessed_at = ? WHERE id = ?")
        .bind(size)
        .bind(added_at)
        .bind(accessed)
        .bind(id.as_bytes().as_slice())
        .execute(fixture.db.writer())
        .await
        .unwrap();
}

/// Records `count` plays of a track by one user.
async fn plays(fixture: &Fixture, track: EntityId, count: usize) {
    let user = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, 'Listener', 'x', 0, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(format!("listener-{user}"))
    .execute(fixture.db.writer())
    .await
    .unwrap();
    for _ in 0..count {
        sqlx::query(
            "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
             VALUES (?, ?, ?, 0, 1000, 1)",
        )
        .bind(uuid::Uuid::now_v7().as_bytes().as_slice())
        .bind(user.as_bytes().as_slice())
        .bind(track.uuid().as_bytes().as_slice())
        .execute(fixture.db.writer())
        .await
        .unwrap();
    }
}

/// A probe that reports a fixed amount of free space.
struct FixedSpace(i64);

impl hearthsound_library::maintenance::SpaceProbe for FixedSpace {
    fn space(&self, _path: &Path) -> Option<hearthsound_library::maintenance::FilesystemSpace> {
        Some(hearthsound_library::maintenance::FilesystemSpace {
            total_bytes: 1 << 40,
            available_bytes: self.0,
        })
    }
}

/// Three cached tracks of 100 bytes each, added and streamed at the given
/// times, in the cache root.
async fn three_cached(
    fixture: &Fixture,
    times: [(i64, Option<i64>); 3],
) -> Vec<(EntityId, uuid::Uuid)> {
    let mut files = Vec::new();
    for (index, (added_at, accessed)) in times.into_iter().enumerate() {
        let track = fixture.track(Availability::Cached).await;
        let id = fixture
            .file(
                track,
                StorageClass::Cache,
                &format!("Artist/Album/{index:02} - Song.flac"),
            )
            .await;
        age(fixture, id, 100, added_at, accessed).await;
        files.push((track, id));
    }
    files
}

/// LRU eviction removes the least recently streamed unpinned cache files until
/// the cache fits, and never a pinned file or a library file.
#[tokio::test]
async fn lru_eviction_removes_the_least_recently_used_files_until_the_cache_fits() {
    let fixture = Fixture::new().await;
    let files = three_cached(&fixture, [(1, Some(300)), (2, Some(100)), (3, None)]).await;
    // A pinned cache file and an unpinned library file are never candidates.
    let pinned = fixture.track(Availability::Local).await;
    let pinned_file = fixture
        .file(pinned, StorageClass::Cache, "Pinned/Song.flac")
        .await;
    age(&fixture, pinned_file, 100, 0, None).await;
    fixture
        .library
        .add_pin_reason(pinned, PinReason::Admin)
        .await
        .unwrap();
    let permanent = fixture.track(Availability::Local).await;
    let permanent_file = fixture
        .file(permanent, StorageClass::Library, "Kept/Song.flac")
        .await;
    age(&fixture, permanent_file, 1000, 0, None).await;
    set(
        &fixture,
        keys::STORAGE_CACHE_MAX_BYTES.name(),
        SettingValue::Integer(250),
    )
    .await;

    let report = fixture.maintainer().evict().await.unwrap();

    // 400 cache bytes against a 250 byte cap: the two least recently used
    // unpinned files go (never streamed counts from its addition at 3 ms).
    assert_eq!(report.files, 2);
    assert_eq!(report.bytes, 200);
    assert!(
        fixture.row(files[2].1).await.is_none(),
        "added at 3, never streamed"
    );
    assert!(fixture.row(files[1].1).await.is_none(), "streamed at 100");
    assert!(
        fixture.row(files[0].1).await.is_some(),
        "streamed most recently"
    );
    assert!(
        !fixture
            .roots
            .cache
            .join("Artist/Album/02 - Song.flac")
            .exists()
    );
    assert!(fixture.row(pinned_file).await.is_some());
    assert!(fixture.row(permanent_file).await.is_some());
    let evicted = fixture
        .library
        .get_track(files[1].0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(evicted.availability, Availability::Remote);
    assert!(
        fixture
            .availability_events()
            .contains(&(files[1].0.to_string(), Availability::Remote))
    );
}

/// FIFO eviction removes the oldest additions first.
#[tokio::test]
async fn fifo_eviction_removes_the_oldest_additions_first() {
    let fixture = Fixture::new().await;
    let files = three_cached(&fixture, [(30, Some(1)), (10, Some(900)), (20, Some(2))]).await;
    set(
        &fixture,
        keys::STORAGE_CACHE_EVICTION.name(),
        SettingValue::String("fifo".to_owned()),
    )
    .await;
    set(
        &fixture,
        keys::STORAGE_CACHE_MAX_BYTES.name(),
        SettingValue::Integer(200),
    )
    .await;

    let report = fixture.maintainer().evict().await.unwrap();

    assert_eq!(report.files, 1);
    assert!(fixture.row(files[1].1).await.is_none(), "added first");
    assert!(fixture.row(files[0].1).await.is_some());
    assert!(fixture.row(files[2].1).await.is_some());
}

/// LFU eviction removes the least played track's file first.
#[tokio::test]
async fn lfu_eviction_removes_the_least_played_files_first() {
    let fixture = Fixture::new().await;
    let files = three_cached(&fixture, [(1, Some(1)), (2, Some(2)), (3, Some(3))]).await;
    plays(&fixture, files[0].0, 3).await;
    plays(&fixture, files[1].0, 1).await;
    plays(&fixture, files[2].0, 2).await;
    set(
        &fixture,
        keys::STORAGE_CACHE_EVICTION.name(),
        SettingValue::String("lfu".to_owned()),
    )
    .await;
    set(
        &fixture,
        keys::STORAGE_CACHE_MAX_BYTES.name(),
        SettingValue::Integer(200),
    )
    .await;

    let report = fixture.maintainer().evict().await.unwrap();

    assert_eq!(report.files, 1);
    assert!(fixture.row(files[1].1).await.is_none(), "played once");
    assert!(fixture.row(files[0].1).await.is_some());
    assert!(fixture.row(files[2].1).await.is_some());
}

/// A maximum age evicts stale files even when the cache is within its size.
#[tokio::test]
async fn files_older_than_the_maximum_age_are_evicted_without_pressure() {
    let fixture = Fixture::new().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let day = 86_400_000;
    let files = three_cached(
        &fixture,
        [
            (now - 10 * day, Some(now - 9 * day)),
            (now - 10 * day, Some(now - day)),
            (now, None),
        ],
    )
    .await;
    set(
        &fixture,
        keys::STORAGE_CACHE_MAX_AGE_DAYS.name(),
        SettingValue::Integer(7),
    )
    .await;

    let report = fixture.maintainer().evict().await.unwrap();

    assert_eq!(report.files, 1);
    assert!(
        fixture.row(files[0].1).await.is_none(),
        "last streamed nine days ago"
    );
    assert!(fixture.row(files[1].1).await.is_some());
    assert!(fixture.row(files[2].1).await.is_some());
}

/// Too little free space evicts until the floor is met, even within the size cap.
#[tokio::test]
async fn too_little_free_space_evicts_until_the_floor_is_met() {
    let fixture = Fixture::new().await;
    let files = three_cached(&fixture, [(1, Some(1)), (2, Some(2)), (3, Some(3))]).await;
    set(
        &fixture,
        keys::STORAGE_CACHE_MIN_FREE_BYTES.name(),
        SettingValue::Integer(1150),
    )
    .await;
    let maintainer = fixture
        .maintainer()
        .with_space_probe(Arc::new(FixedSpace(1000)));

    let report = maintainer.evict().await.unwrap();

    // 1000 available against a 1150 floor: two 100 byte files free enough.
    assert_eq!(report.files, 2);
    assert!(fixture.row(files[0].1).await.is_none());
    assert!(fixture.row(files[1].1).await.is_none());
    assert!(fixture.row(files[2].1).await.is_some());
}

/// The usage report counts the files and bytes per storage class.
#[tokio::test]
async fn the_usage_report_counts_each_storage_class() {
    let fixture = Fixture::new().await;
    let files = three_cached(&fixture, [(1, None), (2, None), (3, None)]).await;
    let kept = fixture.track(Availability::Local).await;
    let kept_file = fixture
        .file(kept, StorageClass::Library, "Kept/Song.flac")
        .await;
    age(&fixture, kept_file, 1000, 0, None).await;
    fixture
        .library
        .add_pin_reason(kept, PinReason::Admin)
        .await
        .unwrap();
    let maintainer = fixture
        .maintainer()
        .with_space_probe(Arc::new(FixedSpace(5)));

    let usage = maintainer.usage().await.unwrap();

    assert_eq!(usage.cache.files, files.len() as u64);
    assert_eq!(usage.cache.bytes, 300);
    assert_eq!(usage.library.files, 1);
    assert_eq!(usage.library.bytes, 1000);
    assert_eq!(usage.folders.files, 0);
    assert_eq!(usage.pinned_tracks, 1);
    assert_eq!(usage.filesystems[0].available_bytes, 5);
}

/// Returns a track's active pin reasons.
async fn reasons(fixture: &Fixture, track: EntityId) -> Vec<PinReason> {
    let mut conn = fixture.db.readers().acquire().await.unwrap();
    hearthsound_library::retention::pin_reasons(&mut conn, track)
        .await
        .unwrap()
}

/// A track played often enough gains the policy pin, and loses it when the
/// threshold rises above its plays or is turned off.
#[tokio::test]
async fn play_counts_pin_and_release_the_policy_reason() {
    let fixture = Fixture::new().await;
    let often = fixture.track(Availability::Cached).await;
    fixture
        .file(often, StorageClass::Cache, "Often/Song.flac")
        .await;
    plays(&fixture, often, 3).await;
    let rarely = fixture.track(Availability::Cached).await;
    fixture
        .file(rarely, StorageClass::Cache, "Rarely/Song.flac")
        .await;
    plays(&fixture, rarely, 1).await;

    // Off by default: nothing is pinned.
    assert_eq!(
        fixture.maintainer().autopin().await.unwrap(),
        Default::default()
    );

    set(
        &fixture,
        keys::STORAGE_AUTOPIN_PLAY_COUNT.name(),
        SettingValue::Integer(3),
    )
    .await;
    let report = fixture.maintainer().autopin().await.unwrap();
    assert_eq!((report.pinned, report.released), (1, 0));
    assert_eq!(reasons(&fixture, often).await, [PinReason::Policy]);
    assert!(reasons(&fixture, rarely).await.is_empty());
    let pinned = fixture.library.get_track(often).await.unwrap().unwrap();
    assert_eq!(pinned.availability, Availability::Local);
    // A second pass changes nothing.
    assert_eq!(
        fixture.maintainer().autopin().await.unwrap(),
        Default::default()
    );

    set(
        &fixture,
        keys::STORAGE_AUTOPIN_PLAY_COUNT.name(),
        SettingValue::Integer(5),
    )
    .await;
    let report = fixture.maintainer().autopin().await.unwrap();
    assert_eq!((report.pinned, report.released), (0, 1));
    assert!(reasons(&fixture, often).await.is_empty());

    set(
        &fixture,
        keys::STORAGE_AUTOPIN_PLAY_COUNT.name(),
        SettingValue::Integer(1),
    )
    .await;
    let report = fixture.maintainer().autopin().await.unwrap();
    assert_eq!((report.pinned, report.released), (2, 0));
    set(
        &fixture,
        keys::STORAGE_AUTOPIN_PLAY_COUNT.name(),
        SettingValue::Integer(0),
    )
    .await;
    let report = fixture.maintainer().autopin().await.unwrap();
    assert_eq!((report.pinned, report.released), (0, 2));
}
