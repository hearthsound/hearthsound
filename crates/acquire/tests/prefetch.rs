//! Integration tests of the prefetch scheduler.
//!
//! Every test drives the scheduler the way the API layer does: it records the
//! listing a client read, notes the track the client streamed, and then asserts
//! what the acquisition ledger holds and which events were published. The
//! orchestrator's scheduler is never started, so an admitted acquisition stays
//! `queued` and the ledger holds exactly what admission wrote.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{Env, drain};
use hearthsound_acquire::prefetch::PrefetchScheduler;
use hearthsound_acquire::repo::{AcquisitionRow, ListFilter};
use hearthsound_core::events::{QueuePrefetch, ServerEvent};
use hearthsound_core::{AcquisitionState, Availability, ClientId, EntityId, PinReason, Priority};

/// Inserts `count` catalog-only tracks and returns their ids.
///
/// # Parameters
///
/// - `env`: the environment to insert into.
/// - `count`: how many tracks to insert.
async fn remote_tracks(env: &Env, count: usize) -> Vec<EntityId> {
    let mut tracks = Vec::new();
    for number in 1..=count {
        tracks.push(
            env.remote_track(&format!("Song {number}"), "Hello Artist")
                .await,
        );
    }
    tracks
}

/// Returns the client ids of a track list, the way a listing names them.
///
/// # Parameters
///
/// - `tracks`: the stored tracks.
fn listing_of(tracks: &[EntityId]) -> Vec<ClientId> {
    tracks.iter().copied().map(ClientId::Entity).collect()
}

/// Returns the acquisition the ledger holds for one track.
///
/// # Parameters
///
/// - `env`: the environment to read.
/// - `track`: the track the acquisition targets.
async fn acquisition_of(env: &Env, track: EntityId) -> AcquisitionRow {
    let listed = env.orchestrator.list(&ListFilter::default()).await.unwrap();
    let summary = listed
        .into_iter()
        .find(|row| row.track_id == Some(track))
        .unwrap_or_else(|| panic!("no acquisition targets {track}"));
    env.acquisition(summary.id).await
}

/// Returns the `queue.prefetch` events of a drained event list.
///
/// # Parameters
///
/// - `events`: the events a receiver drained.
fn prefetch_events(events: &[ServerEvent]) -> Vec<QueuePrefetch> {
    events
        .iter()
        .filter_map(|event| match event {
            ServerEvent::QueuePrefetch(prefetch) => Some(prefetch.clone()),
            _ => None,
        })
        .collect()
}

/// Streaming the first track of a five-track listing prefetches the next three.
#[tokio::test]
async fn a_stream_prefetches_the_next_lookahead_items() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;

    assert_eq!(
        env.acquisitions_for(tracks[1]).await,
        1,
        "the next item is prefetched"
    );
    assert_eq!(
        env.acquisitions_for(tracks[2]).await,
        1,
        "the second item is prefetched"
    );
    assert_eq!(
        env.acquisitions_for(tracks[3]).await,
        1,
        "the third item is prefetched"
    );
    assert_eq!(
        env.acquisitions_for(tracks[4]).await,
        0,
        "the look-ahead stops at three items"
    );
    assert_eq!(
        env.acquisitions_for(tracks[0]).await,
        0,
        "the streamed track is not prefetched"
    );

    let next = acquisition_of(&env, tracks[1]).await;
    assert_eq!(next.priority, Priority::Next);
    assert_eq!(next.reason, "prefetch");
    assert_eq!(next.requested_by, Some(user));
    assert_eq!(next.state, AcquisitionState::Queued);
    assert_eq!(
        acquisition_of(&env, tracks[2]).await.priority,
        Priority::Queue
    );
    assert_eq!(
        acquisition_of(&env, tracks[3]).await.priority,
        Priority::Queue
    );

    let published = prefetch_events(&drain(&mut events));
    assert_eq!(published.len(), 3, "one event per started prefetch");
    assert_eq!(published[0].client, "symfonium");
    assert_eq!(published[0].track_id, tracks[1].to_string());
    assert_eq!(published[0].priority, Priority::Next);
    assert_eq!(published[0].state, "started");
    assert_eq!(published[1].track_id, tracks[2].to_string());
    assert_eq!(published[1].priority, Priority::Queue);
    assert_eq!(published[2].track_id, tracks[3].to_string());
}

/// Noting the same stream again neither joins new work nor repeats an event.
#[tokio::test]
async fn noting_the_same_stream_again_starts_nothing_new() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;
    let _ = drain(&mut events);

    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;

    for track in &tracks[1..4] {
        assert_eq!(
            env.acquisitions_for(*track).await,
            1,
            "{track} keeps its one acquisition"
        );
    }
    assert!(
        prefetch_events(&drain(&mut events)).is_empty(),
        "nothing new happened, so nothing is reported"
    );
}

/// A jump to the end of the listing cancels every prefetch that left the queue.
#[tokio::test]
async fn a_jump_cancels_the_prefetches_that_left_the_queue() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;
    let _ = drain(&mut events);

    scheduler
        .note_stream(user, "symfonium", &listing[4], true)
        .await;

    for track in &tracks[1..4] {
        assert_eq!(
            acquisition_of(&env, *track).await.state,
            AcquisitionState::Cancelled,
            "{track} left the queue"
        );
        assert_eq!(
            env.track(*track).await.availability,
            Availability::Remote,
            "a cancelled prefetch leaves its track catalog-only"
        );
    }

    let dropped = prefetch_events(&drain(&mut events));
    assert_eq!(dropped.len(), 3, "one event per dropped prefetch");
    assert!(dropped.iter().all(|event| event.state == "dropped"));
    assert!(dropped.iter().all(|event| event.client == "symfonium"));
    let mut reported: Vec<String> = dropped.iter().map(|event| event.track_id.clone()).collect();
    reported.sort_unstable();
    let mut expected: Vec<String> = tracks[1..4].iter().map(EntityId::to_string).collect();
    expected.sort_unstable();
    assert_eq!(reported, expected);
}

/// A stream inside the listing cancels only the item that left the window.
#[tokio::test]
async fn a_stream_inside_the_listing_keeps_the_items_still_upcoming() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;
    let _ = drain(&mut events);

    // The client moved on to track 2: tracks 3 and 4 are still upcoming, and
    // track 5 enters the window.
    scheduler
        .note_stream(user, "symfonium", &listing[1], true)
        .await;

    assert_eq!(
        acquisition_of(&env, tracks[1]).await.state,
        AcquisitionState::Cancelled,
        "the track being played is no longer upcoming"
    );
    assert_eq!(
        acquisition_of(&env, tracks[2]).await.state,
        AcquisitionState::Queued,
        "track 3 keeps its acquisition"
    );
    assert_eq!(
        acquisition_of(&env, tracks[2]).await.priority,
        Priority::Next,
        "track 3 is now the next item"
    );
    assert_eq!(
        acquisition_of(&env, tracks[3]).await.priority,
        Priority::Queue
    );
    assert_eq!(env.acquisitions_for(tracks[4]).await, 1, "track 5 enters");

    let published = prefetch_events(&drain(&mut events));
    assert_eq!(published.len(), 2, "one drop and one start");
    assert_eq!(published[0].state, "dropped");
    assert_eq!(published[0].track_id, tracks[1].to_string());
    assert_eq!(published[1].state, "started");
    assert_eq!(published[1].track_id, tracks[4].to_string());
}

/// `prefetch.enabled` off makes the scheduler record and start nothing.
#[tokio::test]
async fn the_scheduler_does_nothing_when_it_is_disabled() {
    let env = Env::normal(&[("HEARTHSOUND__PREFETCH__ENABLED", "false")]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;

    for track in &tracks {
        assert_eq!(
            env.acquisitions_for(*track).await,
            0,
            "nothing is prefetched while the setting is off"
        );
    }
    assert!(prefetch_events(&drain(&mut events)).is_empty());
}

/// `prefetch.lookahead` bounds how far ahead the scheduler reaches.
#[tokio::test]
async fn the_lookahead_setting_bounds_the_prefetch() {
    let env = Env::normal(&[("HEARTHSOUND__PREFETCH__LOOKAHEAD", "1")]).await;
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;

    assert_eq!(env.acquisitions_for(tracks[1]).await, 1);
    assert_eq!(env.acquisitions_for(tracks[2]).await, 0);
    assert_eq!(
        acquisition_of(&env, tracks[1]).await.priority,
        Priority::Next
    );
}

/// An item that already has a file, and an item inside its cooldown, are left
/// alone.
#[tokio::test]
async fn items_that_are_available_or_cooling_down_are_not_prefetched() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let streamed = env.remote_track("Streamed Song", "Hello Artist").await;
    let local = env
        .add_track("Local Song", "Hello Artist", Availability::Local, None)
        .await;
    let cooling = env
        .add_track(
            "Cooling Song",
            "Hello Artist",
            Availability::Unavailable,
            Some(i64::MAX),
        )
        .await;
    let remote = env.remote_track("Remote Song", "Hello Artist").await;
    let listing = listing_of(&[streamed, local, cooling, remote]);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;

    assert_eq!(
        env.acquisitions_for(local).await,
        0,
        "a local track is skipped"
    );
    assert_eq!(
        env.acquisitions_for(cooling).await,
        0,
        "a cooling-down track is skipped"
    );
    assert_eq!(env.acquisitions_for(remote).await, 1);
    assert_eq!(
        acquisition_of(&env, remote).await.priority,
        Priority::Queue,
        "only the first item of the window is `next`"
    );

    let published = prefetch_events(&drain(&mut events));
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].track_id, remote.to_string());
}

/// A pinned item that leaves the queue keeps downloading.
#[tokio::test]
async fn a_pinned_prefetch_keeps_running_when_it_leaves_the_queue() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;
    env.library
        .add_pin_reason(tracks[2], PinReason::UserLibrary)
        .await
        .unwrap();
    let _ = drain(&mut events);

    scheduler
        .note_stream(user, "symfonium", &listing[4], true)
        .await;

    assert_eq!(
        acquisition_of(&env, tracks[1]).await.state,
        AcquisitionState::Cancelled
    );
    assert_eq!(
        acquisition_of(&env, tracks[2]).await.state,
        AcquisitionState::Queued,
        "a pinned track keeps its acquisition"
    );
    assert_eq!(
        acquisition_of(&env, tracks[3]).await.state,
        AcquisitionState::Cancelled
    );

    let dropped = prefetch_events(&drain(&mut events));
    assert_eq!(
        dropped.len(),
        2,
        "the pinned item is not reported as dropped"
    );
    assert!(
        dropped
            .iter()
            .all(|event| event.track_id != tracks[2].to_string())
    );
}

/// An item past half its download keeps downloading.
#[tokio::test]
async fn a_prefetch_past_half_its_download_keeps_running() {
    let env = Env::normal(&[]).await;
    let mut events = env.events.subscribe();
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 5).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], true)
        .await;
    let half = acquisition_of(&env, tracks[2]).await.id;
    env.orchestrator
        .repository()
        .set_progress(half, Some(r#"{"bytes":600,"totalBytes":1000}"#))
        .await
        .unwrap();
    let _ = drain(&mut events);

    scheduler
        .note_stream(user, "symfonium", &listing[4], true)
        .await;

    assert_eq!(
        acquisition_of(&env, tracks[1]).await.state,
        AcquisitionState::Cancelled
    );
    assert_eq!(
        acquisition_of(&env, tracks[2]).await.state,
        AcquisitionState::Queued,
        "a download past half is finished rather than abandoned"
    );
    assert_eq!(
        acquisition_of(&env, tracks[3]).await.state,
        AcquisitionState::Cancelled
    );
    assert_eq!(prefetch_events(&drain(&mut events)).len(), 2);
}

/// A listing is inferred per user and per client instance.
#[tokio::test]
async fn another_client_or_user_infers_nothing() {
    let env = Env::normal(&[]).await;
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 3).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;
    let other = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "feishin", &listing[0], true)
        .await;
    scheduler
        .note_stream(other, "symfonium", &listing[0], true)
        .await;

    for track in &tracks {
        assert_eq!(
            env.acquisitions_for(*track).await,
            0,
            "{track} is prefetched for nobody"
        );
    }
}

/// A stream of a track no listing contains starts nothing.
#[tokio::test]
async fn a_track_outside_the_listing_infers_nothing() {
    let env = Env::normal(&[]).await;
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 3).await;
    let outside = env.remote_track("Outside Song", "Hello Artist").await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &ClientId::Entity(outside), true)
        .await;

    for track in &tracks {
        assert_eq!(env.acquisitions_for(*track).await, 0);
    }
}

/// A user who may not acquire gets no prefetch either.
#[tokio::test]
async fn a_user_without_the_permission_gets_no_prefetch() {
    let env = Env::normal(&[]).await;
    let scheduler = PrefetchScheduler::new(Arc::clone(&env.orchestrator));
    let tracks = remote_tracks(&env, 3).await;
    let listing = listing_of(&tracks);
    let user = env.add_user().await;

    scheduler.record_listing(user, "symfonium", listing.clone());
    scheduler
        .note_stream(user, "symfonium", &listing[0], false)
        .await;

    for track in &tracks {
        assert_eq!(env.acquisitions_for(*track).await, 0);
    }
}
