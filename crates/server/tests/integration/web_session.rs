//! Full-stack tests of the web session on the Subsonic surface.
//!
//! A web client signs in through `/api/v1/auth/login` and then calls `/rest`
//! with the cookie that answer set, instead of repeating a credential
//! parameter. An envelope call echoes the session's CSRF token; a media
//! endpoint cannot, so it is accepted without one unless a browser labels the
//! request cross-site.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_auth::Sessions;
use hearthsound_core::{Availability, EntityId, StorageClass};
use hearthsound_library::{LibraryRepository, ManagedMediaFile, Origin, TrackInput};
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, COOKIE};
use serde_json::Value;

use crate::common::{ACCOUNT_PASSWORD, Fixture, USERNAME};
use crate::multi_user::create_user;

/// The cookie and CSRF token of a fresh sign-in.
struct Session {
    /// The `name=value` pair of the `Set-Cookie` header.
    cookie: String,
    /// The token unsafe requests echo in `X-Hearthsound-CSRF`.
    csrf: String,
}

/// Signs the fixture's account in and returns its session.
async fn sign_in(fixture: &Fixture) -> Session {
    let response = fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "username": USERNAME, "password": ACCOUNT_PASSWORD }).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[reqwest::header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    Session {
        cookie,
        csrf: body["csrfToken"].as_str().unwrap().to_owned(),
    }
}

/// Parses a rendered envelope.
fn envelope(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

/// Inserts a local track with one small stored file and returns its id.
///
/// `format=raw` serves these bytes untouched, so the test does not need a real
/// audio file to prove that a cookie authenticates the media endpoint.
async fn stored_file_track(fixture: &Fixture, rel_path: &str) -> EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let track = library
        .insert_track(
            &TrackInput::new("Session Song", Origin::Local, Availability::Local),
            &[],
            &[],
        )
        .await
        .unwrap();
    library
        .insert_managed_media_file(&ManagedMediaFile {
            track_id: track,
            storage: StorageClass::Library,
            rel_path: rel_path.to_owned(),
            size: 4,
            mtime_ms: 1,
            format: Some("flac".to_owned()),
            codec: Some("flac".to_owned()),
            bitrate_kbps: Some(900),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            duration_ms: Some(4_000),
            lossless: Some(true),
            quality_tier: None,
            replaygain: None,
            has_embedded_art: None,
            uploaded_by: None,
            acquisition_id: None,
        })
        .await
        .unwrap();
    let path = fixture
        .services
        .storage_roots()
        .root_for(StorageClass::Library)
        .join(rel_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"flac").unwrap();
    track
}

/// A cookie and the session's CSRF token authenticate an envelope call.
#[tokio::test]
async fn a_session_cookie_with_the_csrf_header_authenticates_rest() {
    let fixture = Fixture::new().await;
    let session = sign_in(&fixture).await;

    let response = fixture
        .client
        .get(format!("{}/rest/ping?f=json", fixture.base_url()))
        .header(COOKIE, &session.cookie)
        .header("x-hearthsound-csrf", &session.csrf)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = envelope(&response.text().await.unwrap());
    assert_eq!(body["subsonic-response"]["status"], "ok", "{body}");
}

/// A cookie without the CSRF header is refused with error 40.
#[tokio::test]
async fn a_session_cookie_without_the_csrf_header_is_refused() {
    let fixture = Fixture::new().await;
    let session = sign_in(&fixture).await;

    let response = fixture
        .client
        .get(format!("{}/rest/ping?f=json", fixture.base_url()))
        .header(COOKIE, &session.cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "envelope failures stay 200"
    );
    let body = envelope(&response.text().await.unwrap());
    assert_eq!(body["subsonic-response"]["status"], "failed", "{body}");
    assert_eq!(body["subsonic-response"]["error"]["code"], 40, "{body}");
    assert_eq!(
        body["subsonic-response"]["error"]["message"],
        "Cookie-authenticated calls must carry the X-Hearthsound-CSRF header.",
        "{body}"
    );
}

/// A credential parameter wins over the cookie, and the cookie's CSRF rule does
/// not apply to it.
#[tokio::test]
async fn a_credential_parameter_is_used_instead_of_the_cookie() {
    let fixture = Fixture::new().await;
    let session = sign_in(&fixture).await;
    let (_, other_key) = create_user(&fixture, "listener", false).await;
    let track = stored_file_track(&fixture, "session/other.flac").await;

    let starred = fixture
        .get(&format!("/rest/star?id={track}&f=json&apiKey={other_key}"))
        .await;
    let starred = envelope(&starred.text().await.unwrap());
    assert_eq!(starred["subsonic-response"]["status"], "ok", "{starred}");

    // The cookie belongs to the administrator, who liked nothing; the API key
    // of the other account names the user whose likes are listed.
    let listed = fixture
        .client
        .get(format!(
            "{}/rest/getStarred2?f=json&apiKey={other_key}",
            fixture.base_url()
        ))
        .header(COOKIE, &session.cookie)
        .send()
        .await
        .unwrap();
    let listed = envelope(&listed.text().await.unwrap());
    let songs = listed["subsonic-response"]["starred2"]["song"]
        .as_array()
        .unwrap();
    assert_eq!(songs.len(), 1, "{listed}");
    assert_eq!(songs[0]["id"], track.to_string(), "{listed}");
}

/// A cookie authenticates a media request without a CSRF header.
#[tokio::test]
async fn a_session_cookie_streams_a_local_track() {
    let fixture = Fixture::new().await;
    let session = sign_in(&fixture).await;
    let track = stored_file_track(&fixture, "session/local.flac").await;

    let response = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&format=raw",
            fixture.base_url()
        ))
        .header(COOKIE, &session.cookie)
        .send()
        .await
        .unwrap();
    assert!(
        matches!(
            response.status(),
            StatusCode::OK | StatusCode::PARTIAL_CONTENT
        ),
        "the stream answered {}",
        response.status()
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), b"flac");
}

/// A cross-site media request authenticated by cookie is refused with 401.
#[tokio::test]
async fn a_cross_site_media_request_is_refused() {
    let fixture = Fixture::new().await;
    let session = sign_in(&fixture).await;
    let track = stored_file_track(&fixture, "session/cross.flac").await;

    let response = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&format=raw",
            fixture.base_url()
        ))
        .header(COOKIE, &session.cookie)
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.bytes().await.unwrap().is_empty());
}

/// An unknown or expired cookie is refused with 401 by a media endpoint.
#[tokio::test]
async fn an_unknown_or_expired_cookie_is_refused_on_a_stream() {
    let fixture = Fixture::new().await;
    let track = stored_file_track(&fixture, "session/expired.flac").await;

    let unknown = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&format=raw",
            fixture.base_url()
        ))
        .header(COOKIE, "hs_session=notatoken")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);

    let expired = Sessions::new(fixture.services.database())
        .create(fixture.user_id, None, Duration::ZERO)
        .await
        .unwrap();
    let response = fixture
        .client
        .get(format!(
            "{}/rest/stream?id={track}&format=raw",
            fixture.base_url()
        ))
        .header(COOKIE, format!("hs_session={}", expired.token))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A rate-limited sign-in says when the budget refills, and waiting that long
/// admits the next attempt.
#[tokio::test]
async fn a_rate_limited_login_says_when_to_retry() {
    let fixture = Fixture::with_config(|config| {
        config.settings.insert(
            "auth.login_rate_limit_per_minute".to_owned(),
            toml::Value::Integer(10),
        );
    })
    .await;
    let attempt = |password: &'static str| {
        let base = fixture.base_url();
        let client = fixture.client.clone();
        async move {
            client
                .post(format!("{base}/api/v1/auth/login"))
                .header(CONTENT_TYPE, "application/json")
                .body(serde_json::json!({ "username": USERNAME, "password": password }).to_string())
                .send()
                .await
                .unwrap()
        }
    };
    for _ in 0..10 {
        let wrong = attempt("not-the-password").await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    }
    let limited = attempt(ACCOUNT_PASSWORD).await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    let wait = limited.headers()[reqwest::header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    // Ten per minute refill one cell every six seconds, so the reported wait is
    // the real one rather than a fixed minute.
    assert!((1..=60).contains(&wait), "Retry-After is {wait}");
    let body: Value = serde_json::from_str(&limited.text().await.unwrap()).unwrap();
    assert_eq!(body["code"], "rate-limited");

    tokio::time::sleep(Duration::from_secs(wait)).await;
    assert_eq!(attempt(ACCOUNT_PASSWORD).await.status(), StatusCode::OK);

    fixture.stop().await;
}

/// A stranger's wrong passwords for one account do not lock its owner out from
/// another address: the limiter counts the pair of address and username.
#[tokio::test]
async fn a_stranger_cannot_lock_an_account_out_from_another_address() {
    let fixture = Fixture::new().await;
    let limiter = fixture.services.authenticator().limiter();
    let attacker: std::net::IpAddr = "10.0.0.7".parse().unwrap();
    let owner: std::net::IpAddr = "10.0.0.8".parse().unwrap();

    for _ in 0..10 {
        limiter.record_failure(attacker, USERNAME);
    }
    assert!(
        limiter.check(attacker, USERNAME).is_err(),
        "the attacking address is refused"
    );
    assert!(
        limiter.check(owner, USERNAME).is_ok(),
        "the owner signs in from anywhere else"
    );

    // And the same through the route.
    assert!(sign_in_ok(&fixture).await);

    fixture.stop().await;
}

/// Signs in with the fixture account and reports whether it worked.
async fn sign_in_ok(fixture: &Fixture) -> bool {
    fixture
        .client
        .post(format!("{}/api/v1/auth/login", fixture.base_url()))
        .header(CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "username": USERNAME, "password": ACCOUNT_PASSWORD }).to_string())
        .send()
        .await
        .unwrap()
        .status()
        == StatusCode::OK
}
