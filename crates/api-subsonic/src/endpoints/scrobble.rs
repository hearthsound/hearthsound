//! The `scrobble` endpoint: recording a play or a now-playing report.
//!
//! A client reports playback with `scrobble`. Each `id` is a track to record; a
//! submission (`submission=true`, the default) records a play through the
//! activity service, and a now-playing report (`submission=false`) is accepted
//! and discarded. The service itself enforces the "no song audio delivered"
//! guard, so a client that only ever heard silence padding while an acquisition
//! failed does not create a false play.
//!
//! Only a stored track is recorded. An `x_` catalog id names an entity the
//! server has not stored, so it is accepted and discarded, as is any other id
//! that is not a stored track: a client's play history must reference a row the
//! server has.

use hearthsound_activity::Scrobble;
use hearthsound_core::{ClientId, IdPrefix};

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};

use super::browse::number;

/// Answers `scrobble`: one play record per stored track id.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` is repeatable, `time` is when the
///   client played the track in unix milliseconds (the moment of the request
///   when absent), and `submission` decides between a play and a now-playing
///   report.
///
/// # Returns
///
/// An empty successful envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when no `id` is given, a generic error when `time`
/// is not a number, and a generic error when a play cannot be recorded, which
/// means the track the id names is not a row the server can reference.
pub async fn scrobble(call: Call) -> Encoded {
    let ids: Vec<String> = call.get_all("id").map(str::to_owned).collect();
    if ids.is_empty() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    }
    let started_at = match number(&call, "time", now_millis()) {
        Ok(started_at) => started_at,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let submission = submission(&call);
    let user_id = call.principal().user_id.uuid();

    for value in &ids {
        let Ok(ClientId::Entity(id)) = ClientId::parse(value) else {
            tracing::debug!(
                id = value,
                "a scrobble named an id that is not a stored entity; it was discarded"
            );
            continue;
        };
        if id.kind() != IdPrefix::Track {
            tracing::debug!(
                id = %id,
                "a scrobble named a stored entity that is not a track; it was discarded"
            );
            continue;
        }
        let scrobble = Scrobble {
            user_id,
            track_id: id.uuid(),
            client: call.client_name(),
            submission,
            started_at,
            listened_ms: 0,
        };
        if let Err(error) = call.state().activity().scrobble(&scrobble).await {
            tracing::error!(error = %error, track = %id, "could not record a play");
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    }

    call.encode(Response::ok())
}

/// Reads `submission`, which defaults to a play.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// `false` for the values a client uses for a now-playing report (`false` or
/// `0`), and `true` otherwise, including when the parameter is absent.
fn submission(call: &Call) -> bool {
    !matches!(
        call.get("submission"),
        Some(value) if matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "0")
    )
}

/// Returns the current time in unix milliseconds.
///
/// # Returns
///
/// The number of milliseconds since the Unix epoch, or zero when the clock is
/// outside the representable range.
fn now_millis() -> i64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(0)
}
