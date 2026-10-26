//! Tests for the identity rule's consolidation and the one-time pass that
//! applies it to rows which already exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_jobs::{JobQueue, JobState, QueueOptions};
use hearthsound_library::identity::{
    REIDENTITY_JOB_KIND, REIDENTITY_PASS, ReidentityJobHandler, consolidate_albums,
    consolidate_artists, enqueue_if_pending, maintenance_pending, mark_maintenance_complete,
};
use hearthsound_library::locks::FolderLocks;
use hearthsound_library::scan::{LibraryScanner, ScanRequest};
use hearthsound_library::{LibraryRepository, sort_key};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use uuid::Uuid;

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Builds a scanner over a database, with the built-in settings.
async fn scanner_for(db: &Database) -> Arc<LibraryScanner> {
    let settings = Settings::load(
        db.writer().clone(),
        SettingsRegistry::builtin(),
        SettingSources::default(),
    )
    .await
    .unwrap();
    Arc::new(LibraryScanner::new(
        db,
        Arc::new(EventBus::new()),
        settings,
        Arc::new(FolderLocks::new()),
    ))
}

/// Inserts an account and returns its id.
async fn insert_user(db: &Database) -> Uuid {
    let id = Uuid::now_v7();
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

/// Inserts an artist row directly, which is how two rows for one act look.
async fn insert_artist(db: &Database, name: &str, mbid: Option<&str>) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO artists (id, name, sort_name, mb_artist_id, origin, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'local', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(name)
    .bind(sort_key(name))
    .bind(mbid)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts an `external_ids` alias for an entity.
async fn insert_alias(db: &Database, namespace: &str, value: &str, kind: &str, entity: Uuid) {
    sqlx::query(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) VALUES (?, ?, ?, ?)",
    )
    .bind(namespace)
    .bind(value)
    .bind(kind)
    .bind(entity.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Inserts an album row credited at position 0 to `primary`.
async fn insert_album(
    db: &Database,
    title: &str,
    year: Option<i64>,
    mbid: Option<&str>,
    primary: Uuid,
) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO albums (id, title, sort_title, display_artist, year, mb_release_id, origin, \
                             created_at, updated_at) \
         VALUES (?, ?, ?, NULL, ?, ?, 'local', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(title)
    .bind(sort_key(title))
    .bind(year)
    .bind(mbid)
    .execute(db.writer())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO album_artists (album_id, artist_id, position, role) VALUES (?, ?, 0, 'main')",
    )
    .bind(id.as_bytes().as_slice())
    .bind(primary.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts a track on `album`, credited to `credits` in order.
async fn insert_track(db: &Database, title: &str, album: Uuid, credits: &[Uuid]) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, origin, availability, \
                             created_at, updated_at) \
         VALUES (?, ?, ?, ?, NULL, 'local', 'local', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(album.as_bytes().as_slice())
    .bind(title)
    .bind(sort_key(title))
    .execute(db.writer())
    .await
    .unwrap();
    for (position, artist) in credits.iter().enumerate() {
        sqlx::query(
            "INSERT INTO track_artists (track_id, artist_id, position, role) \
             VALUES (?, ?, ?, 'main')",
        )
        .bind(id.as_bytes().as_slice())
        .bind(artist.as_bytes().as_slice())
        .bind(position as i64)
        .execute(db.writer())
        .await
        .unwrap();
    }
    id
}

/// Inserts a media file row for a track.
async fn insert_media_file(db: &Database, track: Uuid, rel_path: &str) {
    sqlx::query(
        "INSERT INTO media_files (id, track_id, storage, folder_id, rel_path, size, mtime_ms, \
                                  added_at) \
         VALUES (?, ?, 'folder', NULL, ?, 1, 0, 0)",
    )
    .bind(Uuid::now_v7().as_bytes().as_slice())
    .bind(track.as_bytes().as_slice())
    .bind(rel_path)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a like, a save and a rating of one entity by one user.
async fn feedback(db: &Database, user: Uuid, kind: &str, entity: Uuid) {
    for statement in [
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, ?, ?, 'like', 0)",
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, ?, ?, 'saved', 0)",
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, ?, ?, 4, 0)",
    ] {
        sqlx::query(statement)
            .bind(user.as_bytes().as_slice())
            .bind(kind)
            .bind(entity.as_bytes().as_slice())
            .execute(db.writer())
            .await
            .unwrap();
    }
}

/// Counts the rows of a table.
async fn count(db: &Database, statement: &'static str) -> i64 {
    sqlx::query_scalar(statement)
        .fetch_one(db.readers())
        .await
        .unwrap()
}

/// A write with an explicit year, for the pass's full re-probe.
async fn write_year(
    dir: &Path,
    title: &str,
    artist: &str,
    album: &str,
    year: u32,
) -> std::path::PathBuf {
    AudioFixture::new(AudioFormat::Flac)
        .seconds(0.3)
        .file_name(format!("{title}.flac"))
        .tags(
            TrackTags::new(title, artist)
                .album(album)
                .album_artist(artist)
                .track(1, 1)
                .year(year),
        )
        .write_into(dir)
        .await
        .unwrap()
}

/// Builds a claimed `library.reidentity` job.
fn reidentity_job() -> hearthsound_jobs::Job {
    hearthsound_jobs::Job {
        id: Uuid::now_v7(),
        kind: REIDENTITY_JOB_KIND.into(),
        payload: None,
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

/// Runs the pass over a database.
async fn run_pass(db: &Arc<Database>) {
    let handler = ReidentityJobHandler::new(scanner_for(db).await, Arc::clone(db));
    hearthsound_jobs::JobHandler::handle(&handler, reidentity_job())
        .await
        .unwrap();
}

/// Rows that share a MusicBrainz id are one artist, whether the id is a column
/// or an alias.
#[tokio::test]
async fn consolidate_artists_merges_rows_sharing_a_music_brainz_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);

    let column = insert_artist(&db, "Energy 52", Some("mbid-x")).await;
    let same = insert_artist(&db, "energy 52", Some("mbid-x")).await;
    let aliased = insert_artist(&db, "Force 9", Some("mbid-y")).await;
    let alias_twin = insert_artist(&db, "force 9", None).await;
    insert_alias(&db, "mb-artist", "mbid-y", "artist", alias_twin).await;

    let merged = consolidate_artists(&db).await.unwrap();
    assert_eq!(merged, 2, "each pair collapses into one row");
    let artists = LibraryRepository::new(&db).list_artists().await.unwrap();
    assert_eq!(artists.len(), 2);
    assert!(
        artists
            .iter()
            .any(|artist| artist.name.eq_ignore_ascii_case("Energy 52"))
    );
    assert!(
        artists
            .iter()
            .any(|artist| artist.name.eq_ignore_ascii_case("Force 9")
                && artist.mb_artist_id.as_deref() == Some("mbid-y"))
    );
    // The survivor of the column pair is the older row.
    assert!(artists.iter().any(|artist| artist.id.uuid() == column));
    assert!(!artists.iter().any(|artist| artist.id.uuid() == same));
    assert!(!artists.iter().any(|artist| artist.id.uuid() == alias_twin));
    assert!(artists.iter().any(|artist| artist.id.uuid() == aliased));
}

/// Two identified artists of one name keep the untagged rows apart from both.
#[tokio::test]
async fn consolidate_artists_keeps_the_unidentified_row_when_two_identified_artists_share_a_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    insert_artist(&db, "Alex G", Some("mbid-a")).await;
    insert_artist(&db, "Alex G", Some("mbid-b")).await;
    insert_artist(&db, "Alex G", None).await;
    insert_artist(&db, "alex g", None).await;

    let merged = consolidate_artists(&db).await.unwrap();
    assert_eq!(merged, 1, "the two untagged rows are one artist");
    let artists = LibraryRepository::new(&db).list_artists().await.unwrap();
    assert_eq!(artists.len(), 3);
    assert_eq!(
        artists
            .iter()
            .filter(|artist| artist.mb_artist_id.is_none())
            .count(),
        1
    );
}

/// Merging two artists moves their credits, likes, saves and ratings.
#[tokio::test]
async fn merging_two_artists_moves_their_credits_likes_saves_and_ratings() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let user = insert_user(&db).await;
    let first = insert_artist(&db, "Energy 52", Some("mbid-x")).await;
    let second = insert_artist(&db, "energy 52", Some("mbid-x")).await;
    let album = insert_album(&db, "Cafe del Mar", Some(1993), None, first).await;
    let track = insert_track(&db, "Cafe del Mar", album, &[first, second]).await;
    insert_media_file(&db, track, "Cafe del Mar.flac").await;
    feedback(&db, user, "artist", first).await;
    feedback(&db, user, "artist", second).await;
    // Both rows credited on one album: the merge must not duplicate the credit.
    sqlx::query(
        "INSERT INTO album_artists (album_id, artist_id, position, role) VALUES (?, ?, 1, 'main')",
    )
    .bind(album.as_bytes().as_slice())
    .bind(second.as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();

    let merged = consolidate_artists(&db).await.unwrap();
    assert_eq!(merged, 1);
    assert_eq!(count(&db, "SELECT count(*) FROM artists").await, 1);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM track_artists WHERE track_id = (SELECT id FROM tracks LIMIT 1)",
        )
        .await,
        1,
        "the track lists the artist once"
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM album_artists").await,
        1,
        "the album lists the artist once"
    );
    for statement in [
        "SELECT count(*) FROM user_feedback",
        "SELECT count(*) FROM user_library",
        "SELECT count(*) FROM user_ratings",
    ] {
        assert_eq!(count(&db, statement).await, 1, "{statement}");
    }
}

/// Rows that share a release id are one album, and the id-less twin joins.
#[tokio::test]
async fn consolidate_albums_merges_rows_sharing_a_release_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let artist = insert_artist(&db, "Energy 52", None).await;
    let first = insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-r"), artist).await;
    let second = insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-r"), artist).await;
    let third = insert_album(&db, "Cafe del Mar", Some(1993), None, artist).await;

    let merged = consolidate_albums(&db).await.unwrap();
    assert_eq!(merged, 2, "the pair and the id-less twin collapse");
    let albums = LibraryRepository::new(&db)
        .list_albums_for_artist(hearthsound_core::EntityId::from_uuid(
            hearthsound_core::IdPrefix::Artist,
            artist,
        ))
        .await
        .unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].id.uuid(), first);
    assert_ne!(albums[0].id.uuid(), second);
    assert_ne!(albums[0].id.uuid(), third);
}

/// Two identified albums of one identity keep the id-less rows apart from both.
#[tokio::test]
async fn consolidate_albums_keeps_the_unidentified_row_when_two_identified_albums_share_the_identity()
 {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let artist = insert_artist(&db, "Energy 52", None).await;
    insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-a"), artist).await;
    insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-b"), artist).await;
    insert_album(&db, "Cafe del Mar", Some(1993), None, artist).await;
    insert_album(&db, "Cafe del Mar", Some(1993), None, artist).await;

    let merged = consolidate_albums(&db).await.unwrap();
    assert_eq!(merged, 1, "the two id-less rows are one album");
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, 3);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM albums WHERE mb_release_id IS NULL"
        )
        .await,
        1
    );
}

/// Merging two albums moves their tracks, aliases, likes, saves and ratings.
#[tokio::test]
async fn merging_two_albums_moves_tracks_likes_saves_ratings_and_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let user = insert_user(&db).await;
    let artist = insert_artist(&db, "Energy 52", None).await;
    let survivor = insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-r"), artist).await;
    let loser = insert_album(&db, "Cafe del Mar", Some(1993), None, artist).await;
    let track = insert_track(&db, "Cafe del Mar", loser, &[artist]).await;
    insert_media_file(&db, track, "Cafe del Mar.flac").await;
    feedback(&db, user, "album", loser).await;
    insert_alias(&db, "p:deezer", "302127", "album", loser).await;

    let merged = consolidate_albums(&db).await.unwrap();
    assert_eq!(merged, 1);
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, 1);
    let moved: Option<Vec<u8>> = sqlx::query_scalar("SELECT album_id FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(moved.unwrap(), survivor.as_bytes().to_vec());
    let aliased: Vec<u8> = sqlx::query_scalar("SELECT entity_id FROM external_ids")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(aliased, survivor.as_bytes().to_vec());
    for statement in [
        "SELECT count(*) FROM user_feedback",
        "SELECT count(*) FROM user_library",
        "SELECT count(*) FROM user_ratings",
    ] {
        assert_eq!(count(&db, statement).await, 1, "{statement}");
    }
}

/// The pass re-probes every file and records itself.
#[tokio::test]
async fn the_reidentity_job_runs_a_full_scan_and_records_itself() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_year(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2001).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();
    scanner_for(&db).await.scan_folder(folder.id).await.unwrap();
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, 1);

    // The tags change in place: only a full scan re-probes the file.
    write_year(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2010).await;
    run_pass(&db).await;

    assert_eq!(
        count(&db, "SELECT count(*) FROM albums").await,
        2,
        "the file moved to the album of its own year"
    );
    assert!(!maintenance_pending(&db, REIDENTITY_PASS).await.unwrap());
    let folder = library
        .get_library_folder(folder.id)
        .await
        .unwrap()
        .unwrap();
    assert!(folder.last_scan_at.is_some());
}

/// A pass that ran once does nothing when the job is claimed again.
#[tokio::test]
async fn the_reidentity_job_does_nothing_once_the_marker_exists() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_year(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2001).await;
    library.add_library_folder(&music, None).await.unwrap();

    run_pass(&db).await;
    let artists = count(&db, "SELECT count(*) FROM artists").await;
    let albums = count(&db, "SELECT count(*) FROM albums").await;
    let tracks = count(&db, "SELECT count(*) FROM tracks").await;

    run_pass(&db).await;
    assert_eq!(count(&db, "SELECT count(*) FROM artists").await, artists);
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, albums);
    assert_eq!(count(&db, "SELECT count(*) FROM tracks").await, tracks);
}

/// A folder the pass cannot scan leaves the pass pending.
#[tokio::test]
async fn a_failed_folder_scan_leaves_the_pass_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let good = dir.path().join("good");
    let broken = dir.path().join("broken");
    std::fs::create_dir(&good).unwrap();
    std::fs::create_dir(&broken).unwrap();
    write_year(&good, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2001).await;
    library.add_library_folder(&good, None).await.unwrap();
    library.add_library_folder(&broken, None).await.unwrap();
    std::fs::remove_dir(&broken).unwrap();
    std::fs::write(&broken, b"not a directory").unwrap();

    run_pass(&db).await;
    assert!(maintenance_pending(&db, REIDENTITY_PASS).await.unwrap());
    assert_eq!(
        count(&db, "SELECT count(*) FROM tracks").await,
        1,
        "the healthy folder was still re-evaluated"
    );
}

/// A fresh install never queues the pass.
#[tokio::test]
async fn enqueue_if_pending_skips_a_fresh_install() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let queue = JobQueue::new(&db, QueueOptions::default());

    assert!(!enqueue_if_pending(&db, &queue).await.unwrap());
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 0);

    // A folder makes it worth running, and a second call is a no-op only once
    // the marker is written.
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    LibraryRepository::new(&db)
        .add_library_folder(&music, None)
        .await
        .unwrap();
    assert!(enqueue_if_pending(&db, &queue).await.unwrap());
    assert_eq!(queue.count(JobState::Ready).await.unwrap(), 1);
    mark_maintenance_complete(&db, REIDENTITY_PASS)
        .await
        .unwrap();
    assert!(!enqueue_if_pending(&db, &queue).await.unwrap());
}

/// Two overlapping folders are re-evaluated once, by the inner one.
#[tokio::test]
async fn two_overlapping_folders_are_re_evaluated_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    let inner = music.join("inner");
    std::fs::create_dir_all(&inner).unwrap();
    write_year(&inner, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2001).await;
    library.add_library_folder(&music, None).await.unwrap();
    library.add_library_folder(&inner, None).await.unwrap();

    run_pass(&db).await;

    assert_eq!(count(&db, "SELECT count(*) FROM tracks").await, 1);
    assert_eq!(count(&db, "SELECT count(*) FROM media_files").await, 1);
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, 1);
    let started = Instant::now();
    while maintenance_pending(&db, REIDENTITY_PASS).await.unwrap() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the pass records itself"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The full pass registers its scans so `scan.progress` reports them.
#[tokio::test]
async fn the_pass_scans_through_the_normal_scan_path() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let library = LibraryRepository::new(&db);
    let music = dir.path().join("music");
    std::fs::create_dir(&music).unwrap();
    write_year(&music, "Cafe del Mar", "Energy 52", "Cafe del Mar", 2001).await;
    let folder = library.add_library_folder(&music, None).await.unwrap();

    let scanner = scanner_for(&db).await;
    let report = scanner
        .scan(&ScanRequest::folder(folder.id).full())
        .await
        .unwrap();
    assert_eq!(report[0].files_seen, 1);
}
