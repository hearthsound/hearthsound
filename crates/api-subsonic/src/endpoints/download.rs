//! The `download` endpoint: the stored file of one track, as an attachment.
//!
//! A download is not playback. It serves the stored bytes with the range and
//! validator handling a resuming client needs, registers no stream session, and
//! starts no acquisition: a client that asks to download a track the server has
//! not stored is told that it has nothing to give, rather than being handed a
//! stream of an acquisition in progress.

use axum::http::StatusCode;
use axum::http::header::{CONTENT_DISPOSITION, HeaderValue};
use axum::response::{IntoResponse, Response};
use hearthsound_core::ClientId;

use crate::auth::BinaryCall;
use crate::stream::StreamRequest;

/// Answers `download`: the stored file of one track, as an attachment.
///
/// # Parameters
///
/// - `call`: the authenticated call. `id` names a stored track.
///
/// # Returns
///
/// The file's bytes with `Content-Disposition: attachment`, a `304` when the
/// client's copy is current, a `416` for a range past the end of the file, and
/// an empty `400`, `401`, `403`, or `404` when the request, the credentials,
/// the permission, or the track are the problem.
pub async fn download(call: BinaryCall) -> Response {
    if !call.principal().can_download {
        tracing::debug!("a download was refused by the account's permission");
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(id) = call.get("id") else {
        tracing::debug!("a download request carried no id");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let request = StreamRequest {
        id: id.to_owned(),
        user_id: call.principal().user_id.uuid(),
        client: None,
        method: call.method().clone(),
        headers: call.headers().clone(),
        format: crate::stream::StreamFormat::Raw,
        max_bitrate_kbps: None,
        offset: None,
        max_channels: None,
        estimate_content_length: false,
        can_acquire: false,
    };
    match call.state().stream().download(&request).await {
        Ok(response) => {
            let name = file_name(id);
            let mut response = response;
            if let Ok(value) = HeaderValue::from_str(&disposition(&name)) {
                response.headers_mut().insert(CONTENT_DISPOSITION, value);
            }
            response
        }
        Err(error) => {
            let status = error.status();
            if status.is_server_error() {
                tracing::error!(id, %error, "could not serve a download");
            } else {
                tracing::debug!(id, %error, "refused a download");
            }
            error.response()
        }
    }
}

/// Returns the file name a download is offered under.
///
/// # Parameters
///
/// - `id`: the id the client asked for.
///
/// # Returns
///
/// The track's client id with an extension a client can save, which is a name
/// the server knows is its own and therefore safe to offer.
fn file_name(id: &str) -> String {
    let name = match ClientId::parse(id) {
        Ok(ClientId::Entity(entity)) => entity.to_string(),
        _ => id.to_owned(),
    };
    format!("{name}.audio")
}

/// Builds the `Content-Disposition` of a download.
///
/// The plain `filename` carries an ASCII-only form for clients that read no
/// more than that, and the `filename*` carries the same name for the ones that
/// do.
///
/// # Parameters
///
/// - `name`: the file name.
///
/// # Returns
///
/// The header value.
fn disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = name
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect();
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")
}
