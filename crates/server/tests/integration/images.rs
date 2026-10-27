//! Full-stack test of the stored images.
//!
//! The server persists every image it uses: the indexer stores what a provider
//! names, and the first read stores the bytes. A second read is served from the
//! store, so the provider sees one request for one picture no matter how often a
//! client asks for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hearthsound_core::{EntityKind, Layer};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::{HELLO_CATALOG, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, ETAG};
use serde_json::json;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Fixture;

/// The manifest id of the hello catalog fixture.
const HELLO_ID: &str = "com.example.hello-catalog";

/// The slug of the hello catalog instance.
const HELLO_SLUG: &str = "hello";

/// The bytes the provider serves for every cover rendition.
///
/// The value is never decoded: the test asks for the original bytes, so the
/// server stores and serves them unchanged.
const COVER: &[u8] = b"stored-cover-bytes";

/// Installs the hello catalog fixture and assigns it to the catalog layer.
///
/// The instance reports a cover URL template that points at `mock`, which is
/// where the images the server stores come from.
///
/// # Parameters
///
/// - `services`: the running services.
/// - `mock`: the server the covers are fetched from.
async fn install_catalog(services: &Arc<hearthsound_server::Services>, mock: &MockServer) {
    let installed = services
        .plugins()
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, HELLO_ID);
    services
        .instances()
        .create(&InstanceSpec {
            slug: HELLO_SLUG.to_owned(),
            plugin_id: HELLO_ID.to_owned(),
            display_name: "Hello catalog".to_owned(),
            server_config: json!({
                "greeting": "Hello",
                "coverUrl": format!("{}/cover/{{size}}.jpg", mock.uri()),
            }),
            host_config: serde_json::Value::Null,
        })
        .await
        .unwrap();
    services
        .instances()
        .set_layer(Layer::Catalog, &[HELLO_SLUG.to_owned()])
        .await
        .unwrap();
}

/// Sends one authenticated request for an image.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `id`: the entity's id.
async fn image(fixture: &Fixture, id: &str) -> reqwest::Response {
    fixture
        .client
        .get(format!("{}/api/v1/images/{id}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap()
}

/// A cover the indexer stored is served from the store, not from the provider.
#[tokio::test]
async fn a_stored_cover_is_served_without_asking_the_provider_again() {
    let mock = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex("^/cover/.*\\.jpg$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(COVER),
        )
        .mount(&mock)
        .await;

    let fixture = Fixture::new().await;
    let services = Arc::clone(&fixture.services);
    install_catalog(&services, &mock).await;
    services.registry().rebuild().await.unwrap();

    // The album page is walked to its end, which is what stores the provider's
    // image rows.
    let album = services
        .catalog()
        .search(fixture.user_id, "Hello", &[EntityKind::Album], 5, 0)
        .await
        .unwrap()
        .albums
        .into_iter()
        .next()
        .expect("the fixture answers an album")
        .album
        .id;
    services
        .catalog()
        .ensure_album_page(album, Duration::from_secs(5))
        .await
        .unwrap();

    // The indexer stores the provider's URL, not its bytes: the first read is
    // the one that fetches and stores them.
    let response = image(&fixture, &album.to_string()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/jpeg")
    );
    assert!(
        response.headers().get(ETAG).is_some(),
        "a served image carries its validator"
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), COVER);

    let fetched = mock.received_requests().await.unwrap().len();
    assert_eq!(fetched, 1, "the first read fetched the cover once");

    // The second read comes from the store alone.
    let response = image(&fixture, &album.to_string()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap().as_ref(), COVER);
    assert_eq!(
        mock.received_requests().await.unwrap().len(),
        fetched,
        "the stored cover answered without another provider request"
    );

    fixture.stop().await;
}
