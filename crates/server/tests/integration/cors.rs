//! Full-stack tests of the cross-origin policy `server.cors_origins` sets.
//!
//! A browser-engine client such as the Feishin desktop app can read a `/rest`
//! response only when it carries `Access-Control-Allow-Origin`, so these tests
//! send the `Origin` header such a client sends and read the answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use hearthsound_server::Services;
use reqwest::header::{
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_METHOD, HeaderValue, ORIGIN,
};
use reqwest::{Method, Response};

/// The origin of a page loaded from a `file://` URL, as the Feishin desktop app
/// is.
const OPAQUE_ORIGIN: &str = "null";

/// Starts a server whose `server.cors_origins` is `origins`.
async fn server_with(origins: &[&str]) -> crate::common::Fixture {
    let origins: Vec<String> = origins.iter().map(|origin| (*origin).to_owned()).collect();
    crate::common::Fixture::with_config(|config| config.server.cors_origins = origins).await
}

/// Sends a request with an `Origin` header.
async fn send(
    fixture: &crate::common::Fixture,
    method: Method,
    path: &str,
    origin: &str,
) -> Response {
    fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(ORIGIN, origin)
        .header(ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .send()
        .await
        .unwrap()
}

/// Returns the `Access-Control-Allow-Origin` header of a response.
fn allowed_origin(response: &Response) -> Option<&HeaderValue> {
    response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN)
}

#[tokio::test]
async fn no_origins_configured_sends_no_cors_header() {
    let fixture = server_with(&[]).await;
    let path = format!("/rest/ping.view?apiKey={}&f=json", fixture.api_key);

    let response = send(&fixture, Method::GET, &path, OPAQUE_ORIGIN).await;

    assert_eq!(response.status(), 200);
    assert_eq!(allowed_origin(&response), None);
}

#[tokio::test]
async fn the_wildcard_allows_every_origin_on_every_answer() {
    let fixture = server_with(&["*"]).await;
    let ping = format!("/rest/ping.view?apiKey={}&f=json", fixture.api_key);

    for (method, path) in [
        // A successful call.
        (Method::GET, ping.as_str()),
        // A credential error inside the envelope.
        (Method::GET, "/rest/ping.view?u=echo&p=wrong&f=json"),
        // An endpoint this server does not implement: error 70.
        (Method::GET, "/rest/getUser.view?u=echo&p=wrong&f=json"),
        // A binary endpoint's HTTP error.
        (Method::GET, "/rest/stream.view?u=echo&p=wrong&id=x"),
        // The preflight a browser sends before a non-simple request.
        (Method::OPTIONS, "/rest/ping.view"),
    ] {
        let response = send(&fixture, method.clone(), path, OPAQUE_ORIGIN).await;
        assert_eq!(
            allowed_origin(&response).and_then(|value| value.to_str().ok()),
            Some("*"),
            "{method} {path} answered {} without the header",
            response.status()
        );
    }
}

#[tokio::test]
async fn a_list_allows_only_the_origins_it_names() {
    let fixture = server_with(&["https://music.example", OPAQUE_ORIGIN]).await;
    let ping = format!("/rest/ping.view?apiKey={}&f=json", fixture.api_key);

    for origin in ["https://music.example", OPAQUE_ORIGIN] {
        let response = send(&fixture, Method::GET, &ping, origin).await;
        assert_eq!(
            allowed_origin(&response).and_then(|value| value.to_str().ok()),
            Some(origin)
        );
    }

    let response = send(&fixture, Method::GET, &ping, "https://elsewhere.example").await;
    assert_eq!(allowed_origin(&response), None);
}

#[tokio::test]
async fn a_value_that_is_not_an_origin_stops_startup() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::common::write_config(dir.path());
    config.server.cors_origins = vec!["https://music.example/".to_owned()];

    let error = Services::build(config, &BTreeMap::new())
        .await
        .err()
        .expect("a trailing slash never matches a browser's origin");

    assert!(error.to_string().contains("https://music.example/"));
}
