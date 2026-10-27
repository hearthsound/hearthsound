//! The personal-library vendor endpoints.
//!
//! `hsSave` and `hsUnsave` are the library membership operations: they add an
//! entity to the requesting account's personal library or remove it again, which
//! is what makes a track, an album or an artist permanent for that account.
//! `hsSetFeedback` is the like, dislike and clear operation that `star` and
//! `unstar` express only part of: a like behaves exactly like a star, a dislike
//! is recorded in place of a like and pins nothing, and `none` removes whichever
//! the account held.
//!
//! An id resolves through the catalog layer's redirects the way `star` resolves
//! it, so a client can save, like or dislike what a search returned before the
//! server has stored a file for it. A save and a like are also adoptions:
//! `library.acquire_on_save` asks the acquisition layer for the remote tracks
//! the request names, under the requester's permission and quota.
//!
//! `hsSetPlaylistLifetime` belongs here too, because a playlist's lifetime is a
//! statement about the library: a permanent playlist pins every track it holds
//! and a temporary one pins none, so converting a playlist to permanent is what
//! makes the tracks of a synchronized playlist permanent for its owner.
//!
//! `hsGetLibraryCounts` reports how many items each list of the library holds,
//! which is what lets a client size the placeholders of a view it has not read
//! yet: the standard lists carry no total, and counting them client-side would
//! cost the read the placeholder is meant to stand in for.
//!
//! Every endpoint here belongs to the `hearthsound` vendor extension, so each one
//! is answered only when the request carries `hearthsound=1` and answers Subsonic
//! error 70 without it.
//!
//! The three bulk writes - `hsSave`, `hsUnsave` and `hsSetFeedback` - apply each
//! id in a transaction of its own and answer a `bulkResult` member naming the ids
//! they applied and the ones they did not, with a reason. A client that selected
//! thirty songs therefore learns that twenty-nine landed and which one did not,
//! instead of seeing one error and guessing.

use hearthsound_activity::Feedback;
use hearthsound_core::EntityKind;
use hearthsound_library::query;
use uuid::Uuid;

use crate::auth::Call;
use crate::browse;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::scope;
use crate::value::{Array, Object};

use super::feedback::{adopt_saved, resolve};

/// The operation name a playlist lifetime change reports.
const LIFETIME_OPERATION: &str = "hsSetPlaylistLifetime";

/// The `value` a feedback request carries for a like.
const LIKE: &str = "like";

/// The `value` a feedback request carries for a dislike.
const DISLIKE: &str = "dislike";

/// The `value` a feedback request carries for removing feedback.
const NONE: &str = "none";

/// Answers `hsSave`: adds the named entities to the personal library.
///
/// The parameter may be repeated and each value names a track, an album or an
/// artist. A save is an adoption (`library.acquire_on_save`),
/// so a saved track that is not stored is requested at priority `queue`; a
/// refusal or an error there is only logged, because the save itself is already
/// written.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated; the request must
///   carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `bulkResult`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`
/// and error 10 when it names no id. Each id is applied in its own transaction,
/// so an id that fails does not undo the others and is reported in
/// `bulkResult.failed` with its reason instead of failing the request.
pub async fn hs_save(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let named = match resolved(&call).await {
        Ok(named) => named,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let activity = call.state().activity();
    let mut applied = Vec::new();
    let mut failed = Vec::new();
    let mut adopted = Vec::new();
    for entry in named {
        match entry.entity {
            Ok((kind, entity)) => match activity.save(user, &[(kind, entity)]).await {
                Ok(()) => {
                    applied.push(entry.raw);
                    adopted.push((kind, entity));
                }
                Err(error) => failed.push((entry.raw, error.to_string())),
            },
            Err(reason) => failed.push((entry.raw, reason)),
        }
    }
    adopt_saved(&call, user, &adopted).await;
    call.encode(bulk_result(applied, failed))
}

/// Answers `hsUnsave`: removes the named entities from the personal library.
///
/// The parameter may be repeated and each value names a track, an album or an
/// artist. Only the membership a save wrote is removed; an
/// entity the account also uploaded keeps its row, and a track that a like or a
/// playlist still holds stays pinned.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated; the request must
///   carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `bulkResult`, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`hs_save`] documents, with the same per-id answer.
pub async fn hs_unsave(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let named = match resolved(&call).await {
        Ok(named) => named,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let activity = call.state().activity();
    let mut applied = Vec::new();
    let mut failed = Vec::new();
    for entry in named {
        match entry.entity {
            Ok((kind, entity)) => match activity.unsave(user, &[(kind, entity)]).await {
                Ok(()) => applied.push(entry.raw),
                Err(error) => failed.push((entry.raw, error.to_string())),
            },
            Err(reason) => failed.push((entry.raw, reason)),
        }
    }
    call.encode(bulk_result(applied, failed))
}

/// Answers `hsSetFeedback`: records or removes the account's opinion of the
/// named entities.
///
/// The same `value` is applied to every id the request names, in request order:
/// `like` is the `star` operation and adopts what it saves, `dislike` replaces a
/// like and pins nothing, and `none` removes whichever the account held. Each id
/// is one transaction, so an id that fails leaves the others as they are.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and names tracks,
///   albums and artists; `value` is `like`, `dislike` or
///   `none`. The request must carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `bulkResult`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// error 10 when the request names no id or no `value`, and error 0 when `value`
/// is none of the three. A failing id is reported in `bulkResult.failed`
/// instead.
pub async fn hs_set_feedback(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let named = match resolved(&call).await {
        Ok(named) => named,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(value) = parameter(&call, "value") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("value")));
    };
    let feedback = match value {
        LIKE => Feedback::Like,
        DISLIKE => Feedback::Dislike,
        NONE => Feedback::None,
        _ => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                "A feedback value is `like`, `dislike`, or `none`.",
            )));
        }
    };
    let user = call.principal().user_id.uuid();
    let activity = call.state().activity();
    let mut applied = Vec::new();
    let mut failed = Vec::new();
    let mut adopted = Vec::new();
    for entry in named {
        match entry.entity {
            Ok((kind, entity)) => match activity.set_feedback(user, kind, entity, feedback).await {
                Ok(()) => {
                    applied.push(entry.raw);
                    adopted.push((kind, entity));
                }
                Err(error) => failed.push((entry.raw, error.to_string())),
            },
            Err(reason) => failed.push((entry.raw, reason)),
        }
    }
    if feedback == Feedback::Like {
        adopt_saved(&call, user, &adopted).await;
    }
    call.encode(bulk_result(applied, failed))
}

/// Answers `hsGetLibraryCounts`: how many items each library list holds.
///
/// The counts are read for the scope `musicFolderId` names, exactly as the
/// browse listings resolve it, so a count always matches the number of items the
/// matching listing returns. The like counts and the playlist count are the
/// requesting account's own, because those lists are personal.
///
/// # Parameters
///
/// - `call`: the authenticated call. `musicFolderId` optionally narrows the
///   scope, as it does on the browse endpoints. The request must carry
///   `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `libraryCounts` with the members `artists`,
/// `albums`, `songs`, `playlists`, `likedArtists`, `likedAlbums`, `likedSongs`
/// and `uploads`, all integers; or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1` or
/// names an unknown `musicFolderId`, and a generic error when a read fails.
pub async fn hs_get_library_counts(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let scope = match scope::resolve(&call).await {
        Ok(scope) => scope,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user_id = call.principal().user_id.uuid();
    let mut conn = match call.state().database().readers().acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    // One connection serves every count, so the rows are counted against one
    // read snapshot rather than against a library that changes between them.
    let counts = match query::count_artists(&mut conn, scope).await {
        Ok(artists) => artists,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    let albums = match query::count_albums(&mut conn, scope).await {
        Ok(counted) => counted,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    let songs = match query::count_tracks(&mut conn, scope).await {
        Ok(counted) => counted,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    let liked = match query::starred_counts(&mut conn, user_id).await {
        Ok(liked) => liked,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    let uploads = match query::uploaded_tracks(&mut conn, user_id).await {
        Ok(uploads) => uploads.len() as i64,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };
    let playlists = match call
        .state()
        .activity()
        .playlists()
        .list(user_id, None)
        .await
    {
        Ok(playlists) => playlists.len() as i64,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "hsGetLibraryCounts",
                &error,
            )));
        }
    };

    call.encode(
        Response::ok().member(
            "libraryCounts",
            Object::new()
                .set("artists", counts)
                .set("albums", albums)
                .set("songs", songs)
                .set("playlists", playlists)
                .set("likedArtists", liked.artists)
                .set("likedAlbums", liked.albums)
                .set("likedSongs", liked.tracks)
                .set("uploads", uploads),
        ),
    )
}

/// Answers `hsSetPlaylistLifetime`: converts a playlist between temporary and
/// permanent.
///
/// A permanent playlist pins every track it holds, so converting one to
/// permanent makes its tracks permanent in the requesting account's library; a
/// temporary playlist pins none of them, so converting one back releases them
/// unless a like, an upload or another playlist still holds them. Only the
/// playlist's owner may do this.
///
/// # Parameters
///
/// - `call`: the authenticated call. `playlistId` names the playlist,
///   `temporary` is `true` or `false`, and `expiresAt` optionally names the
///   instant the playlist expires at, in unix milliseconds. The request must
///   carry `hearthsound=1`.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// when `playlistId` is malformed, names no playlist, or names a private
/// playlist of another account, error 10 when `playlistId` or `temporary` is
/// missing, error 0 when `temporary` is not a boolean or `expiresAt` is not a
/// non-negative number, error 50 when the playlist is public but belongs to
/// another account, and a generic error when a write fails.
pub async fn hs_set_playlist_lifetime(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let Some(value) = parameter(&call, "playlistId") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "playlistId",
        )));
    };
    let id = match super::playlists::parse_playlist_id(value) {
        Ok(id) => id,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let Some(value) = parameter(&call, "temporary") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "temporary",
        )));
    };
    let temporary = match value.parse::<bool>() {
        Ok(temporary) => temporary,
        Err(_) => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                "`temporary` is true or false.",
            )));
        }
    };
    let expires_at = match parameter(&call, "expiresAt") {
        Some(value) => match value.parse::<i64>() {
            Ok(expires_at) if expires_at >= 0 => Some(expires_at),
            _ => {
                return call.encode(Response::failed(SubsonicError::new(
                    codes::GENERIC,
                    "`expiresAt` is a unix time in milliseconds.",
                )));
            }
        },
        None => None,
    };

    let user = call.principal().user_id.uuid();
    match call
        .state()
        .activity()
        .playlists()
        .set_lifetime(id, user, temporary, expires_at)
        .await
    {
        Ok(_) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(playlist_failure(&error))),
    }
}

/// Maps a playlist write failure onto the failure envelope.
///
/// # Parameters
///
/// - `error`: the failure the repository returned.
///
/// # Returns
///
/// Subsonic error 70 for a playlist that does not exist, error 50 for one that
/// belongs to another account, and the generic browse failure otherwise.
fn playlist_failure(error: &hearthsound_activity::ActivityError) -> SubsonicError {
    match error {
        hearthsound_activity::ActivityError::UnknownPlaylist { .. } => SubsonicError::not_found(),
        hearthsound_activity::ActivityError::NotOwner { .. } => SubsonicError::not_authorized(),
        other => browse::failure(LIFETIME_OPERATION, other),
    }
}

/// One id a bulk request named, with how it resolved.
struct BulkId {
    /// The value as the client wrote it, which is what the answer names.
    raw: String,
    /// The entity it names, or the reason it could not be resolved.
    entity: Result<(EntityKind, Uuid), String>,
}

/// Resolves every id a bulk request names, one by one.
///
/// A value that does not resolve is not a request error here: a bulk write
/// applies what it can and says what it could not, so the reason travels in the
/// answer. A request that names no id is still malformed.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// One entry per id, in request order.
///
/// # Errors
///
/// Answers Subsonic error 10 when the request names no id.
async fn resolved(call: &Call) -> Result<Vec<BulkId>, SubsonicError> {
    let mut named = Vec::new();
    for value in call.params().get_all("id") {
        if value.is_empty() {
            continue;
        }
        // The id itself decides the kind, so a request may name tracks, albums
        // and artists in one call.
        let entity = match resolve(call, "id", value, None).await {
            Ok(pair) => Ok(pair),
            Err(error) if error.code() == codes::NOT_FOUND => Err("not found".to_owned()),
            Err(_) => Err("the server could not resolve this id".to_owned()),
        };
        named.push(BulkId {
            raw: value.to_owned(),
            entity,
        });
    }
    if named.is_empty() {
        return Err(SubsonicError::missing_parameter("id"));
    }
    Ok(named)
}

/// Builds the answer of a bulk write.
///
/// # Parameters
///
/// - `applied`: the ids that were applied, in request order.
/// - `failed`: the ids that were not, each with its reason.
///
/// # Returns
///
/// A successful envelope whose `bulkResult` names both lists. A client that does
/// not read the member sees the same successful answer it always did.
fn bulk_result(applied: Vec<String>, failed: Vec<(String, String)>) -> Response {
    let failed: Vec<Object> = failed
        .into_iter()
        .map(|(id, reason)| Object::new().set("id", id).set("reason", reason))
        .collect();
    Response::ok().member(
        "bulkResult",
        Object::new()
            .set("applied", Array::new(applied))
            .set("failed", Array::new(failed)),
    )
}

/// Returns the value of a required parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `name`: the parameter name.
///
/// # Returns
///
/// The value, or `None` when the request did not carry it or carried it empty.
fn parameter<'a>(call: &'a Call, name: &'static str) -> Option<&'a str> {
    call.get(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
