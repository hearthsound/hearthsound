//! Full-stack tests of the native API: authentication, problem documents, the
//! OpenAPI document, and the resources under `/api/v1`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use crate::common::{self, Fixture};

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

/// Signs in and returns the session cookie pair and the CSRF token.
async fn login(fixture: &Fixture, username: &str, password: &str) -> reqwest::Response {
    fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "username": username, "password": password }).to_string())
        .send()
        .await
        .unwrap()
}

/// The `name=value` part of a `Set-Cookie` header.
fn cookie_pair(response: &reqwest::Response) -> String {
    response.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned()
}

/// A sign-in sets a hardened session cookie that authenticates reads, unsafe
/// requests need the CSRF header, and signing out ends the session.
#[tokio::test]
async fn a_web_session_signs_in_checks_csrf_and_signs_out() {
    let fixture = Fixture::new().await;
    let signed_in = login(&fixture, common::USERNAME, common::ACCOUNT_PASSWORD).await;
    assert_eq!(signed_in.status(), StatusCode::OK);
    let set_cookie = signed_in.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .unwrap()
        .to_owned();
    for attribute in [
        "HttpOnly",
        "Secure",
        "SameSite=Lax",
        "Path=/",
        "Max-Age=2592000",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "{set_cookie} has {attribute}"
        );
    }
    let cookie = cookie_pair(&signed_in);
    let body = json(signed_in).await;
    assert_eq!(body["user"]["username"], common::USERNAME);
    let csrf = body["csrfToken"].as_str().unwrap().to_owned();

    let session = fixture
        .client
        .get(format!("{}/api/v1/auth/session", fixture.base_url()))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), StatusCode::OK);
    let session = json(session).await;
    assert_eq!(session["credential"], "session");
    assert_eq!(session["csrfToken"], csrf.as_str());

    let logout = |csrf: Option<&str>| {
        let mut request = fixture
            .client
            .post(format!("{}/api/v1/auth/logout", fixture.base_url()))
            .header(reqwest::header::COOKIE, &cookie);
        if let Some(csrf) = csrf {
            request = request.header("X-Hearthsound-CSRF", csrf);
        }
        request.send()
    };
    assert_problem(
        logout(None).await.unwrap(),
        StatusCode::FORBIDDEN,
        "csrf-failed",
    )
    .await;
    assert_problem(
        logout(Some("wrong")).await.unwrap(),
        StatusCode::FORBIDDEN,
        "csrf-failed",
    )
    .await;
    let out = logout(Some(&csrf)).await.unwrap();
    assert_eq!(out.status(), StatusCode::NO_CONTENT);
    assert!(
        out.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Max-Age=0")
    );
    let after = fixture
        .client
        .get(format!("{}/api/v1/auth/session", fixture.base_url()))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_problem(after, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    // A bearer key needs no CSRF header for an unsafe request.
    let bearer = fixture
        .client
        .post(format!("{}/api/v1/auth/logout", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap();
    assert_eq!(bearer.status(), StatusCode::NO_CONTENT);
    fixture.stop().await;
}

/// Wrong passwords, a disabled account and an expired session are refused,
/// and a spent failure budget refuses even the right password.
#[tokio::test]
async fn sign_ins_are_refused_when_they_should_be() {
    let fixture = Fixture::new().await;
    let wrong = login(&fixture, common::USERNAME, "not the password").await;
    assert_problem(wrong, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    // An expired session no longer authenticates.
    let signed_in = login(&fixture, common::USERNAME, common::ACCOUNT_PASSWORD).await;
    let cookie = cookie_pair(&signed_in);
    sqlx::query("UPDATE sessions SET expires_at = 0")
        .execute(fixture.services.database().writer())
        .await
        .unwrap();
    let expired = fixture
        .client
        .get(format!("{}/api/v1/auth/session", fixture.base_url()))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_problem(expired, StatusCode::UNAUTHORIZED, "unauthenticated").await;

    // A disabled account cannot sign in.
    let users = hearthsound_auth::Users::new(fixture.services.database());
    users.disable(fixture.user_id).await.unwrap();
    let disabled = login(&fixture, common::USERNAME, common::ACCOUNT_PASSWORD).await;
    assert_problem(disabled, StatusCode::UNAUTHORIZED, "unauthenticated").await;
    users.enable(fixture.user_id).await.unwrap();

    // Ten failures a minute spend the budget; the right password is then
    // refused too until it refills.
    for _ in 0..10 {
        login(&fixture, common::USERNAME, "still wrong").await;
    }
    let limited = login(&fixture, common::USERNAME, common::ACCOUNT_PASSWORD).await;
    assert_eq!(limited.headers()[reqwest::header::RETRY_AFTER], "60");
    assert_problem(limited, StatusCode::TOO_MANY_REQUESTS, "rate-limited").await;
    fixture.stop().await;
}

/// Sends a request with the fixture's API key as a bearer credential.
async fn bearer(
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

/// `true` when a `/rest/ping` with the query's credentials answers `ok`.
async fn rest_ping_ok(fixture: &Fixture, credentials: &str) -> bool {
    let response = fixture
        .get(&format!("/rest/ping?{credentials}&v=1.16.1&c=test&f=json"))
        .await;
    response.text().await.unwrap().contains("\"status\":\"ok\"")
}

/// An API key created here authenticates `/rest` until it is revoked, and an
/// app password created here serves the legacy Subsonic sign-in.
#[tokio::test]
async fn api_keys_and_app_passwords_are_created_listed_and_revoked() {
    let fixture = Fixture::new().await;
    let created = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/me/api-keys",
        Some(serde_json::json!({ "name": "phone" })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json(created).await;
    let key = created["secret"].as_str().unwrap().to_owned();
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("ak_") && key.starts_with("hs_"));
    assert!(rest_ping_ok(&fixture, &format!("apiKey={key}")).await);

    let listed =
        json(bearer(&fixture, reqwest::Method::GET, "/api/v1/me/api-keys", None).await).await;
    let listed = listed.as_array().unwrap();
    let entry = listed
        .iter()
        .find(|entry| entry["id"] == id.as_str())
        .unwrap();
    assert_eq!(entry["name"], "phone");
    assert!(
        entry.get("secret").is_none(),
        "a listing never shows a secret"
    );

    let revoked = bearer(
        &fixture,
        reqwest::Method::DELETE,
        &format!("/api/v1/me/api-keys/{id}"),
        None,
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert!(!rest_ping_ok(&fixture, &format!("apiKey={key}")).await);
    let again = bearer(
        &fixture,
        reqwest::Method::DELETE,
        &format!("/api/v1/me/api-keys/{id}"),
        None,
    )
    .await;
    assert_problem(again, StatusCode::NOT_FOUND, "not-found").await;

    let password = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/me/app-passwords",
        Some(serde_json::json!({ "name": "old client" })),
    )
    .await;
    assert_eq!(password.status(), StatusCode::CREATED);
    let password = json(password).await;
    let secret = password["secret"].as_str().unwrap().to_owned();
    let credentials = format!("u={}&p={secret}", common::USERNAME);
    assert!(rest_ping_ok(&fixture, &credentials).await);
    let id = password["id"].as_str().unwrap();
    let revoked = bearer(
        &fixture,
        reqwest::Method::DELETE,
        &format!("/api/v1/me/app-passwords/{id}"),
        None,
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::NO_CONTENT);
    assert!(!rest_ping_ok(&fixture, &credentials).await);

    let nameless = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/me/api-keys",
        Some(serde_json::json!({ "name": "  " })),
    )
    .await;
    assert_problem(nameless, StatusCode::BAD_REQUEST, "invalid-request").await;
    fixture.stop().await;
}

/// Finds one entry of a settings listing.
fn setting<'a>(listing: &'a Value, key: &str) -> &'a Value {
    listing["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap_or_else(|| panic!("{key} is listed: {listing}"))
}

/// A user overrides the settings users may override, and only those.
#[tokio::test]
async fn personal_settings_override_and_fall_back_to_the_server() {
    let fixture = Fixture::new().await;
    let listing =
        json(bearer(&fixture, reqwest::Method::GET, "/api/v1/me/settings", None).await).await;
    let scope = setting(&listing, "subsonic.default_scope");
    assert_eq!(scope["value"], "library");
    assert_eq!(scope["source"], "server");
    assert!(
        listing["settings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["key"] != "storage.cache.max_bytes"),
        "server settings are not offered"
    );

    let patched = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/me/settings",
        Some(serde_json::json!({ "settings": { "subsonic.default_scope": "server" } })),
    )
    .await;
    assert_eq!(patched.status(), StatusCode::OK);
    let patched = json(patched).await;
    let scope = setting(&patched, "subsonic.default_scope");
    assert_eq!(scope["value"], "server");
    assert_eq!(scope["source"], "user");
    assert_eq!(scope["serverValue"], "library");

    let cleared = json(
        bearer(
            &fixture,
            reqwest::Method::PATCH,
            "/api/v1/me/settings",
            Some(serde_json::json!({ "settings": { "subsonic.default_scope": null } })),
        )
        .await,
    )
    .await;
    assert_eq!(
        setting(&cleared, "subsonic.default_scope")["source"],
        "server"
    );

    for refused in [
        serde_json::json!({ "settings": { "storage.cache.max_bytes": 1 } }),
        serde_json::json!({ "settings": { "subsonic.default_scope": "everywhere" } }),
        serde_json::json!({ "settings": { "no.such.key": true } }),
    ] {
        let response = bearer(
            &fixture,
            reqwest::Method::PATCH,
            "/api/v1/me/settings",
            Some(refused),
        )
        .await;
        assert_problem(response, StatusCode::BAD_REQUEST, "invalid-request").await;
    }
    fixture.stop().await;
}

/// Signs in and returns the cookie pair and CSRF token of the new session.
async fn session_of(fixture: &Fixture, username: &str, password: &str) -> (String, String) {
    let response = login(fixture, username, password).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = cookie_pair(&response);
    let body = json(response).await;
    (cookie, body["csrfToken"].as_str().unwrap().to_owned())
}

/// Administrators create, page through, change and disable accounts; other
/// accounts may not, and nobody can lock themselves out.
#[tokio::test]
async fn administrators_manage_accounts_permissions_and_quotas() {
    let fixture = Fixture::new().await;
    let created = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/users",
        Some(serde_json::json!({
            "username": "listener",
            "password": "correct horse",
            "permissions": { "acquire": false, "upload": true, "download": true },
            "quota": { "acquisitionsPerDay": 5 },
        })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json(created).await;
    let id = created["id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("us_"));
    assert_eq!(created["displayName"], "listener");
    assert_eq!(created["isAdmin"], false);
    assert_eq!(created["permissions"]["acquire"], false);
    assert_eq!(created["quota"]["acquisitionsPerDay"], 5);
    assert!(created["quota"].get("acquisitionBytesPerDay").is_none());

    let taken = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/users",
        Some(serde_json::json!({ "username": "LISTENER", "password": "x" })),
    )
    .await;
    assert_problem(taken, StatusCode::CONFLICT, "conflict").await;
    let empty = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/users",
        Some(serde_json::json!({ "username": "other", "password": "" })),
    )
    .await;
    assert_problem(empty, StatusCode::BAD_REQUEST, "invalid-request").await;

    // Two accounts, one per page.
    let first = json(
        bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/users?limit=1",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(first["users"].as_array().unwrap().len(), 1);
    assert_eq!(first["users"][0]["username"], common::USERNAME);
    let cursor = first["nextCursor"].as_str().unwrap();
    let second = json(
        bearer(
            &fixture,
            reqwest::Method::GET,
            &format!("/api/v1/admin/users?limit=1&cursor={cursor}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(second["users"][0]["username"], "listener");
    assert!(second.get("nextCursor").is_none());

    // The new account may not administer.
    let (cookie, _) = session_of(&fixture, "listener", "correct horse").await;
    let forbidden = fixture
        .client
        .get(format!("{}/api/v1/admin/users", fixture.base_url()))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_problem(forbidden, StatusCode::FORBIDDEN, "forbidden").await;

    // Administrators cannot lock themselves out.
    let me =
        hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::User, fixture.user_id);
    for patch in [
        serde_json::json!({ "isAdmin": false }),
        serde_json::json!({ "disabled": true }),
    ] {
        let refused = bearer(
            &fixture,
            reqwest::Method::PATCH,
            &format!("/api/v1/admin/users/{me}"),
            Some(patch),
        )
        .await;
        assert_problem(refused, StatusCode::CONFLICT, "conflict").await;
    }

    // Changing permissions and quota, then disabling, ends the account's
    // sessions; enabling lets it sign in again.
    let changed = json(
        bearer(
            &fixture,
            reqwest::Method::PATCH,
            &format!("/api/v1/admin/users/{id}"),
            Some(serde_json::json!({
                "displayName": "Listener",
                "permissions": { "acquire": true, "upload": false, "download": true },
                "quota": { "acquisitionBytesPerDay": 1000 },
            })),
        )
        .await,
    )
    .await;
    assert_eq!(changed["displayName"], "Listener");
    assert_eq!(changed["permissions"]["acquire"], true);
    assert_eq!(changed["permissions"]["upload"], false);
    assert!(changed["quota"].get("acquisitionsPerDay").is_none());
    assert_eq!(changed["quota"]["acquisitionBytesPerDay"], 1000);

    let disabled = json(
        bearer(
            &fixture,
            reqwest::Method::PATCH,
            &format!("/api/v1/admin/users/{id}"),
            Some(serde_json::json!({ "disabled": true })),
        )
        .await,
    )
    .await;
    assert!(disabled["disabledAt"].is_i64());
    let ended = fixture
        .client
        .get(format!("{}/api/v1/auth/session", fixture.base_url()))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await
        .unwrap();
    assert_problem(ended, StatusCode::UNAUTHORIZED, "unauthenticated").await;
    assert_eq!(
        login(&fixture, "listener", "correct horse").await.status(),
        StatusCode::UNAUTHORIZED
    );
    bearer(
        &fixture,
        reqwest::Method::PATCH,
        &format!("/api/v1/admin/users/{id}"),
        Some(serde_json::json!({ "disabled": false })),
    )
    .await;
    assert_eq!(
        login(&fixture, "listener", "correct horse").await.status(),
        StatusCode::OK
    );

    let missing = bearer(
        &fixture,
        reqwest::Method::GET,
        "/api/v1/admin/users/us_00000000000000000000000000",
        None,
    )
    .await;
    assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;
    fixture.stop().await;
}
