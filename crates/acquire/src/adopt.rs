//! Acquiring what a user adopts.
//!
//! Saving or liking an item, and adding a track to a playlist that is not
//! temporary, are statements that the user wants to keep it. When the item is
//! not stored, `library.acquire_on_save` and `library.acquire_on_playlist_add`
//! (both on by default) turn that statement into an acquisition at priority
//! `queue`, so the item is on disk by the time it is played. The acquisition
//! carries the user as its requester, so their `can_acquire` permission and
//! their daily quota decide whether it may start.
//!
//! Adoption never depends on the acquisition: the like or the playlist entry
//! is already written when these helpers run, and a refusal, a cooldown or an
//! error is reported back for logging only. A stored track answers
//! "already available" and costs nothing.
//!
//! # Examples
//!
//! ```no_run
//! # async fn example(
//! #     orchestrator: &hearthsound_acquire::orchestrator::AcquisitionOrchestrator,
//! #     user: uuid::Uuid,
//! #     album: hearthsound_core::EntityId,
//! # ) -> Result<(), hearthsound_acquire::AcquireError> {
//! use hearthsound_acquire::adopt::acquire_saved;
//! use hearthsound_core::EntityKind;
//!
//! let report = acquire_saved(orchestrator, user, true, &[(EntityKind::Album, album)]).await?;
//! println!("{} acquisitions started", report.started);
//! # Ok(())
//! # }
//! ```

use hearthsound_config::keys;
use hearthsound_core::{EntityId, EntityKind, Priority};
use uuid::Uuid;

use crate::error::AcquireError;
use crate::orchestrator::{
    AcquireOutcome, AcquireRequest, AcquisitionOrchestrator, AlbumAcquireRequest,
};

/// The reason an acquisition started by a save or a like records.
pub const SAVE_REASON: &str = "save";

/// The reason an acquisition started by a playlist addition records.
pub const PLAYLIST_REASON: &str = "playlist";

/// What adopting a set of items asked of the acquisition layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AdoptionReport {
    /// Tracks whose acquisition started.
    pub started: usize,
    /// Tracks that joined an acquisition already running.
    pub joined: usize,
    /// Tracks that are already stored.
    pub available: usize,
    /// Tracks refused because of the permission or the quota.
    pub denied: usize,
    /// Tracks whose last attempt failed recently and are cooling down.
    pub cooling_down: usize,
}

impl AdoptionReport {
    /// Counts one outcome.
    fn count(&mut self, outcome: &AcquireOutcome) {
        match outcome {
            AcquireOutcome::Started(_) => self.started += 1,
            AcquireOutcome::Joined(_) => self.joined += 1,
            AcquireOutcome::AlreadyAvailable { .. } => self.available += 1,
            AcquireOutcome::Denied { .. } => self.denied += 1,
            AcquireOutcome::CoolingDown { .. } => self.cooling_down += 1,
        }
    }
}

/// Acquires the tracks of items a user saved or liked.
///
/// A track is requested on its own and an album track by track; an artist or
/// a playlist acquires nothing, because liking an artist adopts no track.
/// Nothing is requested when `library.acquire_on_save` is false.
///
/// # Parameters
///
/// - `orchestrator`: the acquisition orchestrator.
/// - `user`: the user who adopted the items, recorded as the requester.
/// - `can_acquire`: whether the user holds the `can_acquire` permission.
/// - `entities`: the adopted items with their kinds.
///
/// # Returns
///
/// What the requests led to.
///
/// # Errors
///
/// Returns [`AcquireError`] when a request cannot be read or written; the
/// requests made before it stand.
pub async fn acquire_saved(
    orchestrator: &AcquisitionOrchestrator,
    user: Uuid,
    can_acquire: bool,
    entities: &[(EntityKind, EntityId)],
) -> Result<AdoptionReport, AcquireError> {
    let mut report = AdoptionReport::default();
    if !orchestrator
        .settings()
        .current()
        .get(keys::LIBRARY_ACQUIRE_ON_SAVE)?
    {
        return Ok(report);
    }
    for (kind, id) in entities {
        match kind {
            EntityKind::Track => {
                let outcome = orchestrator
                    .request(track_request(*id, SAVE_REASON, user, can_acquire))
                    .await?;
                report.count(&outcome);
            }
            EntityKind::Album => {
                let outcomes = orchestrator
                    .request_album(AlbumAcquireRequest {
                        album_id: *id,
                        priority: Priority::Queue,
                        reason: SAVE_REASON.to_owned(),
                        requested_by: Some(user),
                        can_acquire,
                        force: false,
                    })
                    .await?;
                for track in &outcomes {
                    report.count(&track.outcome);
                }
            }
            _ => {}
        }
    }
    log_report("a save", user, &report);
    Ok(report)
}

/// Acquires tracks a user added to a playlist that is not temporary.
///
/// Nothing is requested when `library.acquire_on_playlist_add` is false. The
/// caller passes only the tracks of non-temporary playlists: a temporary
/// playlist pins nothing, so adding to it is no statement of keeping.
///
/// # Parameters
///
/// - `orchestrator`: the acquisition orchestrator.
/// - `user`: the user who edited the playlist, recorded as the requester.
/// - `can_acquire`: whether the user holds the `can_acquire` permission.
/// - `tracks`: the added tracks.
///
/// # Returns
///
/// What the requests led to.
///
/// # Errors
///
/// Returns [`AcquireError`] when a request cannot be read or written; the
/// requests made before it stand.
pub async fn acquire_playlist_adds(
    orchestrator: &AcquisitionOrchestrator,
    user: Uuid,
    can_acquire: bool,
    tracks: &[EntityId],
) -> Result<AdoptionReport, AcquireError> {
    let mut report = AdoptionReport::default();
    if !orchestrator
        .settings()
        .current()
        .get(keys::LIBRARY_ACQUIRE_ON_PLAYLIST_ADD)?
    {
        return Ok(report);
    }
    for track in tracks {
        let outcome = orchestrator
            .request(track_request(*track, PLAYLIST_REASON, user, can_acquire))
            .await?;
        report.count(&outcome);
    }
    log_report("a playlist addition", user, &report);
    Ok(report)
}

/// Builds the request for one adopted track.
fn track_request(track: EntityId, reason: &str, user: Uuid, can_acquire: bool) -> AcquireRequest {
    AcquireRequest::new(track, Priority::Queue, reason)
        .with_requested_by(Some(user))
        .with_can_acquire(can_acquire)
}

/// Logs what an adoption led to, when it asked for anything.
fn log_report(trigger: &str, user: Uuid, report: &AdoptionReport) {
    if report.started + report.joined + report.denied + report.cooling_down > 0 {
        tracing::debug!(
            %user,
            started = report.started,
            joined = report.joined,
            denied = report.denied,
            cooling_down = report.cooling_down,
            "acquisitions requested for {trigger}"
        );
    }
}
