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
//! A catalog id is materialized first, the way `star` does it, so a client can
//! save, like or dislike what a search returned before the server has stored it.
//! A save and a like are also adoptions: `library.acquire_on_save` asks the
//! acquisition layer for the remote tracks the request names, under the
//! requester's permission and quota.
//!
//! Every endpoint here belongs to the `hearthsound` vendor extension, so each one
//! is answered only when the request carries `hearthsound=1` and answers Subsonic
//! error 70 without it.

use hearthsound_activity::Feedback;
use hearthsound_core::EntityKind;
use uuid::Uuid;

use crate::auth::Call;
use crate::browse;
use crate::response::{Encoded, Response, SubsonicError, codes};

use super::feedback::{adopt_saved, resolve};

/// The operation name a save reports.
const SAVE_OPERATION: &str = "hsSave";

/// The operation name an unsave reports.
const UNSAVE_OPERATION: &str = "hsUnsave";

/// The operation name a feedback change reports.
const FEEDBACK_OPERATION: &str = "hsSetFeedback";

/// The `value` a feedback request carries for a like.
const LIKE: &str = "like";

/// The `value` a feedback request carries for a dislike.
const DISLIKE: &str = "dislike";

/// The `value` a feedback request carries for removing feedback.
const NONE: &str = "none";

/// Answers `hsSave`: adds the named entities to the personal library.
///
/// The parameter may be repeated and each value names a track, an album or an
/// artist, stored or catalog. A save is an adoption (`library.acquire_on_save`),
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
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1` and
/// when a value is not an id of a kind the library holds or names no stored
/// entity, error 10 when it names no entity, and a generic error when a catalog
/// id cannot be materialized or a write fails.
pub async fn hs_save(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let entities = match named(&call).await {
        Ok(entities) => entities,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    if let Err(error) = call.state().activity().save(user, &entities).await {
        return call.encode(Response::failed(browse::failure(SAVE_OPERATION, &error)));
    }
    adopt_saved(&call, user, &entities).await;
    call.encode(Response::ok())
}

/// Answers `hsUnsave`: removes the named entities from the personal library.
///
/// The parameter may be repeated and each value names a track, an album or an
/// artist, stored or catalog. Only the membership a save wrote is removed; an
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
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers the errors [`hs_save`] documents.
pub async fn hs_unsave(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let entities = match named(&call).await {
        Ok(entities) => entities,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let user = call.principal().user_id.uuid();
    if let Err(error) = call.state().activity().unsave(user, &entities).await {
        return call.encode(Response::failed(browse::failure(UNSAVE_OPERATION, &error)));
    }
    call.encode(Response::ok())
}

/// Answers `hsSetFeedback`: records or removes the account's opinion of the
/// named entities.
///
/// The same `value` is applied to every id the request names, in request order:
/// `like` is the `star` operation and adopts what it saves, `dislike` replaces a
/// like and pins nothing, and `none` removes whichever the account held. A
/// failure on one id leaves the changes the earlier ids made, which is the same
/// rule `hsAcquire` follows.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` may be repeated and names tracks,
///   albums and artists, stored or catalog; `value` is `like`, `dislike` or
///   `none`. The request must carry `hearthsound=1`.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1` and
/// when a value is not an id of a kind that carries feedback or names no stored
/// entity, error 10 when it names no entity or no `value`, error 0 when `value`
/// is none of the three, and a generic error when a catalog id cannot be
/// materialized or a write fails.
pub async fn hs_set_feedback(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let entities = match named(&call).await {
        Ok(entities) => entities,
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
    for (kind, entity) in &entities {
        if let Err(error) = activity.set_feedback(user, *kind, *entity, feedback).await {
            return call.encode(Response::failed(browse::failure(
                FEEDBACK_OPERATION,
                &error,
            )));
        }
    }
    if feedback == Feedback::Like {
        adopt_saved(&call, user, &entities).await;
    }
    call.encode(Response::ok())
}

/// Reads the entities a request names, materializing catalog ids.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose `id` parameter is read.
///
/// # Returns
///
/// One entry per value, in request order, as the kind and the stored id.
///
/// # Errors
///
/// Answers Subsonic error 10 when the request names no entity, error 70 when a
/// value is not an id of a kind the library holds or names no stored entity, and
/// a generic error carrying the catalog's message when a catalog id cannot be
/// materialized.
async fn named(call: &Call) -> Result<Vec<(EntityKind, Uuid)>, SubsonicError> {
    let mut entities = Vec::new();
    for value in call.params().get_all("id") {
        if value.is_empty() {
            continue;
        }
        // The id itself decides the kind, so a request may name tracks, albums
        // and artists in one call.
        entities.push(resolve(call, "id", value, None).await?);
    }
    if entities.is_empty() {
        return Err(SubsonicError::missing_parameter("id"));
    }
    Ok(entities)
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
