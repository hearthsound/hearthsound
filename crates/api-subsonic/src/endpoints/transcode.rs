//! The transcoding endpoints: `getTranscodeDecision` and
//! `getTranscodeStream`.
//!
//! The extension has a client describe itself in a JSON body and ask the server
//! what it would do with one media file. The answer is a decision - the file as
//! it is, or an encoding described by a stateless `transcodeParams` string -
//! and the client then asks for the stream with the parameters it was given.
//! Deciding and serving are separate calls because the decision is a
//! description and the stream is the bytes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_core::{ClientId, IdPrefix};
use hearthsound_media::TranscodeFormat;

use crate::auth::{BinaryCall, JsonCall};
use crate::response::{Encoded, Response, SubsonicError};
use crate::stream::{StreamFormat, StreamRequest};
use crate::transcode_decision::{ClientInfo, Decision, SourceStream, TranscodeTarget};
use crate::value::{Array, Object};

/// The `transcodeParams` value that asks for the stored file as it is.
const DIRECT: &str = "direct";

/// Answers `getTranscodeDecision`: what the server would do with one file for
/// one client.
///
/// # Parameters
///
/// - `call`: the authenticated call. `mediaId` names the track and `mediaType`
///   is `song`; the body is the client description the extension defines.
///
/// # Returns
///
/// A successful envelope carrying `transcodeDecision`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 10 when `mediaId` is missing, when the request carried
/// no client description, or when it was made with `GET`, error 0 when
/// `mediaType` is not a value the extension defines or the body is not a client
/// description, error 70 when `mediaType` is `podcast` and when `mediaId` names
/// no stored track, and a generic error when a read fails.
pub async fn get_transcode_decision(call: JsonCall) -> Encoded {
    if call.method() != axum::http::Method::POST {
        tracing::debug!("a transcode decision was asked for with a method that carries no body");
        return call.encode(Response::failed(SubsonicError::new(
            crate::response::codes::MISSING_PARAMETER,
            "A transcode decision is a POST with the client's description.",
        )));
    }
    let Some(media_id) = call.get("mediaId").filter(|value| !value.is_empty()) else {
        return call.encode(Response::failed(SubsonicError::missing_parameter(
            "mediaId",
        )));
    };
    match call.get("mediaType") {
        Some("podcast") => {
            return call.encode(Response::failed(SubsonicError::not_found()));
        }
        Some("song") | None => {}
        Some(other) => {
            return call.encode(Response::failed(SubsonicError::new(
                crate::response::codes::GENERIC,
                format!("`{other}` is not a media type the extension defines."),
            )));
        }
    }
    let client: ClientInfo = match call.body("clientInfo") {
        Ok(client) => client,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let source = match source_stream(&call, media_id).await {
        Ok(source) => source,
        Err(error) => return call.encode(Response::failed(error)),
    };
    let decision = match &source {
        Some(source) => crate::transcode_decision::decide(&client, source),
        // A track the server holds no file for is decided from the profiles
        // alone: the client is told what it would be given once there is a file.
        None => crate::transcode_decision::decide(&client, &SourceStream::default()),
    };
    let description = Object::new()
        .set("canDirectPlay", decision.can_direct_play)
        .set("canTranscode", decision.transcode.is_some())
        .set(
            "transcodeReason",
            Array::named(
                "reason",
                decision
                    .reasons
                    .iter()
                    .map(|reason| reason_literal(*reason)),
            ),
        )
        .set("transcodeParams", params(&decision))
        .set("sourceStream", stream(&source));
    // A track with no file of its own is described by the decision alone: there
    // is no stream to report until the server has one.
    let description = match &source {
        Some(_) => description.set(
            "transcodeStream",
            stream(&decision.transcode.as_ref().map(target_stream)),
        ),
        None => description,
    };
    call.encode(Response::ok().member("transcodeDecision", description))
}

/// Answers `getTranscodeStream`: the bytes a decision asked for.
///
/// # Parameters
///
/// - `call`: the authenticated binary call. `mediaId` names the track,
///   `mediaType` is `song`, `offset` starts the stream that many seconds in, and
///   `transcodeParams` is the value a decision returned.
///
/// # Returns
///
/// The audio bytes, a `304` when the client's copy is current, a `416` for a
/// range past the end of the file, and an empty `400`, `401`, `403`, or `404`
/// when the request, the credentials, the permission, or the track are the
/// problem.
pub async fn get_transcode_stream(call: BinaryCall) -> HttpResponse {
    let Some(media_id) = call.get("mediaId") else {
        tracing::debug!("a transcode stream carried no mediaId");
        return StatusCode::BAD_REQUEST.into_response();
    };
    match call.get("mediaType") {
        Some("podcast") => return StatusCode::NOT_FOUND.into_response(),
        Some("song") | None => {}
        Some(other) => {
            tracing::debug!(
                media_type = other,
                "a transcode stream named an unknown media type"
            );
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let Some(value) = call.get("transcodeParams") else {
        tracing::debug!("a transcode stream carried no transcodeParams");
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(target) = parse_params(value) else {
        tracing::debug!(
            params = value,
            "a transcode stream carried unusable transcodeParams"
        );
        return StatusCode::BAD_REQUEST.into_response();
    };
    let offset = match call.get("offset") {
        Some(seconds) => match seconds.parse::<u64>() {
            Ok(seconds) => Some(std::time::Duration::from_secs(seconds)),
            Err(_) => {
                tracing::debug!(offset = seconds, "a transcode stream carried a bad offset");
                return StatusCode::BAD_REQUEST.into_response();
            }
        },
        None => None,
    };

    let request = StreamRequest {
        id: media_id.to_owned(),
        user_id: call.principal().user_id.uuid(),
        client: call.client_name().map(str::to_owned),
        method: call.method().clone(),
        headers: call.headers().clone(),
        format: match &target {
            Some(target) => StreamFormat::Transcode(target.format),
            None => StreamFormat::Raw,
        },
        max_bitrate_kbps: target.as_ref().map(|target| target.bitrate_kbps),
        offset,
        max_channels: target.as_ref().and_then(|target| target.max_channels),
        estimate_content_length: false,
        can_acquire: call.principal().can_acquire,
    };
    match call.state().stream().serve(&request).await {
        Ok(response) => response,
        Err(error) => {
            let status = error.status();
            if status.is_server_error() {
                tracing::error!(media_id, %error, "could not serve a transcode stream");
            } else {
                tracing::debug!(media_id, %error, "refused a transcode stream");
            }
            error.response()
        }
    }
}

/// Reads the media file one track would be served from.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `value`: the `mediaId` the client sent.
///
/// # Returns
///
/// The file as the decision describes it, or `None` when the track has no file
/// of its own yet.
///
/// # Errors
///
/// Answers Subsonic error 70 when the id names no stored track, and a generic
/// error when a read fails.
async fn source_stream(
    call: &JsonCall,
    value: &str,
) -> Result<Option<SourceStream>, SubsonicError> {
    let track = match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Track => id,
        _ => {
            tracing::debug!("a transcode decision named no stored track");
            return Err(SubsonicError::not_found());
        }
    };
    let mut conn = match call.state().database().readers().acquire().await {
        Ok(conn) => conn,
        Err(error) => return Err(crate::browse::failure("getTranscodeDecision", &error)),
    };
    let file = match hearthsound_library::query::preferred_media_file(
        &mut conn,
        call.state().storage_roots(),
        track,
    )
    .await
    {
        Ok(file) => file,
        Err(error) => return Err(crate::browse::failure("getTranscodeDecision", &error)),
    };
    Ok(file.map(|file| SourceStream {
        container: file.media_file.suffix().to_ascii_lowercase(),
        codec: file
            .media_file
            .codec
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase(),
        audio_channels: file
            .media_file
            .channels
            .and_then(|channels| u8::try_from(channels).ok())
            .unwrap_or(0),
        audio_bitrate: file
            .media_file
            .bitrate_kbps
            .and_then(|kbps| u32::try_from(kbps).ok())
            .unwrap_or(0)
            * 1000,
        audio_samplerate: file
            .media_file
            .sample_rate
            .and_then(|rate| u32::try_from(rate).ok())
            .unwrap_or(0),
        audio_bitdepth: file
            .media_file
            .bit_depth
            .and_then(|depth| u32::try_from(depth).ok())
            .unwrap_or(0),
    }))
}

/// Returns the literal of one transcode reason.
///
/// # Parameters
///
/// - `reason`: the reason.
///
/// # Returns
///
/// The literal the extension defines for it.
fn reason_literal(reason: crate::transcode_decision::TranscodeReason) -> &'static str {
    use crate::transcode_decision::TranscodeReason;
    match reason {
        TranscodeReason::ContainerNotSupported => "ContainerNotSupported",
        TranscodeReason::AudioCodecNotSupported => "AudioCodecNotSupported",
        TranscodeReason::ProtocolNotSupported => "ProtocolNotSupported",
        TranscodeReason::AudioChannelsNotSupported => "AudioChannelsNotSupported",
        TranscodeReason::AudioBitrateNotSupported => "AudioBitrateNotSupported",
        TranscodeReason::CodecLimitationNotMet => "CodecLimitationNotMet",
    }
}

/// Builds the `transcodeParams` value of a decision.
///
/// # Parameters
///
/// - `decision`: the decision.
///
/// # Returns
///
/// `direct`, or the format, the bitrate, and the channel count of the encoding.
fn params(decision: &Decision) -> String {
    match &decision.transcode {
        None => DIRECT.to_owned(),
        Some(target) => match target.max_channels {
            Some(channels) => format!(
                "{}-{}-{channels}",
                target.format.as_str(),
                target.bitrate_kbps
            ),
            None => format!("{}-{}", target.format.as_str(), target.bitrate_kbps),
        },
    }
}

/// Parses a `transcodeParams` value.
///
/// # Parameters
///
/// - `value`: the value a decision returned.
///
/// # Returns
///
/// The target, or `None` for `direct` and for a value this server did not
/// write, which the caller answers as a bad request.
fn parse_params(value: &str) -> Option<Option<TranscodeTarget>> {
    if value == DIRECT {
        return Some(None);
    }
    let mut parts = value.split('-');
    let format = parts.next()?.parse::<TranscodeFormat>().ok()?;
    let bitrate_kbps = parts.next()?.parse::<u32>().ok()?;
    let max_channels = match parts.next() {
        Some(channels) => Some(channels.parse::<u8>().ok()?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Some(TranscodeTarget {
        format,
        bitrate_kbps: bitrate_kbps.max(8),
        max_channels: max_channels.filter(|channels| *channels > 0),
    }))
}

/// Describes one media file for a decision.
///
/// # Parameters
///
/// - `source`: the file, or `None` when the track has none.
///
/// # Returns
///
/// The `sourceStream` or `transcodeStream` member; empty when there is no file
/// to describe.
fn stream(source: &Option<SourceStream>) -> Object {
    match source {
        None => Object::new(),
        Some(source) => Object::new()
            .set("protocol", "http")
            .set("container", source.container.clone())
            .set("codec", source.codec.clone())
            .set("audioChannels", i64::from(source.audio_channels))
            .set("audioBitrate", i64::from(source.audio_bitrate))
            // This server writes no audio profile, and the extension asks for
            // the member.
            .set("audioProfile", "")
            .set("audioSamplerate", i64::from(source.audio_samplerate))
            .set("audioBitdepth", i64::from(source.audio_bitdepth)),
    }
}

/// Describes the stream an encoding would produce.
///
/// # Parameters
///
/// - `target`: the encoding, or `None` for a direct play.
///
/// # Returns
///
/// The `transcodeStream` member, empty for a direct play.
fn target_stream(target: &TranscodeTarget) -> SourceStream {
    SourceStream {
        container: target.format.as_str().to_owned(),
        codec: target.format.as_str().to_owned(),
        audio_channels: target.max_channels.unwrap_or(0),
        audio_bitrate: target.bitrate_kbps * 1000,
        audio_samplerate: 0,
        audio_bitdepth: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target's parameters are what the parser reads back.
    #[test]
    fn the_parameters_round_trip() {
        for target in [
            TranscodeTarget {
                format: TranscodeFormat::Mp3,
                bitrate_kbps: 320,
                max_channels: None,
            },
            TranscodeTarget {
                format: TranscodeFormat::Opus,
                bitrate_kbps: 96,
                max_channels: Some(1),
            },
            TranscodeTarget {
                format: TranscodeFormat::Aac,
                bitrate_kbps: 256,
                max_channels: Some(2),
            },
        ] {
            let decision = Decision {
                can_direct_play: false,
                transcode: Some(target.clone()),
                reasons: Vec::new(),
            };
            let text = params(&decision);
            assert_eq!(
                parse_params(&text),
                Some(Some(target)),
                "the parameters read back as the target they describe"
            );
        }
    }

    /// `direct` is a direct play, and anything else this server did not write
    /// is not a target.
    #[test]
    fn only_the_parameters_this_server_writes_parse() {
        assert_eq!(parse_params("direct"), Some(None));
        for value in [
            "flac-320",
            "mp3",
            "mp3-",
            "mp3-320-2-extra",
            "mp3-high",
            "opus-96-lots",
        ] {
            assert_eq!(parse_params(value), None, "`{value}` is not a target");
        }
    }
}
