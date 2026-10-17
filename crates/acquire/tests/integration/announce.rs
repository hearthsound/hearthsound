//! Tests of the announce gate, driven by an injected clock.
//!
//! The gate takes the current time from its caller, so every window is pinned
//! by offsetting one base instant rather than sleeping.

#![allow(clippy::expect_used)]

use std::time::{Duration, Instant};

use hearthsound_acquire::announce::{ANNOUNCE_INTERVAL, ANNOUNCE_INTERVAL_MS, AnnounceGate};
use hearthsound_acquire::orchestrator::AcquisitionProgress;
use hearthsound_core::{AcquisitionState, FailureReason, Priority};
use uuid::Uuid;

/// A snapshot of an acquisition in `state`, with no progress reported yet.
fn snapshot(id: Uuid, state: AcquisitionState) -> AcquisitionProgress {
    AcquisitionProgress {
        acquisition_id: id,
        target_id: "trk_1".to_owned(),
        state,
        priority: Priority::Queue,
        percent: None,
        eta_secs: None,
        instance: None,
        failure_reason: None,
        file: None,
        requested_by: None,
    }
}

#[test]
fn the_window_is_two_seconds() {
    assert_eq!(ANNOUNCE_INTERVAL_MS, 2000);
    assert_eq!(ANNOUNCE_INTERVAL, Duration::from_millis(2000));
}

#[test]
fn the_first_snapshot_of_an_acquisition_is_published() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Queued), base));
}

#[test]
fn an_unchanged_snapshot_inside_the_window_is_held() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));
    let inside = base + Duration::from_millis(1500);
    assert!(!gate.admit(&snapshot(id, AcquisitionState::Downloading), inside));
}

#[test]
fn an_unchanged_snapshot_is_published_once_the_window_has_passed() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));
    let due = base + ANNOUNCE_INTERVAL;
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), due));
}

#[test]
fn a_state_change_is_published_at_once() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));
    let soon = base + Duration::from_millis(100);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Verifying), soon));
}

#[test]
fn an_instance_or_failure_change_is_published_at_once() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Searching), base));

    let mut with_instance = snapshot(id, AcquisitionState::Searching);
    with_instance.instance = Some("slskd".to_owned());
    let soon = base + Duration::from_millis(10);
    assert!(gate.admit(&with_instance, soon));

    let mut failed = with_instance.clone();
    failed.failure_reason = Some(FailureReason::Timeout);
    let sooner = base + Duration::from_millis(20);
    assert!(gate.admit(&failed, sooner));
}

#[test]
fn a_percentage_change_alone_waits_for_the_window() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));

    let mut progressed = snapshot(id, AcquisitionState::Downloading);
    progressed.percent = Some(40);
    let soon = base + Duration::from_millis(100);
    assert!(!gate.admit(&progressed, soon));

    let held = gate
        .heartbeat(id, base + ANNOUNCE_INTERVAL)
        .expect("a live acquisition has a held snapshot");
    assert_eq!(held.percent, Some(40));
}

#[test]
fn a_heartbeat_returns_the_latest_snapshot_and_restarts_the_window() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));

    let tick = base + ANNOUNCE_INTERVAL;
    let beat = gate.heartbeat(id, tick).expect("a live entry");
    assert_eq!(beat.state, AcquisitionState::Downloading);

    // The tick restarted the window, so a poll right after it is held.
    let just_after = tick + Duration::from_millis(500);
    assert!(!gate.admit(&snapshot(id, AcquisitionState::Downloading), just_after));

    let next_window = tick + ANNOUNCE_INTERVAL;
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), next_window));
}

#[test]
fn a_heartbeat_without_a_live_entry_returns_none() {
    let mut gate = AnnounceGate::default();
    assert!(gate.heartbeat(Uuid::from_u128(9), Instant::now()).is_none());
}

#[test]
fn a_terminal_snapshot_is_published_and_forgets_the_acquisition() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let id = Uuid::from_u128(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Downloading), base));

    let soon = base + Duration::from_millis(1);
    assert!(gate.admit(&snapshot(id, AcquisitionState::Completed), soon));
    assert!(gate.heartbeat(id, soon).is_none());
}

#[test]
fn acquisitions_are_gated_independently() {
    let base = Instant::now();
    let mut gate = AnnounceGate::default();
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    assert!(gate.admit(&snapshot(first, AcquisitionState::Downloading), base));
    assert!(gate.admit(&snapshot(second, AcquisitionState::Downloading), base));

    let soon = base + Duration::from_millis(10);
    assert!(!gate.admit(&snapshot(first, AcquisitionState::Downloading), soon));
    assert!(!gate.admit(&snapshot(second, AcquisitionState::Downloading), soon));
}
