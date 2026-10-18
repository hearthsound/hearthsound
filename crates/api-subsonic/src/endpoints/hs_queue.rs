//! The play queue vendor endpoint.
//!
//! `hsSetQueue` is how a client that knows its own queue tells the server what it
//! is playing and what comes next, so the prefetch scheduler looks ahead from the
//! client's own order instead of inferring one from a listing. Nothing is stored:
//! the declaration feeds the scheduler for this user and client instance, and the
//! saved queue of `savePlayQueue` stays that endpoint's business.
//!
//! The endpoint belongs to the `hearthsound` vendor extension, so it is answered
//! only when the request carries `hearthsound=1` and answers Subsonic error 70
//! without it.

use hearthsound_core::ClientId;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};

/// Answers `hsSetQueue`: declares the queue one client is playing.
///
/// The declaration is `current` followed by `next` in playing order, and it is
/// recorded for the requesting account and the named client instance. The current
/// track then starts the prefetch the scheduler looks ahead with: the next
/// `prefetch.lookahead` items are acquired, the first at the `next` priority and
/// the rest at `queue`.
///
/// # Parameters
///
/// - `call`: the authenticated call. `current` names the playing track, `next`
///   may be repeated and holds the upcoming entries in play order, and `client`
///   names the client instance. All three are required. The request must carry
///   `hearthsound=1`.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1` and
/// when an id is not a client id, error 10 when a required parameter is absent,
/// and a generic error when the surface has no prefetch scheduler.
pub async fn hs_set_queue(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let Some(current) = parameter(&call, "current") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "current",
        )));
    };
    let Some(client) = parameter(&call, "client") else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("client")));
    };
    let upcoming: Vec<&str> = call
        .params()
        .get_all("next")
        .filter(|value| !value.is_empty())
        .collect();
    if upcoming.is_empty() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("next")));
    }
    let mut queue = Vec::with_capacity(upcoming.len() + 1);
    for value in std::iter::once(current).chain(upcoming) {
        match ClientId::parse(value) {
            Ok(id) => queue.push(id),
            Err(error) => {
                tracing::debug!(%error, "a queue declaration named an id that is not a client id");
                return call.encode(Response::failed(SubsonicError::not_found()));
            }
        }
    }
    let playing = queue[0].clone();
    let state = call.state();
    let Some(prefetch) = state.prefetch() else {
        tracing::error!("the /rest surface has no prefetch scheduler to declare a queue to");
        return call.encode(Response::failed(SubsonicError::generic()));
    };
    let user = call.principal().user_id.uuid();
    let can_acquire = call.principal().can_acquire;
    prefetch.record_queue(user, client, queue);
    prefetch
        .note_stream(user, client, &playing, can_acquire)
        .await;
    call.encode(Response::ok())
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
