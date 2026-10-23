//! Full-stack tests of scrobble delivery.
//!
//! Every test builds the server, installs the scrobble fixture and assigns it to
//! the scrobble layer, links the account, and drives playback through `/rest`,
//! so the play, the outbox row, the plugin call and the HTTP request to the
//! service are all the ones a deployment makes. The fixture's endpoint is a
//! wiremock server; no test reaches the network.
//!
//! The server's delivery worker runs the pass the kick wakes, so the tests wait
//! for the sink to receive a request rather than driving the deliverer
//! themselves. The retry ladder and the batching are covered by the activity
//! crate's tests, which own a clock of their own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use hearthsound_core::{Availability, EntityId, IdPrefix, Layer};
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use hearthsound_plugin_host::{PluginRepository, PluginSource};
use hearthsound_test_support::{SCROBBLE_FIXTURE, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::{any, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Fixture;

/// The scrobble fixture's manifest id.
const SCROBBLE_ID: &str = "com.example.scrobble-fixture";

/// The slug every test gives its instance.
const SLUG: &str = "listenbrainz";

/// Installs the scrobble fixture once.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn install_scrobble(fixture: &Fixture) {
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(SCROBBLE_FIXTURE))
        .await
        .unwrap();
    PluginRepository::new(fixture.services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
}

/// Creates the scrobble instance over an endpoint and assigns it to the layer.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `endpoint`: the mock service's base URL.
async fn create_scrobble(fixture: &Fixture, endpoint: &str) {
    fixture
        .services
        .instances()
        .create(&hearthsound_plugin_host::InstanceSpec {
            slug: SLUG.to_owned(),
            plugin_id: SCROBBLE_ID.to_owned(),
            display_name: "Fixture scrobbler".to_owned(),
            server_config: json!({ "endpoint": endpoint }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Scrobble, &[SLUG.to_owned()])
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();
}

/// Links the test account to the scrobble instance.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn link(fixture: &Fixture) {
    let response = fixture
        .client
        .put(format!(
            "{}/api/v1/me/providers/{SLUG}/config",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({"config": {"token": "good"}}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Inserts a remote track and returns its client id.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn insert_track(fixture: &Fixture) -> EntityId {
    let database = fixture.services.database();
    let library = LibraryRepository::new(database);
    let mut album = AlbumInput::new("Fixture Album", Origin::Catalog);
    album.display_artist = Some("Fixture Artist".to_owned());
    let album_id = library
        .upsert_album(&album, &[ArtistCredit::new("Fixture Artist", "main")])
        .await
        .unwrap();
    let mut input = TrackInput::new("Fixture Song", Origin::Catalog, Availability::Remote);
    input.album_id = Some(EntityId::from_uuid(IdPrefix::Album, album_id.uuid()));
    input.track_number = Some(1);
    library
        .insert_track(&input, &[ArtistCredit::new("Fixture Artist", "main")], &[])
        .await
        .unwrap()
}

/// Sends a `/rest` request and asserts the envelope is `ok`.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `query`: the endpoint and its parameters, without the API key.
async fn rest(fixture: &Fixture, query: &str) {
    let response = fixture
        .get(&format!("/rest/{query}&apiKey={}&f=json", fixture.api_key))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(body["subsonic-response"]["status"], "ok", "{body}");
}

/// Mounts a catch-all answer on the mock service.
///
/// # Parameters
///
/// - `server`: the mock service.
/// - `status`: the status every request receives.
async fn answer(server: &MockServer, status: u16) {
    Mock::given(any())
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

/// Waits for the mock service to receive a request to one path and returns its
/// JSON body.
///
/// # Parameters
///
/// - `server`: the mock service.
/// - `target`: the request path.
async fn wait_for(server: &MockServer, target: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        if let Some(request) = requests.iter().find(|request| request.url.path() == target) {
            return serde_json::from_slice(&request.body).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "the service received no request to {target}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Returns the number of queued outbox rows.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn outbox_rows(fixture: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM scrobble_outbox")
        .fetch_one(fixture.services.database().readers())
        .await
        .unwrap()
}

/// Returns the rows' state, attempts and ids for a failing assertion.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn rows_debug(fixture: &Fixture) -> String {
    let rows = sqlx::query_as::<_, (Vec<u8>, Vec<u8>, String, i64, i64, Option<String>)>(
        "SELECT user_id, instance_id, state, attempts, next_attempt_at, last_error
         FROM scrobble_outbox",
    )
    .fetch_all(fixture.services.database().readers())
    .await
    .unwrap();
    rows.iter()
        .map(|row| {
            format!(
                "user={} instance={} state={} attempts={} next={} error={:?}",
                Uuid::from_slice(&row.0).unwrap(),
                Uuid::from_slice(&row.1).unwrap(),
                row.2,
                row.3,
                row.4,
                row.5
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Waits until the outbox holds a row.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn wait_for_row(fixture: &Fixture) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while outbox_rows(fixture).await == 0 {
        assert!(Instant::now() < deadline, "no scrobble was queued");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn listens_now_playing_and_love_reach_the_service() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/listens"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/now-playing"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/feedback"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, server.uri().as_str()).await;
    link(&fixture).await;
    let track = insert_track(&fixture).await;
    let id = track.to_string();

    rest(
        &fixture,
        &format!("scrobble?id={id}&submission=true&c=dsub"),
    )
    .await;
    let body = wait_for(&server, "/listens").await;
    assert_eq!(body["listens"][0]["title"], "Fixture Song", "{body}");
    assert_eq!(body["listens"][0]["artists"][0], "Fixture Artist", "{body}");
    assert_eq!(body["listens"][0]["album"], "Fixture Album", "{body}");
    assert_eq!(body["listens"][0]["client"], "dsub", "{body}");
    assert_eq!(body["listens"][0]["trackNumber"], 1, "{body}");

    rest(
        &fixture,
        &format!("scrobble?id={id}&submission=false&c=dsub"),
    )
    .await;
    let body = wait_for(&server, "/now-playing").await;
    assert_eq!(body["title"], "Fixture Song", "{body}");

    rest(&fixture, &format!("star?id={id}")).await;
    let body = wait_for(&server, "/feedback").await;
    assert_eq!(body["value"], "love", "{body}");
    assert_eq!(body["track"]["title"], "Fixture Song", "{body}");
}

#[tokio::test]
async fn a_failed_service_leaves_a_pending_row_with_its_error() {
    let server = MockServer::start().await;
    answer(&server, 500).await;

    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, server.uri().as_str()).await;
    link(&fixture).await;
    let track = insert_track(&fixture).await;
    rest(
        &fixture,
        &format!("scrobble?id={track}&submission=true&c=dsub"),
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = fixture
            .client
            .get(format!("{}/api/v1/me/providers", fixture.base_url()))
            .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
        let provider = &body["providers"][0];
        if provider["pendingScrobbles"] == 1 && provider["lastError"].is_string() {
            assert_eq!(provider["failedScrobbles"], 0, "{body}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the failure was never reported: {body}; requests={:?}, rows={}",
            server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .map(|request| request.url.path().to_owned())
                .collect::<Vec<_>>(),
            rows_debug(&fixture).await
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Rows the outbox gave up on are reported separately.
    sqlx::query("UPDATE scrobble_outbox SET state = 'failed'")
        .execute(fixture.services.database().writer())
        .await
        .unwrap();
    let response = fixture
        .client
        .get(format!("{}/api/v1/me/providers", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap();
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let provider = &body["providers"][0];
    assert_eq!(provider["pendingScrobbles"], 0, "{body}");
    assert_eq!(provider["failedScrobbles"], 1, "{body}");
    assert!(provider["lastError"].is_string(), "{body}");
}

#[tokio::test]
async fn unlinking_deletes_the_queued_scrobbles_and_the_schedule_is_registered() {
    let server = MockServer::start().await;
    answer(&server, 500).await;

    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, server.uri().as_str()).await;
    link(&fixture).await;
    let track = insert_track(&fixture).await;
    rest(
        &fixture,
        &format!("scrobble?id={track}&submission=true&c=dsub"),
    )
    .await;
    wait_for_row(&fixture).await;

    let response = fixture
        .client
        .delete(format!(
            "{}/api/v1/me/providers/{SLUG}/link",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(outbox_rows(&fixture).await, 0);

    let cron: String =
        sqlx::query_scalar("SELECT cron FROM schedules WHERE kind = 'scrobble.deliver'")
            .fetch_one(fixture.services.database().readers())
            .await
            .unwrap();
    assert_eq!(cron, "* * * * *");
}
