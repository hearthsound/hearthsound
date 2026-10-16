//! Per-user ratings.
//!
//! A rating is a whole number from 1 to 5, or the absence of a row: a client
//! clears a rating by sending 0, which deletes the row rather than storing a
//! zero. Ratings are per user and per entity, and the entity may be a track, an
//! album, or an artist.
//!
//! A rating is not a like and not a dislike. What a client shows as a dislike
//! is the `user_feedback` row, so clearing a rating leaves a dislike in place
//! and vice versa.

use hearthsound_core::EntityKind;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::ActivityError;

/// The highest rating a client may set.
pub const MAX_RATING: u8 = 5;

/// Sets or clears a user's rating of an entity.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `user_id`: the account whose rating changes.
/// - `kind`: the kind of entity that is rated.
/// - `entity_id`: the entity's id.
/// - `rating`: the rating, 1 to 5, or 0 to clear the rating.
/// - `at`: when the rating was set, in unix milliseconds.
///
/// # Errors
///
/// Returns [`ActivityError::InvalidRating`] when `rating` is above
/// [`MAX_RATING`], and [`ActivityError::Database`] when the row cannot be
/// written.
pub async fn set_rating(
    conn: &mut SqliteConnection,
    user_id: Uuid,
    kind: EntityKind,
    entity_id: Uuid,
    rating: u8,
    at: i64,
) -> Result<(), ActivityError> {
    if rating > MAX_RATING {
        return Err(ActivityError::InvalidRating {
            rating,
            max: MAX_RATING,
        });
    }
    if rating == 0 {
        sqlx::query!(
            "DELETE FROM user_ratings \
             WHERE user_id = ? AND entity_kind = ? AND entity_id = ?",
            user_id.as_bytes().as_slice(),
            kind_literal(kind),
            entity_id.as_bytes().as_slice(),
        )
        .execute(&mut *conn)
        .await?;
        return Ok(());
    }
    sqlx::query!(
        "INSERT INTO user_ratings (user_id, entity_kind, entity_id, rating, at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (user_id, entity_kind, entity_id) \
         DO UPDATE SET rating = excluded.rating, at = excluded.at",
        user_id.as_bytes().as_slice(),
        kind_literal(kind),
        entity_id.as_bytes().as_slice(),
        rating,
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Returns the `user_ratings.entity_kind` literal of a kind.
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
