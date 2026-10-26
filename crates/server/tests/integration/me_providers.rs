//! Full-stack tests of the listener's links to scrobble and discovery services.
//!
//! The server is built by the composition root and the scrobble fixture is
//! installed and assigned to the scrobble layer, so every request walks the real
//! plugin boundary. The fixture links accounts both ways: its `config` method
//! accepts the token `good` and names `fixture-user`, and its `browser` method
//! answers the mock service's authorization URL and accepts the callback token
//! `approved`. The suite makes no network request; the fixture's endpoint is a
//! wiremock server that only has to exist for the configuration to validate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::Layer;
use hearthsound_plugin_host::{PluginRepository, PluginSource};
use hearthsound_test_support::{ALBUM_CATALOG, SCROBBLE_FIXTURE, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, LOCATION};
use serde_json::{Value, json};
use wiremock::MockServer;

use crate::common::Fixture;

/// The scrobble fixture's manifest id.
const SCROBBLE_ID: &str = "com.example.scrobble-fixture";

/// The catalog fixture's manifest id.
const CATALOG_ID: &str = "com.example.album-catalog";

/// Installs the scrobble fixture once.
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

/// Creates one scrobble instance over an endpoint.
async fn create_scrobble(fixture: &Fixture, slug: &str, endpoint: &str, link_method: &str) {
    fixture
        .services
        .instances()
        .create(&hearthsound_plugin_host::InstanceSpec {
            slug: slug.to_owned(),
            plugin_id: SCROBBLE_ID.to_owned(),
            display_name: format!("Fixture {slug}"),
            server_config: json!({
                "endpoint": endpoint,
                "linkMethod": link_method,
            }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
}

/// Assigns the scrobble instances to the scrobble layer and rebuilds the
/// registry.
async fn assign_scrobble(fixture: &Fixture, slugs: &[&str]) {
    let slugs: Vec<String> = slugs.iter().map(|slug| (*slug).to_owned()).collect();
    fixture
        .services
        .instances()
        .set_layer(Layer::Scrobble, &slugs)
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();
}

/// Sends one authenticated request.
async fn send(
    fixture: &Fixture,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key));
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    request.send().await.unwrap()
}

/// Reads a response body as JSON.
async fn json(response: reqwest::Response) -> Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

/// Asserts a response is a problem document with the given status and code.
async fn assert_problem(response: reqwest::Response, status: StatusCode, code: &str) -> Value {
    assert_eq!(response.status(), status);
    assert_eq!(response.headers()[CONTENT_TYPE], "application/problem+json");
    let body = json(response).await;
    assert_eq!(body["code"], code, "{body}");
    body
}

/// Returns the number of stored link rows.
async fn link_rows(fixture: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM provider_user_configs")
        .fetch_one(fixture.services.database().readers())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_configuration_link_is_stored_and_listed() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "listenbrainz", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["listenbrainz"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/listenbrainz/config",
        Some(json!({"config": {"token": "good"}})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let link = json(response).await;
    assert_eq!(link["slug"], "listenbrainz");
    assert_eq!(link["linked"], true);
    assert_eq!(link["accountName"], "fixture-user");
    assert_eq!(link["linkMethod"], "config");
    assert_eq!(link["pendingScrobbles"], 0);
    assert_eq!(link["failedScrobbles"], 0);
    assert!(link["linkedAt"].is_i64(), "{link}");
    assert!(link["lastError"].is_null(), "{link}");

    let listed =
        json(send(&fixture, reqwest::Method::GET, "/api/v1/me/providers", None).await).await;
    let providers = listed["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["slug"], "listenbrainz");
    assert_eq!(providers[0]["pluginId"], SCROBBLE_ID);
    assert_eq!(providers[0]["layers"], json!(["scrobble"]));
    assert_eq!(providers[0]["linked"], true);
    assert_eq!(providers[0]["accountName"], "fixture-user");
    assert_eq!(
        providers[0]["userConfig"]["schema"]["required"],
        json!(["token"])
    );
    assert!(providers[0]["userConfig"]["uiSchema"].is_object());
    assert_eq!(link_rows(&fixture).await, 1);
}

#[tokio::test]
async fn a_configuration_that_misses_a_required_field_is_rejected() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "service", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["service"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/service/config",
        Some(json!({"config": {}})),
    )
    .await;
    let problem =
        assert_problem(response, StatusCode::UNPROCESSABLE_ENTITY, "invalid-config").await;
    assert!(
        problem["detail"].as_str().unwrap().contains("token"),
        "{problem}"
    );
    assert_eq!(link_rows(&fixture).await, 0);
}

#[tokio::test]
async fn a_rejected_token_fails_the_link() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "service", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["service"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/service/config",
        Some(json!({"config": {"token": "bad"}})),
    )
    .await;
    assert_problem(response, StatusCode::UNPROCESSABLE_ENTITY, "link-failed").await;
    assert_eq!(link_rows(&fixture).await, 0);
}

#[tokio::test]
async fn each_link_method_refuses_the_other_route() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "browser", endpoint.uri().as_str(), "browser").await;
    create_scrobble(&fixture, "configured", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["browser", "configured"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/browser/config",
        Some(json!({"config": {"token": "good"}})),
    )
    .await;
    assert_problem(response, StatusCode::CONFLICT, "wrong-link-method").await;

    let response = send(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/me/providers/configured/link",
        None,
    )
    .await;
    assert_problem(response, StatusCode::CONFLICT, "wrong-link-method").await;
}

/// Starts a browser flow and returns the URL the listener would open.
async fn start_browser_link(fixture: &Fixture) -> String {
    let response = send(
        fixture,
        reqwest::Method::POST,
        "/api/v1/me/providers/browser/link",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    json(response).await["url"].as_str().unwrap().to_owned()
}

/// Returns the callback address the fixture put in the authorization URL.
fn callback_of(url: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "cb")
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

/// Returns the `state` query parameter of a callback URL.
fn state_of(callback: &str) -> String {
    let query = callback.split_once('?').unwrap().1;
    form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .unwrap()
}

/// Sends one callback request without following the redirect.
async fn callback(fixture: &Fixture, slug: &str, query: &str) -> reqwest::Response {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    client
        .get(format!(
            "{}/api/v1/me/providers/{slug}/link/callback?{query}",
            fixture.base_url()
        ))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_browser_link_starts_at_the_service_and_finishes_at_the_callback() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "browser", endpoint.uri().as_str(), "browser").await;
    assign_scrobble(&fixture, &["browser"]).await;

    let url = start_browser_link(&fixture).await;
    assert!(url.starts_with(endpoint.uri().as_str()), "{url}");
    let callback_url = callback_of(&url);
    let expected = format!(
        "{}/api/v1/me/providers/browser/link/callback",
        fixture.base_url()
    );
    assert!(
        callback_url.starts_with(&expected),
        "{callback_url} should start with {expected}"
    );
    let state = state_of(&callback_url);

    let response = callback(
        &fixture,
        "browser",
        &format!("token=approved&state={state}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        "/account/services?linked=browser"
    );
    assert_eq!(link_rows(&fixture).await, 1);

    let listed =
        json(send(&fixture, reqwest::Method::GET, "/api/v1/me/providers", None).await).await;
    assert_eq!(listed["providers"][0]["accountName"], "browser-user");
    assert_eq!(listed["providers"][0]["linked"], true);
    assert!(listed["providers"][0]["userConfig"].is_null());
}

#[tokio::test]
async fn a_state_is_single_use_and_unknown_states_fail() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "browser", endpoint.uri().as_str(), "browser").await;
    assign_scrobble(&fixture, &["browser"]).await;

    let state = state_of(&callback_of(&start_browser_link(&fixture).await));
    let response = callback(
        &fixture,
        "browser",
        &format!("token=approved&state={state}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        "/account/services?linked=browser"
    );

    let response = callback(
        &fixture,
        "browser",
        &format!("token=approved&state={state}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        "/account/services?linkError=browser"
    );

    let response = callback(&fixture, "browser", "token=approved&state=unknown").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        "/account/services?linkError=browser"
    );

    let state = state_of(&callback_of(&start_browser_link(&fixture).await));
    let response = callback(&fixture, "browser", &format!("token=denied&state={state}")).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[LOCATION],
        "/account/services?linkError=browser"
    );
}

#[tokio::test]
async fn delete_removes_the_link() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "service", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["service"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/service/config",
        Some(json!({"config": {"token": "good"}})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(link_rows(&fixture).await, 1);

    let response = send(
        &fixture,
        reqwest::Method::DELETE,
        "/api/v1/me/providers/service/link",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(link_rows(&fixture).await, 0);

    let response = send(
        &fixture,
        reqwest::Method::DELETE,
        "/api/v1/me/providers/service/link",
        None,
    )
    .await;
    assert_problem(response, StatusCode::NOT_FOUND, "not-found").await;
}

#[tokio::test]
async fn an_instance_that_cannot_link_accounts_is_not_found() {
    let fixture = Fixture::new().await;
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(ALBUM_CATALOG))
        .await
        .unwrap();
    PluginRepository::new(fixture.services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .create(&hearthsound_plugin_host::InstanceSpec {
            slug: "catalog".to_owned(),
            plugin_id: CATALOG_ID.to_owned(),
            display_name: "Catalog".to_owned(),
            server_config: json!({}),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Catalog, &["catalog".to_owned()])
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/catalog/config",
        Some(json!({"config": {}})),
    )
    .await;
    assert_problem(response, StatusCode::NOT_FOUND, "not-found").await;

    let response = send(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/me/providers/catalog/link",
        None,
    )
    .await;
    assert_problem(response, StatusCode::NOT_FOUND, "not-found").await;

    let response = send(
        &fixture,
        reqwest::Method::DELETE,
        "/api/v1/me/providers/catalog/link",
        None,
    )
    .await;
    assert_problem(response, StatusCode::NOT_FOUND, "not-found").await;

    let listed =
        json(send(&fixture, reqwest::Method::GET, "/api/v1/me/providers", None).await).await;
    assert_eq!(listed["providers"], json!([]));
}

/// Looks up one service's listed link.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `slug`: the instance slug to read.
///
/// # Returns
///
/// The service's row of the listing.
async fn listed(fixture: &Fixture, slug: &str) -> Value {
    let listed =
        json(send(fixture, reqwest::Method::GET, "/api/v1/me/providers", None).await).await;
    listed["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["slug"] == slug)
        .cloned()
        .unwrap_or_else(|| panic!("{slug} is not listed: {listed}"))
}

/// The stored configuration of one link, unsealed by the service.
///
/// # Parameters
///
/// - `fixture`: the running fixture.
/// - `slug`: the instance slug whose link is read.
///
/// # Returns
///
/// The configuration the row holds.
async fn stored_config(fixture: &Fixture, slug: &str) -> Value {
    let instance = fixture.services.instances().get(slug).await.unwrap();
    let state = fixture.services.native_state();
    let link = state
        .user_configs()
        .get(instance.id, fixture.user_id)
        .await
        .unwrap()
        .expect("the service is linked");
    link.config
}

#[tokio::test]
async fn a_linked_service_reports_its_masked_configuration() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "listenbrainz", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["listenbrainz"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/listenbrainz/config",
        Some(json!({"config": {"token": "good"}})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let provider = listed(&fixture, "listenbrainz").await;
    assert_eq!(
        provider["config"]["token"], "[sealed]",
        "a stored write-only field is masked: {provider}"
    );
    assert_eq!(
        stored_config(&fixture, "listenbrainz").await["token"],
        "good",
        "the stored secret is untouched by the masking"
    );

    // A service that is not linked reports no configuration at all.
    create_scrobble(&fixture, "other", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["listenbrainz", "other"]).await;
    let unlinked = listed(&fixture, "other").await;
    assert!(unlinked.get("config").is_none(), "{unlinked}");
}

#[tokio::test]
async fn an_unchanged_placeholder_keeps_the_stored_token() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "listenbrainz", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["listenbrainz"]).await;

    let first = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/listenbrainz/config",
        Some(json!({"config": {"token": "good"}})),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/listenbrainz/config",
        Some(json!({"config": {"token": "[sealed]"}})),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an untouched placeholder is a valid configuration"
    );
    let link = json(response).await;
    assert_eq!(link["config"]["token"], "[sealed]", "{link}");
    assert_eq!(
        stored_config(&fixture, "listenbrainz").await["token"],
        "good",
        "the placeholder kept the stored secret"
    );
}

#[tokio::test]
async fn a_placeholder_without_a_stored_link_is_refused() {
    let endpoint = MockServer::start().await;
    let fixture = Fixture::new().await;
    install_scrobble(&fixture).await;
    create_scrobble(&fixture, "listenbrainz", endpoint.uri().as_str(), "config").await;
    assign_scrobble(&fixture, &["listenbrainz"]).await;

    let response = send(
        &fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/listenbrainz/config",
        Some(json!({"config": {"token": "[sealed]"}})),
    )
    .await;
    let problem =
        assert_problem(response, StatusCode::UNPROCESSABLE_ENTITY, "invalid-config").await;
    assert!(
        problem["detail"].as_str().unwrap().contains("token"),
        "{problem}"
    );
    assert_eq!(link_rows(&fixture).await, 0);
}
