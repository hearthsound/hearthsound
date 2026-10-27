//! The `stream` endpoint: the audio bytes of one track.
//!
//! `stream` answers with the track's media bytes rather than with an envelope,
//! so it takes a [`BinaryCall`](crate::BinaryCall) and maps every failure onto
//! an HTTP status code. The bytes come from
//! [`StreamService`](crate::StreamService), which owns the file lookup, the
//! transcode, the range and validator handling, and the stream session the
//! scrobble guard reads; this module only reads the request's parameters and
//! the head.
//!
//! The parameters this endpoint reads are `id`, `format`, `maxBitRate`,
//! `timeOffset`, and `estimateContentLength`. A value it cannot make sense of -
//! a missing id, an unknown format, a cap or an offset that is not a number -
//! is a bad request rather than a Subsonic error envelope, because a client
//! that treats any 200 body as audio would play the error document.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hearthsound_core::{EntityId, IdPrefix};
use hearthsound_media::TranscodeFormat;

use crate::auth::BinaryCall;
use crate::stream::{StreamFormat, StreamRequest};

/// Answers `stream`: the track's audio bytes.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the track; `format` asks for a
///   transcode format (`mp3`, `opus`, `aac`) or for `raw`, the stored file
///   untouched; `maxBitRate` caps the bitrate in kbit/s and its `0` means no
///   cap; `timeOffset` starts the stream that many seconds in;
///   `estimateContentLength` asks for a length on a transcode. The request's
///   method and headers carry the `Range` and conditional-request headers a
///   seeking client sends.
///
/// # Returns
///
/// The audio bytes, a `304` when the client's copy is current, a `416` for a
/// range past the end of the file, and an empty `400`, `401`, or `404` when the
/// request, the credentials, or the track are the problem.
pub async fn stream(call: BinaryCall) -> Response {
    let Some(id) = call.get("id") else {
        tracing::debug!("a stream request carried no id");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let format = match read_format(&call) {
        Ok(format) => format,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let max_bitrate_kbps = match read_max_bitrate(&call) {
        Ok(max_bitrate_kbps) => max_bitrate_kbps,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let offset = match read_offset(&call) {
        Ok(offset) => offset,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let request = StreamRequest {
        id: id.to_owned(),
        user_id: call.principal().user_id.uuid(),
        client: call.client_name().map(str::to_owned),
        method: call.method().clone(),
        headers: call.headers().clone(),
        format,
        max_bitrate_kbps,
        offset,
        max_channels: None,
        can_acquire: call.principal().can_acquire,
    };
    match call.state().stream().serve(&request).await {
        Ok(response) => {
            note_stream(&call, id);
            response
        }
        Err(error) => {
            let status = error.status();
            if status.is_server_error() {
                tracing::error!(id, %error, "could not serve a stream");
            } else {
                tracing::debug!(id, %error, "refused a stream");
            }
            error.response()
        }
    }
}

/// Reports a served track to the prefetch scheduler.
///
/// The scheduler infers the queue a client is about to play from the listing it
/// last read, and a streamed track is the signal that the client reached that
/// listing. The work is spawned rather than awaited, because a look-ahead must
/// never delay the audio: the response is already built when this runs, and a
/// state built without a scheduler notes nothing.
///
/// # Parameters
///
/// - `call`: the authenticated call. Its principal names the user and its
///   permission, and its `c` parameter names the client instance.
/// - `id`: the `id` parameter the stream was served for, which names a track.
fn note_stream(call: &BinaryCall, id: &str) {
    let Some(prefetch) = call.state().prefetch().cloned() else {
        return;
    };
    let Ok(track) = EntityId::parse(id) else {
        return;
    };
    if track.kind() != IdPrefix::Track {
        return;
    }
    let user = call.principal().user_id.uuid();
    let client = call.client_name().unwrap_or_default().to_owned();
    let can_acquire = call.principal().can_acquire;
    tokio::spawn(async move {
        prefetch
            .note_stream(user, &client, &track, can_acquire)
            .await;
    });
}

/// Reads the `format` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// [`StreamFormat::Raw`] for `format=raw`, the named format for one the server
/// can encode, and [`StreamFormat::Unspecified`] when the parameter is absent
/// or empty.
///
/// # Errors
///
/// Returns `Err(())` for a format the server cannot encode, which the caller
/// answers as a bad request.
fn read_format(call: &BinaryCall) -> Result<StreamFormat, ()> {
    let Some(value) = call.get("format").map(str::trim) else {
        return Ok(StreamFormat::Unspecified);
    };
    match value {
        "" => Ok(StreamFormat::Unspecified),
        "raw" => Ok(StreamFormat::Raw),
        name => match name.parse::<TranscodeFormat>() {
            Ok(format) => Ok(StreamFormat::Transcode(format)),
            Err(error) => {
                tracing::debug!(value = name, %error, "a stream request named a format the server cannot encode");
                Err(())
            }
        },
    }
}

/// Reads the `maxBitRate` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The cap in kbit/s, or `None` when the parameter is absent, empty, or `0`,
/// which the protocol defines as no limit.
///
/// # Errors
///
/// Returns `Err(())` for a value that is not a bitrate.
fn read_max_bitrate(call: &BinaryCall) -> Result<Option<u32>, ()> {
    let Some(value) = call.get("maxBitRate").map(str::trim) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<u32>() {
        Ok(0) => Ok(None),
        Ok(kbps) => Ok(Some(kbps)),
        Err(error) => {
            tracing::debug!(value, %error, "a stream request carried a non-numeric bitrate cap");
            Err(())
        }
    }
}

/// Reads the `timeOffset` parameter.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// The start offset, or `None` when the parameter is absent or empty.
///
/// # Errors
///
/// Returns `Err(())` for a value that is not a number of seconds, including a
/// negative one.
fn read_offset(call: &BinaryCall) -> Result<Option<Duration>, ()> {
    let Some(value) = call.get("timeOffset").map(str::trim) else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    match value
        .parse::<f64>()
        .ok()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
    {
        Some(offset) => Ok(Some(offset)),
        None => {
            tracing::debug!(value, "a stream request carried an unusable time offset");
            Err(())
        }
    }
}
