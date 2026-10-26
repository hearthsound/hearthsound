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
//! A dislike is the other `user_feedback` value and pins nothing. A rating of
//! one records one only when the user does not like the entity
//! (`subsonic.rating_one_is_dislike`) and leaves a like in place, while the
//! dislike a user asks for directly replaces whatever they held; clearing
//! removes either.
//!
//! Each write answers whether it changed what the user's library holds, which is
//! what the activity service turns into a `library.changed` event for that user:
//! liking something already liked, or disliking something that was never liked,
//! changes nothing and publishes nothing.

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
/// # Returns
///
/// `true` when the user did not already like the entity, so the like is new.
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
) -> Result<bool, ActivityError> {
    let previous = value_of(conn, user_id, kind, entity_id).await?;
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
    refresh_pins(conn, kind, entity_id).await?;
    Ok(previous.as_deref() != Some("like"))
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

/// Records a dislike, replacing whatever feedback the user held.
///
/// Unlike [`dislike_unless_liked`], which a rating of one uses and which leaves
/// an explicit like in place, this is the dislike a user asked for directly: a
/// like is replaced. A dislike pins nothing, so the personal-library pin of
/// every track the change affects is refreshed in the same transaction and a
/// like that was the only holder of a pin releases it.
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
/// `true` when the user liked the entity before, so a like left their library.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be written and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub async fn dislike(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
    at: i64,
) -> Result<bool, ActivityError> {
    let previous = value_of(conn, user_id, kind, entity_id).await?;
    sqlx::query!(
        "INSERT INTO user_feedback (user_id, entity_kind, entity_id, value, at) \
         VALUES (?, ?, ?, 'dislike', ?) \
         ON CONFLICT (user_id, entity_kind, entity_id) \
         DO UPDATE SET value = 'dislike', at = excluded.at",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
        at,
    )
    .execute(&mut *conn)
    .await?;
    refresh_pins(conn, kind, entity_id).await?;
    Ok(previous.as_deref() == Some("like"))
}

/// Removes whatever feedback a user holds about an entity.
///
/// Both a like and a dislike are removed, which is what the vendor extension's
/// `none` means: the entity is unrated. The personal-library pin of every track
/// the removal affects is refreshed in the same transaction, so clearing a like
/// releases the pin nothing else holds.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account whose feedback is removed.
/// - `kind`: the kind of entity.
/// - `entity_id`: the entity's id.
///
/// # Returns
///
/// `true` when the user liked the entity, so clearing removed it from their
/// library. Clearing a dislike alone changes no membership and answers `false`.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the rows cannot be removed and
/// [`ActivityError::Library`] when a pin cannot be updated.
pub async fn clear(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<bool, ActivityError> {
    let previous = value_of(conn, user_id, kind, entity_id).await?;
    sqlx::query!(
        "DELETE FROM user_feedback \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ?",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?;
    refresh_pins(conn, kind, entity_id).await?;
    Ok(previous.as_deref() == Some("like"))
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
/// # Returns
///
/// `true` when the user had not saved the entity, so the save is new.
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
) -> Result<bool, ActivityError> {
    let existed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM user_library \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND origin = 'saved'",
    )
    .bind(user_id.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(entity_id.as_bytes().as_slice())
    .fetch_one(&mut *conn)
    .await?;
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
    refresh_pins(conn, kind, entity_id).await?;
    Ok(existed == 0)
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
/// # Returns
///
/// `true` when a saved row was removed.
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
) -> Result<bool, ActivityError> {
    let removed = sqlx::query!(
        "DELETE FROM user_library \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ? AND origin = 'saved'",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(&mut *conn)
    .await?
    .rows_affected();
    refresh_pins(conn, kind, entity_id).await?;
    Ok(removed > 0)
}

/// Returns the feedback a user holds about one entity, when any.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `user_id`: the account whose feedback is read.
/// - `kind`: the kind of entity.
/// - `entity_id`: the entity's id.
///
/// # Returns
///
/// `like`, `dislike`, or `None` when the user holds no opinion of it.
///
/// # Errors
///
/// Returns [`ActivityError::Database`] when the row cannot be read.
async fn value_of(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
) -> Result<Option<String>, ActivityError> {
    let value = sqlx::query_scalar(
        "SELECT value FROM user_feedback \
         WHERE user_id = ? AND entity_kind = ? AND entity_id = ?",
    )
    .bind(user_id.as_bytes().as_slice())
    .bind(kind_literal(kind))
    .bind(entity_id.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(value)
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
            // A track that has no row cannot be liked or saved: the pin would
            // refer to nothing, and `user_feedback` has no foreign key to catch
            // it.
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM tracks WHERE id = ?")
                .bind(entity_id.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?;
            if exists.is_none() {
                return Err(ActivityError::UnknownTrack {
                    id: EntityId::from_uuid(IdPrefix::Track, entity_id).to_string(),
                });
            }
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
/// written. A track with no row is reported as [`ActivityError::UnknownTrack`]
/// by the caller that changes its pins, rather than as an unpinned track.
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
