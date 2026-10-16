//! Integration tests for the scan endpoints: the status a client polls, the
//! scan it starts, and the administrator gate on starting one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_auth::{ApiKeys, NewUser, Users};

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library.
///
/// # Returns
///
/// The fixture and the ids of the seeded entities; the fixture keeps its
/// temporary directory alive.
async fn fixture() -> (Fixture, Seeded) {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    (fixture, seeded)
}

/// Creates an account with the given name and admin flag.
///
/// # Parameters
///
/// - `fixture`: the fixture whose database the account is written to.
/// - `username`: the login name.
/// - `is_admin`: whether the account administers the server.
///
/// # Returns
///
/// One of the account's API keys.
async fn account(fixture: &Fixture, username: &str, is_admin: bool) -> String {
    let user = Users::new(&fixture.database)
        .create(
            &fixture.settings.current(),
            &NewUser::new(username, username, "hunter2", is_admin),
        )
        .await
        .unwrap();
    ApiKeys::new(&fixture.database)
        .generate(user.id, username)
        .await
        .unwrap()
        .plaintext
}

/// The status counts the folders and the files the library holds between scans.
#[tokio::test]
async fn the_status_counts_the_library_between_scans() {
    let (fixture, _seeded) = fixture().await;
    let app = fixture.app();

    let status = get(
        &app,
        &format!("/rest/getScanStatus?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&status);
    assert!(
        status.body.contains("\"scanning\":false"),
        "{}",
        status.body
    );
    assert!(
        status.body.contains("\"folderCount\":2"),
        "the seeded library has two folders: {}",
        status.body
    );
    // The seed attaches four folder files; the cached one is not one of them.
    assert!(status.body.contains("\"count\":4"), "{}", status.body);

    let xml = get(
        &app,
        &format!("/rest/getScanStatus.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    assert!(xml.body.contains("folderCount=\"2\""), "{}", xml.body);
}

/// `startScan` enqueues a scan and answers the status it started.
#[tokio::test]
async fn starting_a_scan_enqueues_one() {
    let (fixture, _seeded) = fixture().await;
    let admin = account(&fixture, "root", true).await;
    let app = fixture.app();

    let started = get(&app, &format!("/rest/startScan?apiKey={admin}&f=json")).await;
    assert_ok(&started);
    assert!(
        started.body.contains("\"scanning\":true"),
        "the answer reports the scan it started: {}",
        started.body
    );

    // The request is queued until a runner claims it, which this fixture has
    // none of, so the scanner reports it as pending.
    let status = get(&app, &format!("/rest/getScanStatus?apiKey={admin}&f=json")).await;
    assert_ok(&status);
    assert!(status.body.contains("\"scanning\":true"), "{}", status.body);
    assert!(
        fixture.scanner.status().scanning,
        "the scan is queued on the shared scanner"
    );

    // A full scan is the same request with every file probed.
    let full = get(
        &app,
        &format!("/rest/startScan?fullScan=true&apiKey={admin}&f=json"),
    )
    .await;
    assert_ok(&full);
    let queued = fixture
        .jobs
        .list_by_state(hearthsound_jobs::JobState::Ready, 10)
        .await
        .unwrap();
    assert_eq!(
        queued
            .iter()
            .filter(|job| job.kind == hearthsound_library::scan::SCAN_JOB_KIND.into())
            .count(),
        2,
        "each request is one queued scan"
    );
}

/// Only an administrator starts a scan, and only a boolean asks for a full one.
#[tokio::test]
async fn starting_a_scan_is_for_administrators() {
    let (fixture, _seeded) = fixture().await;
    let limited = account(&fixture, "limited", false).await;
    let app = fixture.app();

    let refused = get(&app, &format!("/rest/startScan?apiKey={limited}&f=json")).await;
    assert_error(&refused, 50);
    assert!(
        !fixture.scanner.status().scanning,
        "a refused request enqueues nothing"
    );

    let admin = account(&fixture, "root", true).await;
    let bad = get(
        &app,
        &format!("/rest/startScan?fullScan=maybe&apiKey={admin}&f=json"),
    )
    .await;
    assert_error(&bad, 0);
}
