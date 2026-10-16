//! Integration tests for the endpoints: one snapshot pair per endpoint, and the
//! credential check through the whole router.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};

use crate::common::{Fixture, assert_error, assert_ok, get};

#[tokio::test]
async fn ping_answers_an_empty_envelope_in_both_encodings() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/ping?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert_eq!(json.content_type(), Some("application/json; charset=utf-8"));
    insta::assert_snapshot!("ping-json", json.body);

    let xml = get(&app, &format!("/rest/ping.view?apiKey={}", fixture.api_key)).await;
    assert_ok(&xml);
    assert_eq!(xml.content_type(), Some("text/xml; charset=utf-8"));
    insta::assert_snapshot!("ping-xml", xml.body);
}

#[tokio::test]
async fn get_license_answers_a_valid_license() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/getLicense?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    insta::assert_snapshot!("license-json", json.body);

    let xml = get(
        &app,
        &format!("/rest/getLicense.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::assert_snapshot!("license-xml", xml.body);
}

#[tokio::test]
async fn get_open_subsonic_extensions_answers_without_credentials() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(&app, "/rest/getOpenSubsonicExtensions?f=json").await;
    assert_ok(&json);
    insta::assert_snapshot!("extensions-json", json.body);

    let xml = get(&app, "/rest/getOpenSubsonicExtensions").await;
    assert_ok(&xml);
    insta::assert_snapshot!("extensions-xml", xml.body);
}

#[tokio::test]
async fn token_info_echoes_the_user_the_api_key_belongs_to() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/tokenInfo?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    insta::assert_snapshot!("token-info-json", json.body);

    let xml = get(&app, &format!("/rest/tokenInfo?apiKey={}", fixture.api_key)).await;
    assert_ok(&xml);
    insta::assert_snapshot!("token-info-xml", xml.body);
}

#[tokio::test]
async fn token_info_rejects_a_credential_that_is_not_an_api_key() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/tokenInfo?u={}&p={}&f=json",
            crate::common::USERNAME,
            fixture.app_password
        ),
    )
    .await;
    assert_error(&probe, 44);
}

#[tokio::test]
async fn get_music_folders_answers_the_library_and_the_server() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!("/rest/getMusicFolders?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    insta::assert_snapshot!("music-folders-json", json.body);

    let xml = get(
        &app,
        &format!("/rest/getMusicFolders.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    insta::assert_snapshot!("music-folders-xml", xml.body);
}

#[tokio::test]
async fn get_music_folders_omits_the_server_while_server_browse_is_disabled() {
    let fixture = Fixture::new().await;
    let app = fixture.app();
    fixture
        .settings
        .set(
            keys::PRIVACY_SERVER_BROWSE.name(),
            SettingValue::String("disabled".to_owned()),
            None,
        )
        .await
        .unwrap();

    let json = get(
        &app,
        &format!("/rest/getMusicFolders?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    insta::assert_snapshot!("music-folders-disabled-json", json.body);

    let xml = get(
        &app,
        &format!("/rest/getMusicFolders.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&xml);
    assert!(!xml.body.contains("Server"), "{}", xml.body);
}

#[tokio::test]
async fn missing_credentials_answer_error_10() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(&app, "/rest/ping?f=json").await;
    assert_error(&probe, 10);

    let probe = get(&app, "/rest/ping").await;
    assert_error(&probe, 10);
}

#[tokio::test]
async fn an_unknown_api_key_answers_error_44() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(&app, "/rest/ping?apiKey=hs_not-a-key&f=json").await;
    assert_error(&probe, 44);
    assert!(probe.body.contains("\"message\":\"Invalid API key.\""));

    let probe = get(&app, "/rest/ping?apiKey=hs_not-a-key").await;
    assert_error(&probe, 44);
}

#[tokio::test]
async fn an_api_key_alongside_a_username_answers_error_43() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/ping?apiKey={}&u={}&f=json",
            fixture.api_key,
            crate::common::USERNAME
        ),
    )
    .await;
    assert_error(&probe, 43);
}

#[tokio::test]
async fn two_credential_mechanisms_at_once_answer_error_42() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/ping?u={}&p=sesame&t=26719a1196d2a940705a59634eb18eab&s=c19b2d&f=json",
            crate::common::USERNAME
        ),
    )
    .await;
    assert_error(&probe, 42);
}

#[tokio::test]
async fn an_app_password_authenticates() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/ping?u={}&p={}&f=json",
            crate::common::USERNAME,
            fixture.app_password
        ),
    )
    .await;
    assert_ok(&probe);
}

#[tokio::test]
async fn a_password_that_matches_no_app_password_is_refused_with_a_help_url() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    // The account password is not accepted on /rest while
    // `subsonic.allow_account_password` is off, which is the default, and the
    // server does not tell the client whether it presented the account password
    // or a wrong app password: both are answered with the same refusal.
    for secret in [
        crate::common::ACCOUNT_PASSWORD.to_owned(),
        "wrong".to_owned(),
    ] {
        let probe = get(
            &app,
            &format!("/rest/ping?u={}&p={secret}&f=json", crate::common::USERNAME),
        )
        .await;
        assert_error(&probe, 40);
        assert!(
            probe
                .body
                .contains(hearthsound_auth::APP_PASSWORDS_HELP_URL),
            "{}",
            probe.body
        );
    }
}

#[tokio::test]
async fn the_vendor_flag_does_not_change_a_standard_response() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let plain = get(
        &app,
        &format!("/rest/getLicense?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    let vendor = get(
        &app,
        &format!(
            "/rest/getLicense?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_eq!(plain.body, vendor.body);

    let plain = get(&app, &format!("/rest/ping?apiKey={}", fixture.api_key)).await;
    let vendor = get(
        &app,
        &format!("/rest/ping?apiKey={}&hearthsound=1", fixture.api_key),
    )
    .await;
    assert_eq!(plain.body, vendor.body);
}
