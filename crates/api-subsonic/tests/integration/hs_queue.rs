//! Integration tests for the play queue vendor endpoint.
//!
//! `hsSetQueue` stores nothing: it records the declared queue for the prefetch
//! scheduler and reports the playing track as streamed, which is what makes the
//! scheduler look ahead from the client's own order. These tests cover what the
//! declaration starts, the order the prefetched items are requested in, and the
//! errors of the vendor gate and of the required parameters.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use hearthsound_core::EntityId;
use hearthsound_core::Priority;
use hearthsound_core::events::{EventEnvelope, ServerEvent};

use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// The number of characters in a client id's payload.
const PAYLOAD: usize = 26;

/// Builds a fixture with the acquisition layer and one remote album.
///
/// # Returns
///
/// The fixture and the album's track ids, in playing order.
async fn fixture(tracks: usize) -> (Fixture, Vec<EntityId>) {
    let mut fixture = Fixture::new().await;
    fixture.add_acquisition_layer().await;
    let (_album, tracks) = fixture.remote_album("Faraway", tracks).await;
    (fixture, tracks)
}

/// Replaces each acquisition id in `body` with a marker numbered in order.
fn number_acquisitions(body: &str) -> String {
    let mut names: HashMap<&str, usize> = HashMap::new();
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("ac_") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 3..];
        let payload = &tail[..PAYLOAD.min(tail.len())];
        let complete =
            payload.len() == PAYLOAD && payload.bytes().all(|byte| byte.is_ascii_alphanumeric());
        if !complete {
            out.push_str("ac_");
            rest = tail;
            continue;
        }
        let next = names.len() + 1;
        let number = *names.entry(payload).or_insert(next);
        out.push_str(&format!("ac_{number}"));
        rest = &tail[PAYLOAD..];
    }
    out.push_str(rest);
    out
}

/// Returns the `targetId` values of a listing, in the order they appear.
fn listing_targets(body: &str) -> Vec<&str> {
    body.match_indices("\"targetId\":\"")
        .map(|(start, marker)| {
            let tail = &body[start + marker.len()..];
            &tail[..tail.find('"').unwrap_or(tail.len())]
        })
        .collect()
}

/// Drains the `queue.prefetch` events a receiver has collected.
fn prefetches(
    events: &mut tokio::sync::broadcast::Receiver<EventEnvelope>,
) -> Vec<(Priority, String, String)> {
    let mut found = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let ServerEvent::QueuePrefetch(prefetch) = envelope.event {
            found.push((prefetch.priority, prefetch.state, prefetch.track_id));
        }
    }
    found
}

#[tokio::test]
async fn declaring_a_queue_prefetches_the_next_three_items() {
    let (fixture, tracks) = fixture(4).await;
    let app = fixture.app();
    let mut events = fixture.events.subscribe();
    let query = format!(
        "/rest/hsSetQueue?apiKey={}&hearthsound=1&f=json&client=web&current={}&next={}&next={}&next={}",
        fixture.api_key, tracks[0], tracks[1], tracks[2], tracks[3]
    );

    let probe = get(&app, &query).await;
    assert_ok(&probe);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!(
            "hs-set-queue-json",
            number_acquisitions(&probe.body)
        );
    });

    let started = prefetches(&mut events);
    let priorities: Vec<Priority> = started.iter().map(|(priority, _, _)| *priority).collect();
    assert_eq!(
        priorities,
        vec![Priority::Next, Priority::Queue, Priority::Queue],
        "{started:?}"
    );
    assert!(
        started.iter().all(|(_, state, _)| state == "started"),
        "{started:?}"
    );
    let targets: Vec<&str> = started.iter().map(|(_, _, track)| track.as_str()).collect();
    assert_eq!(
        targets,
        vec![
            tracks[1].to_string().as_str(),
            tracks[2].to_string().as_str(),
            tracks[3].to_string().as_str()
        ]
    );

    let listed = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&listed);
    let targets = listing_targets(&listed.body);
    let stabilized = listed
        .body
        .replace(&tracks[1].to_string(), "tr_faraway_2")
        .replace(&tracks[2].to_string(), "tr_faraway_3")
        .replace(&tracks[3].to_string(), "tr_faraway_4");
    assert_eq!(targets.len(), 3, "{}", listed.body);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-set-queue-acquisitions-json", number_acquisitions(&stabilized));
    });
}

#[tokio::test]
async fn declaring_a_queue_answers_an_empty_response_in_xml() {
    let (fixture, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetQueue?apiKey={}&hearthsound=1&client=web&current={}&next={}",
            fixture.api_key, tracks[0], tracks[1]
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("hs-set-queue-xml", &probe.body);
}

#[tokio::test]
async fn a_queue_that_names_no_client_is_refused() {
    let (fixture, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetQueue?apiKey={}&hearthsound=1&f=json&current={}&next={}",
            fixture.api_key, tracks[0], tracks[1]
        ),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn a_queue_without_upcoming_items_is_refused() {
    let (fixture, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetQueue?apiKey={}&hearthsound=1&f=json&client=web&current={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn a_queue_entry_that_is_not_a_client_id_is_refused() {
    let (fixture, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetQueue?apiKey={}&hearthsound=1&f=json&client=web&current={}&next=nonsense",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn a_queue_declaration_answers_error_70_without_the_vendor_flag() {
    let (fixture, tracks) = fixture(2).await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsSetQueue?apiKey={}&client=web&current={}&next={}",
            fixture.api_key, tracks[0], tracks[1]
        ),
    )
    .await;
    assert_error(&probe, 70);
}
