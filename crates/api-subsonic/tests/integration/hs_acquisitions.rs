//! Integration tests for the acquisition vendor endpoints.
//!
//! `hsGetAvailability` is answered from the same facts the `hearthsound` member
//! is built from, so these tests cover what the availability list reports: the
//! state of a stored entity, the requesting account's library membership, the
//! acquisition in flight for a track, the aggregation an album's entry is, and
//! the ids that are left out. The error codes of the vendor gate and of a
//! missing parameter are covered here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_acquire::orchestrator::{AcquireOutcome, AcquireRequest};
use hearthsound_core::{CatalogId, EntityId, EntityKind, IdPrefix, Priority, Slug};

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// The filters that make an availability snapshot stable across runs.
///
/// An acquisition's client id carries the UUIDv7 its admission generated, so it
/// differs from run to run; the marker keeps the snapshot readable.
const FILTERS: [(&str, &str); 2] = [SNAPSHOT_FILTERS[0], (r"ac_[0-9a-z]{26}", "ac_ACQUISITION")];

/// Builds the `x_` id of one hello-catalog entity.
fn catalog_id(kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new("hello").unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

/// Builds a fixture with the seeded library, the acquisition layer, and one
/// remote album of two tracks.
///
/// # Returns
///
/// The fixture, the seeded ids, and the remote album with its tracks.
async fn fixture() -> (Fixture, Seeded, EntityId, Vec<EntityId>) {
    let mut fixture = Fixture::new().await;
    fixture.add_acquisition_layer().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    let (album, tracks) = fixture.remote_album("Faraway", 2).await;
    (fixture, seeded, album, tracks)
}

/// Builds a fixture with the hello catalog installed and the acquisition layer.
async fn catalog_fixture() -> (Fixture, Seeded) {
    let mut fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    fixture.add_acquisition_layer().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    (fixture, seeded)
}

/// Admits an acquisition for `track` on behalf of the fixture's account.
///
/// # Returns
///
/// The acquisition's id.
async fn acquire(fixture: &Fixture, track: EntityId) -> uuid::Uuid {
    let outcome = fixture
        .state
        .acquisitions()
        .unwrap()
        .request(
            AcquireRequest::new(track, Priority::Queue, "test")
                .with_requested_by(Some(fixture.user_id.uuid())),
        )
        .await
        .unwrap();
    match outcome {
        AcquireOutcome::Started(handle) => handle.id,
        other => panic!("expected a started acquisition, got {other:?}"),
    }
}

#[tokio::test]
async fn availability_reports_states_and_membership_in_both_encodings() {
    let (fixture, seeded, album, tracks) = fixture().await;
    let app = fixture.app();
    let query = format!(
        "/rest/hsGetAvailability?apiKey={}&hearthsound=1&id={}&id={}&id={}&id={}&id={}",
        fixture.api_key, seeded.alpha_tracks[0], seeded.cache_track, album, tracks[0], seeded.alpha
    );
    // The album and its tracks were inserted by this test, so their ids are the
    // UUIDv7 the insert generated; the markers keep the snapshot readable.
    let stabilize = |body: &str| {
        seeded
            .stabilize(body)
            .replace(&album.to_string(), "al_faraway")
            .replace(&tracks[0].to_string(), "tr_faraway_1")
    };

    let json = get(&app, &format!("{query}&f=json")).await;
    assert_ok(&json);
    insta::with_settings!({filters => FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-json", stabilize(&json.body));
    });

    let xml = get(&app, &query).await;
    assert_ok(&xml);
    insta::with_settings!({filters => FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-xml", stabilize(&xml.body));
    });
}

#[tokio::test]
async fn an_acquiring_track_carries_its_acquisition() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    let expected = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();
    assert!(
        probe.body.contains("\"state\":\"acquiring\""),
        "{}",
        probe.body
    );
    assert!(
        probe
            .body
            .contains(&format!("\"acquisitionId\":\"{expected}\"")),
        "{}",
        probe.body
    );
    assert!(!probe.body.contains("percent"), "{}", probe.body);
    assert!(!probe.body.contains("etaSecs"), "{}", probe.body);
}

#[tokio::test]
async fn an_album_aggregates_over_its_member_tracks() {
    let (fixture, _seeded, album, tracks) = fixture().await;
    let app = fixture.app();
    acquire(&fixture, tracks[0]).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1&f=json&id={album}&id={}",
            fixture.api_key, tracks[1]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe
            .body
            .contains(&format!("\"id\":\"{album}\",\"state\":\"acquiring\"")),
        "{}",
        probe.body
    );
    assert!(probe.body.contains("\"inLibrary\":false"), "{}", probe.body);
    assert!(!probe.body.contains("acquisitionId"), "{}", probe.body);
}

#[tokio::test]
async fn an_unknown_stored_id_is_left_out() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let unknown = EntityId::new(IdPrefix::Track);

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1&f=json&id={unknown}&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&format!("\"id\":\"{}\"", tracks[0])),
        "{}",
        probe.body
    );
    assert!(
        !probe.body.contains(&format!("\"id\":\"{unknown}\"")),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn a_catalog_id_is_answered_from_what_the_catalog_knows() {
    let (fixture, _seeded) = catalog_fixture().await;
    let app = fixture.app();
    let known = catalog_id(EntityKind::Track, "hello-track-1");
    let unknown = catalog_id(EntityKind::Track, "hello-track-9");

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1&f=json&id={known}&id={unknown}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert_eq!(
        probe.body.matches("\"state\":\"remote\"").count(),
        2,
        "{}",
        probe.body
    );
    assert!(probe.body.contains("\"inLibrary\":false"), "{}", probe.body);
    insta::with_settings!({filters => FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-catalog-json", &probe.body);
    });
}

#[tokio::test]
async fn availability_answers_error_70_without_the_vendor_flag() {
    let (fixture, seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&id={}",
            fixture.api_key, seeded.alpha_tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn availability_answers_error_10_without_an_id() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 10);
}
