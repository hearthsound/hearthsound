//! The play queue endpoints: `savePlayQueue`, `getPlayQueue`,
//! `savePlayQueueByIndex`, and `getPlayQueueByIndex`.
//!
//! A saved queue belongs to one account and one client instance: the server
//! keeps the most recent save per client, so two clients of the same account
//! never overwrite each other's queue. What is stored is the list of ids the
//! client sent, in queue order, so an entry that names a stored track is served
//! as it is and an entry that names a catalog track is projected from what the
//! catalog reports. A queue a client saved is also what the prefetch scheduler
//! looks ahead from.

use hearthsound_catalog::aliases::provider_namespace;
use hearthsound_core::{ClientId, EntityKind, IdPrefix};

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError, codes};
use crate::value::{Array, Object};
use crate::{browse, catalog, projection};

/// Answers `savePlayQueue`: stores the queue the client is playing.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and holds the queue in
///   playing order, `current` names the entry the client is on, and `position`
///   is how far into it the client is, in milliseconds. No `id` clears the saved
///   queue.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 0 when `position` is not a number or `current` names an
/// entry the queue does not hold, error 70 when an id is not a client id, and a
/// generic error when the save fails.
pub async fn save_play_queue(call: Call) -> Encoded {
    let values = ids(&call);
    let queue = match parse(&values) {
        Ok(queue) => queue,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let position = match position(&call) {
        Ok(position) => position,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let index = match call.get("current").filter(|value| !value.is_empty()) {
        Some(current) => match values.iter().position(|value| value == current) {
            Some(index) => index,
            None => {
                return call.encode(Response::failed(SubsonicError::new(
                    codes::GENERIC,
                    "`current` names an entry the queue does not hold.",
                )));
            }
        },
        None => 0,
    };
    store(&call, &values, index, position, &queue).await
}

/// Answers `savePlayQueueByIndex`: stores the queue the client is playing, with
/// the entry it is on named by its position.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and holds the queue in
///   playing order, `currentIndex` is the zero-based position of the entry the
///   client is on, and `position` is how far into it the client is, in
///   milliseconds. No `id` clears the saved queue.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`save_play_queue`] documents, and error 0 when
/// `currentIndex` is past the end of the queue.
pub async fn save_play_queue_by_index(call: Call) -> Encoded {
    let values = ids(&call);
    let queue = match parse(&values) {
        Ok(queue) => queue,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let position = match position(&call) {
        Ok(position) => position,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let index = match crate::endpoints::browse::number(&call, "currentIndex", 0) {
        Ok(index) => usize::try_from(index.max(0)).unwrap_or(usize::MAX),
        Err(error) => return call.encode(Response::failed(error)),
    };
    store(&call, &values, index, position, &queue).await
}

/// Answers `getPlayQueue`: the queue the account saved most recently.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `playQueue`, or an envelope without that
/// member when the account has no saved queue. A failed envelope is answered
/// when a read fails.
pub async fn get_play_queue(call: Call) -> Encoded {
    let saved = match call
        .state()
        .activity()
        .play_queues()
        .latest(call.principal().user_id.uuid())
        .await
    {
        Ok(saved) => saved,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getPlayQueue", &error)));
        }
    };
    let Some(saved) = saved else {
        return call.encode(Response::ok());
    };
    let entries = match entries(&call, &saved.track_ids).await {
        Ok(entries) => entries,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let current = saved
        .track_ids
        .get(usize::try_from(saved.current_index).unwrap_or(usize::MAX))
        .cloned()
        .unwrap_or_default();
    let queue = Object::new()
        .set("current", current)
        .set("position", saved.position_ms)
        .set("username", call.principal().username.clone())
        .set("changed", projection::timestamp(saved.updated_at))
        .set("changedBy", saved.client.clone())
        .set("entry", Array::new(entries));
    call.encode(Response::ok().member("playQueue", queue))
}

/// Answers `getPlayQueueByIndex`: the saved queue with its position in it.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `playQueueByIndex`, or an envelope without
/// that member when the account has no saved queue. A failed envelope is
/// answered when a read fails.
pub async fn get_play_queue_by_index(call: Call) -> Encoded {
    let saved = match call
        .state()
        .activity()
        .play_queues()
        .latest(call.principal().user_id.uuid())
        .await
    {
        Ok(saved) => saved,
        Err(error) => {
            return call.encode(Response::failed(browse::failure(
                "getPlayQueueByIndex",
                &error,
            )));
        }
    };
    let Some(saved) = saved else {
        return call.encode(Response::ok());
    };
    let entries = match entries(&call, &saved.track_ids).await {
        Ok(entries) => entries,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let queue = Object::new()
        .set("currentIndex", saved.current_index)
        .set("position", saved.position_ms)
        .set("username", call.principal().username.clone())
        .set("changed", projection::timestamp(saved.updated_at))
        .set("changedBy", saved.client.clone())
        .set("entry", Array::new(entries));
    call.encode(Response::ok().member("playQueueByIndex", queue))
}

/// Writes one saved queue and tells the prefetch scheduler about it.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `values`: the ids the client sent, in queue order.
/// - `index`: the position of the entry the client is on.
/// - `position`: how far into that entry the client is, in milliseconds.
/// - `queue`: the parsed ids.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 0 when the index is past the end of the queue, and a
/// generic error when the save fails.
async fn store(
    call: &Call,
    values: &[String],
    index: usize,
    position: i64,
    queue: &[ClientId],
) -> Encoded {
    if !values.is_empty() && index >= values.len() {
        return call.encode(Response::failed(SubsonicError::new(
            codes::GENERIC,
            "The current index is past the end of the queue.",
        )));
    }
    let user = call.principal().user_id.uuid();
    let client = call.params().client_name().unwrap_or_default().to_owned();
    match call
        .state()
        .activity()
        .play_queues()
        .save(user, &client, values, index, position)
        .await
    {
        Ok(()) => {}
        Err(error) => {
            return call.encode(Response::failed(browse::failure("savePlayQueue", &error)));
        }
    }
    if let Some(prefetch) = call.state().prefetch() {
        prefetch.record_queue(user, &client, queue.to_vec());
    }
    call.encode(Response::ok())
}

/// Returns the ids a save request carries.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The values of the `id` parameter, in request order, with empty values left
/// out.
fn ids(call: &Call) -> Vec<String> {
    call.params()
        .get_all("id")
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses the ids a save request carries.
///
/// # Parameters
///
/// - `values`: the ids the client sent.
///
/// # Returns
///
/// The parsed ids, in queue order.
///
/// # Errors
///
/// Answers Subsonic error 70 when a value is not a client id.
fn parse(values: &[String]) -> Result<Vec<ClientId>, SubsonicError> {
    let mut queue = Vec::with_capacity(values.len());
    for value in values {
        match ClientId::parse(value) {
            Ok(id) => queue.push(id),
            Err(error) => {
                tracing::debug!(%error, "a play queue named an id that is not a client id");
                return Err(SubsonicError::not_found());
            }
        }
    }
    Ok(queue)
}

/// Reads the `position` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The position in milliseconds; an absent parameter is zero.
///
/// # Errors
///
/// Answers Subsonic error 0 when the value is not a number.
fn position(call: &Call) -> Result<i64, SubsonicError> {
    match call.get("position").filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<i64>() {
            Ok(position) => Ok(position.max(0)),
            Err(_) => Err(SubsonicError::new(
                codes::GENERIC,
                "A queue position is a number of milliseconds.",
            )),
        },
        None => Ok(0),
    }
}

/// Projects the entries of a saved queue.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `values`: the ids the client saved, in queue order.
///
/// # Returns
///
/// One `Child` per entry that resolves, in queue order. An entry that names
/// something other than a track, and a catalog entry no instance can answer, is
/// left out.
///
/// # Errors
///
/// Answers a generic error when a read fails or a catalog title suffix cannot be
/// read.
async fn entries(call: &Call, values: &[String]) -> Result<Vec<Object>, SubsonicError> {
    let suffix = catalog::title_suffix(&call.state().settings_snapshot())?;
    let database = call.state().database();
    let mut entries = Vec::with_capacity(values.len());
    for value in values {
        match ClientId::parse(value) {
            Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Track => {
                match browse::track_any(database, id).await {
                    Ok(Some(loaded)) => {
                        let annotation = match browse::annotation(
                            call.state(),
                            call.principal().user_id.uuid(),
                            EntityKind::Track,
                            id.uuid(),
                        )
                        .await
                        {
                            Ok(annotation) => annotation,
                            Err(error) => {
                                return Err(browse::failure("a play queue entry", &error));
                            }
                        };
                        entries.push(projection::child(
                            &loaded.view,
                            loaded.file.as_ref(),
                            &loaded.album_artists,
                            &annotation,
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => return Err(browse::failure("a play queue entry", &error)),
                }
            }
            Ok(ClientId::Catalog(id)) if id.kind() == EntityKind::Track => {
                let catalog = call.state().catalog();
                let namespace = provider_namespace(id.slug());
                let stored = match catalog
                    .entity_by_alias(&namespace, EntityKind::Track, id.provider_id())
                    .await
                {
                    Ok(stored) => stored,
                    Err(error) => {
                        tracing::debug!(%error, "could not resolve a queue entry's alias");
                        None
                    }
                };
                match stored {
                    Some(stored) => match browse::track_any(database, stored).await {
                        Ok(Some(loaded)) => entries.push(projection::child(
                            &loaded.view,
                            loaded.file.as_ref(),
                            &loaded.album_artists,
                            &hearthsound_activity::Annotation::default(),
                        )),
                        Ok(None) => {}
                        Err(error) => return Err(browse::failure("a play queue entry", &error)),
                    },
                    None => match catalog.track(&id).await {
                        Ok(track) => entries.push(projection::catalog_child(
                            &track.id,
                            &track.track,
                            &suffix,
                        )),
                        Err(error) => {
                            tracing::debug!(
                                %error,
                                "a play queue entry names a catalog track no instance answers"
                            );
                        }
                    },
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "a saved play queue holds an id that is not a client id");
            }
        }
    }
    Ok(entries)
}
