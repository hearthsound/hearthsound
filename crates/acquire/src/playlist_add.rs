//! Acquisition of the tracks a playlist edit added.
//!
//! A permanent playlist pins the tracks it holds, so an edit that appends to
//! one is the user saying they mean to keep those tracks. When
//! `library.acquire_on_playlist_add` is on, a track that is not stored is
//! requested at priority `queue` under the editor's permission and quota. That
//! step runs after the playlist row is already written, from both the Subsonic
//! playlist endpoints and the native playlist route, so it lives here instead
//! of in either API crate.
//!
//! Nothing here fails an edit: the tracks are in the playlist either way, and a
//! refused or failed request is logged rather than answered.

use hearthsound_core::EntityId;
use uuid::Uuid;

use crate::adopt::acquire_playlist_adds;
use crate::orchestrator::AcquisitionOrchestrator;

/// Acquires the tracks one playlist edit added.
///
/// Nothing is requested when there is no orchestrator, when the edit added no
/// track, or when `library.acquire_on_playlist_add` is false. A refusal or an
/// error is logged and discarded, because the playlist is stored already.
///
/// # Parameters
///
/// - `orchestrator`: the acquisition orchestrator, or `None` for a server
///   built without an acquisition layer.
/// - `user`: the user who edited the playlist, recorded as the requester.
/// - `can_acquire`: whether that user holds the `can_acquire` permission.
/// - `tracks`: the tracks the edit added.
///
/// # Examples
///
/// ```no_run
/// # async fn example(
/// #     orchestrator: &hearthsound_acquire::orchestrator::AcquisitionOrchestrator,
/// #     user: uuid::Uuid,
/// #     track: hearthsound_core::EntityId,
/// # ) {
/// use hearthsound_acquire::playlist_add::acquire_playlist_additions;
///
/// acquire_playlist_additions(Some(orchestrator), user, true, &[track]).await;
/// # }
/// ```
pub async fn acquire_playlist_additions(
    orchestrator: Option<&AcquisitionOrchestrator>,
    user: Uuid,
    can_acquire: bool,
    tracks: &[EntityId],
) {
    if tracks.is_empty() {
        return;
    }
    let Some(orchestrator) = orchestrator else {
        return;
    };
    if let Err(error) = acquire_playlist_adds(orchestrator, user, can_acquire, tracks).await {
        tracing::warn!(%user, %error, "could not acquire what a user added to a playlist");
    }
}
