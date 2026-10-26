//! The `reportPlayback` endpoint: the OpenSubsonic playback report.
//!
//! A client that implements the playback-report extension tells the server what
//! it is doing instead of waiting to scrobble a finished play: it reports that
//! playback started, that it is playing, paused, or stopped, how far into the
//! track it is, and at what rate. The activity layer turns that series of
//! reports into the now-playing row a listener sees and into the play a
//! finished session earns, which is why this endpoint resolves the id the same
//! way `scrobble` does.

use hearthsound_activity::{MAX_PLAYBACK_RATE, PlaybackReport, PlaybackState};

use crate::auth::Call;
use crate::browse;
use crate::response::{Encoded, Response, SubsonicError, codes};

use super::scrobble::resolve;

/// Answers `reportPlayback`: applies one playback report.
///
/// # Parameters
///
/// - `call`: the authenticated call. `mediaId` names the track, `mediaType` is
///   `song`, `positionMs` is how far into it the client is - a position past the
///   track's end is applied as its end, `state` is
///   `starting`, `playing`, `paused`, or `stopped`, `playbackRate` is the rate
///   (default 1.0), and `ignoreScrobble` asks the server to record no play for
///   the session.
///
/// # Returns
///
/// An empty successful envelope, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `mediaId`, `positionMs`, or `state` is
/// missing or not a value the extension defines, error 0 when `playbackRate` is
/// not a number, is above the fastest rate the server accepts, or
/// `ignoreScrobble` is not a boolean, error 70 when `mediaType` is `podcast` or
/// `mediaId` names no stored track, and a generic error when the report cannot
/// be applied.
pub async fn report_playback(call: Call) -> Encoded {
    let Some(media_id) = call.get("mediaId").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "mediaId",
        )));
    };
    // The extension defines a song report and a podcast report; this server
    // serves songs, so a podcast is unknown here and anything else is a value
    // the extension does not define.
    match call.get("mediaType") {
        Some("podcast") => {
            return call.encode(Response::failed(SubsonicError::not_found()));
        }
        Some("song") | None => {}
        Some(other) => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                format!("`{other}` is not a media type the extension defines."),
            )));
        }
    }
    let Some(position) = call.get("positionMs").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "positionMs",
        )));
    };
    let reported_ms = match position.parse::<i64>() {
        Ok(position) => position.max(0),
        Err(_) => {
            return call.encode(Response::failed(SubsonicError::new(
                codes::GENERIC,
                "A playback position is a number of milliseconds.",
            )));
        }
    };
    let Some(state) = call.get("state").and_then(PlaybackState::parse) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter("state")));
    };
    let rate = match call.get("playbackRate").filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<f64>() {
            Ok(rate) if rate.is_finite() && rate > 0.0 && rate <= MAX_PLAYBACK_RATE => rate,
            // The session clamps as well, so a wrong report cannot inflate the
            // listening time even if it reaches the layer another way.
            _ => {
                return call.encode(Response::failed(SubsonicError::new(
                    codes::GENERIC,
                    "A playback rate is greater than 0 and at most 8.",
                )));
            }
        },
        None => 1.0,
    };
    let ignore_scrobble = match call.get("ignoreScrobble").filter(|value| !value.is_empty()) {
        Some(value) => match value.parse::<bool>() {
            Ok(ignore) => ignore,
            Err(_) => {
                return call.encode(Response::failed(SubsonicError::new(
                    codes::GENERIC,
                    "`ignoreScrobble` is true or false.",
                )));
            }
        },
        None => false,
    };

    let track = match resolve(&call, media_id).await {
        Ok(Some(track)) => track,
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(()) => {
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    };
    let library = hearthsound_library::LibraryRepository::new(call.state().database());
    let duration_ms = match library.get_track(track).await {
        Ok(Some(row)) => row.duration_ms.unwrap_or(0),
        Ok(None) => return call.encode(Response::failed(SubsonicError::not_found())),
        Err(error) => {
            return call.encode(Response::failed(browse::failure("reportPlayback", &error)));
        }
    };

    // A position past the end of the track is applied as the end of the track:
    // the activity layer turns the position into earned listening time, so an
    // impossible one would inflate the credit and every completion test with it.
    let position_ms = if duration_ms > 0 {
        reported_ms.min(duration_ms)
    } else {
        reported_ms
    };

    // A report and a scrobble of the same client must key the same row, so an
    // unnamed client is stored under the name the activity layer defaults to
    // rather than under an empty one.
    let client = call
        .params()
        .client_name()
        .unwrap_or(hearthsound_activity::DEFAULT_CLIENT);
    let report = PlaybackReport {
        user_id: call.principal().user_id.uuid(),
        client,
        track_id: track,
        duration_ms,
        state,
        position_ms,
        rate,
        ignore_scrobble,
        now: crate::params::now_millis(),
    };
    match call.state().activity().reports().report(&report).await {
        Ok(_) => call.encode(Response::ok()),
        Err(error) => call.encode(Response::failed(browse::failure("reportPlayback", &error))),
    }
}
