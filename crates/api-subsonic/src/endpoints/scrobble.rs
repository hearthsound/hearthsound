//! The `scrobble` endpoint: recording a play or a now-playing report.
//!
//! A client reports playback with `scrobble`. Each `id` is a track to record; a
//! submission (`submission=true`, the default) records a play through the
//! activity service, and a now-playing report (`submission=false`) is stored so
//! `getNowPlaying` lists it. The service itself enforces the "no song audio
//! delivered" guard, so a client that gave up before any audio arrived does not
//! create a false play.
//!
//! Only a stored track is recorded. The id is resolved through the catalog
//! layer's redirects, so a play of an id that was merged into an older row is
//! recorded against the survivor. An id that names no row, or names something
//! that is not a track, is accepted and discarded: a client's play history must
//! reference a row the server has.

use hearthsound_activity::Scrobble;
use hearthsound_core::{EntityId, EntityKind, IdPrefix};

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};

use super::browse::number;

/// How far a client's clock may be ahead of the server's, in milliseconds.
///
/// Five minutes: enough for a client whose clock drifted, small enough that a
/// report from the future cannot keep a now-playing row alive.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

/// How old a scrobble may be, in milliseconds.
///
/// Thirty days: no scrobble service accepts a listen older than that, and a
/// timestamp from years ago is a broken client rather than a play.
const MAX_SCROBBLE_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

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
/// An empty successful envelope. A now-playing report is stored so
/// `getNowPlaying` lists it.
///
/// # Errors
///
/// Answers Subsonic error 10 when no `id` is given, a generic error when `time`
/// is not a number or is more than thirty days old, and a generic error when a
/// play cannot be recorded, which means the track the id names is not a row the
/// server can reference.
///
/// A `time` a few minutes in the future is clamped to now, because a client
/// clock may be fast; one far in the future would otherwise keep a now-playing
/// row visible for as long as it claimed.
///
/// A request with an id that cannot be resolved is refused before any play is
/// written: every id is resolved first, so a malformed id late in the list cannot
/// leave the earlier ids recorded under a failed answer.
pub async fn scrobble(call: Call) -> Encoded {
    let ids: Vec<String> = call.get_all("id").map(str::to_owned).collect();
    if ids.is_empty() {
        return call.encode(Response::failed(SubsonicError::missing_parameter("id")));
    }
    let now = crate::params::now_millis();
    let reported = match number(&call, "time", now) {
        Ok(started_at) => started_at,
        Err(error) => return call.encode(Response::failed(error)),
    };
    if reported < now - MAX_SCROBBLE_AGE_MS {
        return call.encode(Response::failed(SubsonicError::new(
            crate::response::codes::GENERIC,
            "A scrobble time is milliseconds since the epoch, at most 30 days ago.",
        )));
    }
    let started_at = if reported > now + CLOCK_SKEW_MS {
        tracing::debug!(
            reported,
            now,
            "a scrobble named a time in the future; it was clamped to now"
        );
        now
    } else {
        reported
    };
    let submission = submission(&call);
    let user_id = call.principal().user_id.uuid();

    // Every id is resolved before anything is written: `resolve` has no write
    // side effect, so a bad id late in the list costs the request instead of
    // leaving the earlier ids recorded under a failed answer.
    let mut tracks = Vec::with_capacity(ids.len());
    for value in &ids {
        match resolve(&call, value).await {
            Ok(Some(track_id)) => tracks.push(track_id),
            Ok(None) => continue,
            Err(()) => return call.encode(Response::failed(SubsonicError::generic())),
        }
    }

    for track_id in tracks {
        let scrobble = Scrobble {
            user_id,
            track_id: track_id.uuid(),
            client: call.client_name(),
            submission,
            started_at,
            listened_ms: 0,
        };
        if let Err(error) = call.state().activity().scrobble(&scrobble).await {
            tracing::error!(error = %error, track = %track_id, "could not record a play");
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    }

    call.encode(Response::ok())
}

/// Resolves one `id` of a scrobble into a stored track.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose catalog layer resolves the redirects
///   a merge leaves.
/// - `value`: the id the client sent.
///
/// # Returns
///
/// The stored track, or `None` when the id names no stored track, which the
/// caller discards.
///
/// # Errors
///
/// Returns `Err(())` when the lookup itself fails, which is a failure of this
/// server and is logged with its cause.
pub(super) async fn resolve(call: &Call, value: &str) -> Result<Option<EntityId>, ()> {
    let Ok(id) = EntityId::parse(value) else {
        tracing::debug!(
            id = value,
            "a scrobble named an id that is not an entity id; it was discarded"
        );
        return Ok(None);
    };
    if id.kind() != IdPrefix::Track {
        tracing::debug!(
            id = %id,
            "a scrobble named an entity that is not a track; it was discarded"
        );
        return Ok(None);
    }
    match call
        .state()
        .catalog()
        .canonical(EntityKind::Track, id)
        .await
    {
        Ok(Some(track)) => Ok(Some(track)),
        Ok(None) => {
            tracing::debug!(
                id = value,
                "a scrobble named a track the server does not store; it was discarded"
            );
            Ok(None)
        }
        Err(error) => {
            tracing::error!(
                id = value,
                error = %error,
                "could not resolve the track of a scrobble"
            );
            Err(())
        }
    }
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
