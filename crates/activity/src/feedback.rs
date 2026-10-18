//! Likes and the personal-library pin that follows them.
//!
//! `star`/`unstar` write a `user_feedback` row with `value = 'like'`. A like is
//! what makes a track part of a user's personal library, so the same
//! transaction keeps the `user_library` pin reason in step: a track is pinned
//! while any user likes it or likes the album it belongs to, or while any
//! `user_library` row names it or its album. An artist like is recorded and
//! pins nothing, because an artist has no file of its own.
//!
//! Nothing here acquires or moves a file: the API layer asks the acquisition
//! layer for what a user likes, and the storage maintainer moves the files the
//! pins imply.
//!
//! A dislike is the other `user_feedback` value. It pins nothing and never
//! replaces a like here: a rating of one counts as a dislike only when the
//! user does not like the entity (`subsonic.rating_one_is_dislike`).

use hearthsound_core::{EntityId, EntityKind, IdPrefix, PinReason};
use hearthsound_library::retention;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::ActivityError;

/// Records that a user likes an entity.
///
/// Liking something already liked refreshes the instant; liking something
/// disliked replaces the dislike.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that likes the entity.
/// - `kind`: the kind of entity that is liked.
/// - `entity_id`: the entity's id.
/// - `at`: when the like happened, in unix milliseconds.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be written and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub async fn like(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
    at: i64,
) -> Result<(), ActivityError> {
    sqlx::query!(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, ?, ?, 'like', ?) \
         ON CONFLICT (user_id, entity_kind, entity_id) \
         DO UPDATE SET value = 'like', at = excluded.at",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
        at,
    )
    .execute(&mut *conn)
    .await?;
    refresh_pins(conn, kind, entity_id).await
}

/// Removes a user's like of an entity.
///
/// A dislike is left alone: only the like is removed, so a track a user
/// disliked and then liked is disliked again rather than unrated.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that no longer likes the entity.
/// - `kind`: the kind of entity that was liked.
/// - `entity_id`: the entity's id.
///
/// # Returns
///
/// `true` when a like was removed, `false` when the user had not liked it.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be removed and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub async fn unlike(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<bool, ActivityError> {
    let removed = sqlx::query!(
        "DELETE FROM user_feedback \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND value = 'like'",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if removed > 0 {
        refresh_pins(conn, kind, entity_id).await?;
    }
    Ok(removed > 0)
}

/// Saves an entity into a user's personal library.
///
/// A saved entity is a `user_library` row whose origin is `saved`, so it is the
/// user's own decision rather than an upload's side effect; saving something
/// already saved refreshes the instant. The personal-library pin of every track
/// the save affects is refreshed in the same transaction, which is what makes a
/// saved album pin its member tracks.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that saves the entity.
/// - `kind`: the kind of entity that is saved.
/// - `entity_id`: the entity's id.
/// - `at`: when the save happened, in unix milliseconds.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be written and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub(crate) async fn save(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
    at: i64,
) -> Result<(), ActivityError> {
    sqlx::query!(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, ?, ?, 'saved', ?) \
         ON CONFLICT (user_id, entity_kind, entity_id, origin) \
         DO UPDATE SET added_at = excluded.added_at",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
        at,
    )
    .execute(&mut *conn)
    .await?;
    refresh_pins(conn, kind, entity_id).await
}

/// Removes an entity from a user's personal library.
///
/// Only the rows a save wrote are removed: a row whose origin is `upload` stays,
/// because uploading a file is also a decision to keep it. The personal-library
/// pin of every track the removal affects is refreshed in the same transaction,
/// so unsaving an album releases the pins of its member tracks unless a like,
/// a playlist or an upload still holds them.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that no longer saves the entity.
/// - `kind`: the kind of entity that was saved.
/// - `entity_id`: the entity's id.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the rows cannot be removed and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub(crate) async fn unsave(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<(), ActivityError> {
    sqlx::query!(
        "DELETE FROM user_library \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND origin = 'saved'",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?;
    refresh_pins(conn, kind, entity_id).await
}

/// Brings the personal-library pin of every track a like or a save affects up to
/// date.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the kind of entity whose likes or saves changed.
/// - `entity_id`: the entity's id.
///
/// # Errors
///
/// Returns [`ActivityError`] when a row cannot be read or a pin cannot be
/// written.
pub(crate) async fn refresh_pins(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<(), ActivityError> {
    match kind {
        EntityKind::Track => {
            refresh_user_library_pin(conn, EntityId::from_uuid(IdPrefix::Track, entity_id)).await?;
        }
        EntityKind::Album => {
            let tracks = sqlx::query_scalar!(
                "SELECT id FROM tracks WHERE album_id = ?",
                entity_id.as_bytes().as_slice(),
            )
            .fetch_all(&mut *conn)
            .await?;
            for track in tracks {
                let track = EntityId::from_uuid(IdPrefix::Track, Uuid::from_slice(&track)?);
                refresh_user_library_pin(conn, track).await?;
            }
        }
        // An artist has no file, and a playlist is pinned by the playlist
        // tables rather than by a like.
        EntityKind::Artist | EntityKind::Playlist => {}
    }
    Ok(())
}

/// Re-derives the personal-library pin of one track.
///
/// The pin is set while any user likes the track or the album it belongs to, or
/// while a `user_library` row names it or its album; it is removed otherwise.
/// The availability the pin implies is recomputed by the retention layer.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to re-derive.
///
/// # Returns
///
/// `true` when the track is pinned by a like or a saved row.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the likes cannot be read and
/// [`ActivityError::Library`] when the pin or the availability cannot be
/// written.
pub async fn refresh_user_library_pin(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<bool, ActivityError> {
    let wanted: i64 = sqlx::query_scalar(
        "WITH target AS (SELECT id, album_id FROM tracks WHERE id = ?) \
         SELECT \
           (SELECT count(*) FROM user_feedback f \
            WHERE f.value = 'like' \
              AND ((f.entity_kind = 'track' AND f.entity_id = (SELECT id FROM target)) \
                OR (f.entity_kind = 'album' AND f.entity_id = (SELECT album_id FROM target)))) \
         + (SELECT count(*) FROM user_library l \
            WHERE (l.entity_kind = 'track' AND l.entity_id = (SELECT id FROM target)) \
               OR (l.entity_kind = 'album' AND l.entity_id = (SELECT album_id FROM target)))",
    )
    .bind(track_id.uuid().as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await?;
    let wanted = wanted > 0;

    let pinned = retention::pin_reasons(conn, track_id)
        .await?
        .contains(&PinReason::UserLibrary);
    if wanted && !pinned {
        retention::add_pin_reason(conn, track_id, PinReason::UserLibrary).await?;
    } else if !wanted && pinned {
        retention::remove_pin_reason(conn, track_id, PinReason::UserLibrary).await?;
    }
    Ok(wanted)
}

/// Records a dislike, unless the user likes the entity.
///
/// A like is the stronger, explicit signal, so it is left in place; an existing
/// dislike has its instant refreshed. A dislike pins nothing.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that dislikes the entity.
/// - `kind`: the kind of entity.
/// - `entity_id`: the entity's id.
/// - `at`: when the dislike happened, in unix milliseconds.
///
/// # Returns
///
/// `true` when a dislike is now recorded, `false` when a like kept it out.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be written.
pub async fn dislike_unless_liked(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
    at: i64,
) -> Result<bool, ActivityError> {
    let written = sqlx::query!(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, ?, ?, 'dislike', ?) \
         ON CONFLICT (user_id, entity_kind, entity_id) \
         DO UPDATE SET at = excluded.at WHERE user_feedback.value = 'dislike'",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
        at,
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(written > 0)
}

/// Removes a user's dislike of an entity, leaving a like alone.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account that no longer dislikes the entity.
/// - `kind`: the kind of entity.
/// - `entity_id`: the entity's id.
///
/// # Returns
///
/// `true` when a dislike was removed.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be removed.
pub async fn remove_dislike(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<bool, ActivityError> {
    let removed = sqlx::query!(
        "DELETE FROM user_feedback \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND value = 'dislike'",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(removed > 0)
}

/// Returns the `user_feedback.entity_kind` literal of a kind.
///
/// # Parameters
///
/// - `kind`: the kind to name.
///
/// # Returns
///
/// The literal the column stores.
fn kind_literal(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    }
}
