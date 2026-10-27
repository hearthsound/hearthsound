//! Tests for the batch state read behind the `hearthsound` member: library
//! membership, feedback, rating, and the availability an album aggregates from
//! its tracks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use hearthsound_core::{Availability, EntityId, EntityKind};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::entity_facts;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, TrackInput};
use uuid::Uuid;

async fn migrated(dir: &Path) -> Database {
    let db = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    db.migrate(&MIGRATOR, &dir.join("backups")).await.unwrap();
    db
}

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

/// Returns the literal an `entity_kind` column stores.
fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}

/// Records an explicit save into the account's library.
async fn save(db: &Database, user: Uuid, kind: EntityKind, entity: EntityId) {
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, ?, ?, 'saved', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(entity.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a like.
async fn like(db: &Database, user: Uuid, kind: EntityKind, entity: EntityId) {
    sqlx::query(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, ?, ?, 'like', 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(entity.uuid().as_bytes().as_slice())
    .execute(db.writer())
    .await
    .unwrap();
}

/// Records a rating.
async fn rate(db: &Database, user: Uuid, kind: EntityKind, entity: EntityId, rating: i64) {
    sqlx::query(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, ?, ?, ?, 0)",
    )
    .bind(user.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(entity.uuid().as_bytes().as_slice())
    .bind(rating)
    .execute(db.writer())
    .await
    .unwrap();
}

/// A liked, rated, saved album whose only track is local reports all of it, and
/// an id that names no row is absent from the answer.
#[tokio::test]
async fn an_album_reports_its_membership_feedback_rating_and_availability() {
    let dir = tempfile::tempdir().unwrap();
    let db = migrated(dir.path()).await;
    let library = LibraryRepository::new(&db);
    let user = insert_user(&db, "listener").await;

    let credits = [ArtistCredit::new("Fixture Artist", "main")];
    let album_id = library
        .upsert_album(&AlbumInput::new("Fixture Album"), &credits)
        .await
        .unwrap();
    let mut track = TrackInput::new("Fixture Track", Availability::Local);
    track.album_id = Some(album_id);
    library.insert_track(&track, &credits, &[]).await.unwrap();

    save(&db, user, EntityKind::Album, album_id).await;
    like(&db, user, EntityKind::Album, album_id).await;
    rate(&db, user, EntityKind::Album, album_id, 4).await;

    let unknown = Uuid::now_v7();
    let facts = entity_facts(&db, user, EntityKind::Album, &[album_id.uuid(), unknown])
        .await
        .unwrap();

    let album = facts
        .get(&album_id.uuid())
        .expect("the stored album has facts");
    assert!(album.in_library, "the album was saved into the library");
    assert!(album.liked, "the album was liked");
    assert!(!album.disliked);
    assert_eq!(album.rating, Some(4));
    assert_eq!(
        album.availability,
        Availability::Local,
        "the album's only track is local"
    );
    assert!(
        !facts.contains_key(&unknown),
        "an id that names no row is absent"
    );
}
