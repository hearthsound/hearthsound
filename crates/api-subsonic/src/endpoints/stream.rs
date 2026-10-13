//! The `stream` endpoint: the audio bytes of one track.
//!
//! `stream` answers with the track's media bytes rather than with an envelope,
//! so it takes a [`BinaryCall`](crate::BinaryCall) and maps every failure onto
//! an HTTP status code. The bytes come from
//! [`StreamService`](crate::StreamService), which owns the file lookup, the
//! range and validator handling, and the stream session the scrobble guard
//! reads; this module only reads the request's parameters and the head.
//!
//! The `id` is the only parameter this endpoint needs today. A request without
//! one is a bad request, and an id that names no stored track is a missing
//! resource; neither is answered with a Subsonic envelope, because a client
//! that treats any 200 body as audio would play the error document.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::auth::BinaryCall;
use crate::stream::StreamRequest;

/// Answers `stream`: the track's audio bytes.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names the track; the request's method
///   and headers carry the `Range` and conditional-request headers a seeking
///   client sends. `maxBitRate`, `format`, `timeOffset`, and
///   `estimateContentLength` are not read yet, so every request serves the
///   stored file as it is.
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
    let request = StreamRequest {
        id: id.to_owned(),
        user_id: call.principal().user_id.uuid(),
        client: call.client_name().map(str::to_owned),
        method: call.method().clone(),
        headers: call.headers().clone(),
    };
    match call.state().stream().serve(&request).await {
        Ok(response) => response,
        Err(error) => {
            let status = error.status();
            if status.is_server_error() {
                tracing::error!(id, %error, "could not serve a stream");
            } else {
                tracing::debug!(id, %error, "refused a stream");
            }
            status.into_response()
        }
    }
}
