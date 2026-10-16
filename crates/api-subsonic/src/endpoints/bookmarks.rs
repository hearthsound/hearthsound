//! The bookmark endpoints: `createBookmark`, `deleteBookmark`, and
//! `getBookmarks`.
//!
//! A bookmark is where a user left off in a track, with an optional note. It is
//! per user and per track, so saving one again moves the position instead of
//! adding a second bookmark.

use hearthsound_core::{ClientId, EntityId, EntityKind, IdPrefix};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, projection};

use super::feedback::resolve;

/// Answers `createBookmark`: saves where the user left off in one track.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored or catalog track,
///   `position` is the position in milliseconds, and `comment` is the user's
///   note, when there is one.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` or `position` is missing, error 0 when
/// `position` is not a number, error 70 when `id` is not a track id or names no
/// stored track, and a generic error when a catalog track cannot be
/// materialized or the write fails.
pub async fn create_bookmark(call: Call) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let Some(position) = call.get("position").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "position",
        )));
    };
    let position = match position.parse::<i64>() {
        Ok(position) => position.max(0),
        Err(_) => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                "A bookmark position is a number of milliseconds.",
            )));
        }
    };
    let (kind, id) = match resolve(
        &call,
        "id",
        value,
        Some((EntityKind::Track, IdPrefix::Track)),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let _ = kind;
    let track = EntityId::from_uuid(IdPrefix::Track, id);
    let comment = call.get("comment").filter(|comment| !comment.is_empty());
    match call
        .state()
        .activity()
        .bookmarks()
        .upsert(call.principal().user_id.uuid(), track, position, comment)
        .await
    {
        Ok(_) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(browse::failure("createBookmark", &error))),
    }
}

/// Answers `deleteBookmark`: removes the user's bookmark of one track.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored track.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when `id` is not a
/// track id, and a generic error when the write fails.
pub async fn delete_bookmark(call: Call) -> Encoded {
    let Some(value) = call.get("id").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    };
    let track = match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Track => id,
        _ => {
            tracing::debug!("a bookmark request named an id that is not a track id");
            return call.encode(Response::failed(SubsonicError::not_found()));
        }
    };
    match call
        .state()
        .activity()
        .bookmarks()
        .delete(call.principal().user_id.uuid(), track)
        .await
    {
        Ok(_) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(browse::failure("deleteBookmark", &error))),
    }
}

/// Answers `getBookmarks`: the user's bookmarks, newest first.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `bookmarks`, or a failed envelope.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub async fn get_bookmarks(call: Call) -> Encoded {
    let saved = match call
        .state()
        .activity()
        .bookmarks()
        .list(call.principal().user_id.uuid())
        .await
    {
        Ok(saved) => saved,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getBookmarks", &error)));
        }
    };

    let ids: Vec<EntityId> = saved.iter().map(|bookmark| bookmark.track_id).collect();
    let annotations = match annotations(&call, &ids).await {
        Ok(annotations) => annotations,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut bookmarks = Vec::with_capacity(saved.len());
    for bookmark in &saved {
        let loaded = match bookmark_track(&call, bookmark.track_id).await {
            Ok(loaded) => loaded,
            Err(error) => return call.encode(Response::failed(error)),
        };
        let Some(loaded) = loaded else {
            continue;
        };
        let entry = projection::child(
            &loaded.view,
            loaded.file.as_ref(),
            &loaded.album_artists,
            &annotations
                .get(&bookmark.track_id.uuid())
                .copied()
                .unwrap_or_default(),
        );
        // A bookmark has one instant: the protocol asks for both a creation and
        // a change time, and the row records when it was last saved.
        let changed = projection::timestamp(bookmark.updated_at);
        bookmarks.push(
            Object::new()
                .set("position", bookmark.position_ms)
                .set("username", call.principal().username.clone())
                .set("comment", bookmark.comment.clone().unwrap_or_default())
                .set("created", changed.clone())
                .set("changed", changed)
                .set("entry", entry),
        );
    }
    call.encode(Response::ok().member(
        "bookmarks",
        Object::new().set("bookmark", Array::new(bookmarks)),
    ))
}

/// Loads the track a bookmark names.
///
/// A bookmarked track is read whatever its availability, because a bookmark is
/// the user's own record of where they were in it.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `track`: the bookmarked track.
///
/// # Returns
///
/// The track, or `None` when it no longer exists.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn bookmark_track(
    call: &Call,
    track: EntityId,
) -> Result<Option<browse::LoadedTrack>, SubsonicError> {
    match browse::track_any(call.state().database(), track).await {
        Ok(loaded) => Ok(loaded),
        Err(error) => Err(browse::failure("getBookmarks", &error)),
    }
}

/// Loads the user's annotations for the bookmarked tracks.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the bookmarked tracks.
///
/// # Returns
///
/// The annotations, keyed by track id.
///
/// # Errors
///
/// Answers a generic error when the read fails.
async fn annotations(
    call: &Call,
    ids: &[EntityId],
) -> Result<std::collections::HashMap<Uuid, hearthsound_activity::Annotation>, SubsonicError> {
    let plain: Vec<Uuid> = ids.iter().map(EntityId::uuid).collect();
    match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &plain,
    )
    .await
    {
        Ok(annotations) => Ok(annotations),
        Err(error) => Err(browse::failure("getBookmarks", &error)),
    }
}
