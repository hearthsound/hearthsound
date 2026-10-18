//! Integration tests for the server information vendor endpoint.
//!
//! `hsGetServerInfo` reports the pending mode, whether catalog search is on,
//! whether an acquisition provider is enabled, the enabled layer assignments,
//! and the vendor features the server implements. These tests cover both
//! encodings, the state of a server without any provider, and the vendor gate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_config::{SettingValue, keys};

use crate::common::{Fixture, assert_error, assert_ok, get};

#[tokio::test]
async fn server_info_reports_the_pending_mode_and_the_catalog_switch_in_both_encodings() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let json = get(
        &app,
        &format!(
            "/rest/hsGetServerInfo?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&json);
    insta::assert_snapshot!("server-info-json", &json.body);
    assert!(json.body.contains("\"pendingMode\":"), "{}", json.body);
    assert!(
        json.body.contains("\"catalogEnabled\":true"),
        "{}",
        json.body
    );
    assert!(
        json.body.contains("\"acquisitionEnabled\":false"),
        "a server with no provider has no acquisition layer: {}",
        json.body
    );
    assert!(
        json.body
            .contains("\"features\":[\"prefetch\",\"vendor-v1\"]"),
        "{}",
        json.body
    );

    let xml = get(
        &app,
        &format!(
            "/rest/hsGetServerInfo?apiKey={}&hearthsound=1",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&xml);
    insta::assert_snapshot!("server-info-xml", &xml.body);
}

#[tokio::test]
async fn server_info_follows_the_settings_it_reports() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    fixture
        .settings
        .set(
            keys::STREAM_PENDING_MODE.name(),
            SettingValue::String("fail".to_owned()),
            None,
        )
        .await
        .unwrap();
    fixture
        .settings
        .set(
            keys::SEARCH_CATALOG_ENABLED.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();
    fixture
        .settings
        .set(
            keys::PREFETCH_ENABLED.name(),
            SettingValue::Bool(false),
            None,
        )
        .await
        .unwrap();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetServerInfo?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains("\"pendingMode\":\"fail\""),
        "{}",
        probe.body
    );
    assert!(
        probe.body.contains("\"catalogEnabled\":false"),
        "{}",
        probe.body
    );
    assert!(
        probe.body.contains("\"features\":[\"vendor-v1\"]"),
        "a server without prefetch reports the extension alone: {}",
        probe.body
    );
}

#[tokio::test]
async fn server_info_lists_the_enabled_layer_assignments() {
    let fixture = Fixture::new().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetServerInfo?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    insta::assert_snapshot!("server-info-layers-json", &probe.body);
    assert!(
        probe
            .body
            .contains("\"layers\":{\"layer\":[{\"name\":\"catalog\",\"instances\":{\"instance\":[\"hello\"]}}]}"),
        "{}",
        probe.body
    );
}

#[tokio::test]
async fn server_info_reports_the_acquisition_layer_when_one_is_enabled() {
    let mut fixture = Fixture::new().await;
    fixture.add_acquisition_layer().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!(
            "/rest/hsGetServerInfo?apiKey={}&hearthsound=1&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&probe);
    assert!(
        probe.body.contains("\"layers\":{\"layer\":[]}"),
        "an enabled layer with no assigned instance is not listed: {}",
        probe.body
    );
}

#[tokio::test]
async fn server_info_answers_error_70_without_the_vendor_flag() {
    let fixture = Fixture::new().await;
    let app = fixture.app();

    let probe = get(
        &app,
        &format!("/rest/hsGetServerInfo?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_error(&probe, 70);
}
