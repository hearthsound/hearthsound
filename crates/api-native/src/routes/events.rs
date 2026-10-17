//! The server event stream and the tickets that let a client open it.
//!
//! `GET /api/v1/events` is a Server-Sent Events feed of [`EventEnvelope`]s.
//! Each frame carries the envelope id as its SSE `id`, the wire event name as
//! its `event`, and the envelope JSON as its `data`. A reconnecting client
//! sends the last id it saw in `Last-Event-ID` and receives the events it
//! missed before the live feed resumes. When the missed events have left the
//! replay buffer, or the id belongs to an earlier server run, the stream sends
//! one `system.notice` frame with the code `events-lost` and no `id`, telling
//! the client to refetch its state over REST.
//!
//! Browsers cannot attach headers to an `EventSource`, so a client may instead
//! redeem a single-use ticket from `POST /api/v1/events/ticket` through the
//! `ticket` query parameter.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::stream::{self, Stream, StreamExt};
use hearthsound_auth::Principal;
use hearthsound_core::events::{EventBus, EventEnvelope, Replay};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::error::RecvError;
use utoipa::{IntoParams, ToSchema};

use crate::auth::Authed;
use crate::problem::Problem;
use crate::state::NativeState;
use crate::views::now_millis;

/// The message carried by the `events-lost` notice.
const REFETCH_NOTICE: &str = "event replay is no longer available; refetch state over REST";

/// How often a keep-alive comment is sent on an idle stream, so proxies keep
/// the connection open.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// A short-lived ticket that authorizes one event-stream connection.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StreamTicket {
    /// The ticket token: 52 lowercase base32 characters, single-use.
    #[schema(example = "mfrggzdfmztwq2lknnwg23tpobyxe43uovzxs4tuf5ohtxs7x4ta")]
    pub ticket: String,
    /// The unix millisecond timestamp after which the ticket is refused.
    pub expires_at: i64,
}

/// The query parameters of the event stream.
#[derive(Debug, Clone, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StreamQuery {
    /// A ticket from `POST /api/v1/events/ticket`, used in place of a bearer
    /// key or session cookie. Redeemed once.
    pub ticket: Option<String>,
}

/// Issues a single-use ticket for the event stream.
///
/// The ticket authorizes one connection to `GET /api/v1/events` for 60 seconds
/// and is consumed by that connection, so a client that needs to reconnect
/// requests a new one.
///
/// # Parameters
///
/// - `state`: the native state, which holds the ticket store.
/// - `authed`: the authenticated caller; the ticket acts as this principal.
///
/// # Returns
///
/// `201 Created` with the ticket token and its expiry.
///
/// # Errors
///
/// `401 unauthenticated` when the request carries no valid credential.
#[utoipa::path(
    post,
    path = "/api/v1/events/ticket",
    tag = "events",
    responses(
        (status = 201, description = "A single-use ticket for the event stream", body = StreamTicket),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_ticket(
    State(state): State<NativeState>,
    authed: Authed,
) -> (StatusCode, Json<StreamTicket>) {
    let (ticket, expires_at) = state.tickets().issue(authed.principal);
    (
        StatusCode::CREATED,
        Json(StreamTicket { ticket, expires_at }),
    )
}

/// Streams server events to the caller as Server-Sent Events.
///
/// The stream is authorized by a bearer key, a session cookie, or a `ticket`
/// query parameter. Each event is forwarded when its audience admits the
/// caller; administrators see every event. When the `Last-Event-ID` header is
/// present, events after that id are replayed before the live feed starts. The
/// response carries a comment keep-alive every 15 seconds.
///
/// # Parameters
///
/// - `state`: the native state, which holds the event bus and the ticket store.
/// - `query`: the optional single-use `ticket`.
/// - `parts`: the request head, from which the credential and the
///   `Last-Event-ID` header are read.
///
/// # Returns
///
/// A `text/event-stream` response that stays open until the server stops.
///
/// # Errors
///
/// `400 invalid-request` when `Last-Event-ID` is not an unsigned integer
/// (checked before a ticket is redeemed, so a malformed request does not spend
/// the ticket). `401 unauthenticated` when the credential or ticket is missing,
/// unknown, expired, or already used.
#[utoipa::path(
    get,
    path = "/api/v1/events",
    tag = "events",
    params(
        ("Last-Event-ID" = Option<u64>, Header, description = "The id of the last frame the client received; missed events are replayed after it"),
        StreamQuery
    ),
    responses(
        (status = 200, description = "A text/event-stream of server events; each frame's data is an event envelope", content_type = "text/event-stream"),
        (status = 400, description = "Last-Event-ID is not an unsigned integer", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No valid credential or ticket", body = Problem, content_type = "application/problem+json")
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn stream(
    State(state): State<NativeState>,
    Query(query): Query<StreamQuery>,
    mut parts: Parts,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Problem> {
    let last_event_id = last_event_id(&parts)?;
    let principal = match query.ticket {
        Some(ticket) => state.tickets().redeem(&ticket).ok_or_else(|| {
            Problem::unauthenticated("the stream ticket is unknown, expired or already used")
        })?,
        None => {
            Authed::from_request_parts(&mut parts, &state)
                .await?
                .principal
        }
    };
    let bus = Arc::clone(&state.services().events);
    // Subscribe before reading the replay window, so no event published in
    // between is lost; the dedup in `next_live_frame` covers the overlap.
    let receiver = bus.subscribe();
    let (opening, replayed_through) = opening_frames(&bus, last_event_id, &principal);
    let live = Live {
        receiver,
        principal,
        replayed_through,
    };
    let frames =
        stream::iter(opening.into_iter().map(Ok)).chain(stream::unfold(live, next_live_frame));
    Ok(Sse::new(frames).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)))
}

/// Reads the `Last-Event-ID` header, when the client sent one.
///
/// # Parameters
///
/// - `parts`: the request head.
///
/// # Returns
///
/// `None` when the header is absent, otherwise the id it carries.
///
/// # Errors
///
/// `400 invalid-request` when the header is present but is not an unsigned
/// integer, including an empty value.
fn last_event_id(parts: &Parts) -> Result<Option<u64>, Problem> {
    let Some(value) = parts.headers.get("last-event-id") else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|text| text.parse::<u64>().ok())
        .map(Some)
        .ok_or_else(|| {
            Problem::invalid_request("Last-Event-ID must be an unsigned integer event id")
        })
}

/// Plans the frames sent before the live feed, and the replay boundary.
///
/// The replay boundary is the highest id the replay covered. Live envelopes
/// with an id at or below it were already sent here and are skipped.
///
/// # Parameters
///
/// - `bus`: the event bus; its replay window is read after the caller subscribed.
/// - `last_event_id`: the id the client reported, if any.
/// - `principal`: the caller, whose audience decides which replayed events
///   are forwarded.
///
/// # Returns
///
/// The frames to send before the live feed and the replay boundary. The
/// boundary is `0` when nothing was replayed.
fn opening_frames(
    bus: &EventBus,
    last_event_id: Option<u64>,
    principal: &Principal,
) -> (Vec<Event>, u64) {
    let Some(last) = last_event_id else {
        return (Vec::new(), 0);
    };
    // An id beyond the latest one belongs to an earlier server run, whose ids
    // this run will never reuse in order, so the client must refetch.
    if last > bus.latest_id() {
        return (vec![events_lost_frame()], 0);
    }
    match bus.replay_after(last) {
        Replay::Evicted => (vec![events_lost_frame()], 0),
        Replay::Events(events) => {
            let through = events.iter().map(|event| event.id).fold(last, u64::max);
            let frames = events
                .iter()
                .filter(|event| visible(event, principal))
                .filter_map(envelope_frame)
                .collect();
            (frames, through)
        }
    }
}

/// The state carried between frames of the live feed.
struct Live {
    /// The subscription to the event bus.
    receiver: tokio::sync::broadcast::Receiver<EventEnvelope>,
    /// The caller the frames are filtered for.
    principal: Principal,
    /// The highest id already sent by the replay; live envelopes at or below
    /// it are skipped.
    replayed_through: u64,
}

/// Produces the next live frame, or `None` once the bus has shut down.
///
/// Envelopes the caller may not see, and envelopes the replay already sent,
/// are skipped. A subscriber that fell behind the bus gets one `events-lost`
/// notice and then continues with whatever the bus still holds.
///
/// # Parameters
///
/// - `live`: the feed state, returned with the frame.
///
/// # Returns
///
/// The next frame and the updated state, or `None` when the stream ends.
async fn next_live_frame(mut live: Live) -> Option<(Result<Event, Infallible>, Live)> {
    loop {
        match live.receiver.recv().await {
            Ok(envelope) => {
                if envelope.id <= live.replayed_through || !visible(&envelope, &live.principal) {
                    continue;
                }
                if let Some(frame) = envelope_frame(&envelope) {
                    return Some((Ok(frame), live));
                }
            }
            Err(RecvError::Lagged(_)) => return Some((Ok(events_lost_frame()), live)),
            Err(RecvError::Closed) => return None,
        }
    }
}

/// Reports whether an envelope's audience admits the caller.
///
/// # Parameters
///
/// - `envelope`: the envelope to test.
/// - `principal`: the caller.
///
/// # Returns
///
/// `true` when the caller is an administrator or the envelope is addressed to
/// everyone or to the caller.
fn visible(envelope: &EventEnvelope, principal: &Principal) -> bool {
    envelope
        .audience
        .is_visible_to(principal.user_id.uuid(), principal.is_admin)
}

/// Builds the frame for one envelope.
///
/// # Parameters
///
/// - `envelope`: the envelope to send.
///
/// # Returns
///
/// A frame with the envelope id, wire event name and JSON body, or `None` when
/// the envelope cannot be serialized (logged and dropped).
fn envelope_frame(envelope: &EventEnvelope) -> Option<Event> {
    match serde_json::to_string(envelope) {
        Ok(data) => Some(
            Event::default()
                .id(envelope.id.to_string())
                .event(envelope.event.type_str())
                .data(data),
        ),
        Err(error) => {
            tracing::warn!(%error, "dropping an event that failed to serialize");
            None
        }
    }
}

/// Builds the `events-lost` notice frame.
///
/// The frame carries no `id`, so it does not move the client's `Last-Event-ID`.
///
/// # Returns
///
/// A `system.notice` frame whose body has the envelope shape with the code
/// `events-lost`.
fn events_lost_frame() -> Event {
    let notice = serde_json::json!({
        "at": now_millis(),
        "type": "system.notice",
        "data": {
            "level": "warn",
            "message": REFETCH_NOTICE,
            "code": "events-lost",
        },
    });
    Event::default()
        .event("system.notice")
        .data(notice.to_string())
}
