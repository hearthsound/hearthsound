//! Tests for the identity rule's consolidation of rows which already exist.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::identity::{consolidate_albums, consolidate_artists};
use hearthsound_library::{LibraryRepository, identity_key, sort_key};
use uuid::Uuid;

/// The MusicBrainz id of alt-J.
const ALT_J: &str = "fc7bbf00-fbaa-4736-986b-b3ac0266ca9b";

/// The MusicBrainz id of Mountain Man, which one ListenBrainz credit pairs with
/// alt-J's.
const MOUNTAIN_MAN: &str = "ecc6e248-9ec4-4150-bbd2-d2b112f0b365";

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
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
        "INSERT INTO artists (id, name, sort_name, match_key, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(name)
    .bind(sort_key(name))
    .bind(identity_key(name))
    .execute(db.writer())
    .await
    .unwrap();
    if let Some(mbid) = mbid {
        insert_alias(db, "mb-artist", mbid, "artist", id).await;
    }
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
        "INSERT INTO albums (id, title, sort_title, display_artist, year, created_at, updated_at) \
         VALUES (?, ?, ?, NULL, ?, 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(title)
    .bind(sort_key(title))
    .bind(year)
    .execute(db.writer())
    .await
    .unwrap();
    if let Some(mbid) = mbid {
        insert_alias(db, "mb-release", mbid, "album", id).await;
    }
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
        "INSERT INTO tracks (id, album_id, title, sort_title, display_artist, availability, \
                             created_at, updated_at) \
         VALUES (?, ?, ?, ?, NULL, 'local', 0, 0)",
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

/// An identified artist absorbs the untagged row of its name.
#[tokio::test]
async fn consolidate_artists_joins_an_untagged_row_to_its_identified_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);

    let identified = insert_artist(&db, "Energy 52", Some("mbid-x")).await;
    let twin = insert_artist(&db, "energy 52", None).await;

    let merged = consolidate_artists(&db).await.unwrap();
    assert_eq!(merged, 1, "the untagged twin joins its artist");
    let artists = LibraryRepository::new(&db).list_artists().await.unwrap();
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].id.uuid(), identified, "the older row survives");
    assert!(!artists.iter().any(|artist| artist.id.uuid() == twin));
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
    let unidentified: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM artists a WHERE NOT EXISTS ( \
             SELECT 1 FROM external_ids e \
              WHERE e.entity_kind = 'artist' AND e.entity_id = a.id)",
    )
    .fetch_one(db.readers())
    .await
    .unwrap();
    assert_eq!(unidentified, 1);
}

/// Merging two artists moves their credits, likes, saves and ratings.
#[tokio::test]
async fn merging_two_artists_moves_their_credits_likes_saves_and_ratings() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let user = insert_user(&db).await;
    let first = insert_artist(&db, "Energy 52", Some("mbid-x")).await;
    let second = insert_artist(&db, "energy 52", None).await;
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

/// An album identified by a release id takes in its id-less twin.
#[tokio::test]
async fn consolidate_albums_joins_an_untagged_twin_to_its_identified_album() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(migrated(dir.path()).await);
    let artist = insert_artist(&db, "Energy 52", None).await;
    let identified = insert_album(&db, "Cafe del Mar", Some(1993), Some("mbid-r"), artist).await;
    let twin = insert_album(&db, "Cafe del Mar", Some(1993), None, artist).await;

    let merged = consolidate_albums(&db).await.unwrap();
    assert_eq!(merged, 1, "the id-less twin joins its album");
    let albums = LibraryRepository::new(&db)
        .list_albums_for_artist(hearthsound_core::EntityId::from_uuid(
            hearthsound_core::IdPrefix::Artist,
            artist,
        ))
        .await
        .unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].id.uuid(), identified);
    assert_ne!(albums[0].id.uuid(), twin);
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
            "SELECT count(*) FROM albums a WHERE NOT EXISTS ( \
                 SELECT 1 FROM external_ids e \
                  WHERE e.entity_kind = 'album' AND e.entity_id = a.id \
                    AND e.namespace = 'mb-release')",
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

    // The provider alias makes the loser identified, so the name rule leaves the
    // two rows apart: an identifier conflict is what joins them, and the older
    // row survives.
    let mut tx = db.writer().begin().await.unwrap();
    let merged = hearthsound_library::entities::merge(
        &mut tx,
        hearthsound_core::EntityKind::Album,
        loser,
        survivor,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(merged, survivor, "the older row survives");
    assert_eq!(count(&db, "SELECT count(*) FROM albums").await, 1);
    let moved: Option<Vec<u8>> = sqlx::query_scalar("SELECT album_id FROM tracks")
        .fetch_one(db.readers())
        .await
        .unwrap();
    assert_eq!(moved.unwrap(), survivor.as_bytes().to_vec());
    let aliased: Vec<u8> =
        sqlx::query_scalar("SELECT entity_id FROM external_ids WHERE namespace = 'p:deezer'")
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

/// A second MusicBrainz id on one artist is dropped, not attached.
///
/// ListenBrainz reports a collaboration as one credit carrying every artist's
/// MusicBrainz id, so the popularity path once attached both to one row: the
/// artist's biography and portrait then came from whichever id was read first,
/// which is how alt-J's page showed Mountain Man's. One artist is one
/// MusicBrainz entity, so the row keeps the id it has.
#[tokio::test]
async fn a_second_musicbrainz_artist_id_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let artist = insert_artist(&db, "alt-J", Some(ALT_J)).await;

    let mut conn = db.writer().acquire().await.unwrap();
    let conflicts = hearthsound_library::external_ids::attach(
        &mut conn,
        hearthsound_core::EntityKind::Artist,
        artist,
        &[hearthsound_core::providers::ExternalId {
            namespace: "mb-artist".to_owned(),
            value: MOUNTAIN_MAN.to_owned(),
        }],
    )
    .await
    .unwrap();

    assert!(
        conflicts.is_empty(),
        "a second id is dropped, not reported as a conflict to merge"
    );
    let ids = hearthsound_library::external_ids::ids_of(
        &mut conn,
        hearthsound_core::EntityKind::Artist,
        artist,
    )
    .await
    .unwrap();
    assert_eq!(ids.len(), 1, "the row holds one identifier, got {ids:?}");
    assert_eq!(ids[0].value, ALT_J, "the id it already held is kept");
}

/// A provider's credit for an artist row spelled with a Unicode hyphen finds it.
///
/// `alt-J` and `alt\u{2010}J` are the same name: the identity key folds the
/// hyphen-like characters to `-`, so a Deezer credit does not open a second page
/// for the act the MusicBrainz id already names.
#[tokio::test]
async fn a_provider_credit_finds_the_unicode_hyphen_artist() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let artist = insert_artist(&db, "alt\u{2010}J", Some(ALT_J)).await;

    let mut input = hearthsound_library::ArtistInput::new("alt-J");
    input.ids = vec![hearthsound_core::providers::ExternalId {
        namespace: "p:deezer".to_owned(),
        value: "1606315".to_owned(),
    }];
    let mut conn = db.writer().acquire().await.unwrap();
    let resolved = hearthsound_library::artists::upsert_artist(&mut conn, &input)
        .await
        .unwrap();

    assert_eq!(
        resolved.uuid(),
        artist,
        "the credit resolves to the same row"
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artists")
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(rows, 1, "no second row was created");
    let ids = hearthsound_library::external_ids::ids_of(
        &mut conn,
        hearthsound_core::EntityKind::Artist,
        artist,
    )
    .await
    .unwrap();
    assert!(
        ids.iter()
            .any(|id| id.namespace == "p:deezer" && id.value == "1606315"),
        "the provider id was attached to the existing row, got {ids:?}"
    );
}
