//! Tests for the collection queue: every source's order, its membership in
//! the caller's personal library, the annotations each row carries, and a
//! six-thousand-track library that has to answer in one statement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use hearthsound_core::{Availability, EntityId, IdPrefix, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::{QueueCollection, QueueItem, queue};
use hearthsound_library::{
    AlbumInput, ArtistCredit, LibraryRepository, ManagedMediaFile, Origin, ScannedMediaFile,
    TrackInput,
};
use uuid::Uuid;

/// How many tracks the generated library holds.
const GENERATED: usize = 6_000;

/// The most a queue of the generated library may take.
const GENERATED_BUDGET: Duration = Duration::from_secs(10);

/// Opens a migrated database in `dir`.
async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

/// Inserts an account and returns its id.
async fn insert_user(db: &Database, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(format!("{name}-{id}"))
    .bind(name)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Inserts an album by `artist` and returns both ids.
async fn make_album(
    library: &LibraryRepository,
    artist: &str,
    title: &str,
) -> (EntityId, EntityId) {
    let album = AlbumInput::new(title, Origin::Local);
    let credits = [ArtistCredit::new(artist, "main")];
    let album_id = library.upsert_album(&album, &credits).await.unwrap();
    let artist_id = library.list_album_artists(album_id).await.unwrap()[0].id;
    (album_id, artist_id)
}

/// Inserts a track and returns its id.
async fn make_track(
    library: &LibraryRepository,
    album: Option<EntityId>,
    artist: &str,
    title: &str,
    track_number: Option<i64>,
) -> EntityId {
    let mut input = TrackInput::new(title, Origin::Local, Availability::Local);
    input.album_id = album;
    input.display_artist = Some(artist.to_owned());
    input.track_number = track_number;
    input.duration_ms = Some(180_000);
    library
        .insert_track(&input, &[ArtistCredit::new(artist, "main")], &[])
        .await
        .unwrap()
}

/// Records an explicit save.
async fn save(db: &Database, user: Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records an upload into the account's library.
async fn upload(db: &Database, user: Uuid, track: EntityId, added_at: i64) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'track', ?, 'upload', ?)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(added_at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a like at one instant.
async fn like_at(db: &Database, user: Uuid, track: EntityId, at: i64) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, 'track', ?, 'like', ?)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(at)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a dislike.
async fn dislike(db: &Database, user: Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, 'track', ?, 'dislike', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a rating.
async fn rate(db: &Database, user: Uuid, track: EntityId, rating: i64) {
    sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, 'track', ?, ?, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .bind(rating)
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a play.
async fn play(db: &Database, user: Uuid, track: EntityId) {
    sqlx::query(
        "INSERT INTO plays (id, user_id, track_id, started_at, listened_ms, audio_delivered) \
         VALUES (?, ?, ?, 0, 180000, 1)",
    )
    .bind(Uuid::now_v7().as_bytes().as_slice())
    .bind(user.as_bytes().as_slice())
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Creates a playlist and returns its id.
async fn playlist(db: &Database, owner: Uuid, public: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO playlists (id, owner_user_id, name, is_public, is_temporary, created_at, updated_at) \
         VALUES (?, ?, 'mix', ?, 0, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(owner.as_bytes().as_slice())
    .bind(public)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Adds a track to a playlist at `position`.
async fn add_to_playlist(db: &Database, playlist: Uuid, track: EntityId, position: i64) {
    sqlx::query(
        "INSERT INTO playlist_tracks (playlist_id, position, track_id, added_at) VALUES (?, ?, ?, 0)",
    )
    .bind(playlist.as_bytes().as_slice())
    .bind(position)
    .bind(track.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Puts a track's file in a folder owned by `owner`.
async fn own_in_folder(
    library: &LibraryRepository,
    dir: &Path,
    owner: Option<Uuid>,
    track: EntityId,
) {
    let folder = library.add_library_folder(dir, owner).await.unwrap();
    library
        .upsert_folder_media_file(
            track,
            &ScannedMediaFile {
                folder_id: folder.id,
                rel_path: format!("{}.mp3", track.uuid()),
                size: 1_000,
                mtime_ms: 0,
                format: Some("mpeg".to_owned()),
                codec: Some("mp3".to_owned()),
                bitrate_kbps: Some(320),
                sample_rate: Some(44_100),
                bit_depth: None,
                channels: Some(2),
                duration_ms: Some(180_000),
                lossless: Some(false),
                replaygain: None,
                has_embedded_art: Some(false),
            },
        )
        .await
        .unwrap();
}

/// Adds a managed library file to a track, so the row projects a file.
async fn attach_managed(library: &LibraryRepository, track: EntityId, rel_path: &str) {
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: rel_path.to_owned(),
            size: 1_000,
            mtime_ms: 0,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(180_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: Some(false),
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
}

/// Returns the track ids of a queue, in order.
fn ids(items: &[QueueItem]) -> Vec<EntityId> {
    items.iter().map(|item| item.track_id).collect()
}

/// The library the source tests share.
struct Seeded {
    /// The migrated database.
    db: Database,
    /// The account whose queue is read.
    user: Uuid,
    /// Another account, whose collections must never appear.
    other: Uuid,
    /// A saved track on the first album, first in album order.
    alpha_one: EntityId,
    /// A liked track on the first album, second in album order and played.
    alpha_two: EntityId,
    /// A saved track on the second album.
    bravo_one: EntityId,
    /// An uploaded track.
    upload_one: EntityId,
    /// A newer uploaded track on the first album.
    upload_two: EntityId,
    /// A track in the user's own folder, unplayed.
    folder_track: EntityId,
    /// A track on another user's private playlist and folder.
    other_track: EntityId,
    /// A track in a generic folder nobody owns.
    generic_track: EntityId,
    /// The user's playlist, holding two tracks in reverse library order.
    mix: Uuid,
    /// Another user's private playlist.
    other_mix: Uuid,
    /// Another user's public playlist.
    public_mix: Uuid,
    /// The first album.
    alpha_album: EntityId,
    /// The artist of the first album.
    alpha_artist: EntityId,
    /// An album only another user's library holds.
    other_album: EntityId,
}

/// Builds the shared library.
async fn seed(dir: &Path) -> Seeded {
    let db = migrated(dir).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;
    let other = insert_user(&db, "other").await;

    let (alpha_album, alpha_artist) = make_album(&library, "Alpha", "Alpha Album").await;
    let alpha_one = make_track(&library, Some(alpha_album), "Alpha", "Alpha One", Some(1)).await;
    let alpha_two = make_track(&library, Some(alpha_album), "Alpha", "Alpha Two", Some(2)).await;
    attach_managed(&library, alpha_one, "alpha/one.flac").await;
    attach_managed(&library, alpha_two, "alpha/two.flac").await;
    save(&db, user, alpha_one).await;
    like_at(&db, user, alpha_two, 20).await;
    rate(&db, user, alpha_two, 4).await;
    play(&db, user, alpha_two).await;
    play(&db, user, alpha_two).await;

    let (bravo_album, _) = make_album(&library, "Bravo", "Bravo Album").await;
    let bravo_one = make_track(&library, Some(bravo_album), "Bravo", "Bravo One", Some(1)).await;
    attach_managed(&library, bravo_one, "bravo/one.flac").await;
    save(&db, user, bravo_one).await;
    dislike(&db, user, bravo_one).await;

    let (upload_album, _) = make_album(&library, "Uploader", "Upload Album").await;
    let upload_one = make_track(&library, None, "Uploader", "Zulu Upload", None).await;
    let upload_two = make_track(
        &library,
        Some(upload_album),
        "Uploader",
        "Yankee Upload",
        Some(1),
    )
    .await;
    attach_managed(&library, upload_one, "uploads/one.flac").await;
    attach_managed(&library, upload_two, "uploads/two.flac").await;
    upload(&db, user, upload_one, 7).await;
    upload(&db, user, upload_two, 9).await;

    let (folder_album, _) = make_album(&library, "Folder", "Folder Album").await;
    let folder_track = make_track(
        &library,
        Some(folder_album),
        "Folder",
        "Mike Folder",
        Some(1),
    )
    .await;
    let owned = dir.join("owned");
    std::fs::create_dir_all(&owned).unwrap();
    own_in_folder(&library, &owned, Some(user), folder_track).await;

    let (other_album, _) = make_album(&library, "Other", "Other Album").await;
    let other_track =
        make_track(&library, Some(other_album), "Other", "Oscar Other", Some(1)).await;
    let others = dir.join("others");
    std::fs::create_dir_all(&others).unwrap();
    own_in_folder(&library, &others, Some(other), other_track).await;

    let generic_track = make_track(&library, None, "Generic", "Golf Generic", None).await;
    let generic = dir.join("generic");
    std::fs::create_dir_all(&generic).unwrap();
    own_in_folder(&library, &generic, None, generic_track).await;

    let mix = playlist(&db, user, false).await;
    add_to_playlist(&db, mix, alpha_two, 0).await;
    add_to_playlist(&db, mix, bravo_one, 1).await;

    let other_mix = playlist(&db, other, false).await;
    add_to_playlist(&db, other_mix, other_track, 0).await;

    let public_mix = playlist(&db, other, true).await;
    add_to_playlist(&db, public_mix, other_track, 0).await;

    Seeded {
        db,
        user,
        other,
        alpha_one,
        alpha_two,
        bravo_one,
        upload_one,
        upload_two,
        folder_track,
        other_track,
        generic_track,
        mix,
        other_mix,
        public_mix,
        alpha_album,
        alpha_artist,
        other_album,
    }
}

/// The whole personal library is queued in the Songs view's order, and no
/// other account's playlist or unowned folder content appears.
#[tokio::test]
async fn the_songs_queue_lists_the_personal_library_in_sort_order() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(&mut conn, seeded.user, QueueCollection::Songs)
        .await
        .unwrap()
        .expect("songs always reads");

    // Sorted by the folded title: alpha one, alpha two, bravo one, mike
    // folder, yankee upload, zulu upload.
    assert_eq!(
        ids(&items),
        vec![
            seeded.alpha_one,
            seeded.alpha_two,
            seeded.bravo_one,
            seeded.folder_track,
            seeded.upload_two,
            seeded.upload_one,
        ]
    );
    for absent in [seeded.other_track, seeded.generic_track] {
        assert!(
            !ids(&items).contains(&absent),
            "{absent} belongs to nobody's library the account holds"
        );
    }

    let alpha_one = items.first().unwrap();
    assert_eq!(alpha_one.title, "Alpha One");
    assert_eq!(alpha_one.artist.as_deref(), Some("Alpha"));
    assert_eq!(alpha_one.album_id, Some(seeded.alpha_album));
    assert_eq!(alpha_one.album.as_deref(), Some("Alpha Album"));
    assert_eq!(alpha_one.duration_ms, Some(180_000));
    assert_eq!(alpha_one.availability, Availability::Local);
    assert!(!alpha_one.liked);
    assert!(!alpha_one.disliked);
    assert_eq!(alpha_one.rating, None);

    let alpha_two = items.get(1).unwrap();
    assert!(alpha_two.liked);
    assert_eq!(alpha_two.rating, Some(4));

    let bravo_one = items.get(2).unwrap();
    assert!(bravo_one.disliked);

    // The scope is the caller's, so the other account sees only its own
    // folder track.
    let others = queue(&mut conn, seeded.other, QueueCollection::Songs)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids(&others), vec![seeded.other_track]);
}

/// The liked source is the caller's likes, most recently liked first, with the
/// like and rating the rows carry.
#[tokio::test]
async fn the_liked_queue_lists_likes_most_recent_first() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    like_at(&seeded.db, seeded.user, seeded.folder_track, 30).await;
    like_at(&seeded.db, seeded.other, seeded.other_track, 40).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(&mut conn, seeded.user, QueueCollection::Liked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ids(&items),
        vec![seeded.folder_track, seeded.alpha_two],
        "the newest like comes first"
    );
    assert!(items.iter().all(|item| item.liked));
    assert!(items.iter().all(|item| !item.disliked));
    assert_eq!(items[1].rating, Some(4), "the rating is the queue row's");
}

/// The uploads source is what the caller uploaded, newest first.
#[tokio::test]
async fn the_uploads_queue_lists_uploads_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(&mut conn, seeded.user, QueueCollection::Uploads)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ids(&items), vec![seeded.upload_two, seeded.upload_one]);

    let others = queue(&mut conn, seeded.other, QueueCollection::Uploads)
        .await
        .unwrap()
        .unwrap();
    assert!(others.is_empty(), "the other account uploaded nothing");
}

/// A playlist queues in its own position order, and only when the caller owns
/// it or it is public.
#[tokio::test]
async fn the_playlist_queue_uses_playlist_order_and_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Playlist(EntityId::from_uuid(IdPrefix::Playlist, seeded.mix)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        ids(&items),
        vec![seeded.alpha_two, seeded.bravo_one],
        "the playlist's own order, not the library's"
    );

    let hidden = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Playlist(EntityId::from_uuid(IdPrefix::Playlist, seeded.other_mix)),
    )
    .await
    .unwrap();
    assert!(
        hidden.is_none(),
        "another account's private playlist is hidden"
    );

    let public = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Playlist(EntityId::from_uuid(IdPrefix::Playlist, seeded.public_mix)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ids(&public), vec![seeded.other_track]);

    let unknown = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Playlist(EntityId::new(IdPrefix::Playlist)),
    )
    .await
    .unwrap();
    assert!(unknown.is_none(), "an unknown playlist is missing");
}

/// An album queues in disc and track order when the caller's library holds it,
/// and is missing when it does not.
#[tokio::test]
async fn the_album_queue_uses_album_order_and_library_membership() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Album(seeded.alpha_album),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ids(&items), vec![seeded.alpha_one, seeded.alpha_two]);

    let missing = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Album(seeded.other_album),
    )
    .await
    .unwrap();
    assert!(
        missing.is_none(),
        "an album only another account's library holds is missing"
    );

    let unknown = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Album(EntityId::new(IdPrefix::Album)),
    )
    .await
    .unwrap();
    assert!(unknown.is_none());
}

/// An artist's queue puts the played tracks first, by play count, then the
/// rest in album order; `artist-top` keeps only the played ones.
#[tokio::test]
async fn the_artist_queue_puts_played_tracks_first() {
    let dir = tempfile::tempdir().unwrap();
    let seeded = seed(dir.path()).await;
    let mut conn = seeded.db.readers().acquire().await.unwrap();

    let items = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Artist(seeded.alpha_artist),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        ids(&items),
        vec![seeded.alpha_two, seeded.alpha_one],
        "two plays put Alpha Two first, then the unplayed track"
    );

    let top = queue(
        &mut conn,
        seeded.user,
        QueueCollection::ArtistTop(seeded.alpha_artist),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(ids(&top), vec![seeded.alpha_two]);

    let hidden = queue(
        &mut conn,
        seeded.user,
        QueueCollection::Artist(EntityId::new(IdPrefix::Artist)),
    )
    .await
    .unwrap();
    assert!(hidden.is_none());
}

/// A six-thousand-track library queues completely, in sort order, in one read.
#[tokio::test]
async fn a_six_thousand_track_library_queues_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;
    let (album, _) = make_album(&library, "Bulk", "Bulk Album").await;

    // The inserts run backwards, so only the query's own sort can produce the
    // ascending order the assertion checks.
    let mut tracks = Vec::with_capacity(GENERATED);
    for index in (0..GENERATED).rev() {
        let title = format!("Bulk Track {index:05}");
        let mut input = TrackInput::new(&title, Origin::Local, Availability::Local);
        input.album_id = Some(album);
        input.display_artist = Some("Bulk Artist".to_owned());
        input.track_number = Some(i64::try_from(index).unwrap());
        input.duration_ms = Some(200_000);
        tracks.push(
            library
                .insert_track(&input, &[ArtistCredit::new("Bulk Artist", "main")], &[])
                .await
                .unwrap(),
        );
    }
    for track in &tracks {
        save(&db, user, *track).await;
    }

    let mut conn = db.readers().acquire().await.unwrap();
    let started = Instant::now();
    let items = queue(&mut conn, user, QueueCollection::Songs)
        .await
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    println!("queued {} tracks in {elapsed:?}", items.len());

    assert_eq!(items.len(), GENERATED);
    for (index, item) in items.iter().enumerate() {
        assert_eq!(item.title, format!("Bulk Track {index:05}"));
    }
    assert_eq!(items.first().unwrap().track_id, tracks[GENERATED - 1]);
    assert_eq!(items.last().unwrap().track_id, tracks[0]);
    assert!(
        elapsed < GENERATED_BUDGET,
        "a queue of {GENERATED} tracks took {elapsed:?}"
    );
}
