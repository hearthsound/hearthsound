//! Tests for track retention: pin reasons, the availability they imply, folder
//! re-association, and the import destination.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{Availability, EntityId, PinReason, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::retention::{
    self, import_destination, pin_reasons, recompute_availability, repin_folder_tracks,
    set_pin_reasons, track_is_pinned,
};
use hearthsound_library::{
    LibraryRepository, ManagedMediaFile, ScannedMediaFile, StorageRoots, TrackInput,
};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

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

/// Inserts a track with the given starting availability.
async fn insert_track(library: &LibraryRepository, availability: Availability) -> EntityId {
    library
        .insert_track(&TrackInput::new("Cafe del Mar", availability), &[], &[])
        .await
        .unwrap()
}

/// Inserts a managed media file for a track.
async fn insert_managed(
    library: &LibraryRepository,
    track: EntityId,
    storage: StorageClass,
    rel_path: &str,
) {
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage,
            rel_path: rel_path.to_owned(),
            size: 1,
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
}

#[tokio::test]
async fn pin_reasons_accumulate_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let track = insert_track(&library, Availability::Cached).await;
    insert_managed(&library, track, StorageClass::Cache, "Artist/track.mp3").await;

    assert!(!library.track_is_pinned(track).await.unwrap());
    assert!(
        library
            .add_pin_reason(track, PinReason::UserLibrary)
            .await
            .unwrap()
    );
    assert!(
        !library
            .add_pin_reason(track, PinReason::UserLibrary)
            .await
            .unwrap()
    );
    assert!(
        library
            .add_pin_reason(track, PinReason::Playlist)
            .await
            .unwrap()
    );

    assert!(library.track_is_pinned(track).await.unwrap());
    let mut conn = db.readers().acquire().await.unwrap();
    assert_eq!(
        pin_reasons(&mut conn, track).await.unwrap(),
        [PinReason::UserLibrary, PinReason::Playlist]
    );
    drop(conn);

    // Releasing one reason keeps the track pinned.
    assert!(
        library
            .remove_pin_reason(track, PinReason::UserLibrary)
            .await
            .unwrap()
    );
    assert!(library.track_is_pinned(track).await.unwrap());
    assert!(
        !library
            .remove_pin_reason(track, PinReason::UserLibrary)
            .await
            .unwrap()
    );

    // Releasing the last reason unpins it.
    assert!(
        library
            .remove_pin_reason(track, PinReason::Playlist)
            .await
            .unwrap()
    );
    assert!(!library.track_is_pinned(track).await.unwrap());
    let stored: String =
        sqlx::query_scalar("SELECT pin_reasons FROM track_retention WHERE track_id = ?")
            .bind(track.uuid().as_bytes().as_slice())
            .fetch_one(db.writer())
            .await
            .unwrap();
    assert_eq!(stored, "[]");
}

#[tokio::test]
async fn a_cache_file_follows_the_pins_between_local_and_cached() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let track = insert_track(&library, Availability::Cached).await;
    insert_managed(&library, track, StorageClass::Cache, "Artist/track.mp3").await;

    library
        .add_pin_reason(track, PinReason::Admin)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local,
        "a pinned managed file is permanent"
    );

    library
        .remove_pin_reason(track, PinReason::Admin)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Cached,
        "an unpinned managed file is evictable"
    );
}

#[tokio::test]
async fn permanent_files_and_acquiring_tracks_keep_their_availability() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    // A library file stays local whether or not the track is pinned.
    let library_track = insert_track(&library, Availability::Local).await;
    insert_managed(
        &library,
        library_track,
        StorageClass::Library,
        "Artist/track.flac",
    )
    .await;
    library
        .remove_pin_reason(library_track, PinReason::UserLibrary)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(library_track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local
    );

    // A folder file is permanent by virtue of being in a scanned folder.
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let folder = library.add_library_folder(&music, None).await.unwrap();
    let folder_track = insert_track(&library, Availability::Local).await;
    library
        .upsert_folder_media_file(
            folder_track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: "track.flac".to_owned(),
                size: 1,
                mtime_ms: 1,
                format: None,
                codec: None,
                bitrate_kbps: None,
                sample_rate: None,
                bit_depth: None,
                channels: None,
                duration_ms: None,
                lossless: None,
                replaygain: None,
                has_embedded_art: None,
            },
        )
        .await
        .unwrap();
    library
        .remove_pin_reason(folder_track, PinReason::Policy)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(folder_track)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Local
    );

    // An acquiring track is never disturbed.
    let acquiring = insert_track(&library, Availability::Acquiring).await;
    insert_managed(
        &library,
        acquiring,
        StorageClass::Cache,
        "Artist/pending.mp3",
    )
    .await;
    library
        .add_pin_reason(acquiring, PinReason::Admin)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(acquiring)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Acquiring
    );

    // A track with no file at all keeps whatever it had.
    let fileless = insert_track(&library, Availability::Remote).await;
    library
        .add_pin_reason(fileless, PinReason::Admin)
        .await
        .unwrap();
    assert_eq!(
        library
            .get_track(fileless)
            .await
            .unwrap()
            .unwrap()
            .availability,
        Availability::Remote
    );
}

#[tokio::test]
async fn a_folder_owner_change_repins_every_track() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let owner = insert_user(&db).await;
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let mut tracks = Vec::new();
    for index in 0..2 {
        let track = insert_track(&library, Availability::Local).await;
        library
            .upsert_folder_media_file(
                track,
                &ScannedMediaFile {
                    folder_id: folder.id,
                    rel_path: format!("track{index}.flac"),
                    size: 1,
                    mtime_ms: 1,
                    format: None,
                    codec: None,
                    bitrate_kbps: None,
                    sample_rate: None,
                    bit_depth: None,
                    channels: None,
                    duration_ms: None,
                    lossless: None,
                    replaygain: None,
                    has_embedded_art: None,
                },
            )
            .await
            .unwrap();
        tracks.push(track);
    }
    // A managed track is untouched by the folder's pins.
    let managed = insert_track(&library, Availability::Cached).await;
    insert_managed(&library, managed, StorageClass::Cache, "other/track.mp3").await;

    let mut tx = db.writer().begin().await.unwrap();
    assert!(
        retention::set_folder_owner(&mut tx, folder.id, Some(owner))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    for track in &tracks {
        assert!(library.track_is_pinned(*track).await.unwrap());
        let mut conn = db.readers().acquire().await.unwrap();
        assert_eq!(
            pin_reasons(&mut conn, *track).await.unwrap(),
            [PinReason::Folder]
        );
    }
    assert!(!library.track_is_pinned(managed).await.unwrap());

    // Making the folder generic releases the folder pin again.
    let mut tx = db.writer().begin().await.unwrap();
    assert!(
        retention::set_folder_owner(&mut tx, folder.id, None)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
    for track in &tracks {
        assert!(!library.track_is_pinned(*track).await.unwrap());
        assert_eq!(
            library
                .get_track(*track)
                .await
                .unwrap()
                .unwrap()
                .availability,
            Availability::Local,
            "a folder file stays local without a pin"
        );
    }

    // Re-pinning is idempotent and reports how many tracks changed.
    let mut tx = db.writer().begin().await.unwrap();
    assert_eq!(
        repin_folder_tracks(&mut tx, folder.id, true).await.unwrap(),
        2
    );
    assert_eq!(
        repin_folder_tracks(&mut tx, folder.id, true).await.unwrap(),
        0
    );
    tx.commit().await.unwrap();

    let mut tx = db.writer().begin().await.unwrap();
    assert!(
        !retention::set_folder_owner(&mut tx, uuid::Uuid::now_v7(), Some(owner))
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn pin_sets_and_recomputation_are_direct() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let track = insert_track(&library, Availability::Cached).await;
    insert_managed(&library, track, StorageClass::Cache, "Artist/track.mp3").await;

    let mut tx = db.writer().begin().await.unwrap();
    assert!(
        set_pin_reasons(&mut tx, track, &[PinReason::UserLibrary, PinReason::Admin])
            .await
            .unwrap()
    );
    assert!(
        !set_pin_reasons(&mut tx, track, &[PinReason::UserLibrary, PinReason::Admin])
            .await
            .unwrap()
    );
    assert!(track_is_pinned(&mut tx, track).await.unwrap());
    assert_eq!(
        recompute_availability(&mut tx, track).await.unwrap(),
        Some(Availability::Local)
    );
    assert!(
        set_pin_reasons(&mut tx, track, &[]).await.unwrap(),
        "an empty set unpins the track"
    );
    assert_eq!(
        recompute_availability(&mut tx, track).await.unwrap(),
        Some(Availability::Cached)
    );
    tx.commit().await.unwrap();

    let mut conn = db.readers().acquire().await.unwrap();
    assert_eq!(
        recompute_availability(&mut conn, EntityId::new(hearthsound_core::IdPrefix::Track))
            .await
            .unwrap(),
        None,
        "an unknown track has no availability"
    );
}

#[test]
fn the_import_destination_follows_the_pin() {
    let roots = StorageRoots::new("/srv/library", "/srv/cache");
    let relative = Path::new("Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac");
    assert_eq!(
        import_destination(true, &roots, relative),
        Path::new("/srv/library/Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );
    assert_eq!(
        import_destination(false, &roots, relative),
        Path::new("/srv/cache/Energy 52/Cafe del Mar (1993)/01 - Cafe del Mar.flac")
    );
}
