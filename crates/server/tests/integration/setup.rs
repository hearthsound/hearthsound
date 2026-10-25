//! Full-stack tests of the first-run setup link.
//!
//! A server whose users table is empty logs a URL carrying a random token; the
//! setup routes are the only ones that answer without a credential, and they
//! create the administrator the whole server is administered through.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_auth::Users;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use serde_json::{Value, json};

use crate::common::Fixture;

/// Sends the setup request with a token.
async fn complete_setup(fixture: &Fixture, token: &str, username: &str) -> reqwest::Response {
    fixture
        .client
        .post(format!("{}/api/v1/setup", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(
            json!({
                "token": token,
                "username": username,
                "displayName": "Echo",
                "password": "hunter22",
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

/// Reads a response body as JSON.
async fn body(response: reqwest::Response) -> Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

/// A fresh server reports the setup, creates the administrator from the link,
/// and is closed to it afterwards.
#[tokio::test]
async fn a_fresh_server_creates_its_first_administrator_from_the_link() {
    let fixture = Fixture::without_account().await;

    let status = fixture.get("/api/v1/setup").await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body(status).await, json!({ "required": true }));

    // A wrong token is refused and does not spend the real one.
    let refused = complete_setup(&fixture, "not the token", "admin").await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    let problem = body(refused).await;
    assert_eq!(problem["code"], "unauthenticated");
    assert_eq!(problem["detail"], "The setup link is not valid.");

    let token = fixture.services.setup().token().await.unwrap();
    let created = complete_setup(&fixture, &token, "admin").await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let cookie = created.headers()[SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        !created.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Secure"),
        "a plain-HTTP setup must not set a Secure cookie"
    );
    let signed_in = body(created).await;
    assert_eq!(signed_in["user"]["username"], "admin");
    assert_eq!(signed_in["user"]["displayName"], "Echo");
    assert_eq!(signed_in["user"]["isAdmin"], true);
    let csrf = signed_in["csrfToken"].as_str().unwrap().to_owned();

    // The cookie the setup answered with is a session.
    let session = fixture
        .client
        .get(format!("{}/api/v1/auth/session", fixture.base_url()))
        .header(COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session = body(session).await;
    assert_eq!(session["credential"], "session");
    assert_eq!(session["user"]["username"], "admin");
    assert_eq!(session["csrfToken"], csrf.as_str());

    // The setup is complete: the status says so, the link is spent, and a
    // second attempt is a conflict.
    let status = fixture.get("/api/v1/setup").await;
    assert_eq!(body(status).await, json!({ "required": false }));
    assert!(fixture.services.setup().token().await.is_none());
    let again = complete_setup(&fixture, &token, "admin2").await;
    assert_eq!(again.status(), StatusCode::CONFLICT);
    assert_eq!(body(again).await["code"], "conflict");

    let users = Users::new(fixture.services.database())
        .list()
        .await
        .unwrap();
    assert_eq!(users.len(), 1);
    assert!(users[0].is_admin);
    fixture.stop().await;
}

/// A server process that starts over a database with an account holds no setup
/// token and refuses the setup.
#[tokio::test]
async fn a_server_with_an_account_refuses_the_setup() {
    let fixture = Fixture::new().await;
    // A first process creates the account; the next one, which is what a
    // deployed server looks like, finds it there.
    let api_key = fixture.api_key.clone();
    let dir = fixture.stop().await;
    let fixture = Fixture::reopen(dir, api_key).await;
    assert!(fixture.services.setup().token().await.is_none());

    let refused = complete_setup(&fixture, "anything", "admin").await;
    assert_eq!(refused.status(), StatusCode::CONFLICT);
    assert_eq!(body(refused).await["detail"], "Setup is already complete.");

    let status = fixture.get("/api/v1/setup").await;
    assert_eq!(body(status).await, json!({ "required": false }));
    fixture.stop().await;
}

/// An empty username or password is refused before the link is spent.
#[tokio::test]
async fn the_setup_needs_a_username_and_a_password() {
    let fixture = Fixture::without_account().await;
    let token = fixture.services.setup().token().await.unwrap();

    let blank = complete_setup(&fixture, &token, "   ").await;
    assert_eq!(blank.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(blank).await["code"], "invalid-request");
    assert!(
        fixture.services.setup().token().await.is_some(),
        "a refused request must not spend the link"
    );

    let empty_password = fixture
        .client
        .post(format!("{}/api/v1/setup", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "token": token, "username": "admin", "password": "" }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(empty_password.status(), StatusCode::BAD_REQUEST);
    fixture.stop().await;
}

/// A username that breaks the shape rule is refused without spending the
/// link, so the next attempt with a proper name still creates the account.
#[tokio::test]
async fn the_setup_refuses_a_username_that_breaks_the_rule() {
    let fixture = Fixture::without_account().await;
    let token = fixture.services.setup().token().await.unwrap();

    let refused = complete_setup(&fixture, &token, "bad user").await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body(refused).await["code"], "invalid-request");
    assert!(
        fixture.services.setup().token().await.is_some(),
        "a refused username must not spend the link"
    );

    let accepted = complete_setup(&fixture, &token, "echo").await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
    fixture.stop().await;
}

/// A password shorter than eight characters is refused without spending the
/// link, and the retry with a valid one creates the administrator.
#[tokio::test]
async fn the_setup_refuses_a_short_password_without_spending_the_link() {
    let fixture = Fixture::without_account().await;
    let token = fixture.services.setup().token().await.unwrap();

    let refused = fixture
        .client
        .post(format!("{}/api/v1/setup", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(json!({ "token": token, "username": "echo", "password": "short22" }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let refused = body(refused).await;
    assert_eq!(refused["code"], "invalid-request");
    assert!(
        refused["detail"]
            .as_str()
            .unwrap()
            .contains("at least 8 characters long"),
        "{refused}"
    );
    assert!(
        fixture.services.setup().token().await.is_some(),
        "a refused password must not spend the link"
    );

    let accepted = complete_setup(&fixture, &token, "echo").await;
    assert_eq!(accepted.status(), StatusCode::CREATED);
    fixture.stop().await;
}

// A route-level test of "a failed insert restores the link" needs an insert
// that fails while the database stays usable, and every injection available to
// an integration test - a read-only data directory, a read-only database file -
// leaves SQLite writing through its already-open handle, so the insert
// succeeds and the test would prove nothing. The contract is proven instead by
// the `SetupGate::restore` unit tests (`crates/auth/src/setup.rs`), which cover
// the restore, the wrong token, the gate that never had a token and the second
// restore; the handler's `restore` call is the four lines between them.

/// Setup failures count against the caller's address only, so a wrong token
/// from one machine cannot block the operator's link from another.
#[tokio::test]
async fn a_wrong_token_only_spends_the_callers_address_budget() {
    let fixture = Fixture::without_account_with_config(|config| {
        config.settings.insert(
            "auth.login_rate_limit_per_minute".to_owned(),
            toml::Value::Integer(2),
        );
    })
    .await;
    let token = fixture.services.setup().token().await.unwrap();

    for _ in 0..2 {
        let refused = complete_setup(&fixture, "not-the-token", "echo").await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(
        fixture.services.setup().token().await.is_some(),
        "a wrong token never spends the link"
    );

    // The same address has spent its budget, so even the right token is
    // refused now; the address is what binds, not a shared "setup" key.
    let limited = complete_setup(&fixture, &token, "echo").await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let limited = body(limited).await;
    assert_eq!(limited["code"], "rate-limited");
    assert!(
        fixture.services.setup().token().await.is_some(),
        "a refusal does not spend the link"
    );
    fixture.stop().await;
}
