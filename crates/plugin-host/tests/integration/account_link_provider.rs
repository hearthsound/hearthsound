//! Integration tests for calling an account-link plugin through the core trait.
//!
//! The fixture implements both link methods: the `config` method names the
//! account from the token the listener entered, and the `browser` method answers
//! the mock service's authorization URL and accepts the callback token the mock
//! service sends back.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{AccountLinkProvider, LinkMethod, UserContext};
use hearthsound_core::{ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{SCROBBLE_FIXTURE, fixture_plugin};

/// The endpoint used by the browser-flow tests.
const ENDPOINT: &str = "https://service.example";

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

/// Installs the fixture and returns an account-link provider for `link_method`.
async fn provider(link_method: &str) -> (tempfile::TempDir, Arc<dyn AccountLinkProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(SCROBBLE_FIXTURE))
        .await
        .unwrap();

    let config = serde_json::json!({
        "endpoint": ENDPOINT,
        "linkMethod": link_method,
    });
    let provider = host
        .account_link_provider(
            &installed,
            InstanceConfig::new(Slug::new("link").unwrap()).with_server_config(config.to_string()),
        )
        .unwrap();
    (dir, provider)
}

/// The listener linking their account.
fn listener(config: Option<&str>) -> UserContext {
    UserContext {
        user_id: "0194f3b0-2b3c-7d4e-8f00-000000000001".to_owned(),
        config: config.map(str::to_owned),
    }
}

#[tokio::test]
async fn the_link_method_comes_from_the_configuration() {
    let (_dir, configured) = provider("config").await;
    assert_eq!(
        configured.get_link_method().await.unwrap(),
        LinkMethod::Config
    );

    let (_dir, browser) = provider("browser").await;
    assert_eq!(
        browser.get_link_method().await.unwrap(),
        LinkMethod::Browser
    );
}

#[tokio::test]
async fn a_good_token_names_the_account_and_echoes_the_configuration() {
    let (_dir, provider) = provider("config").await;
    let config = r#"{"token":"good"}"#;

    let account = provider
        .verify_account(&listener(Some(config)))
        .await
        .unwrap();

    assert_eq!(account.name, "fixture-user");
    assert_eq!(
        account.config, config,
        "the fixture echoes the configuration"
    );
}

#[tokio::test]
async fn a_bad_token_is_unauthorized() {
    let (_dir, provider) = provider("config").await;

    let error = provider
        .verify_account(&listener(Some(r#"{"token":"bad"}"#)))
        .await
        .unwrap_err();

    assert_eq!(error, ProviderError::Unauthorized);
}

#[tokio::test]
async fn begin_link_answers_the_percent_encoded_callback() {
    let (_dir, provider) = provider("browser").await;
    let callback =
        "https://hearthsound.example/api/v1/me/providers/lastfm/link/callback?state=abc123";

    let url = provider
        .begin_link(&listener(None), callback)
        .await
        .unwrap();

    assert_eq!(
        url,
        "https://service.example/authorize?cb=https%3A%2F%2Fhearthsound.example%2Fapi%2Fv1%2Fme%2Fproviders%2Flastfm%2Flink%2Fcallback%3Fstate%3Dabc123"
    );
}

#[tokio::test]
async fn the_approved_callback_returns_the_browser_account() {
    let (_dir, provider) = provider("browser").await;
    let query = vec![("token".to_owned(), "approved".to_owned())];

    let account = provider
        .complete_link(&listener(None), &query)
        .await
        .unwrap();

    assert_eq!(account.name, "browser-user");
    assert_eq!(account.config, r#"{"session":"s1"}"#);
}

#[tokio::test]
async fn a_denied_callback_is_unauthorized() {
    let (_dir, provider) = provider("browser").await;
    let query = vec![("token".to_owned(), "denied".to_owned())];

    let error = provider
        .complete_link(&listener(None), &query)
        .await
        .unwrap_err();

    assert_eq!(error, ProviderError::Unauthorized);
}
