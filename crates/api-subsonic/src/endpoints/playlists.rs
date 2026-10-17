//! The playlist endpoints: `getPlaylists`, `getPlaylist`, `createPlaylist`,
//! `updatePlaylist`, and `deletePlaylist`.
//!
//! A playlist belongs to the account that created it: only its owner may change
//! it, and only its owner and an administrator may see a private one. The rows
//! and the pin maintenance are the activity layer's; this module reads the
//! parameters, resolves the ids, and projects the responses.

use std::collections::HashMap;

use hearthsound_acquire::adopt::acquire_playlist_adds;
use hearthsound_activity::{ActivityError, Playlist, PlaylistUpdate};
use hearthsound_core::{ClientId, EntityId, EntityKind, IdPrefix};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};
use crate::{browse, projection};

use super::feedback::resolve;

/// The prefix a playlist id carries.
const PLAYLIST_PREFIX: IdPrefix = IdPrefix::Playlist;

/// Answers `getPlaylists`: the playlists the caller may see.
///
/// # Parameters
///
/// - `call`: the authenticated call. `username` asks for another account's
///   playlists, which only an administrator may do.
///
/// # Returns
///
/// A successful envelope carrying `playlists`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 50 when a non-administrator names another account,
/// error 70 when the named account does not exist, and a generic error when a
/// read fails.
pub async fn get_playlists(call: Call) -> Encoded {
    let viewer = call.principal().user_id.uuid();
    let owner = match requested_owner(&call).await {
        Ok(owner) => owner,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let listed = match call
        .state()
        .activity()
        .playlists()
        .list(viewer, owner)
        .await
    {
        Ok(listed) => listed,
        Err(error) => return call.encode(Response::failed(failure("getPlaylists", &error))),
    };
    let names = match owner_names(&call, &listed).await {
        Ok(names) => names,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut playlists = Vec::with_capacity(listed.len());
    for playlist in &listed {
        let duration = match duration(&call, playlist).await {
            Ok(duration) => duration,
            Err(error) => return call.encode(Response::failed(error)),
        };
        playlists.push(playlist_object(playlist, &names, duration, Vec::new()));
    }
    call.encode(Response::ok().member(
        "playlists",
        Object::new().set("playlist", Array::new(playlists)),
    ))
}

/// Answers `getPlaylist`: one playlist with its entries.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the playlist.
///
/// # Returns
///
/// A successful envelope carrying `playlist`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 70 when the id is
/// malformed, names no playlist, or names a playlist the caller may not see,
/// and a generic error when a read fails.
pub async fn get_playlist(call: Call) -> Encoded {
    let id = match playlist_id(&call, "id") {
        Ok(id) => id,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let viewer = call.principal().user_id.uuid();
    let playlist = match call.state().activity().playlists().get(id, viewer).await {
        Ok(Some(playlist)) => playlist,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => return call.encode(Response::failed(failure("getPlaylist", &error))),
    };
    let entries = match entries(&call, &playlist).await {
        Ok(entries) => entries,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let names = match owner_names(&call, std::slice::from_ref(&playlist)).await {
        Ok(names) => names,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let duration = match duration(&call, &playlist).await {
        Ok(duration) => duration,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let playlist = playlist_object(&playlist, &names, duration, entries);
    call.encode(Response::ok().member("playlist", playlist))
}

/// Answers `createPlaylist`: creates a playlist, or replaces an owned one's
/// tracks.
///
/// # Parameters
///
/// - `call`: the authenticated call. `playlistId` names an existing playlist to
///   replace the tracks of; otherwise `name` is required. `songId` may be
///   repeated, and each value is a stored or catalog song id.
///
/// # Returns
///
/// A successful envelope carrying the playlist, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when neither `playlistId` nor `name` is given,
/// error 50 when the playlist belongs to another account, error 70 when
/// `playlistId` is malformed or names no playlist and when a song id cannot be
/// resolved, and a generic error when a catalog song cannot be materialized or
/// a write fails.
pub async fn create_playlist(call: Call) -> Encoded {
    let songs = match songs(&call, "songId").await {
        Ok(songs) => songs,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    let playlists = call.state().activity().playlists();

    let playlist = match call.get("playlistId").filter(|value| !value.is_empty()) {
        Some(value) => {
            let id = match parse_playlist_id(value) {
                Ok(id) => id,
                Err(error) => return call.encode(Response::failed(error)),
            };
            match playlists.replace_tracks(id, user, &songs).await {
                Ok(playlist) => playlist,
                Err(error) => {
                    return call.encode(Response::failed(failure("createPlaylist", &error)));
                }
            }
        }
        None => {
            let Some(name) = call.get("name").filter(|name| !name.is_empty()) else {
                return call.encode(Response::failed(SubsonicError::missing_parameter("name")));
            };
            match playlists.create(user, name, None, false, &songs).await {
                Ok(playlist) => playlist,
                Err(error) => {
                    return call.encode(Response::failed(failure("createPlaylist", &error)));
                }
            }
        }
    };
    acquire_added(&call, &songs).await;

    let entries = match entries(&call, &playlist).await {
        Ok(entries) => entries,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let names = match owner_names(&call, std::slice::from_ref(&playlist)).await {
        Ok(names) => names,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let duration = match duration(&call, &playlist).await {
        Ok(duration) => duration,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let playlist = playlist_object(&playlist, &names, duration, entries);
    call.encode(Response::ok().member("playlist", playlist))
}

/// Answers `updatePlaylist`: changes one owned playlist.
///
/// # Parameters
///
/// - `call`: the authenticated call. `playlistId` names the playlist, `name`,
///   `comment`, and `public` change its fields, `songIdToAdd` may be repeated,
///   and `songIndexToRemove` may be repeated.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `playlistId` is missing, error 0 when
/// `public` is not a boolean or a `songIndexToRemove` is not a number, error 50
/// when the playlist belongs to another account, error 70 when the playlist id
/// is malformed, names no playlist, or a song id cannot be resolved, and a
/// generic error when a catalog song cannot be materialized or a write fails.
pub async fn update_playlist(call: Call) -> Encoded {
    let id = match playlist_id(&call, "playlistId") {
        Ok(id) => id,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let add = match songs(&call, "songIdToAdd").await {
        Ok(add) => add,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let mut remove_indexes = Vec::new();
    for value in call.params().get_all("songIndexToRemove") {
        if value.is_empty() {
            continue;
        }
        match value.parse::<usize>() {
            Ok(index) => remove_indexes.push(index),
            Err(_) => {
                return call.encode(Response::failed(SubsonicError::new(
                    crate::response::codes::GENERIC,
                    "A song index is a number.",
                )));
            }
        }
    }
    let public = match call.get("public").filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<bool>() {
            Ok(public) => Some(public),
            Err(_) => {
                return call.encode(Response::failed(SubsonicError::new(
                    crate::response::codes::GENERIC,
                    "`public` is true or false.",
                )));
            }
        },
        None => None,
    };

    let added = add.clone();
    let update = PlaylistUpdate {
        name: call.get("name").map(str::to_owned),
        comment: call.get("comment").map(str::to_owned),
        public,
        add,
        remove_indexes,
    };
    let user = call.principal().user_id.uuid();
    match call
        .state()
        .activity()
        .playlists()
        .update(id, user, &update)
        .await
    {
        Ok(_) => {
            acquire_added(&call, &added).await;
            call.encode(Response::ok())
        }
        Err(error) => call.encode(Response::failed(failure("updatePlaylist", &error))),
    }
}

/// Acquires the remote tracks just added to a playlist.
///
/// A playlist a Subsonic client creates is never temporary, so its tracks are
/// pinned and `library.acquire_on_playlist_add` asks for every track that is
/// not stored at priority `queue`, under the user's permission and quota. The
/// playlist is already written; a refusal or an error here is only logged.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `songs`: the tracks the request added.
async fn acquire_added(call: &Call, songs: &[EntityId]) {
    if songs.is_empty() {
        return;
    }
    let Some(prefetch) = call.state().prefetch() else {
        return;
    };
    let user = call.principal().user_id.uuid();
    if let Err(error) = acquire_playlist_adds(
        prefetch.orchestrator(),
        user,
        call.principal().can_acquire,
        songs,
    )
    .await
    {
        tracing::warn!(%user, %error, "could not acquire what a user added to a playlist");
    }
}

/// Answers `deletePlaylist`: deletes one owned playlist.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the playlist.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `id` is missing, error 50 when the playlist
/// belongs to another account, error 70 when the id is malformed or names no
/// playlist, and a generic error when a write fails.
pub async fn delete_playlist(call: Call) -> Encoded {
    let id = match playlist_id(&call, "id") {
        Ok(id) => id,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    match call.state().activity().playlists().delete(id, user).await {
        Ok(_) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(failure("deletePlaylist", &error))),
    }
}

/// Reads the `playlistId` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `parameter`: the parameter name.
///
/// # Returns
///
/// The playlist id.
///
/// # Errors
///
/// Answers Subsonic error 10 when the parameter is missing and error 70 when it
/// is not a playlist id.
fn playlist_id(call: &Call, parameter: &'static str) -> Result<EntityId, SubsonicError> {
    let Some(value) = call.get(parameter).filter(|value| !value.is_empty()) else {
        return Err(SubsonicError::missing_parameter(parameter));
    };
    parse_playlist_id(value)
}

/// Parses a playlist id.
///
/// # Parameters
///
/// - `value`: the id the client wrote.
///
/// # Returns
///
/// The playlist id.
///
/// # Errors
///
/// Answers Subsonic error 70 when the value is not a playlist id.
fn parse_playlist_id(value: &str) -> Result<EntityId, SubsonicError> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == PLAYLIST_PREFIX => Ok(id),
        _ => {
            tracing::debug!("a playlist request named an unknown id");
            Err(SubsonicError::not_found())
        }
    }
}

/// Reads the song ids a request carries, materializing catalog ones.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `parameter`: the parameter name.
///
/// # Returns
///
/// The stored song ids, in request order.
///
/// # Errors
///
/// Answers Subsonic error 70 when a value is not a song id or names no stored
/// song, and a generic error when a catalog song cannot be materialized.
async fn songs(call: &Call, parameter: &str) -> Result<Vec<EntityId>, SubsonicError> {
    let mut songs = Vec::new();
    for value in call.params().get_all(parameter) {
        if value.is_empty() {
            continue;
        }
        let (kind, id) = resolve(
            call,
            parameter,
            value,
            Some((EntityKind::Track, IdPrefix::Track)),
        )
        .await?;
        let _ = kind;
        songs.push(EntityId::from_uuid(IdPrefix::Track, id));
    }
    Ok(songs)
}

/// Reads the account whose playlists a request asks for.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The account's id, or `None` when the request asks for the caller's own
/// playlists.
///
/// # Errors
///
/// Answers Subsonic error 50 when a non-administrator names another account,
/// error 70 when the named account does not exist, and a generic error when the
/// account cannot be read.
async fn requested_owner(call: &Call) -> Result<Option<Uuid>, SubsonicError> {
    let Some(username) = call.get("username").filter(|name| !name.is_empty()) else {
        return Ok(None);
    };
    let principal = call.principal();
    if !principal.is_admin && !principal.username.eq_ignore_ascii_case(username) {
        tracing::debug!(
            username,
            "a non-administrator asked for another account's playlists"
        );
        return Err(SubsonicError::not_authorized());
    }
    match call.state().users().find_by_username(username).await {
        Ok(Some(user)) => Ok(Some(user.id)),
        Ok(None) => {
            tracing::debug!(username, "a playlist request named an unknown account");
            Err(SubsonicError::not_found())
        }
        Err(error) => Err(browse::failure("getPlaylists", &error)),
    }
}

/// Resolves the usernames of the accounts that own some playlists.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `playlists`: the playlists of the response.
///
/// # Returns
///
/// The usernames, keyed by account id.
///
/// # Errors
///
/// Answers a generic error when an account cannot be read.
async fn owner_names(
    call: &Call,
    playlists: &[Playlist],
) -> Result<HashMap<Uuid, String>, SubsonicError> {
    let mut names = HashMap::new();
    let principal = call.principal();
    names.insert(principal.user_id.uuid(), principal.username.clone());
    for playlist in playlists {
        if names.contains_key(&playlist.owner_user_id) {
            continue;
        }
        match call
            .state()
            .users()
            .find_by_id(playlist.owner_user_id)
            .await
        {
            Ok(Some(user)) => {
                names.insert(user.id, user.username);
            }
            Ok(None) => {}
            Err(error) => return Err(browse::failure("getPlaylists", &error)),
        }
    }
    Ok(names)
}

/// Projects one playlist.
///
/// # Parameters
///
/// - `playlist`: the playlist row with its entries.
/// - `names`: the usernames, keyed by account id.
/// - `duration`: the total playing time of its songs, in seconds.
/// - `entries`: the projected entries; empty for a listing.
///
/// # Returns
///
/// The playlist.
fn playlist_object(
    playlist: &Playlist,
    names: &HashMap<Uuid, String>,
    duration: i64,
    entries: Vec<Object>,
) -> Object {
    let owner = names
        .get(&playlist.owner_user_id)
        .cloned()
        .unwrap_or_default();
    Object::new()
        .set("id", playlist.id.to_string())
        .set("name", playlist.name.clone())
        .set("comment", playlist.comment.clone().unwrap_or_default())
        .set("owner", owner)
        .set("public", playlist.is_public)
        .set("songCount", playlist.entries.len() as i64)
        .set("duration", duration)
        .set("created", projection::timestamp(playlist.created_at))
        .set("changed", projection::timestamp(playlist.updated_at))
        .set("coverArt", playlist.id.to_string())
        .set("entry", Array::new(entries))
}

/// Returns the total playing time of a playlist's songs.
///
/// The rows of a playlist's entries are read one by one, because a playlist is
/// as long as its owner made it and the total is what a listing reports.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `playlist`: the playlist whose songs are summed.
///
/// # Returns
///
/// The total, in seconds; a song with no known duration counts zero.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn duration(call: &Call, playlist: &Playlist) -> Result<i64, SubsonicError> {
    let library = hearthsound_library::LibraryRepository::new(call.state().database());
    let mut total = 0;
    for entry in &playlist.entries {
        match library.get_track(entry.track_id).await {
            Ok(Some(track)) => total += track.duration_ms.unwrap_or(0),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("a playlist", &error)),
        }
    }
    Ok(total / 1000)
}

/// Loads and projects the entries of a playlist.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `playlist`: the playlist whose entries are projected.
///
/// # Returns
///
/// One `Child` per entry, in playing order.
///
/// # Errors
///
/// Answers a generic error when a read fails.
async fn entries(call: &Call, playlist: &Playlist) -> Result<Vec<Object>, SubsonicError> {
    let database = call.state().database();
    let ids: Vec<EntityId> = playlist
        .entries
        .iter()
        .map(|entry| entry.track_id)
        .collect();
    let annotations = match annotations_for(call, &ids).await {
        Ok(annotations) => annotations,
        Err(error) => return Err(failure("a playlist", &error)),
    };
    let mut entries = Vec::with_capacity(ids.len());
    for id in ids {
        match browse::track_any(database, id).await {
            Ok(Some(loaded)) => entries.push(projection::child(
                &loaded.view,
                loaded.file.as_ref(),
                &loaded.album_artists,
                &annotations.get(&id.uuid()).copied().unwrap_or_default(),
            )),
            Ok(None) => {}
            Err(error) => return Err(browse::failure("a playlist", &error)),
        }
    }
    Ok(entries)
}

/// Loads the user's annotations for the songs of a playlist.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the songs of the playlist.
///
/// # Returns
///
/// The annotations, keyed by track id.
///
/// # Errors
///
/// Returns [`ActivityError`] when the read fails.
async fn annotations_for(
    call: &Call,
    ids: &[EntityId],
) -> Result<HashMap<Uuid, hearthsound_activity::Annotation>, ActivityError> {
    let plain: Vec<Uuid> = ids.iter().map(EntityId::uuid).collect();
    browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &plain,
    )
    .await
}

/// Maps an activity failure on a playlist to the error a client is answered.
///
/// A playlist that does not exist and one the caller may not see are both
/// reported as missing, so one account cannot learn that another's private
/// playlist exists.
///
/// # Parameters
///
/// - `operation`: the name of the call that failed, for the log.
/// - `error`: the failure.
///
/// # Returns
///
/// The Subsonic error.
fn failure(operation: &'static str, error: &ActivityError) -> SubsonicError {
    match error {
        ActivityError::UnknownPlaylist { .. } => SubsonicError::not_found(),
        ActivityError::NotOwner { .. } => SubsonicError::not_authorized(),
        ActivityError::InvalidName { .. } => SubsonicError::new(
            crate::response::codes::GENERIC,
            "A playlist name is empty or too long.",
        ),
        other => browse::failure(operation, other),
    }
}
