//! The `getNowPlaying` endpoint: what every account the caller may see is
//! playing right now.
//!
//! A now-playing report is stored when a client scrobbles without submitting a
//! play, and it stops being listed once the track has run out (plus a grace
//! period). Who may see whose rows is `privacy.now_playing_visible`: a user's
//! own override when they have one, otherwise the server-wide value. The
//! caller's own rows are always listed.

use std::collections::HashMap;

use hearthsound_activity::Annotation;
use hearthsound_config::keys;
use hearthsound_core::{EntityId, EntityKind};
use uuid::Uuid;

use crate::auth::Call;
use crate::response::{Encoded, Response};
use crate::value::{Array, Object};
use crate::{browse, projection};

/// Returns the current time in unix milliseconds.
///
/// # Returns
///
/// The instant, from the system clock.
fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Answers `getNowPlaying`: the fresh now-playing rows the caller may see.
///
/// # Parameters
///
/// - `call`: the authenticated call.
///
/// # Returns
///
/// A successful envelope carrying `nowPlaying`, or a failed envelope.
///
/// # Errors
///
/// Answers a generic error when a read fails.
pub async fn get_now_playing(call: Call) -> Encoded {
    let now = now_millis();
    let activity = call.state().activity();
    let players = match activity.now_playing().players(now).await {
        Ok(players) => players,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getNowPlaying", &error)));
        }
    };

    let viewer = call.principal().user_id.uuid();
    let mut visible = Vec::with_capacity(players.len());
    for player in players {
        if player == viewer || visible_to(&call, player).await {
            visible.push(player);
        }
    }
    let entries = match activity.now_playing().visible(&visible, now).await {
        Ok(entries) => entries,
        Err(error) => {
            return call.encode(Response::failed(browse::failure("getNowPlaying", &error)));
        }
    };

    let ids: Vec<EntityId> = entries.iter().map(|entry| entry.track_id).collect();
    let annotations = match annotations(&call, &ids).await {
        Ok(annotations) => annotations,
        Err(error) => return call.encode(Response::failed(error)),
    };

    let database = call.state().database();
    let mut playing = Vec::with_capacity(entries.len());
    for entry in &entries {
        let loaded = match browse::track_any(database, entry.track_id).await {
            Ok(loaded) => loaded,
            Err(error) => {
                return call.encode(Response::failed(browse::failure("getNowPlaying", &error)));
            }
        };
        let Some(loaded) = loaded else {
            continue;
        };
        let child = projection::child(
            &loaded.view,
            loaded.file.as_ref(),
            &loaded.album_artists,
            &annotations
                .get(&entry.track_id.uuid())
                .copied()
                .unwrap_or_default(),
        );
        playing.push(
            child
                .set("username", entry.username.clone())
                .set("minutesAgo", (now - entry.started_at).max(0) / 60_000)
                // The protocol asks for a player identity; this server has one
                // player per client instance, and the client name is what a
                // person recognizes.
                .set("playerId", 0)
                .set("playerName", entry.client.clone()),
        );
    }
    call.encode(Response::ok().member(
        "nowPlaying",
        Object::new().set("entry", Array::new(playing)),
    ))
}

/// Reports whether one account's now playing may be shown.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `user`: the account whose rows are asked about.
///
/// # Returns
///
/// `true` when the account's `privacy.now_playing_visible` override, or the
/// server-wide value of that setting when the account has none, is true. An
/// override that cannot be read is treated as hidden, because a setting that
/// cannot be read must not expose activity.
async fn visible_to(call: &Call, user: Uuid) -> bool {
    let snapshot = call.state().settings_snapshot();
    match call
        .state()
        .user_settings()
        .get_typed(user, keys::PRIVACY_NOW_PLAYING_VISIBLE)
        .await
    {
        Ok(Some(visible)) => visible,
        Ok(None) => match snapshot.get(keys::PRIVACY_NOW_PLAYING_VISIBLE) {
            Ok(visible) => visible,
            Err(error) => {
                tracing::warn!(%error, "could not read privacy.now_playing_visible");
                false
            }
        },
        Err(error) => {
            tracing::warn!(
                user = %user,
                %error,
                "could not read a user's now-playing visibility; hiding it"
            );
            false
        }
    }
}

/// Loads the caller's annotations for the listed tracks.
///
/// # Parameters
///
/// - `call`: the authenticated call.
/// - `ids`: the tracks being played.
///
/// # Returns
///
/// The annotations, keyed by track id.
///
/// # Errors
///
/// Answers a generic error when the read fails.
async fn annotations(
    call: &Call,
    ids: &[EntityId],
) -> Result<HashMap<Uuid, Annotation>, crate::response::SubsonicError> {
    let plain: Vec<Uuid> = ids.iter().map(EntityId::uuid).collect();
    match browse::annotations(
        call.state(),
        call.principal().user_id.uuid(),
        EntityKind::Track,
        &plain,
    )
    .await
    {
        Ok(annotations) => Ok(annotations),
        Err(error) => Err(browse::failure("getNowPlaying", &error)),
    }
}
