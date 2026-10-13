//! Track retention: the pin reasons that make a managed file permanent.
//!
//! `track_retention` is the single source of truth for whether a track's managed
//! file is permanent or evictable. `pinned` is 1 when at least one pin reason is
//! active and `pin_reasons` holds the active reasons as a JSON array; the
//! reasons are exactly `user-library`, `playlist`, `folder`, `admin`, and
//! `policy`.
//!
//! Retention is derived, so every pin change here writes the retention row and
//! the availability it implies in one transaction on the caller's connection.
//! The availability follows the preferred file:
//!
//! - a `folder` file or a `library` file is permanent, so the track is `local`;
//! - a `cache` file is permanent only when the track is pinned, so the track is
//!   `local` while pinned and `cached` once the last reason is removed;
//! - a track with no file, and a track that is `acquiring`, is left as it is.
//!
//! # Known limitation
//!
//! A pin change updates the availability but does not move the file: a managed
//! file stays where the import put it. Unpinning a track whose file is in
//! `<storage>/library` therefore leaves the file there and marks the track
//! `cached`, and pinning a track whose file is in `<storage>/cache` leaves the
//! file there and marks it `local`. Moving files between the two roots is the
//! storage-policy work of a later milestone; until then the two roots hold the
//! same kind of file and the availability decides whether eviction may touch it.

use std::path::{Path, PathBuf};

use hearthsound_core::{Availability, EntityId, IdPrefix, PinReason, StorageClass};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::LibraryError;
use crate::model::{StorageRoots, decode_entity, entity_key};
use crate::{folders, media_files, now_millis, tracks};

/// Adds a pin reason to a track.
///
/// The reason is added only when it is not already active, so a repeated save or
/// a rescan does not rewrite the row. Adding a reason pins the track and
/// re-derives its availability.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to pin.
/// - `reason`: the reason to add.
///
/// # Returns
///
/// `true` when the reason was added, `false` when it was already active.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails or a stored reason list cannot
/// be decoded.
pub async fn add_pin_reason(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    reason: PinReason,
) -> Result<bool, LibraryError> {
    let mut reasons = read_pin_reasons(conn, track_id).await?;
    if reasons.contains(&reason) {
        return Ok(false);
    }
    reasons.push(reason);
    write_pin_reasons(conn, track_id, &reasons).await?;
    recompute_availability(conn, track_id).await?;
    Ok(true)
}

/// Removes a pin reason from a track.
///
/// The track stays pinned while any other reason is active. Removing the last
/// reason unpins it and re-derives its availability, which demotes a managed
/// file to `cached`.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to unpin.
/// - `reason`: the reason to remove.
///
/// # Returns
///
/// `true` when the reason was removed, `false` when it was not active.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails or a stored reason list cannot
/// be decoded.
pub async fn remove_pin_reason(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    reason: PinReason,
) -> Result<bool, LibraryError> {
    let mut reasons = read_pin_reasons(conn, track_id).await?;
    if !reasons.contains(&reason) {
        return Ok(false);
    }
    reasons.retain(|active| *active != reason);
    write_pin_reasons(conn, track_id, &reasons).await?;
    recompute_availability(conn, track_id).await?;
    Ok(true)
}

/// Replaces a track's pin reasons.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to change.
/// - `reasons`: the complete set of active reasons.
///
/// # Returns
///
/// `true` when the stored set changed, `false` when it already matched.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails or a stored reason list cannot
/// be decoded.
pub async fn set_pin_reasons(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    reasons: &[PinReason],
) -> Result<bool, LibraryError> {
    let current = read_pin_reasons(conn, track_id).await?;
    if current == reasons {
        return Ok(false);
    }
    write_pin_reasons(conn, track_id, reasons).await?;
    recompute_availability(conn, track_id).await?;
    Ok(true)
}

/// Reports whether a track is pinned.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// `true` when the track has a retention row with at least one pin reason. A
/// track with no retention row is unpinned.
///
/// # Errors
///
/// Returns [`LibraryError::Database`] when the query fails.
pub async fn track_is_pinned(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<bool, LibraryError> {
    let pinned: Option<i64> = sqlx::query_scalar!(
        "SELECT pinned FROM track_retention WHERE track_id = ?",
        entity_key(track_id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(pinned.unwrap_or(0) != 0)
}

/// Reads a track's active pin reasons.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The reasons in the order they are stored, which is the order they were added.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails or the stored list cannot be
/// decoded.
pub async fn pin_reasons(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<PinReason>, LibraryError> {
    read_pin_reasons(conn, track_id).await
}

/// Re-derives a track's availability from its preferred file and its pins.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to recompute.
///
/// # Returns
///
/// The availability the track now has, or `None` when the track does not exist.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails.
pub async fn recompute_availability(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Option<Availability>, LibraryError> {
    let Some(track) = tracks::get_track(conn, track_id).await? else {
        return Ok(None);
    };
    if track.availability == Availability::Acquiring {
        return Ok(Some(track.availability));
    }
    let Some(file) = media_files::preferred_media_file(conn, track_id).await? else {
        return Ok(Some(track.availability));
    };
    let availability = match file.storage {
        StorageClass::Folder | StorageClass::Library => Availability::Local,
        StorageClass::Cache => {
            if track_is_pinned(conn, track_id).await? {
                Availability::Local
            } else {
                Availability::Cached
            }
        }
    };
    if availability != track.availability {
        tracks::set_track_availability(conn, track_id, availability, None).await?;
    }
    Ok(Some(availability))
}

/// Adds or removes the `folder` pin reason on every track of a folder.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `folder_id`: the folder whose tracks change.
/// - `pinned`: `true` to pin the folder's tracks, `false` to unpin them.
///
/// # Returns
///
/// How many tracks actually changed.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails or a stored reason list cannot
/// be decoded.
pub async fn repin_folder_tracks(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
    pinned: bool,
) -> Result<u64, LibraryError> {
    let tracks: Vec<Vec<u8>> = sqlx::query_scalar!(
        "SELECT DISTINCT track_id FROM media_files WHERE storage = 'folder' AND folder_id = ?",
        folder_id.as_bytes().as_slice(),
    )
    .fetch_all(&mut *conn)
    .await?;

    let mut changed = 0;
    for bytes in tracks {
        let track_id = decode_entity(IdPrefix::Track, &bytes)?;
        let changed_here = if pinned {
            add_pin_reason(conn, track_id, PinReason::Folder).await?
        } else {
            remove_pin_reason(conn, track_id, PinReason::Folder).await?
        };
        if changed_here {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Associates a library folder with a user, or makes it generic, and re-pins its
/// tracks.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `folder_id`: the folder to change.
/// - `owner_user_id`: the new owner, or `None` to make the folder generic.
///
/// # Returns
///
/// `true` when the folder exists and was written, `false` when it does not.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails.
pub async fn set_folder_owner(
    conn: &mut SqliteConnection,
    folder_id: Uuid,
    owner_user_id: Option<Uuid>,
) -> Result<bool, LibraryError> {
    if !folders::set_library_folder_owner(conn, folder_id, owner_user_id).await? {
        return Ok(false);
    }
    repin_folder_tracks(conn, folder_id, owner_user_id.is_some()).await?;
    Ok(true)
}

/// Builds the import destination for a track.
///
/// # Parameters
///
/// - `pinned`: whether the track is pinned; a pinned import lands under the
///   permanent root.
/// - `roots`: the managed storage roots.
/// - `relative`: the relative path a [`crate::naming::NamingTemplate`] rendered.
///
/// # Returns
///
/// `<storage>/library/<relative>` for a pinned track and `<storage>/cache/
/// <relative>` for an unpinned one.
pub fn import_destination(pinned: bool, roots: &StorageRoots, relative: &Path) -> PathBuf {
    if pinned {
        roots.library.join(relative)
    } else {
        roots.cache.join(relative)
    }
}

/// Reads a track's stored pin reasons.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track_id`: the track to read.
///
/// # Returns
///
/// The stored reasons, or an empty list when the track has no retention row.
///
/// # Errors
///
/// Returns [`LibraryError`] when the query fails or the stored list cannot be
/// decoded.
async fn read_pin_reasons(
    conn: &mut SqliteConnection,
    track_id: EntityId,
) -> Result<Vec<PinReason>, LibraryError> {
    let stored: Option<String> = sqlx::query_scalar!(
        "SELECT pin_reasons FROM track_retention WHERE track_id = ?",
        entity_key(track_id),
    )
    .fetch_optional(&mut *conn)
    .await?;
    match stored {
        Some(text) => Ok(serde_json::from_str(&text)?),
        None => Ok(Vec::new()),
    }
}

/// Writes a track's pin reasons and the `pinned` flag they imply.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track_id`: the track to write.
/// - `reasons`: the complete set of active reasons.
///
/// # Errors
///
/// Returns [`LibraryError`] when a statement fails.
async fn write_pin_reasons(
    conn: &mut SqliteConnection,
    track_id: EntityId,
    reasons: &[PinReason],
) -> Result<(), LibraryError> {
    let stored = serde_json::to_string(reasons)?;
    let pinned = !reasons.is_empty();
    let now = now_millis();
    sqlx::query!(
        "INSERT INTO track_retention (track_id, pinned, pin_reasons, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT (track_id) DO UPDATE SET pinned = ?, pin_reasons = ?, updated_at = ?",
        entity_key(track_id),
        pinned,
        stored,
        now,
        pinned,
        stored,
        now,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}
