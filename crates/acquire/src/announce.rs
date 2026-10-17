//! The gate that decides which acquisition progress updates reach the event stream.
//!
//! Progress snapshots arrive from poll loops, from state transitions and from
//! the download heartbeat. Publishing every one of them would flood the stream
//! on a fast poll, and publishing none would leave a slow download silent. The
//! gate publishes a change of state, priority, instance, failure reason or
//! file at once, and otherwise at most one snapshot per
//! [`ANNOUNCE_INTERVAL`] per acquisition. The caller passes the current time
//! in, so the window can be pinned in tests without sleeping.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::orchestrator::AcquisitionProgress;

/// How often a running acquisition's progress is republished, in milliseconds.
pub const ANNOUNCE_INTERVAL_MS: u64 = 2000;

/// The window between two publishes of an acquisition whose state did not change.
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_millis(ANNOUNCE_INTERVAL_MS);

/// The time an acquisition's snapshot was last published, and that snapshot's
/// latest values, which may be newer than the published copy.
type Entry = (Instant, AcquisitionProgress);

/// Decides which progress snapshots are published, one entry per acquisition
/// that has not reached a terminal state.
#[derive(Debug, Default)]
pub struct AnnounceGate {
    /// The last published time and the latest snapshot of each live acquisition.
    entries: HashMap<Uuid, Entry>,
}

impl AnnounceGate {
    /// Decides whether `progress` should be published now.
    ///
    /// The snapshot always becomes the latest value of its acquisition, even
    /// when it is not published, so a later heartbeat carries the newest
    /// percentage. A terminal snapshot is always published and forgets its
    /// acquisition.
    ///
    /// # Parameters
    ///
    /// - `progress`: the snapshot the caller wants to announce.
    /// - `now`: the current time, supplied by the caller.
    ///
    /// # Returns
    ///
    /// `true` when the snapshot should be published.
    pub fn admit(&mut self, progress: &AcquisitionProgress, now: Instant) -> bool {
        let id = progress.acquisition_id;
        if progress.is_terminal() {
            self.entries.remove(&id);
            return true;
        }
        match self.entries.get_mut(&id) {
            None => {
                self.entries.insert(id, (now, progress.clone()));
                true
            }
            Some((published_at, latest)) => {
                let changed = significantly_changed(latest, progress);
                let due = now.saturating_duration_since(*published_at) >= ANNOUNCE_INTERVAL;
                *latest = progress.clone();
                if changed || due {
                    *published_at = now;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Returns the latest snapshot of an acquisition for a heartbeat to publish.
    ///
    /// The heartbeat restarts the window, so a poll that lands right after it
    /// does not publish the same progress a second time.
    ///
    /// # Parameters
    ///
    /// - `acquisition_id`: the acquisition the heartbeat is for.
    /// - `now`: the current time, supplied by the caller.
    ///
    /// # Returns
    ///
    /// The latest snapshot, or `None` when the acquisition has no live entry.
    pub fn heartbeat(&mut self, acquisition_id: Uuid, now: Instant) -> Option<AcquisitionProgress> {
        let (published_at, latest) = self.entries.get_mut(&acquisition_id)?;
        *published_at = now;
        Some(latest.clone())
    }
}

/// Reports whether a change must be published at once, regardless of the window.
///
/// # Parameters
///
/// - `previous`: the snapshot last seen for the acquisition.
/// - `next`: the snapshot that arrived.
///
/// # Returns
///
/// `true` when the state, priority, instance, failure reason or file differs.
/// Percentage and estimate changes alone do not count.
fn significantly_changed(previous: &AcquisitionProgress, next: &AcquisitionProgress) -> bool {
    previous.state != next.state
        || previous.priority != next.priority
        || previous.instance != next.instance
        || previous.failure_reason != next.failure_reason
        || previous.file != next.file
}
