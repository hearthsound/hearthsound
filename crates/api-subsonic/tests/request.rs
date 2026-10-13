//! Integration tests for request handling: parameter parsing, the credential
//! check, routing, and the two failure modes of an extractor.
//!
//! The routers here are built with the public registration helper, so the tests
//! exercise the same path an endpoint added to this crate takes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response as HttpResponse};
use hearthsound_api_subsonic::{
    BinaryCall, Call, Encoded, Object, Response, SubsonicState, forbidden, register,
};

use common::{Fixture, assert_error, assert_ok, get, post_form};

/// An endpoint that answers what it saw of the request.
async fn attributes(call: Call) -> Encoded {
    let attributes = Object::new()
        .set("client", call.client_name().unwrap_or("none"))
        .set("version", call.protocol_version().unwrap_or("none"))
        .set("vendor", call.vendor_enabled())
        .set("user", call.principal().username.clone());
    call.encode(Response::ok().member("attributes", attributes))
}

/// A binary endpoint: it answers bytes, or the status its permissions refuse.
async fn audio(call: BinaryCall) -> HttpResponse {
    if !call.principal().can_download {
        return forbidden();
    }
    (
        StatusCode::OK,
        [("content-type", "audio/mpeg")],
        "audio bytes",
    )
        .into_response()
}

/// Registers the test endpoints, so a test can route to them.
fn test_router(state: SubsonicState) -> Router {
    let router = register(Router::new(), "attributes", attributes);
    let router = register(router, "stream", audio);
    router.with_state(state)
}

#[tokio::test]
async fn an_endpoint_answers_at_both_paths() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    for path in ["/rest/attributes", "/rest/attributes.view"] {
        let probe = get(
            &app,
            &format!("{path}?apiKey={}&c=Feishin&v=1.16.1", fixture.api_key),
        )
        .await;
        assert_ok(&probe);
        assert_eq!(probe.content_type(), Some("text/xml; charset=utf-8"));
        assert!(
            probe.body.contains(
                "<attributes client=\"Feishin\" version=\"1.16.1\" vendor=\"false\" user=\"echo\"/>"
            ),
            "{}",
            probe.body
        );
    }
}

#[tokio::test]
async fn a_form_post_body_carries_the_parameters_and_authenticates() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    let probe = post_form(
        &app,
        "/rest/attributes.view?f=json",
        &format!("apiKey={}&c=Symfonium&v=1.16.1", fixture.api_key),
    )
    .await;
    assert_ok(&probe);
    assert_eq!(
        probe.content_type(),
        Some("application/json; charset=utf-8")
    );
    assert!(
        probe
            .body
            .contains("\"attributes\":{\"client\":\"Symfonium\",\"version\":\"1.16.1\",\"vendor\":false,\"user\":\"echo\"}"),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn a_query_parameter_wins_over_a_body_parameter_of_the_same_name() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    let probe = post_form(
        &app,
        &format!("/rest/attributes?apiKey={}", fixture.api_key),
        "apiKey=hs_not-a-key&c=Feishin&f=json",
    )
    .await;
    assert_ok(&probe);
    assert_eq!(
        probe.content_type(),
        Some("application/json; charset=utf-8")
    );
    assert!(
        probe.body.contains("\"client\":\"Feishin\""),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn the_vendor_flag_is_read_from_the_parameters() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    let probe = get(
        &app,
        &format!(
            "/rest/attributes?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert!(probe.body.contains("\"vendor\":true"), "{}", probe.body);
}

#[tokio::test]
async fn missing_credentials_answer_error_10_in_the_requested_encoding() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    let probe = get(&app, "/rest/attributes").await;
    assert_error(&probe, 10);
    assert_eq!(probe.content_type(), Some("text/xml; charset=utf-8"));

    let probe = get(&app, "/rest/attributes?f=json").await;
    assert_error(&probe, 10);
    assert_eq!(
        probe.content_type(),
        Some("application/json; charset=utf-8")
    );
}

#[tokio::test]
async fn an_endpoint_this_server_does_not_implement_answers_error_70() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    for path in [
        "/rest/getNothing",
        "/rest/getNothing.view",
        "/rest/getPodcasts",
        "/rest/getPodcasts.view",
        "/rest/getInternetRadioStations",
        "/rest/createShare",
        "/rest/getChatMessages",
    ] {
        let probe = get(&app, &format!("{path}?apiKey={}&f=json", fixture.api_key)).await;
        assert_error(&probe, 70);
        assert!(
            probe
                .body
                .contains("\"message\":\"The requested data was not found.\""),
            "{}",
            probe.body
        );
    }

    let probe = get(&app, "/rest/getPodcasts").await;
    assert_error(&probe, 70);
    assert_eq!(probe.content_type(), Some("text/xml; charset=utf-8"));
}

#[tokio::test]
async fn a_binary_endpoint_answers_a_status_code_instead_of_an_envelope() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());

    let probe = get(&app, "/rest/stream").await;
    assert_eq!(probe.status, StatusCode::UNAUTHORIZED);
    assert!(probe.body.is_empty(), "{}", probe.body);
    assert_eq!(probe.content_type(), None);

    let probe = get(&app, "/rest/stream?apiKey=hs_not-a-key").await;
    assert_eq!(probe.status, StatusCode::UNAUTHORIZED);
    assert!(probe.body.is_empty(), "{}", probe.body);

    let probe = get(&app, &format!("/rest/stream?apiKey={}", fixture.api_key)).await;
    assert_eq!(probe.status, StatusCode::OK);
    assert_eq!(probe.content_type(), Some("audio/mpeg"));
    assert_eq!(probe.body, "audio bytes");
}

#[tokio::test]
async fn a_binary_endpoint_refuses_a_permission_it_lacks_with_403() {
    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());
    let key = fixture.account_with(&["acquire"]).await;

    let probe = get(&app, &format!("/rest/stream?apiKey={key}")).await;
    assert_eq!(probe.status, StatusCode::FORBIDDEN);
    assert!(probe.body.is_empty(), "{}", probe.body);
}

#[tokio::test]
async fn a_form_body_over_the_cap_is_rejected_before_the_handler_runs() {
    // A declared oversize is answered with 413; a body that turns out to be
    // over the cap while it is read is answered with 400. The router sees the
    // body without a hyper server in front of it, so the length is undeclared
    // here and the rejection is the 400 one.
    use hearthsound_api_subsonic::MAX_FORM_BODY_BYTES;

    let fixture = Fixture::new().await;
    let app = test_router(fixture.state.clone());
    let oversized = "a".repeat(MAX_FORM_BODY_BYTES);

    let probe = post_form(&app, "/rest/attributes", &format!("c={oversized}")).await;
    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
    assert!(probe.body.is_empty(), "{}", probe.body);

    let probe = post_form(&app, "/rest/stream", &format!("c={oversized}")).await;
    assert_eq!(probe.status, StatusCode::BAD_REQUEST);
}
