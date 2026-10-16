//! Integration test for a catalog plugin that reaches the network.
//!
//! The HTTP catalog fixture performs its search with a `host-http` request to
//! the endpoint in its server configuration. The endpoint the administrator
//! approved through `$config:url` answers; any other host is denied, which the
//! adapter reports as an unauthorized provider error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{CatalogProvider, CatalogQuery};
use hearthsound_core::{EntityKind, ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::manifest::{HttpPermission, Permissions};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{HTTP_CATALOG, fixture_plugin};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// The permissions a test approves for the fixture.
fn http_permission(hosts: &[&str]) -> Permissions {
    Permissions {
        http: Some(HttpPermission {
            hosts: hosts.iter().map(|host| (*host).to_owned()).collect(),
            rate_limit: None,
            max_body_bytes: None,
        }),
        ..Permissions::default()
    }
}

/// Installs the fixture and returns a provider pointed at `endpoint`.
async fn provider(endpoint: &str, hosts: &[&str]) -> (tempfile::TempDir, Arc<dyn CatalogProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host.install(&fixture_plugin(HTTP_CATALOG)).await.unwrap();

    let provider = host
        .catalog_provider(
            &installed,
            InstanceConfig::new(Slug::new("httpcat").unwrap())
                .with_server_config(format!(r#"{{"url":"{endpoint}"}}"#))
                .with_permissions(http_permission(hosts)),
        )
        .unwrap();
    (dir, provider)
}

fn query(text: &str) -> CatalogQuery {
    CatalogQuery {
        text: text.to_owned(),
        kinds: vec![EntityKind::Track],
        limit: 10,
        offset: 0,
    }
}

#[tokio::test]
async fn searches_through_the_host_http_import() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "hello world"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"[{"id":"remote-1","title":"Remote Song","artists":["Remote Artist"],"duration_ms":123000,"isrc":"USX1234567"}]"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = format!("{}/search", server.uri());
    let (_dir, provider) = provider(&endpoint, &["$config:url"]).await;

    assert_eq!(provider.health().await.unwrap(), "http-catalog ready");

    let results = provider.search(&query("hello world")).await.unwrap();
    assert_eq!(results.tracks.len(), 1);
    let track = &results.tracks[0];
    assert_eq!(track.id, "remote-1");
    assert_eq!(track.title, "Remote Song");
    assert_eq!(track.artists[0].name, "Remote Artist");
    assert_eq!(track.duration_ms, Some(123_000));
    assert_eq!(track.isrc.as_deref(), Some("USX1234567"));
    assert_eq!(
        track.album.as_ref().map(|album| album.title.as_str()),
        Some("Remote Album")
    );
}

#[tokio::test]
async fn denies_a_search_to_a_host_outside_the_allowlist() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .mount(&server)
        .await;

    let endpoint = format!("{}/search", server.uri());
    let (_dir, provider) = provider(&endpoint, &["api.example.com"]).await;

    assert_eq!(
        provider.search(&query("hello")).await,
        Err(ProviderError::Unauthorized)
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}
