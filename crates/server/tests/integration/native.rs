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
pub(crate) async fn json(response: reqwest::Response) -> Value {
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
pub(crate) async fn bearer(
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

/// Writes a small tagged album into a directory.
pub(crate) async fn write_album(dir: &std::path::Path, artist: &str, album: &str) {
    use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
    std::fs::create_dir_all(dir).unwrap();
    for number in 1..=2u32 {
        AudioFixture::new(AudioFormat::Mp3Cbr)
            .seconds(1.0)
            .file_name(format!("{number:02} - Song {number}.mp3"))
            .tags(
                TrackTags::new(format!("{album} Song {number}"), artist)
                    .album(album)
                    .album_artist(artist)
                    .track(number, 2),
            )
            .write_into(dir)
            .await
            .unwrap();
    }
}

/// Reads the album names `getAlbumList2` answers for a music folder.
pub(crate) async fn albums_in(fixture: &Fixture, api_key: &str, folder: u8) -> Vec<String> {
    let response = fixture
        .get(&format!(
            "/rest/getAlbumList2?type=alphabeticalByName&musicFolderId={folder}&apiKey={api_key}&f=json"
        ))
        .await;
    let body = json(response).await;
    body["subsonic-response"]["albumList2"]["album"]
        .as_array()
        .map(|albums| {
            albums
                .iter()
                .map(|album| album["name"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A folder needs an owner or `generic`; an owned folder is in its owner's
/// library only, and changing the association moves it.
#[tokio::test]
async fn folders_are_added_with_an_owner_or_as_generic() {
    let fixture = Fixture::new().await;
    let root = fixture
        .database_path()
        .parent()
        .unwrap()
        .join("music-owned");
    write_album(&root, "Folder Artist", "Owned Album").await;
    let me =
        hearthsound_core::EntityId::from_uuid(hearthsound_core::IdPrefix::User, fixture.user_id);

    for (body, status) in [
        (serde_json::json!({ "path": root }), StatusCode::BAD_REQUEST),
        (
            serde_json::json!({ "path": root, "owner": me.to_string(), "generic": true }),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({ "path": root, "owner": "us_00000000000000000000000000" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({ "path": "relative/music", "generic": true }),
            StatusCode::BAD_REQUEST,
        ),
        (
            serde_json::json!({ "path": "/no/such/folder", "generic": true }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/folders",
            Some(body),
        )
        .await;
        assert_problem(refused, status, "invalid-request").await;
    }

    let created = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/folders",
        Some(serde_json::json!({ "path": root, "owner": me.to_string(), "watch": false })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created = json(created).await;
    let folder = created["id"].as_str().unwrap().to_owned();
    assert!(folder.starts_with("lf_"));
    assert_eq!(created["owner"], me.to_string());
    assert_eq!(created["generic"], false);
    assert_eq!(created["watch"], false);

    let duplicate = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/folders",
        Some(serde_json::json!({ "path": root, "generic": true })),
    )
    .await;
    assert_problem(duplicate, StatusCode::CONFLICT, "conflict").await;

    // The first scan was queued with the folder; its album is in the owner's
    // library.
    let mut owned = Vec::new();
    for _ in 0..250 {
        owned = albums_in(&fixture, &fixture.api_key, 1).await;
        if !owned.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
    assert_eq!(owned, ["Owned Album"]);

    // Made generic, the album leaves the library and stays on the server.
    let patched = json(
        bearer(
            &fixture,
            reqwest::Method::PATCH,
            &format!("/api/v1/admin/folders/{folder}"),
            Some(serde_json::json!({ "generic": true, "name": "Shared" })),
        )
        .await,
    )
    .await;
    assert_eq!(patched["generic"], true);
    assert_eq!(patched["name"], "Shared");
    assert!(patched.get("owner").is_none());
    assert!(albums_in(&fixture, &fixture.api_key, 1).await.is_empty());

    let listed = json(
        bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/folders",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(listed["folders"].as_array().unwrap().len(), 1);

    let scan = bearer(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/folders/{folder}/scan"),
        Some(serde_json::json!({ "full": true })),
    )
    .await;
    assert_eq!(scan.status(), StatusCode::ACCEPTED);
    assert!(json(scan).await["jobId"].is_string());
    let unknown = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/folders/lf_00000000000000000000000000/scan",
        None,
    )
    .await;
    assert_problem(unknown, StatusCode::NOT_FOUND, "not-found").await;
    fixture.stop().await;
}

/// Administrators read every server setting with its source and change the
/// ones no file or environment variable pins. A batch with a pinned key writes
/// nothing, and other accounts may not reach the listing at all.
#[tokio::test]
async fn server_settings_are_listed_and_changed_unless_pinned() {
    let fixture = Fixture::new().await;
    let listing = json(
        bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/settings",
            None,
        )
        .await,
    )
    .await;
    let poll = setting(&listing, "acquisition.poll_interval_ms.interactive");
    assert_eq!(poll["source"], "file");
    assert_eq!(poll["pinned"], true);
    assert_eq!(poll["kind"], "integer");
    let browse = setting(&listing, "privacy.server_browse");
    assert_eq!(browse["source"], "default");
    assert_eq!(browse["value"], "permanent_only");
    assert_eq!(browse["kind"], "string");

    // A pinned key refuses the whole batch and is named in the problem.
    let refused = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": {
            "privacy.server_browse": "all",
            "acquisition.poll_interval_ms.interactive": 100,
        } })),
    )
    .await;
    let problem = assert_problem(refused, StatusCode::CONFLICT, "conflict").await;
    assert!(
        problem["detail"]
            .as_str()
            .unwrap()
            .contains("acquisition.poll_interval_ms.interactive")
    );
    let after = json(
        bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/settings",
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        setting(&after, "privacy.server_browse")["value"],
        "permanent_only"
    );

    // An unknown key and a value that does not fit are both invalid requests.
    for refused in [
        serde_json::json!({ "settings": { "no.such.key": true } }),
        serde_json::json!({ "settings": { "privacy.server_browse": "everything" } }),
    ] {
        let response = bearer(
            &fixture,
            reqwest::Method::PATCH,
            "/api/v1/admin/settings",
            Some(refused),
        )
        .await;
        assert_problem(response, StatusCode::BAD_REQUEST, "invalid-request").await;
    }

    // A valid batch is stored; a null removes the stored value.
    let changed = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": {
            "privacy.server_browse": "all",
            "uploads.enabled": false,
        } })),
    )
    .await;
    assert_eq!(changed.status(), StatusCode::OK);
    let changed = json(changed).await;
    assert_eq!(setting(&changed, "privacy.server_browse")["value"], "all");
    assert_eq!(
        setting(&changed, "privacy.server_browse")["source"],
        "database"
    );
    assert_eq!(setting(&changed, "uploads.enabled")["value"], false);
    let reset = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": { "privacy.server_browse": null } })),
    )
    .await;
    let reset = json(reset).await;
    assert_eq!(
        setting(&reset, "privacy.server_browse")["value"],
        "permanent_only"
    );
    assert_eq!(
        setting(&reset, "privacy.server_browse")["source"],
        "default"
    );

    // Another account may not read or change the settings.
    let created = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/users",
        Some(serde_json::json!({ "username": "listener", "password": "correct horse" })),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let (cookie, _csrf) = session_of(&fixture, "listener", "correct horse").await;
    let denied = fixture
        .client
        .get(format!("{}/api/v1/admin/settings", fixture.base_url()))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    assert_problem(denied, StatusCode::FORBIDDEN, "forbidden").await;
    fixture.stop().await;
}

/// An administrator lists the ledger across users, cancels a live acquisition,
/// retries the cancelled one, and is refused a second cancellation or a retry
/// of an attempt that is still live.
#[tokio::test]
async fn an_administrator_lists_cancels_and_retries_acquisitions() {
    let fixture = Fixture::new().await;
    let downloads = fixture.database_path().parent().unwrap().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    common::install_acquisition(&fixture.services, &downloads).await;
    fixture.services.registry().rebuild().await.unwrap();

    let library = hearthsound_library::LibraryRepository::new(fixture.services.database());
    let track = common::insert_remote_track(&library).await;
    fixture
        .services
        .orchestrator()
        .request(hearthsound_acquire::orchestrator::AcquireRequest::new(
            track,
            hearthsound_core::Priority::Play,
            "test",
        ))
        .await
        .unwrap();

    let active = json(bearer_get(&fixture, "/api/v1/admin/acquisitions").await).await;
    let live = active["acquisitions"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(active["acquisitions"].as_array().unwrap().len(), 1);

    let cancelled = bearer(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/acquisitions/{live}/cancel"),
        None,
    )
    .await;
    assert_eq!(cancelled.status(), StatusCode::NO_CONTENT);

    let again = bearer(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/acquisitions/{live}/cancel"),
        None,
    )
    .await;
    assert_problem(again, StatusCode::CONFLICT, "conflict").await;

    let ended = json(bearer_get(&fixture, "/api/v1/admin/acquisitions?state=recent").await).await;
    assert_eq!(ended["acquisitions"][0]["id"], live.as_str());
    assert_eq!(ended["acquisitions"][0]["state"], "cancelled");

    let retried = bearer(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/acquisitions/{live}/retry"),
        None,
    )
    .await;
    assert_eq!(retried.status(), StatusCode::ACCEPTED);
    let retried = json(retried).await;
    assert_eq!(retried["outcome"], "started");
    let attempt = retried["id"].as_str().unwrap().to_owned();
    assert_ne!(attempt, live);

    let refused = bearer(
        &fixture,
        reqwest::Method::POST,
        &format!("/api/v1/admin/acquisitions/{attempt}/retry"),
        None,
    )
    .await;
    assert_problem(refused, StatusCode::CONFLICT, "conflict").await;

    let unknown = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/admin/acquisitions/ac_unknown/retry",
        None,
    )
    .await;
    assert_problem(unknown, StatusCode::NOT_FOUND, "not-found").await;
    fixture.stop().await;
}

/// Builds a `multipart/form-data` body with one `file` part per entry, and
/// the `Content-Type` that names its boundary.
fn multipart_body(files: &[(&str, Vec<u8>)]) -> (String, Vec<u8>) {
    let boundary = "hsuploadboundary";
    let mut body = Vec::new();
    for (name, bytes) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Sends a multipart upload, authenticated by bearer key or by a session
/// cookie with its CSRF token.
async fn upload_files(
    fixture: &Fixture,
    session: Option<(&str, &str)>,
    files: &[(&str, Vec<u8>)],
) -> reqwest::Response {
    let (content_type, body) = multipart_body(files);
    let mut request = fixture
        .client
        .post(format!("{}/api/v1/uploads", fixture.base_url()))
        .header(CONTENT_TYPE, content_type);
    request = match session {
        Some((cookie, csrf)) => request
            .header(reqwest::header::COOKIE, cookie)
            .header("x-hearthsound-csrf", csrf),
        None => request.header(AUTHORIZATION, format!("Bearer {}", fixture.api_key)),
    };
    request.body(body).send().await.unwrap()
}

/// Generates a one-second FLAC file titled `title` and returns its bytes.
async fn flac_bytes(dir: &std::path::Path, title: &str) -> Vec<u8> {
    use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
    std::fs::create_dir_all(dir).unwrap();
    let path = AudioFixture::new(AudioFormat::Flac)
        .seconds(1.0)
        .file_name(format!("{title}.flac"))
        .tags(TrackTags::new(title, "Uploader").album("Uploads"))
        .write_into(dir)
        .await
        .unwrap();
    std::fs::read(path).unwrap()
}

/// Files sent over the native API land in the sender's library: lossless
/// audio becomes a track the Subsonic API serves, unsupported content fails
/// its own entry, and the account and setting rules refuse what they should.
#[tokio::test]
async fn uploads_land_in_the_callers_library_and_are_refused_when_they_should_be() {
    let fixture = Fixture::new().await;
    let scratch = fixture
        .database_path()
        .parent()
        .unwrap()
        .join("upload-input");
    let one = flac_bytes(&scratch, "One").await;
    let two = flac_bytes(&scratch, "Two").await;

    // Administrators upload directly; the bad entry does not stop the others.
    let mut events = fixture.services.events().subscribe();
    let mixed = upload_files(
        &fixture,
        None,
        &[
            ("one.flac", one.clone()),
            ("notes.flac", b"this is not audio".to_vec()),
            ("two.flac", two),
        ],
    )
    .await;
    assert_eq!(mixed.status(), StatusCode::CREATED);
    let mixed = json(mixed).await;
    let entries = mixed["uploads"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["fileName"], "one.flac");
    assert_eq!(entries[0]["state"], "completed");
    assert!(entries[0]["trackId"].as_str().unwrap().starts_with("tr_"));
    assert_eq!(entries[1]["state"], "failed");
    assert_eq!(entries[1]["error"], "unsupported-media");
    assert!(entries[1].get("trackId").is_none());
    assert_eq!(entries[2]["state"], "completed");

    // The first file's progress is published in order, once per stage.
    let mut stages = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        if let hearthsound_core::events::ServerEvent::UploadUpdated(update) = envelope.event
            && Some(update.upload_id.as_str()) == entries[0]["uploadId"].as_str()
        {
            stages.push(update.state);
        }
    }
    assert_eq!(stages, ["receiving", "processing", "completed"]);

    let track = entries[0]["trackId"].as_str().unwrap();
    let song = json(
        fixture
            .get(&format!(
                "/rest/getSong?id={track}&apiKey={}&f=json",
                fixture.api_key
            ))
            .await,
    )
    .await;
    assert_eq!(song["subsonic-response"]["song"]["title"], "One");

    // Another user streams the uploaded track; uploads are shared storage.
    let (_, neighbour_key) = crate::multi_user::create_user(&fixture, "neighbour", false).await;
    let streamed = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&apiKey={neighbour_key}",
            fixture.base_url()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(streamed.status(), StatusCode::OK);
    let body = streamed.bytes().await.unwrap();
    assert!(body.starts_with(b"fLaC"), "the stream is the uploaded FLAC");

    // A file over the configured limit ends the request with too-large.
    let limited = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": { "uploads.max_file_bytes": 1024 } })),
    )
    .await;
    assert_eq!(limited.status(), StatusCode::OK);
    let too_big = upload_files(&fixture, None, &[("big.flac", one.clone())]).await;
    assert_problem(too_big, StatusCode::PAYLOAD_TOO_LARGE, "too-large").await;

    // A body that is not multipart is an invalid request.
    let plain = bearer(
        &fixture,
        reqwest::Method::POST,
        "/api/v1/uploads",
        Some(serde_json::json!({ "file": "x" })),
    )
    .await;
    assert_problem(plain, StatusCode::BAD_REQUEST, "invalid-request").await;

    // A listener without the upload permission is refused; an uploader with
    // it lands the file, and cannot upload once the server turns uploads off.
    for (name, upload) in [("listener", false), ("uploader", true)] {
        let created = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/users",
            Some(serde_json::json!({
                "username": name,
                "password": "correct horse",
                "permissions": { "acquire": false, "upload": upload, "download": true },
            })),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
    }
    let (listener, listener_csrf) = session_of(&fixture, "listener", "correct horse").await;
    let refused = upload_files(
        &fixture,
        Some((&listener, &listener_csrf)),
        &[("three.flac", one.clone())],
    )
    .await;
    assert_problem(refused, StatusCode::FORBIDDEN, "permission-denied").await;

    // The limit is restored so the uploader's file fits.
    let restored = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": { "uploads.max_file_bytes": 268_435_456 } })),
    )
    .await;
    assert_eq!(restored.status(), StatusCode::OK);

    let (uploader, uploader_csrf) = session_of(&fixture, "uploader", "correct horse").await;
    let landed = upload_files(
        &fixture,
        Some((&uploader, &uploader_csrf)),
        &[("three.flac", flac_bytes(&scratch, "Three").await)],
    )
    .await;
    assert_eq!(landed.status(), StatusCode::CREATED);
    let landed = json(landed).await;
    assert_eq!(landed["uploads"][0]["state"], "completed");

    let off = bearer(
        &fixture,
        reqwest::Method::PATCH,
        "/api/v1/admin/settings",
        Some(serde_json::json!({ "settings": { "uploads.enabled": false } })),
    )
    .await;
    assert_eq!(off.status(), StatusCode::OK);
    let closed = upload_files(
        &fixture,
        Some((&uploader, &uploader_csrf)),
        &[("four.flac", one)],
    )
    .await;
    assert_problem(closed, StatusCode::FORBIDDEN, "permission-denied").await;
    fixture.stop().await;
}

/// Full-stack tests of `/api/v1/admin/plugins`: installing from a registered
/// index and from a repository URL, uploading an archive, the sideload and
/// repository gates, the approval lifecycle over the API, and the listing of
/// what the indexes offer.
///
/// The indexes and archives are served by a mock HTTP server, so the real
/// download, signature-verification and integrity paths run without a network.
mod plugin_admin {
    use std::io::{Read, Write};
    use std::path::Path;
    use std::time::Duration;

    use hearthsound_api_native::views::client_id;
    use hearthsound_core::IdPrefix;
    use hearthsound_test_support::{METADATA_FIXTURE, fixture_plugin};
    use minisign::KeyPair;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The id of the metadata fixture the tests install.
    const PLUGIN_ID: &str = "com.example.metadata-fixture";

    /// The API version this server hosts.
    const API: &str = "hearthsound:plugin@0.2.0";

    /// The path an index document is served at in these tests.
    const INDEX_PATH: &str = "/v1/index.json";

    /// The path the fixture archive is served at in these tests.
    const ARCHIVE_PATH: &str = "/download/metadata.hsplugin";

    /// The boundary of the multipart bodies the tests send.
    const BOUNDARY: &str = "----hearthsound-test-boundary";

    /// One signing key pair a test publishes an index with.
    struct Signer {
        /// The key pair itself.
        pair: KeyPair,
    }

    impl Signer {
        /// Generates a fresh key pair.
        fn new() -> Signer {
            Signer {
                pair: KeyPair::generate_unencrypted_keypair().unwrap(),
            }
        }

        /// Returns the base64 public key of this signer.
        fn public_key(&self) -> String {
            self.pair.pk.to_base64()
        }

        /// Signs one document and returns the `.minisig` text.
        fn sign(&self, document: &str) -> String {
            minisign::sign(
                Some(&self.pair.pk),
                &self.pair.sk,
                document.as_bytes(),
                None,
                None,
            )
            .unwrap()
            .to_string()
        }
    }

    /// Returns the directory the installed plugins live in.
    fn plugins_dir(fixture: &Fixture) -> &Path {
        fixture.services.plugins().plugins_dir()
    }

    /// The bytes of the metadata fixture archive.
    fn fixture_bytes() -> Vec<u8> {
        std::fs::read(fixture_plugin(METADATA_FIXTURE)).unwrap()
    }

    /// Renders bytes as lowercase hexadecimal.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Serves one 200 response with a text body.
    async fn serve_text(server: &MockServer, document_path: &str, body: &str) {
        Mock::given(method("GET"))
            .and(path(document_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Serves one 200 response with a binary body.
    async fn serve_bytes(server: &MockServer, document_path: &str, body: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(document_path))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(server)
            .await;
    }

    /// Builds one index document.
    fn document(plugins: &[Value]) -> String {
        json!({
            "formatVersion": 1,
            "name": "Test index",
            "generatedAt": "2026-01-01T00:00:00Z",
            "plugins": plugins,
        })
        .to_string()
    }

    /// Builds one plugin entry.
    fn entry(id: &str, official: bool, versions: &[Value]) -> Value {
        json!({
            "id": id,
            "name": "Metadata fixture",
            "description": "A metadata plugin.",
            "provides": ["metadata"],
            "homepage": "https://example.com/metadata",
            "license": "MIT",
            "official": official,
            "versions": versions,
        })
    }

    /// Builds one version entry for an archive.
    fn version(version: &str, api: &str, url: &str, bytes: &[u8], permissions: Value) -> Value {
        json!({
            "version": version,
            "api": api,
            "url": url,
            "sha256": hex(&Sha256::digest(bytes)),
            "size": bytes.len(),
            "publishedAt": "2026-01-01T00:00:00Z",
            "changelog": "A release.",
            "permissions": permissions,
        })
    }

    /// Serves a signed index at [`INDEX_PATH`] and returns its URL.
    async fn serve_signed_index(server: &MockServer, signer: &Signer, index: &str) -> String {
        serve_text(server, INDEX_PATH, index).await;
        serve_text(
            server,
            &format!("{INDEX_PATH}.minisig"),
            &signer.sign(index),
        )
        .await;
        format!("{}{INDEX_PATH}", server.uri())
    }

    /// Registers a repository and returns its client id.
    async fn register_repository(fixture: &Fixture, url: &str, public_key: Option<&str>) -> String {
        let row = fixture
            .services
            .plugin_repository()
            .insert_repository(url, Some("test"), public_key, true)
            .await
            .unwrap();
        client_id(IdPrefix::PluginRepository, row.id)
    }

    /// Installs one plugin from an index through the API and returns its view.
    ///
    /// A repository is registered for the index unless `repository` names one.
    async fn install_from_index(
        fixture: &Fixture,
        server: &MockServer,
        signer: &Signer,
        index: &str,
        repository: Option<&str>,
    ) -> Value {
        let url = serve_signed_index(server, signer, index).await;
        let repository = match repository {
            Some(id) => id.to_owned(),
            None => register_repository(fixture, &url, Some(&signer.public_key())).await,
        };
        let response = bearer(
            fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({
                "source": "index",
                "pluginId": PLUGIN_ID,
                "repositoryId": repository,
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "the install failed");
        json(response).await
    }

    /// Sends a multipart upload of one `.hsplugin` file.
    async fn upload(fixture: &Fixture, bytes: &[u8]) -> reqwest::Response {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"plugin.hsplugin\"\r\n",
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
        fixture
            .client
            .post(format!(
                "{}/api/v1/admin/plugins/upload",
                fixture.base_url()
            ))
            .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .unwrap()
    }

    /// Asserts nothing of a plugin was recorded or written.
    async fn assert_untouched(fixture: &Fixture, id: &str) {
        let listed =
            json(bearer(fixture, reqwest::Method::GET, "/api/v1/admin/plugins", None).await).await;
        assert!(
            listed["plugins"]
                .as_array()
                .unwrap()
                .iter()
                .all(|plugin| plugin["id"] != id),
            "the plugin was recorded: {listed}"
        );
        assert!(!plugins_dir(fixture).join(id).exists());
        let leftovers: Vec<String> = std::fs::read_dir(plugins_dir(fixture))
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(".download-") || name.starts_with(".upload-"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "a temporary file was left behind: {leftovers:?}"
        );
    }

    /// Reads one entry out of an archive.
    fn read_entry(archive: &Path, name: &str) -> Vec<u8> {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(archive).unwrap()).unwrap();
        let mut buffer = Vec::new();
        zip.by_name(name).unwrap().read_to_end(&mut buffer).unwrap();
        buffer
    }

    /// Writes the metadata fixture repacked at `version` with `permissions`.
    ///
    /// The component is copied unchanged, so only the manifest differs.
    fn repack(dir: &Path, version: &str, permissions: Value) -> Vec<u8> {
        let source = fixture_plugin(METADATA_FIXTURE);
        let mut manifest: Value =
            serde_json::from_slice(&read_entry(&source, "manifest.json")).unwrap();
        manifest["version"] = json!(version);
        manifest["permissions"] = permissions;
        let wasm = read_entry(&source, "plugin.wasm");
        let target = dir.join(format!("metadata-{version}.hsplugin"));
        let file = std::fs::File::create(&target).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file("manifest.json", options).unwrap();
        writer
            .write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        writer.start_file("plugin.wasm", options).unwrap();
        writer.write_all(&wasm).unwrap();
        writer.finish().unwrap();
        std::fs::read(&target).unwrap()
    }

    #[tokio::test]
    async fn a_plugin_installs_from_a_registered_index_and_is_listed() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        // The archive itself requests the permission, and the index declares
        // the same set, so the view reports what the install records.
        let scratch = tempfile::tempdir().unwrap();
        let bytes = repack(
            scratch.path(),
            "0.1.0",
            json!({ "http": { "hosts": ["api.example.com"] } }),
        );
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version(
                "0.1.0",
                API,
                &archive,
                &bytes,
                json!({ "http": { "hosts": ["api.example.com"] } }),
            )],
        )]);
        let plugin = install_from_index(&fixture, &server, &signer, &index, None).await;

        assert_eq!(plugin["id"], PLUGIN_ID);
        assert_eq!(plugin["version"], "0.1.0");
        assert_eq!(plugin["api"], API);
        assert_eq!(plugin["provides"], json!(["metadata"]));
        assert_eq!(plugin["source"], "repository");
        assert_eq!(plugin["state"], "pending-approval");
        assert_eq!(plugin["enabled"], false);
        assert_eq!(plugin["approved"], false);
        assert!(plugin["update"].is_null());
        assert!(plugin["installedAt"].is_i64());
        let repository = plugin["repositoryId"].as_str().unwrap().to_owned();
        assert!(repository.starts_with("pr_"));
        let permissions = plugin["requestedPermissions"].as_array().unwrap();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0]["scope"], "http.hosts");
        assert_eq!(permissions[0]["detail"], "api.example.com");
        assert_eq!(permissions[0]["risk"], "low");
        assert!(plugins_dir(&fixture).join(PLUGIN_ID).join("0.1.0").is_dir());

        // The listing and the single read agree with the install's answer.
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugins",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["plugins"].as_array().unwrap().len(), 1);
        assert_eq!(listed["plugins"][0], plugin);
        let read = bearer(
            &fixture,
            reqwest::Method::GET,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}"),
            None,
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(json(read).await, plugin);

        // An unknown plugin is a not-found problem.
        let missing = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/plugins/com.example.unknown",
            None,
        )
        .await;
        assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;

        // The next commits read the same repository row.
        assert_eq!(plugin["repositoryId"], repository);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn approval_gates_enabling_and_the_lifecycle_steers_the_plugin() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            true,
            &[version("0.1.0", API, &archive, &bytes, json!({}))],
        )]);
        install_from_index(&fixture, &server, &signer, &index, None).await;

        // A pending plugin cannot be switched on, and has nothing to approve
        // twice.
        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/enable"),
            None,
        )
        .await;
        assert_problem(refused, StatusCode::CONFLICT, "conflict").await;
        let declined = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/decline-upgrade"),
            None,
        )
        .await;
        assert_problem(declined, StatusCode::CONFLICT, "conflict").await;

        // Approval records the permissions and enables the plugin.
        let approved = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
            None,
        )
        .await;
        assert_eq!(approved.status(), StatusCode::OK);
        let approved = json(approved).await;
        assert_eq!(approved["state"], "enabled");
        assert_eq!(approved["approved"], true);
        assert_eq!(approved["enabled"], true);

        let again = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
            None,
        )
        .await;
        assert_problem(again, StatusCode::CONFLICT, "conflict").await;

        // Disabling and enabling an approved plugin.
        let disabled = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/disable"),
            None,
        )
        .await;
        let disabled = json(disabled).await;
        assert_eq!(disabled["state"], "disabled");
        assert_eq!(disabled["enabled"], false);
        let enabled = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/enable"),
            None,
        )
        .await;
        let enabled = json(enabled).await;
        assert_eq!(enabled["state"], "enabled");
        assert_eq!(enabled["enabled"], true);

        // An unknown plugin is a not-found problem on every mutation.
        let missing = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins/com.example.unknown/approve",
            None,
        )
        .await;
        assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;

        // Removal deletes the rows and the files.
        let removed = bearer(
            &fixture,
            reqwest::Method::DELETE,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}"),
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let gone = bearer(
            &fixture,
            reqwest::Method::GET,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}"),
            None,
        )
        .await;
        assert_problem(gone, StatusCode::NOT_FOUND, "not-found").await;
        assert!(!plugins_dir(&fixture).join(PLUGIN_ID).exists());
        fixture.stop().await;
    }

    #[tokio::test]
    async fn an_upgrade_that_requests_new_permissions_waits_for_approval() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version("0.1.0", API, &archive, &bytes, json!({}))],
        )]);
        install_from_index(&fixture, &server, &signer, &index, None).await;
        bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
            None,
        )
        .await;

        // A newer version asking for a permission the installed one does not
        // hold is staged, and the installed version keeps serving.
        let scratch = tempfile::tempdir().unwrap();
        let upgrade = repack(
            scratch.path(),
            "0.2.0",
            json!({ "http": { "hosts": ["api.example.com"] } }),
        );
        let uploaded = upload(&fixture, &upgrade).await;
        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let staged = json(uploaded).await;
        assert_eq!(staged["state"], "upgrade-pending-approval");
        assert_eq!(staged["version"], "0.1.0");
        assert_eq!(staged["enabled"], true);
        assert!(plugins_dir(&fixture).join(PLUGIN_ID).join("0.2.0").is_dir());

        // Approving applies the upgrade and prunes the old version's files.
        let applied = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
            None,
        )
        .await;
        assert_eq!(applied.status(), StatusCode::OK);
        let applied = json(applied).await;
        assert_eq!(applied["state"], "enabled");
        assert_eq!(applied["version"], "0.2.0");
        assert_eq!(
            applied["requestedPermissions"][0]["detail"],
            "api.example.com"
        );
        assert!(plugins_dir(&fixture).join(PLUGIN_ID).join("0.2.0").is_dir());
        assert!(!plugins_dir(&fixture).join(PLUGIN_ID).join("0.1.0").exists());

        // A further upgrade with a new permission is staged and declined,
        // which keeps the installed version.
        let further = repack(
            scratch.path(),
            "0.3.0",
            json!({ "http": { "hosts": ["api.example.com", "api.other.example"] } }),
        );
        let staged = json(upload(&fixture, &further).await).await;
        assert_eq!(staged["state"], "upgrade-pending-approval");
        let declined = bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/decline-upgrade"),
            None,
        )
        .await;
        assert_eq!(declined.status(), StatusCode::OK);
        let declined = json(declined).await;
        assert_eq!(declined["state"], "enabled");
        assert_eq!(declined["version"], "0.2.0");
        assert!(!plugins_dir(&fixture).join(PLUGIN_ID).join("0.3.0").exists());
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_plugin_installs_from_a_repository_url() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = serve_signed_index(
            &server,
            &signer,
            &document(&[entry(
                PLUGIN_ID,
                false,
                &[version("0.1.0", API, &archive, &bytes, json!({}))],
            )]),
        )
        .await;

        // The URL is registered on the way in and the plugin lands in it.
        let installed = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({ "source": "url", "pluginId": PLUGIN_ID, "url": index })),
        )
        .await;
        assert_eq!(installed.status(), StatusCode::CREATED);
        let installed = json(installed).await;
        assert_eq!(installed["source"], "repository");
        let repository = installed["repositoryId"].as_str().unwrap();
        assert!(repository.starts_with("pr_"));
        let row = fixture
            .services
            .plugin_repository()
            .repositories()
            .await
            .unwrap();
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].url, index);
        assert!(row[0].last_fetched_at.is_some());

        // Installing the same version again is a conflict.
        let again = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({ "source": "url", "pluginId": PLUGIN_ID, "url": index })),
        )
        .await;
        assert_problem(again, StatusCode::CONFLICT, "conflict").await;

        // A source that is neither `index` nor `url` is refused.
        let bad_source = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({ "source": "archive", "pluginId": PLUGIN_ID })),
        )
        .await;
        assert_problem(bad_source, StatusCode::BAD_REQUEST, "invalid-request").await;

        // `source` `url` without a URL is refused before anything is fetched.
        let no_url = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({ "source": "url", "pluginId": PLUGIN_ID })),
        )
        .await;
        assert_problem(no_url, StatusCode::BAD_REQUEST, "invalid-request").await;

        // An unknown repository id is a not-found problem.
        let unknown = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({
                "source": "index",
                "pluginId": PLUGIN_ID,
                "repositoryId": "pr_00000000000000000000000000",
            })),
        )
        .await;
        assert_problem(unknown, StatusCode::NOT_FOUND, "not-found").await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_new_third_party_repository_is_gated() {
        let fixture = Fixture::with_config(|config| {
            config.settings.insert(
                "plugins.allow_third_party_repositories".to_owned(),
                toml::Value::Boolean(false),
            );
        })
        .await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = serve_signed_index(
            &server,
            &signer,
            &document(&[entry(
                PLUGIN_ID,
                false,
                &[version("0.1.0", API, &archive, &bytes, json!({}))],
            )]),
        )
        .await;

        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({ "source": "url", "pluginId": PLUGIN_ID, "url": index })),
        )
        .await;
        assert_problem(refused, StatusCode::FORBIDDEN, "repositories-disabled").await;
        assert!(
            fixture
                .services
                .plugin_repository()
                .repositories()
                .await
                .unwrap()
                .is_empty()
        );
        assert_untouched(&fixture, PLUGIN_ID).await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_plugin_uploads_as_a_sideload_and_the_gate_refuses_it() {
        let fixture = Fixture::new().await;
        let bytes = fixture_bytes();
        let uploaded = upload(&fixture, &bytes).await;
        assert_eq!(uploaded.status(), StatusCode::CREATED);
        let plugin = json(uploaded).await;
        assert_eq!(plugin["id"], PLUGIN_ID);
        assert_eq!(plugin["version"], "0.1.0");
        assert_eq!(plugin["source"], "sideload");
        assert_eq!(plugin["state"], "pending-approval");
        assert!(plugin.get("repositoryId").is_none());
        assert!(plugins_dir(&fixture).join(PLUGIN_ID).join("0.1.0").is_dir());
        let leftovers = std::fs::read_dir(plugins_dir(&fixture))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".upload-"))
            .count();
        assert_eq!(leftovers, 0, "the staged upload was not removed");
        fixture.stop().await;

        // With sideloading off the route refuses before reading the body.
        let closed = Fixture::with_config(|config| {
            config.settings.insert(
                "plugins.allow_sideload".to_owned(),
                toml::Value::Boolean(false),
            );
        })
        .await;
        let refused = upload(&closed, &bytes).await;
        assert_problem(refused, StatusCode::FORBIDDEN, "sideload-disabled").await;
        assert_untouched(&closed, PLUGIN_ID).await;
        closed.stop().await;
    }

    #[tokio::test]
    async fn a_bad_signature_or_a_digest_mismatch_blocks_the_install() {
        // An index whose signature does not verify is refused, and leaves no
        // row and no file.
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version("0.1.0", API, &archive, &bytes, json!({}))],
        )]);
        let url = format!("{}{INDEX_PATH}", server.uri());
        // The document is served, but the signature belongs to another one.
        serve_text(&server, INDEX_PATH, &index).await;
        serve_text(
            &server,
            &format!("{INDEX_PATH}.minisig"),
            &signer.sign("a different document"),
        )
        .await;
        let repository = register_repository(&fixture, &url, Some(&signer.public_key())).await;
        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({
                "source": "index",
                "pluginId": PLUGIN_ID,
                "repositoryId": repository,
            })),
        )
        .await;
        assert_problem(refused, StatusCode::BAD_GATEWAY, "signature-invalid").await;
        assert_untouched(&fixture, PLUGIN_ID).await;
        fixture.stop().await;

        // An archive that is not what the index declares is refused.
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let mut broken = version("0.1.0", API, &archive, &bytes, json!({}));
        broken["sha256"] = json!(hex(&[0u8; 32]));
        let index = document(&[entry(PLUGIN_ID, false, &[broken])]);
        let plugin = install_from_index_expecting_failure(&fixture, &server, &signer, &index).await;
        assert_problem(plugin, StatusCode::BAD_GATEWAY, "integrity-failed").await;
        assert_untouched(&fixture, PLUGIN_ID).await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn an_unreachable_index_is_reported_and_recorded() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        // Nothing serves the index, so the fetch answers 404.
        let url = format!("{}{INDEX_PATH}", server.uri());
        let repository = register_repository(&fixture, &url, None).await;

        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({
                "source": "index",
                "pluginId": PLUGIN_ID,
                "repositoryId": repository,
            })),
        )
        .await;
        assert_problem(refused, StatusCode::BAD_GATEWAY, "index-unavailable").await;
        assert_untouched(&fixture, PLUGIN_ID).await;

        // The failed fetch is recorded on the repository's row.
        let rows = fixture
            .services
            .plugin_repository()
            .repositories()
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].last_error.is_some());
        assert!(rows[0].last_fetched_at.is_some());
        fixture.stop().await;
    }

    /// Installs from a signed index that is expected to fail.
    async fn install_from_index_expecting_failure(
        fixture: &Fixture,
        server: &MockServer,
        signer: &Signer,
        index: &str,
    ) -> reqwest::Response {
        let url = serve_signed_index(server, signer, index).await;
        let repository = register_repository(fixture, &url, Some(&signer.public_key())).await;
        bearer(
            fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugins",
            Some(json!({
                "source": "index",
                "pluginId": PLUGIN_ID,
                "repositoryId": repository,
            })),
        )
        .await
    }

    #[tokio::test]
    async fn an_archive_that_is_not_a_plugin_is_refused() {
        // The index declares an archive that is not a plugin at all.
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let garbage = b"this is not a plugin archive".to_vec();
        serve_bytes(&server, ARCHIVE_PATH, garbage.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version("0.1.0", API, &archive, &garbage, json!({}))],
        )]);
        let refused =
            install_from_index_expecting_failure(&fixture, &server, &signer, &index).await;
        assert_problem(refused, StatusCode::UNPROCESSABLE_ENTITY, "invalid-plugin").await;
        assert_untouched(&fixture, PLUGIN_ID).await;
        fixture.stop().await;

        // The index offers only a version this server cannot host.
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version(
                "0.1.0",
                "hearthsound:plugin@0.3.0",
                &archive,
                &bytes,
                json!({}),
            )],
        )]);
        let refused =
            install_from_index_expecting_failure(&fixture, &server, &signer, &index).await;
        assert_problem(refused, StatusCode::UNPROCESSABLE_ENTITY, "invalid-plugin").await;
        assert_untouched(&fixture, PLUGIN_ID).await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn the_available_listing_merges_sources() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());

        // One signed repository offering the fixture, and one unsigned
        // repository offering another plugin.
        let index = serve_signed_index(
            &server,
            &signer,
            &document(&[entry(
                PLUGIN_ID,
                false,
                &[version("0.1.0", API, &archive, &bytes, json!({}))],
            )]),
        )
        .await;
        let signed = register_repository(&fixture, &index, Some(&signer.public_key())).await;
        let other_path = "/other/index.json";
        let other = format!("{}{other_path}", server.uri());
        serve_text(
            &server,
            other_path,
            &document(&[entry(
                "com.example.other",
                false,
                &[version("2.0.0", API, &archive, &bytes, json!({}))],
            )]),
        )
        .await;
        let unsigned = register_repository(&fixture, &other, None).await;

        // One repository alone lists only its own plugin, and reports whether
        // its signature was verified.
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                &format!("/api/v1/admin/plugins/available?repositoryId={signed}"),
                None,
            )
            .await,
        )
        .await;
        let plugins = listed["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["id"], PLUGIN_ID);
        assert_eq!(plugins[0]["verified"], true);
        assert_eq!(plugins[0]["official"], false);
        assert_eq!(plugins[0]["version"], "0.1.0");
        assert_eq!(plugins[0]["repositoryId"], signed);
        assert_eq!(plugins[0]["provides"], json!(["metadata"]));
        assert!(plugins[0]["unavailable"].is_null());

        // Each registered source lists its own plugin, and reports whether its
        // signature was verified. The compiled-in official index is left out of
        // this call on purpose: the server suite serves no network, and the
        // merged listing that includes it is covered by the plugin host's own
        // distribution tests, where the index client's URL is injected.
        let other_listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                &format!("/api/v1/admin/plugins/available?repositoryId={unsigned}"),
                None,
            )
            .await,
        )
        .await;
        let plugins = other_listed["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0]["id"], "com.example.other");
        assert_eq!(plugins[0]["verified"], false);
        assert_eq!(plugins[0]["repositoryId"], unsigned);

        // An unknown repository id is a not-found problem.
        let unknown = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/plugins/available?repositoryId=pr_00000000000000000000000000",
            None,
        )
        .await;
        assert_problem(unknown, StatusCode::NOT_FOUND, "not-found").await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn the_listing_reports_the_update_a_check_found() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        let signer = Signer::new();
        let bytes = fixture_bytes();
        serve_bytes(&server, ARCHIVE_PATH, bytes.clone()).await;
        let archive = format!("{}{ARCHIVE_PATH}", server.uri());
        let index = document(&[entry(
            PLUGIN_ID,
            false,
            &[version("0.1.0", API, &archive, &bytes, json!({}))],
        )]);
        install_from_index(&fixture, &server, &signer, &index, None).await;
        bearer(
            &fixture,
            reqwest::Method::POST,
            &format!("/api/v1/admin/plugins/{PLUGIN_ID}/approve"),
            None,
        )
        .await;
        let plugins = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugins",
                None,
            )
            .await,
        )
        .await;
        assert!(plugins["plugins"][0]["update"].is_null());

        // The repository starts publishing a newer compatible version.
        server.reset().await;
        serve_signed_index(
            &server,
            &signer,
            &document(&[entry(
                PLUGIN_ID,
                false,
                &[version("0.2.0", API, &archive, &bytes, json!({}))],
            )]),
        )
        .await;

        // A restart runs the update check with the plugin installed, so the
        // listing reports the update the check found.
        let api_key = fixture.api_key.clone();
        let dir = fixture.stop().await;
        let fixture = Fixture::reopen(dir, api_key).await;
        let mut found = false;
        for _ in 0..100 {
            if let Some(report) = fixture.services.update_job().last_check()
                && report
                    .updates
                    .iter()
                    .any(|update| update.plugin_id == PLUGIN_ID)
            {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found, "the update check found no update");

        let plugins = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugins",
                None,
            )
            .await,
        )
        .await;
        let plugin = &plugins["plugins"][0];
        assert_eq!(plugin["version"], "0.1.0");
        assert_eq!(plugin["state"], "enabled");
        assert_eq!(plugin["update"]["version"], "0.2.0");
        // The update asks for nothing new, so approving it is not required.
        assert_eq!(plugin["update"]["requiresApproval"], false);
        fixture.stop().await;
    }
}

mod repository_admin {
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The path an index document is served at in these tests.
    const INDEX_PATH: &str = "/v1/index.json";

    /// The URL of a repository no test serves.
    const UNKNOWN: &str = "pr_00000000000000000000000000";

    #[tokio::test]
    async fn repositories_are_added_listed_changed_and_removed() {
        let fixture = Fixture::new().await;

        // A signed repository with everything the resource takes.
        let created = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugin-repositories",
            Some(json!({
                "url": "https://plugins.example/v1/index.json",
                "name": "Example",
                "publicKey": "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
            })),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = json(created).await;
        let id = created["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("pr_"), "{created}");
        assert_eq!(created["url"], "https://plugins.example/v1/index.json");
        assert_eq!(created["name"], "Example");
        assert_eq!(
            created["publicKey"],
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3"
        );
        assert_eq!(created["enabled"], true);
        assert_eq!(created["verified"], true);
        // Nothing has fetched the index yet, so the row carries no history.
        assert!(created.get("lastFetchedAt").is_none(), "{created}");
        assert!(created.get("lastError").is_none(), "{created}");

        // A repository without a key is read unsigned and reported unverified.
        let unsigned = json(
            bearer(
                &fixture,
                reqwest::Method::POST,
                "/api/v1/admin/plugin-repositories",
                Some(json!({ "url": "https://other.example/v1/index.json" })),
            )
            .await,
        )
        .await;
        assert_eq!(unsigned["name"], serde_json::Value::Null);
        assert_eq!(unsigned["verified"], false);
        assert!(unsigned.get("publicKey").is_none(), "{unsigned}");

        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugin-repositories",
                None,
            )
            .await,
        )
        .await;
        let rows = listed["repositories"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        // The listing is by URL, and agrees with what the writes answered.
        assert_eq!(rows[0], unsigned);
        assert_eq!(rows[1], created);

        // A patch changes the mutable fields and clears what it sends as null.
        let patched = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                &format!("/api/v1/admin/plugin-repositories/{id}"),
                Some(json!({ "name": "Renamed", "publicKey": null, "enabled": false })),
            )
            .await,
        )
        .await;
        assert_eq!(patched["name"], "Renamed");
        assert_eq!(patched["enabled"], false);
        assert_eq!(patched["verified"], false);
        assert!(patched.get("publicKey").is_none(), "{patched}");
        assert_eq!(patched["url"], "https://plugins.example/v1/index.json");

        // An empty patch leaves every field as it was.
        let unchanged = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                &format!("/api/v1/admin/plugin-repositories/{id}"),
                Some(json!({})),
            )
            .await,
        )
        .await;
        assert_eq!(unchanged, patched);

        // Removal answers no content, and the row is gone.
        let removed = bearer(
            &fixture,
            reqwest::Method::DELETE,
            &format!("/api/v1/admin/plugin-repositories/{id}"),
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugin-repositories",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["repositories"].as_array().unwrap().len(), 1);

        // An id that names no repository is a not-found problem on every route.
        for method in [reqwest::Method::PATCH, reqwest::Method::DELETE] {
            let missing = bearer(
                &fixture,
                method.clone(),
                &format!("/api/v1/admin/plugin-repositories/{UNKNOWN}"),
                if method == reqwest::Method::PATCH {
                    Some(json!({ "enabled": true }))
                } else {
                    None
                },
            )
            .await;
            assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;
        }
        let missing = bearer(
            &fixture,
            reqwest::Method::PATCH,
            "/api/v1/admin/plugin-repositories/not-an-id",
            Some(json!({ "enabled": true })),
        )
        .await;
        assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_duplicate_url_is_a_conflict() {
        let fixture = Fixture::new().await;
        let body = json!({ "url": "https://plugins.example/v1/index.json" });
        let first = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugin-repositories",
            Some(body.clone()),
        )
        .await;
        assert_eq!(first.status(), StatusCode::CREATED);

        let again = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugin-repositories",
            Some(body),
        )
        .await;
        assert_problem(again, StatusCode::CONFLICT, "conflict").await;
        assert_eq!(
            fixture
                .services
                .plugin_repository()
                .repositories()
                .await
                .unwrap()
                .len(),
            1
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn registering_a_repository_is_gated() {
        let fixture = Fixture::with_config(|config| {
            config.settings.insert(
                "plugins.allow_third_party_repositories".to_owned(),
                toml::Value::Boolean(false),
            );
        })
        .await;
        let refused = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/plugin-repositories",
            Some(json!({ "url": "https://plugins.example/v1/index.json" })),
        )
        .await;
        assert_problem(refused, StatusCode::FORBIDDEN, "repositories-disabled").await;
        assert!(
            fixture
                .services
                .plugin_repository()
                .repositories()
                .await
                .unwrap()
                .is_empty()
        );
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_failed_fetch_is_recorded_on_the_repository() {
        let fixture = Fixture::new().await;
        let server = MockServer::start().await;
        // The index answers 500, so the fetch fails and the failure is
        // recorded on the row the listing and the views report.
        Mock::given(method("GET"))
            .and(path(INDEX_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let url = format!("{}{INDEX_PATH}", server.uri());
        let created = json(
            bearer(
                &fixture,
                reqwest::Method::POST,
                "/api/v1/admin/plugin-repositories",
                Some(json!({ "url": url, "name": "Broken" })),
            )
            .await,
        )
        .await;
        let id = created["id"].as_str().unwrap().to_owned();

        // Listing what the repository offers reads its index and reports the
        // failure rather than failing itself.
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                &format!("/api/v1/admin/plugins/available?repositoryId={id}"),
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["plugins"].as_array().unwrap().len(), 0);
        assert_eq!(listed["errors"][0]["source"], url.as_str());

        // The fetched repository now carries the outcome.
        let read = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/plugin-repositories",
                None,
            )
            .await,
        )
        .await;
        let repository = &read["repositories"][0];
        assert_eq!(repository["id"], id);
        assert!(repository["lastFetchedAt"].is_i64(), "{repository}");
        assert!(
            repository["lastError"].as_str().unwrap().contains("500"),
            "{repository}"
        );
        fixture.stop().await;
    }
}

mod provider_admin {
    use hearthsound_core::Layer;
    use hearthsound_plugin_host::{PluginRepository, PluginSource};
    use hearthsound_test_support::{ALBUM_CATALOG, HELLO_CATALOG, HTTP_CATALOG, METADATA_FIXTURE};
    use serde_json::json;

    use super::*;

    /// The id of the HTTP catalog fixture.
    const HTTP_CATALOG_ID: &str = "com.example.http-catalog";

    /// The id of the metadata fixture.
    const METADATA_ID: &str = "com.example.metadata-fixture";

    /// Installs a fixture plugin as approved and enabled.
    ///
    /// The provider resources are about configuring instances, so the plugins
    /// they are configured from are installed directly rather than through the
    /// plugin routes.
    async fn install_plugin(fixture: &Fixture, archive: &str, id: &str) {
        let installed = fixture
            .services
            .plugins()
            .install(&hearthsound_test_support::fixture_plugin(archive))
            .await
            .unwrap();
        let registration = PluginRepository::new(fixture.services.database())
            .register_approved(&installed, PluginSource::Sideload)
            .await
            .unwrap();
        assert_eq!(registration.id, id);
    }

    /// Configures one instance through the API and returns its view.
    async fn create_provider(fixture: &Fixture, slug: &str, plugin_id: &str) -> Value {
        let response = bearer(
            fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({ "slug": slug, "pluginId": plugin_id })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED, "the create failed");
        json(response).await
    }

    /// Returns the slugs the registry serves for one layer, in order.
    fn served(fixture: &Fixture, layer: Layer) -> Vec<String> {
        fixture
            .services
            .registry()
            .layers(layer)
            .iter()
            .map(|instance| instance.slug().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_provider_is_configured_changed_and_removed() {
        let fixture = Fixture::new().await;
        install_plugin(&fixture, HTTP_CATALOG, HTTP_CATALOG_ID).await;

        // The instance is configured from the plugin, with a secret field.
        let created = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({
                "slug": "httpcat",
                "pluginId": HTTP_CATALOG_ID,
                "displayName": "HTTP catalog",
                "serverConfig": {
                    "url": "https://catalog.example/search",
                    "coverUrl": "https://covers.example/cover.jpg",
                },
            })),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED, "the create failed");
        let created = json(created).await;
        assert_eq!(created["slug"], "httpcat");
        assert_eq!(created["pluginId"], HTTP_CATALOG_ID);
        assert_eq!(created["displayName"], "HTTP catalog");
        assert_eq!(created["enabled"], true);
        assert_eq!(created["serverConfig"]["url"], "[sealed]");
        assert_eq!(
            created["serverConfig"]["coverUrl"],
            "https://covers.example/cover.jpg"
        );
        assert_eq!(created["hostConfig"]["maxConcurrent"], 2);
        assert!(created["health"].is_null());
        assert!(created["createdAt"].is_i64());

        // The listing and the single read agree with the create.
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/providers",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["providers"].as_array().unwrap().len(), 1);
        assert_eq!(listed["providers"][0], created);
        let read = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/providers/httpcat",
            None,
        )
        .await;
        assert_eq!(read.status(), StatusCode::OK);
        assert_eq!(json(read).await, created);

        // A patch that echoes the sealed placeholder back keeps the stored
        // secret: the row holds a sealed value, never the placeholder.
        let patched = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                "/api/v1/admin/providers/httpcat",
                Some(json!({
                    "displayName": "Renamed",
                    "serverConfig": {
                        "url": "[sealed]",
                        "coverUrl": "https://covers.example/500.jpg",
                    },
                })),
            )
            .await,
        )
        .await;
        assert_eq!(patched["displayName"], "Renamed");
        assert_eq!(patched["serverConfig"]["url"], "[sealed]");
        assert_eq!(
            patched["serverConfig"]["coverUrl"],
            "https://covers.example/500.jpg"
        );
        let stored = fixture
            .services
            .plugin_repository()
            .instance_by_slug("httpcat")
            .await
            .unwrap()
            .unwrap()
            .server_config
            .unwrap();
        assert!(stored.contains("$secret"), "{stored}");
        assert!(!stored.contains("[sealed]"), "{stored}");

        // A patch of the name alone leaves the configuration as it is.
        let renamed = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                "/api/v1/admin/providers/httpcat",
                Some(json!({ "displayName": "Again" })),
            )
            .await,
        )
        .await;
        assert_eq!(renamed["displayName"], "Again");
        assert_eq!(renamed["serverConfig"]["url"], "[sealed]");
        assert_eq!(
            renamed["serverConfig"]["coverUrl"],
            "https://covers.example/500.jpg"
        );

        // The patch switches the instance off and on again.
        let disabled = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                "/api/v1/admin/providers/httpcat",
                Some(json!({ "enabled": false })),
            )
            .await,
        )
        .await;
        assert_eq!(disabled["enabled"], false);
        assert_eq!(disabled["displayName"], "Again");
        let enabled = json(
            bearer(
                &fixture,
                reqwest::Method::PATCH,
                "/api/v1/admin/providers/httpcat",
                Some(json!({ "enabled": true })),
            )
            .await,
        )
        .await;
        assert_eq!(enabled["enabled"], true);

        // An unknown slug is a not-found problem on every route.
        for (method, body) in [
            (reqwest::Method::GET, None),
            (reqwest::Method::PATCH, Some(json!({ "enabled": true }))),
            (reqwest::Method::DELETE, None),
        ] {
            let missing = bearer(&fixture, method, "/api/v1/admin/providers/ghost", body).await;
            assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;
        }

        // Removal takes the instance with it.
        let removed = bearer(
            &fixture,
            reqwest::Method::DELETE,
            "/api/v1/admin/providers/httpcat",
            None,
        )
        .await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);
        let gone = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/providers/httpcat",
            None,
        )
        .await;
        assert_problem(gone, StatusCode::NOT_FOUND, "not-found").await;
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/providers",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["providers"].as_array().unwrap().len(), 0);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_layer_reorders_the_instances_the_registry_serves() {
        let fixture = Fixture::new().await;
        install_plugin(&fixture, HELLO_CATALOG, "com.example.hello-catalog").await;
        install_plugin(&fixture, ALBUM_CATALOG, "com.example.album-catalog").await;
        create_provider(&fixture, "hello", "com.example.hello-catalog").await;
        create_provider(&fixture, "album", "com.example.album-catalog").await;

        // Nothing is assigned yet, and the listing reports every layer this
        // server can drive.
        let listed =
            json(bearer(&fixture, reqwest::Method::GET, "/api/v1/admin/layers", None).await).await;
        assert_eq!(
            listed["layers"],
            json!([
                { "layer": "catalog", "instances": [] },
                { "layer": "metadata", "instances": [] },
                { "layer": "acquisition", "instances": [] },
                { "layer": "artwork", "instances": [] },
            ])
        );
        assert!(served(&fixture, Layer::Catalog).is_empty());

        // The layer takes the instances in the order the body gives them.
        let assigned = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/catalog",
            Some(json!(["album", "hello"])),
        )
        .await;
        assert_eq!(assigned.status(), StatusCode::OK);
        assert_eq!(
            json(assigned).await,
            json!({ "layer": "catalog", "instances": ["album", "hello"] })
        );
        assert_eq!(served(&fixture, Layer::Catalog), ["album", "hello"]);
        assert!(served(&fixture, Layer::Metadata).is_empty());

        // The new order is in service as soon as the assignment answers.
        let reordered = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/catalog",
            Some(json!(["hello"])),
        )
        .await;
        assert_eq!(reordered.status(), StatusCode::OK);
        assert_eq!(served(&fixture, Layer::Catalog), ["hello"]);
        let listed =
            json(bearer(&fixture, reqwest::Method::GET, "/api/v1/admin/layers", None).await).await;
        assert_eq!(listed["layers"][0]["instances"], json!(["hello"]));

        // An empty body leaves the layer with nothing assigned.
        let cleared = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/catalog",
            Some(json!([])),
        )
        .await;
        assert_eq!(cleared.status(), StatusCode::OK);
        assert_eq!(
            json(cleared).await,
            json!({ "layer": "catalog", "instances": [] })
        );
        assert!(served(&fixture, Layer::Catalog).is_empty());
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_bad_layer_or_an_unknown_plugin_is_unprocessable() {
        let fixture = Fixture::new().await;
        install_plugin(&fixture, METADATA_FIXTURE, METADATA_ID).await;
        create_provider(&fixture, "meta", METADATA_ID).await;

        // A plugin that is not installed.
        let unknown = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({ "slug": "nope", "pluginId": "com.example.unknown" })),
        )
        .await;
        assert_problem(
            unknown,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-provider",
        )
        .await;

        // A configuration the plugin's schema rejects.
        let bad_config = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({
                "slug": "bad",
                "pluginId": METADATA_ID,
                "serverConfig": { "recordings": "not a list" },
            })),
        )
        .await;
        assert_problem(
            bad_config,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-provider",
        )
        .await;

        // A slug that is not a legal slug, and one that is taken.
        let bad_slug = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({ "slug": "NO", "pluginId": METADATA_ID })),
        )
        .await;
        assert_problem(bad_slug, StatusCode::BAD_REQUEST, "invalid-request").await;
        let taken = bearer(
            &fixture,
            reqwest::Method::POST,
            "/api/v1/admin/providers",
            Some(json!({ "slug": "meta", "pluginId": METADATA_ID })),
        )
        .await;
        assert_problem(taken, StatusCode::CONFLICT, "conflict").await;

        // A layer this server cannot drive, and one it does not know.
        for layer in ["scrobble", "nonsense"] {
            let refused = bearer(
                &fixture,
                reqwest::Method::PUT,
                &format!("/api/v1/admin/layers/{layer}"),
                Some(json!([])),
            )
            .await;
            assert_problem(refused, StatusCode::UNPROCESSABLE_ENTITY, "invalid-layer").await;
        }

        // A slug that names no instance.
        let ghost = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/metadata",
            Some(json!(["ghost"])),
        )
        .await;
        assert_problem(ghost, StatusCode::NOT_FOUND, "not-found").await;

        // An instance whose plugin does not provide the layer, and the same
        // slug twice in one layer.
        let wrong_layer = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/catalog",
            Some(json!(["meta"])),
        )
        .await;
        assert_problem(
            wrong_layer,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid-layer",
        )
        .await;
        let twice = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/metadata",
            Some(json!(["meta", "meta"])),
        )
        .await;
        assert_problem(twice, StatusCode::UNPROCESSABLE_ENTITY, "invalid-layer").await;

        // A body that is not an array of slugs.
        let wrong_body = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/metadata",
            Some(json!({ "instances": ["meta"] })),
        )
        .await;
        assert_problem(wrong_body, StatusCode::BAD_REQUEST, "invalid-request").await;

        // Nothing the refused calls asked for was written.
        assert!(served(&fixture, Layer::Metadata).is_empty());
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/providers",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["providers"].as_array().unwrap().len(), 1);
        fixture.stop().await;
    }

    #[tokio::test]
    async fn a_health_check_runs_and_is_visible_afterwards() {
        let fixture = Fixture::new().await;
        install_plugin(&fixture, METADATA_FIXTURE, METADATA_ID).await;
        create_provider(&fixture, "meta", METADATA_ID).await;

        // An instance that is not in service cannot be probed.
        let idle = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/providers/meta/health",
            None,
        )
        .await;
        assert_problem(idle, StatusCode::CONFLICT, "conflict").await;

        // Assigning it to its layer puts it in service.
        let assigned = bearer(
            &fixture,
            reqwest::Method::PUT,
            "/api/v1/admin/layers/metadata",
            Some(json!(["meta"])),
        )
        .await;
        assert_eq!(assigned.status(), StatusCode::OK);

        let checked = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/providers/meta/health",
            None,
        )
        .await;
        assert_eq!(checked.status(), StatusCode::OK);
        let checked = json(checked).await;
        assert_eq!(checked["slug"], "meta");
        assert_eq!(checked["ok"], true);
        assert_eq!(checked["detail"], "metadata-fixture ready");
        assert!(checked["checkedAt"].is_i64());

        // The outcome is recorded, so a later read reports it.
        let read = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/providers/meta",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(read["health"], checked);
        let listed = json(
            bearer(
                &fixture,
                reqwest::Method::GET,
                "/api/v1/admin/providers",
                None,
            )
            .await,
        )
        .await;
        assert_eq!(listed["providers"][0]["health"], checked);

        // An unknown instance is a not-found problem.
        let missing = bearer(
            &fixture,
            reqwest::Method::GET,
            "/api/v1/admin/providers/ghost/health",
            None,
        )
        .await;
        assert_problem(missing, StatusCode::NOT_FOUND, "not-found").await;
        fixture.stop().await;
    }
}
