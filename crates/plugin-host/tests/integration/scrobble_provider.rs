//! Integration tests for calling a scrobble plugin through the core trait.
//!
//! The fixture posts every report to a mock service, so these tests cover the
//! whole path: the instance an administrator configured, the call the server
//! makes, the body that crossed the wire, and the error a failing service
//! produces.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{
    ExternalId, FeedbackValue, Listen, ScrobbleProvider, UserContext,
};
use hearthsound_core::{ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{SCROBBLE_FIXTURE, fixture_plugin};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// Installs the fixture and returns a scrobble provider pointed at `service`.
async fn provider(
    service: &MockServer,
    extra: serde_json::Value,
) -> (tempfile::TempDir, Arc<dyn ScrobbleProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(SCROBBLE_FIXTURE))
        .await
        .unwrap();

    let mut config = json!({
        "endpoint": service.uri(),
        "maxBatch": 7,
        "minIntervalMs": 250,
        "feedback": true,
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    let provider = host
        .scrobble_provider(
            &installed,
            InstanceConfig::new(Slug::new("scrobble").unwrap())
                .with_server_config(config.to_string())
                .with_permissions(installed.manifest().permissions.clone()),
        )
        .unwrap();
    (dir, provider)
}

/// The listener the reports belong to.
fn listener() -> UserContext {
    UserContext {
        user_id: "0194f3b0-2b3c-7d4e-8f00-000000000001".to_owned(),
        config: Some(r#"{"token":"good"}"#.to_owned()),
    }
}

/// The listen the tests report.
fn listen(title: &str) -> Listen {
    Listen {
        title: title.to_owned(),
        artists: vec!["Hello Artist".to_owned()],
        album: Some("Hello Album".to_owned()),
        album_artist: Some("Hello Artist".to_owned()),
        duration_ms: Some(180_000),
        track_number: Some(1),
        ids: vec![ExternalId {
            namespace: "mb-recording".to_owned(),
            value: "11111111-1111-4111-8111-111111111111".to_owned(),
        }],
        listened_at: 1_790_000_000_000,
        client: Some("web".to_owned()),
    }
}

/// Mounts a successful answer at `path`.
async fn accept(service: &MockServer, route: &str) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200))
        .mount(service)
        .await;
}

#[tokio::test]
async fn limits_and_reports_reach_the_service() {
    let service = MockServer::start().await;
    accept(&service, "/now-playing").await;
    accept(&service, "/listens").await;
    accept(&service, "/feedback").await;
    let (_dir, provider) = provider(&service, json!({})).await;

    assert_eq!(provider.health().await.unwrap(), "scrobble-fixture ready");
    let limits = provider.get_limits().await.unwrap();
    assert_eq!(limits.max_batch, 7);
    assert_eq!(limits.min_interval_ms, 250);
    assert!(limits.feedback);

    provider
        .now_playing(&listener(), &listen("Now Playing Song"))
        .await
        .unwrap();
    provider
        .submit_listens(&listener(), &[listen("First Song"), listen("Second Song")])
        .await
        .unwrap();
    provider
        .send_feedback(&listener(), &listen("Loved Song"), FeedbackValue::Love)
        .await
        .unwrap();

    let requests = service.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);

    let now_playing = requests
        .iter()
        .find(|request| request.url.path() == "/now-playing")
        .unwrap();
    let body = String::from_utf8_lossy(&now_playing.body);
    assert!(body.contains("Now Playing Song"), "body was {body}");
    assert!(body.contains("Hello Artist"), "body was {body}");
    assert!(
        body.contains(r#""listenedAt":1790000000000"#),
        "body was {body}"
    );

    let listens = requests
        .iter()
        .find(|request| request.url.path() == "/listens")
        .unwrap();
    let body = String::from_utf8_lossy(&listens.body);
    assert!(body.contains("First Song"), "body was {body}");
    assert!(body.contains("Second Song"), "body was {body}");

    let feedback = requests
        .iter()
        .find(|request| request.url.path() == "/feedback")
        .unwrap();
    let body = String::from_utf8_lossy(&feedback.body);
    assert!(body.contains(r#""value":"love""#), "body was {body}");
    assert!(body.contains("Loved Song"), "body was {body}");
}

#[tokio::test]
async fn a_rate_limited_service_reports_its_retry_hint() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/now-playing"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&service)
        .await;
    let (_dir, provider) = provider(&service, json!({})).await;

    let error = provider
        .now_playing(&listener(), &listen("Song"))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after_ms: 1000
        }
    );
}

#[tokio::test]
async fn another_failure_is_reported_as_unavailable() {
    let service = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/feedback"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&service)
        .await;
    let (_dir, provider) = provider(&service, json!({})).await;

    let error = provider
        .send_feedback(&listener(), &listen("Song"), FeedbackValue::Clear)
        .await
        .unwrap_err();

    assert_eq!(error, ProviderError::Unavailable);
}
