//! The `scrobble` endpoint: recording a play or a now-playing report.
//!
//! A client reports playback with `scrobble`. Each `id` is a track to record; a
//! submission (`submission=true`, the default) records a play through the
//! activity service, and a now-playing report (`submission=false`) is accepted
//! and discarded. The service itself enforces the "no song audio delivered"
//! guard, so a client that gave up before any audio arrived does not create a
//! false play.
//!
//! Only a stored track is recorded. A catalog id (`x_`) names an entity the
//! server may not have stored yet: the id is resolved through the provider
//! aliases the catalog layer wrote when it materialized the entity, and is
//! recorded when that resolution finds a row. An entity that was never
//! materialized stays a catalog entity, so it is accepted and discarded, as is
//! any other id that is not a stored track: a client's play history must
//! reference a row the server has.

use hearthsound_activity::Scrobble;
use hearthsound_catalog::aliases::provider_namespace;
use hearthsound_core::{ClientId, EntityId, EntityKind, IdPrefix};

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
/// An empty successful envelope.
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

    for value in &ids {
        let track_id = match resolve(&call, value).await {
            Ok(Some(track_id)) => track_id,
            Ok(None) => continue,
            Err(()) => return call.encode(Response::failed(SubsonicError::generic())),
        };
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
/// A stored track id names the track itself. A catalog id names an entity a
/// provider knows; when the catalog layer has materialized it, the provider id
/// is recorded under the `p:<slug>` namespace of its instance, and that alias is
/// what turns the `x_` id into the stored track. Every other id is not a row the
/// server can reference a play with.
///
/// # Parameters
///
/// - `call`: the authenticated call, whose state holds the catalog layer.
/// - `value`: the id the client sent.
///
/// # Returns
///
/// The stored track, or `None` when the id names no stored track, which the
/// caller discards.
///
/// # Errors
///
/// Returns `Err(())` when the alias lookup itself fails, which is a failure of
/// this server and is logged with its cause.
pub(super) async fn resolve(call: &Call, value: &str) -> Result<Option<EntityId>, ()> {
    match ClientId::parse(value) {
        Ok(ClientId::Entity(id)) if id.kind() == IdPrefix::Track => Ok(Some(id)),
        Ok(ClientId::Entity(id)) => {
            tracing::debug!(
                id = %id,
                "a scrobble named a stored entity that is not a track; it was discarded"
            );
            Ok(None)
        }
        Ok(ClientId::Catalog(catalog)) if catalog.kind() == EntityKind::Track => {
            let namespace = provider_namespace(catalog.slug());
            match call
                .state()
                .catalog()
                .entity_by_alias(&namespace, EntityKind::Track, catalog.provider_id())
                .await
            {
                Ok(Some(entity)) if entity.kind() == IdPrefix::Track => {
                    tracing::debug!(
                        id = value,
                        track = %entity,
                        "a scrobble resolved a catalog id to a stored track"
                    );
                    Ok(Some(entity))
                }
                Ok(_) => {
                    tracing::debug!(
                        id = value,
                        "a scrobble named a catalog track the server has not stored; it was \
                         discarded"
                    );
                    Ok(None)
                }
                Err(error) => {
                    tracing::error!(
                        id = value,
                        error = %error,
                        "could not resolve a catalog id of a scrobble"
                    );
                    Err(())
                }
            }
        }
        Ok(ClientId::Catalog(catalog)) => {
            tracing::debug!(
                id = %catalog,
                "a scrobble named a catalog entity that is not a track; it was discarded"
            );
            Ok(None)
        }
        Err(error) => {
            tracing::debug!(
                id = value,
                error = %error,
                "a scrobble named an id that is not a client id; it was discarded"
            );
            Ok(None)
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
