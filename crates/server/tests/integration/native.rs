//! Full-stack tests of the native API: authentication, problem documents, the
//! OpenAPI document, and the resources under `/api/v1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use crate::common::Fixture;

/// Sends a GET with the fixture's API key as a bearer credential.
async fn bearer_get(fixture: &Fixture, path: &str) -> reqwest::Response {
    fixture
        .client
        .get(format!("{}{path}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap()
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
    assert_eq!(
        body["type"],
        format!("https://hearthsound.app/problems/{code}")
    );
    assert_eq!(body["status"], status.as_u16());
    body
}

/// An API key authenticates as a bearer credential, and the system info names
/// the version, the features and every layer.
#[tokio::test]
async fn a_bearer_key_reads_the_system_info() {
    let fixture = Fixture::new().await;
    let response = bearer_get(&fixture, "/api/v1/system/info").await;
    assert_eq!(response.status(), StatusCode::OK);
    let info = json(response).await;
    assert_eq!(info["serverType"], "hearthsound");
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["apiVersion"], 1);
    assert_eq!(info["features"]["uploads"], true);
    assert_eq!(info["features"]["serverBrowse"], "permanent_only");
    let layers: Vec<&str> = info["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["layer"].as_str().unwrap())
        .collect();
    assert!(layers.contains(&"catalog") && layers.contains(&"acquisition"));
    fixture.stop().await;
}

/// A request without a credential, or with a wrong key, is a problem document.
#[tokio::test]
async fn a_missing_or_wrong_credential_is_unauthenticated() {
    let fixture = Fixture::new().await;
    let anonymous = fixture.get("/api/v1/system/info").await;
    assert_problem(anonymous, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    let wrong = fixture
        .client
        .get(format!("{}/api/v1/system/info", fixture.base_url()))
        .header(AUTHORIZATION, "Bearer hs_notakey")
        .send()
        .await
        .unwrap();
    assert_problem(wrong, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    let unknown = bearer_get(&fixture, "/api/v1/nothing/here").await;
    assert_problem(unknown, StatusCode::NOT_FOUND, "not-found").await;
    fixture.stop().await;
}

/// The OpenAPI document is public and lists the resources and both
/// credential schemes.
#[tokio::test]
async fn the_openapi_document_is_served() {
    let fixture = Fixture::new().await;
    let response = fixture.get("/api/v1/openapi.json").await;
    assert_eq!(response.status(), StatusCode::OK);
    let document = json(response).await;
    assert!(document["openapi"].as_str().unwrap().starts_with("3."));
    assert!(document["paths"]["/api/v1/system/info"]["get"].is_object());
    let schemes = &document["components"]["securitySchemes"];
    assert_eq!(schemes["bearer"]["scheme"], "bearer");
    assert_eq!(schemes["session"]["in"], "cookie");
    assert_eq!(schemes["session"]["name"], "hs_session");
    fixture.stop().await;
}
