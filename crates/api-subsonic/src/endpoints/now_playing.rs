//! The `getNowPlaying` endpoint: what every account the caller may see is
//! playing right now.
//!
//! A now-playing report is stored when a client scrobbles without submitting a
//! play, and it stops being listed once the track has run out (plus a grace
//! period). Who may see whose rows is `privacy.now_playing_visible`: a user's
//! own override when they have one, otherwise the server-wide value. The
//! caller's own rows are always listed.

use hearthsound_config::keys;
use hearthsound_core::EntityId;
use uuid::Uuid;

use crate::auth::Call;
use crate::browse;
use crate::response::{Encoded, Response};
use crate::value::{Array, Object};

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
    let now = crate::params::now_millis();
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
    let page = match browse::SongPage::of(&call, &ids).await {
        Ok(page) => page,
        Err(error) => return call.encode(Response::failed(error)),
    };

    // A track that left the database between the now-playing read and the page
    // is left out rather than listed with stale data.
    let mut playing = Vec::with_capacity(entries.len());
    for entry in &entries {
        let Some(position) = page
            .rows
            .iter()
            .position(|row| row.view.track.id == entry.track_id)
        else {
            continue;
        };
        playing.push(
            page.object(position)
                .set("username", entry.username.clone())
                .set("minutesAgo", (now - entry.started_at).max(0) / 60_000)
                // The protocol asks for a player identity; this server has one
                // player per client instance, and the client name is what a
                // person recognizes.
                .set("playerId", 0)
                .set("playerName", entry.client.clone()),
        );
    }
    call.respond(Response::ok().member(
        "nowPlaying",
        Object::new().set("entry", Array::new(playing)),
    ))
    .await
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
