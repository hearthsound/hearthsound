//! Integration tests for the acquisition vendor endpoints.
//!
//! `hsGetAvailability` is answered from the same facts the `hearthsound` member
//! is built from, so these tests cover what the availability list reports: the
//! state of a stored entity, the requesting account's library membership, the
//! acquisition in flight for a track, the aggregation an album's entry is, and
//! the ids that are left out. The error codes of the vendor gate and of a
//! missing parameter are covered here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use hearthsound_acquire::orchestrator::{AcquireOutcome, AcquireRequest};
use hearthsound_core::{
    Availability, CatalogId, EntityId, EntityKind, FailureReason, IdPrefix, Priority, Slug,
};
use hearthsound_library::tracks;

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// The number of characters in a materialized client id's payload.
const PAYLOAD: usize = 26;

/// Builds the `x_` id of one hello-catalog entity.
fn catalog_id(kind: EntityKind, provider_id: &str) -> String {
    CatalogId::new(Slug::new("hello").unwrap(), kind, provider_id)
        .unwrap()
        .to_string()
}

/// Replaces each acquisition id in `body` with a marker.
///
/// The markers are numbered in the order the ids first appear, and two rows that
/// name the same acquisition carry the same marker, so an attached acquisition is
/// visible in a snapshot. An acquisition's client id carries the UUIDv7 its
/// admission generated, which is what makes this necessary.
///
/// # Parameters
///
/// - `body`: the rendered response.
///
/// # Returns
///
/// The body, with every `ac_...` id replaced by `ac_<number>`.
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
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-json", number_acquisitions(&stabilize(&json.body)));
    });

    let xml = get(&app, &query).await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-xml", number_acquisitions(&stabilize(&xml.body)));
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
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-availability-catalog-json", number_acquisitions(&probe.body));
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

#[tokio::test]
async fn acquire_starts_acquisitions_in_both_encodings() {
    let (fixture, _seeded, album, tracks) = fixture().await;
    let app = fixture.app();
    let query = format!(
        "/rest/hsAcquire?apiKey={}&hearthsound=1&id={}&id={album}&priority=next",
        fixture.api_key, tracks[0]
    );
    let stabilize = |body: &str| {
        body.replace(&album.to_string(), "al_faraway")
            .replace(&tracks[0].to_string(), "tr_faraway_1")
            .replace(&tracks[1].to_string(), "tr_faraway_2")
    };

    let json = get(&app, &format!("{query}&f=json")).await;
    assert_ok(&json);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-acquire-json", number_acquisitions(&stabilize(&json.body)));
    });

    let xml = get(&app, &query).await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-acquire-xml", number_acquisitions(&stabilize(&xml.body)));
    });
}

#[tokio::test]
async fn an_acquiring_track_is_attached_instead_of_requested_again() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;
    let expected = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&format!("\"id\":\"{expected}\"")),
        "{}",
        probe.body
    );
    assert_eq!(
        probe.body.matches("\"state\":\"queued\"").count(),
        1,
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn a_track_with_a_file_answers_its_latest_acquisition() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;
    let expected = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();
    // The import that followed the admission produced the file the track now
    // has, which is what makes the next request answer "already available".
    {
        let mut tx = fixture.database.writer().begin().await.unwrap();
        tracks::set_track_availability(&mut tx, tracks[0], Availability::Local, None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&format!("\"id\":\"{expected}\"")),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn a_track_with_a_file_and_no_acquisition_is_left_out() {
    let (fixture, seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, seeded.alpha_tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(probe.body.contains("\"acquisition\":[]"), "{}", probe.body);
}

#[tokio::test]
async fn a_cooling_down_track_answers_its_latest_acquisition() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;
    fixture
        .state
        .acquisitions()
        .unwrap()
        .repository()
        .fail(id, FailureReason::NoCandidates, None)
        .await
        .unwrap();
    {
        let mut tx = fixture.database.writer().begin().await.unwrap();
        let until = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 3_600_000;
        tracks::set_track_availability(&mut tx, tracks[0], Availability::Unavailable, Some(until))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains("\"state\":\"failed\""),
        "{}",
        probe.body
    );
    assert!(
        probe.body.contains("\"failureReason\":\"no-candidates\""),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn acquire_answers_error_50_without_can_acquire() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let (_, key) = fixture.account(&[]).await;

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={key}&hearthsound=1&f=json&id={}",
            tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 50);
}

#[tokio::test]
async fn acquire_answers_error_70_for_an_entity_that_does_not_exist() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();
    let unknown = EntityId::new(IdPrefix::Track);

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&hearthsound=1&f=json&id={unknown}",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn acquire_answers_error_70_without_the_vendor_flag() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={}&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn acquire_answers_error_10_without_an_id() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/hsAcquire?apiKey={}&hearthsound=1", fixture.api_key),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn acquisitions_lists_the_callers_active_rows_in_both_encodings() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    acquire(&fixture, tracks[0]).await;
    acquire(&fixture, tracks[1]).await;
    let stabilize = |body: &str| {
        number_acquisitions(
            &body
                .replace(&tracks[0].to_string(), "tr_faraway_1")
                .replace(&tracks[1].to_string(), "tr_faraway_2"),
        )
    };
    let query = format!(
        "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1",
        fixture.api_key
    );

    let json = get(&app, &format!("{query}&f=json")).await;
    assert_ok(&json);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-acquisitions-json", stabilize(&json.body));
    });

    let xml = get(&app, &query).await;
    assert_ok(&xml);
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("hs-acquisitions-xml", stabilize(&xml.body));
    });
}

#[tokio::test]
async fn acquisitions_windows_select_active_recent_and_all() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let active = acquire(&fixture, tracks[0]).await;
    let ended = acquire(&fixture, tracks[1]).await;
    fixture
        .state
        .acquisitions()
        .unwrap()
        .repository()
        .fail(ended, FailureReason::NoCandidates, None)
        .await
        .unwrap();
    let active = EntityId::from_uuid(IdPrefix::Acquisition, active).to_string();
    let ended = EntityId::from_uuid(IdPrefix::Acquisition, ended).to_string();

    let recent = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json&state=recent",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&recent);
    assert!(
        recent.body.contains(&format!("\"id\":\"{ended}\"")),
        "{}",
        recent.body
    );
    assert!(
        !recent.body.contains(&format!("\"id\":\"{active}\"")),
        "{}",
        recent.body
    );

    let all = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json&state=all",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&all);
    assert!(
        all.body.contains(&format!("\"id\":\"{ended}\"")),
        "{}",
        all.body
    );
    assert!(
        all.body.contains(&format!("\"id\":\"{active}\"")),
        "{}",
        all.body
    );

    let default = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&default);
    assert!(
        default.body.contains(&format!("\"id\":\"{active}\"")),
        "{}",
        default.body
    );
    assert!(
        !default.body.contains(&format!("\"id\":\"{ended}\"")),
        "{}",
        default.body
    );
}

#[tokio::test]
async fn acquisitions_page_by_count_and_offset() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let first = acquire(&fixture, tracks[0]).await;
    let second = acquire(&fixture, tracks[1]).await;
    let first = EntityId::from_uuid(IdPrefix::Acquisition, first).to_string();
    let second = EntityId::from_uuid(IdPrefix::Acquisition, second).to_string();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json&count=1&offset=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains(&format!("\"id\":\"{first}\"")),
        "{}",
        probe.body
    );
    assert!(
        !probe.body.contains(&format!("\"id\":\"{second}\"")),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn acquisitions_hide_another_accounts_rows_but_not_from_an_admin() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let (_, other_key) = fixture.account(&["acquire"]).await;
    let (_, admin_key) = fixture.admin_account().await;

    acquire(&fixture, tracks[0]).await;
    let their = get(
        &app,
        &format!(
            "/rest/hsAcquire?apiKey={other_key}&hearthsound=1&f=json&id={}",
            tracks[1]
        ),
    )
    .await;
    assert_ok(&their);
    let own = get(
        &app,
        &format!(
            "/rest/hsGetAcquisitions?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&own);
    assert!(
        own.body
            .contains(&format!("\"targetId\":\"{}\"", tracks[0])),
        "{}",
        own.body
    );
    assert!(
        !own.body
            .contains(&format!("\"targetId\":\"{}\"", tracks[1])),
        "{}",
        own.body
    );

    let every = get(
        &app,
        &format!("/rest/hsGetAcquisitions?apiKey={admin_key}&hearthsound=1&f=json"),
    )
    .await;
    assert_ok(&every);
    assert!(
        every
            .body
            .contains(&format!("\"targetId\":\"{}\"", tracks[0])),
        "{}",
        every.body
    );
    assert!(
        every
            .body
            .contains(&format!("\"targetId\":\"{}\"", tracks[1])),
        "{}",
        every.body
    );
}

#[tokio::test]
async fn cancellations_end_an_acquisition_and_release_its_track() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;
    let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();

    let probe = get(
        &app,
        &format!(
            "/rest/hsCancelAcquisition?apiKey={}&hearthsound=1&f=json&id={acquisition}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(!probe.body.contains("acquisition"), "{}", probe.body);

    let availability = get(
        &app,
        &format!(
            "/rest/hsGetAvailability?apiKey={}&hearthsound=1&f=json&id={}",
            fixture.api_key, tracks[0]
        ),
    )
    .await;
    assert_ok(&availability);
    assert!(
        availability.body.contains("\"state\":\"remote\""),
        "{}",
        availability.body
    );
}

#[tokio::test]
async fn a_cancellation_of_a_terminal_acquisition_answers_an_empty_success() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let id = acquire(&fixture, tracks[0]).await;
    fixture
        .state
        .acquisitions()
        .unwrap()
        .repository()
        .fail(id, FailureReason::NoCandidates, None)
        .await
        .unwrap();
    let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();

    let probe = get(
        &app,
        &format!(
            "/rest/hsCancelAcquisition?apiKey={}&hearthsound=1&f=json&id={acquisition}",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(!probe.body.contains("acquisition"), "{}", probe.body);
}

#[tokio::test]
async fn cancellations_answer_error_50_for_another_accounts_acquisition() {
    let (fixture, _seeded, _album, tracks) = fixture().await;
    let app = fixture.app();
    let (_, other_key) = fixture.account(&["acquire"]).await;
    let id = acquire(&fixture, tracks[0]).await;
    let acquisition = EntityId::from_uuid(IdPrefix::Acquisition, id).to_string();

    let probe = get(
        &app,
        &format!(
            "/rest/hsCancelAcquisition?apiKey={other_key}&hearthsound=1&f=json&id={acquisition}"
        ),
    )
    .await;
    assert_error(&probe, 50);
}

#[tokio::test]
async fn cancellations_answer_error_70_for_an_unknown_acquisition() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();
    let unknown = EntityId::new(IdPrefix::Acquisition);

    let probe = get(
        &app,
        &format!(
            "/rest/hsCancelAcquisition?apiKey={}&hearthsound=1&f=json&id={unknown}",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 70);
}

#[tokio::test]
async fn a_cancellation_answers_error_10_without_an_id() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsCancelAcquisition?apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn a_listing_answers_error_70_without_the_vendor_flag() {
    let (fixture, _seeded, _album, _tracks) = fixture().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/hsGetAcquisitions?apiKey={}", fixture.api_key),
    )
    .await;
    assert_error(&probe, 70);
}
