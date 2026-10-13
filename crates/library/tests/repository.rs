//! Tests for the library repository: local entity dedupe, credit and genre
//! writes, the preferred-file rule, and library folder management.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{Availability, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::{
    AlbumInput, ArtistCredit, ArtistInput, LibraryRepository, ManagedMediaFile, MediaFileInput,
    Origin, ScannedMediaFile, TrackInput, sort_key,
};

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Inserts a bare user row and returns its id.
async fn insert_user(db: &Database) -> uuid::Uuid {
    let id = uuid::Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("user-{id}"))
    .bind("User")
    .execute(db.writer())
    .await
    .unwrap();
    id
}

fn track_input(title: &str) -> TrackInput {
    TrackInput::new(title, Origin::Local, Availability::Local)
}

#[tokio::test]
async fn artists_dedupe_by_mbid_then_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let with_mbid = ArtistInput::new("Energy 52").mb_artist_id("mbid-energy-52");
    let first = library.upsert_artist(&with_mbid).await.unwrap();
    let again = library.upsert_artist(&with_mbid).await.unwrap();
    assert_eq!(first, again, "the same MBID must not insert twice");

    // A differently spelled name with the same MBID still merges.
    let respelled = ArtistInput::new("Energy52").mb_artist_id("mbid-energy-52");
    assert_eq!(library.upsert_artist(&respelled).await.unwrap(), first);

    // Without an MBID the name is compared case-insensitively.
    let plain = library
        .upsert_artist(&ArtistInput::new("Cafe del Mar"))
        .await
        .unwrap();
    let same = library
        .upsert_artist(&ArtistInput::new("cafe DEL mar"))
        .await
        .unwrap();
    assert_eq!(plain, same);

    assert_eq!(library.list_artists().await.unwrap().len(), 2);

    // Two artists that differ only by MBID stay distinct.
    let other = library
        .upsert_artist(&ArtistInput::new("Energy 52").mb_artist_id("mbid-other"))
        .await
        .unwrap();
    assert_ne!(other, first);
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

#[tokio::test]
async fn albums_dedupe_by_release_id_then_by_title_and_primary_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let credits = [ArtistCredit::new("Energy 52", "main")];
    let mut album = AlbumInput::new("Cafe del Mar", Origin::Local);
    album.mb_release_id = Some("mbid-release".to_owned());
    album.year = Some(1993);
    let first = library.upsert_album(&album, &credits).await.unwrap();
    assert_eq!(library.upsert_album(&album, &credits).await.unwrap(), first);

    // The same title and primary artist, no release id, merges too.
    let plain = AlbumInput::new("Cafe del Mar", Origin::Local);
    assert_eq!(library.upsert_album(&plain, &credits).await.unwrap(), first);

    // The same title by another artist is a different album.
    let other_credits = [ArtistCredit::new("Someone Else", "main")];
    let other = library.upsert_album(&plain, &other_credits).await.unwrap();
    assert_ne!(other, first);

    let stored = library.get_album(first).await.unwrap().unwrap();
    assert_eq!(stored.year, Some(1993));
    assert_eq!(stored.mb_release_id.as_deref(), Some("mbid-release"));
    let artists = library.list_album_artists(first).await.unwrap();
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].name, "Energy 52");
}

#[tokio::test]
async fn tracks_store_credits_and_case_folded_genres() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let credits = [
        ArtistCredit::new("Energy 52", "main"),
        ArtistCredit::new("Guest Voice", "featured"),
    ];
    let genres = [
        "Trance".to_owned(),
        "trance".to_owned(),
        "Ambient".to_owned(),
    ];
    let track = library
        .insert_track(&track_input("Cafe del Mar"), &credits, &genres)
        .await
        .unwrap();

    let stored = library.get_track(track).await.unwrap().unwrap();
    assert_eq!(stored.title, "Cafe del Mar");
    assert_eq!(stored.sort_title, "cafe del mar");
    assert_eq!(stored.origin, Origin::Local);
    assert_eq!(stored.availability, Availability::Local);

    let artists = library.list_track_artists(track).await.unwrap();
    assert_eq!(
        artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["Energy 52", "Guest Voice"]
    );
    let credits = library.list_track_credits(track).await.unwrap();
    assert_eq!(credits[0].role, "main");
    assert_eq!(credits[1].role, "featured");
    assert_eq!(credits[1].position, 1);

    let genres = library.list_track_genres(track).await.unwrap();
    assert_eq!(
        genres.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        ["ambient", "trance"],
        "case variants must fold into one row"
    );

    // Rewriting replaces the credits and genres rather than appending.
    let mut updated = track_input("Sunset");
    updated.track_number = Some(2);
    let replaced = [ArtistCredit::new("Someone Else", "main")];
    assert!(
        library
            .update_track(track, &updated, &replaced, &["house".to_owned()])
            .await
            .unwrap()
    );
    assert_eq!(
        library.get_track(track).await.unwrap().unwrap().title,
        "Sunset"
    );
    assert_eq!(library.list_track_artists(track).await.unwrap().len(), 1);
    assert_eq!(
        library.list_track_genres(track).await.unwrap()[0].name,
        "house"
    );
    assert_eq!(library.list_artists().await.unwrap().len(), 3);
}

#[tokio::test]
async fn the_preferred_file_is_the_permanent_managed_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let track = library
        .insert_track(&track_input("Cafe del Mar"), &[], &[])
        .await
        .unwrap();

    // A cache copy is preferred while it is the only file.
    let cache = library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Cache,
            rel_path: "cache/track.mp3".to_owned(),
            size: 1,
            mtime_ms: 1,
            format: Some("mpeg".to_owned()),
            codec: Some("mp3".to_owned()),
            bitrate_kbps: Some(320),
            sample_rate: Some(44_100),
            bit_depth: None,
            channels: Some(2),
            duration_ms: Some(1_000),
            lossless: Some(false),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        cache
    );

    // A folder copy outranks the evictable cache copy.
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let folder_row = library.add_library_folder(&music, None).await.unwrap();
    let folder = library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder_row.id,
                rel_path: "Artist/Album/track.flac".to_owned(),
                size: 2,
                mtime_ms: 2,
                format: Some("flac".to_owned()),
                codec: Some("flac".to_owned()),
                bitrate_kbps: None,
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                duration_ms: Some(1_000),
                lossless: Some(true),
                has_embedded_art: Some(true),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        folder
    );

    // A permanent managed copy outranks the folder copy.
    let library_file = library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: "Artist/Album/01 - Cafe del Mar.flac".to_owned(),
            size: 3,
            mtime_ms: 3,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(1_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(true),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        library_file
    );

    // Exactly one file is preferred at a time.
    let files = library.list_media_files_for_track(track).await.unwrap();
    assert_eq!(files.len(), 3);
    assert_eq!(files.iter().filter(|file| file.is_preferred).count(), 1);

    // Deleting the winner hands the flag to the next rank.
    assert!(library.delete_media_file(library_file).await.unwrap());
    assert_eq!(
        library
            .preferred_media_file(track)
            .await
            .unwrap()
            .unwrap()
            .id,
        folder
    );
}

#[tokio::test]
async fn media_file_input_resolves_paths_by_storage_class() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let track = library
        .insert_track(&track_input("Cafe del Mar"), &[], &[])
        .await
        .unwrap();
    let id = library
        .insert_media_file(&MediaFileInput {
            track_id: track,
            storage: StorageClass::Cache,
            folder_id: None,
            rel_path: "Artist/Album/track.MP3".to_owned(),
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

    let file = library.get_media_file(id).await.unwrap().unwrap();
    assert_eq!(file.suffix(), "mp3");
    let roots = hearthsound_library::StorageRoots::new("/srv/library", "/srv/cache");
    assert_eq!(
        file.absolute_path(None, &roots),
        Path::new("/srv/cache/Artist/Album/track.MP3")
    );
}

#[tokio::test]
async fn library_folders_canonicalize_rename_and_reassociate() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);

    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    let added = library.add_library_folder(&music, None).await.unwrap();
    assert_eq!(added.name, "music");
    assert!(added.path.is_absolute());
    assert_eq!(added.owner_user_id, None);
    assert!(added.watch);
    assert_eq!(added.last_scan_at, None);

    // The same directory through a dotted spelling returns the same row.
    let dotted = music.join(".");
    let owner = insert_user(&db).await;
    let again = library
        .add_library_folder(&dotted, Some(owner))
        .await
        .unwrap();
    assert_eq!(again.id, added.id);
    assert_eq!(
        again.owner_user_id, None,
        "the owner is not applied on a re-add"
    );

    assert!(
        library
            .rename_library_folder(added.id, "Music")
            .await
            .unwrap()
    );
    assert!(
        library
            .set_library_folder_owner(added.id, Some(owner))
            .await
            .unwrap()
    );
    let stored = library.get_library_folder(added.id).await.unwrap().unwrap();
    assert_eq!(stored.name, "Music");
    assert_eq!(stored.owner_user_id, Some(owner));
    assert_eq!(library.list_library_folders().await.unwrap().len(), 1);

    // A file that is not a directory is rejected.
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    assert!(library.add_library_folder(&file, None).await.is_err());
    assert!(
        library
            .add_library_folder(&dir.path().join("missing"), None)
            .await
            .is_err()
    );

    assert!(
        !library
            .rename_library_folder(uuid::Uuid::now_v7(), "x")
            .await
            .unwrap()
    );
}

#[test]
fn sort_key_folds_case_and_whitespace() {
    assert_eq!(sort_key("  The  Beatles "), "the beatles");
    assert_eq!(sort_key("CAFE\u{a0}DEL MAR"), "cafe del mar");
    assert_eq!(sort_key("   "), "");
}
