//! Integration tests for calling a verification plugin through the core trait.
//!
//! The fixture rejects the fingerprints its configuration lists and matches
//! every other print, so both verdicts and the details that come with them are
//! covered without a fingerprinting service.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::Slug;
use hearthsound_core::providers::{
    ExpectedTrack, ExternalId, Fingerprint, Verdict, VerificationProvider,
};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{VERIFICATION_FIXTURE, fixture_plugin};

/// The fingerprint the fixture is configured to reject.
const REJECTED: &str = "AQAAAMSSA0sRE";

/// The server configuration of the fixture.
const CONFIG: &str = r#"{"rejectFingerprints": ["AQAAAMSSA0sRE"]}"#;

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

/// Installs the fixture and returns a verification provider.
async fn provider() -> (tempfile::TempDir, Arc<dyn VerificationProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(VERIFICATION_FIXTURE))
        .await
        .unwrap();

    let provider = host
        .verification_provider(
            &installed,
            InstanceConfig::new(Slug::new("verification").unwrap()).with_server_config(CONFIG),
        )
        .unwrap();
    (dir, provider)
}

/// The recording the downloaded file is expected to be.
fn expected() -> ExpectedTrack {
    ExpectedTrack {
        title: "Hello Song".to_owned(),
        artists: vec!["Hello Artist".to_owned()],
        album: Some("Hello Album".to_owned()),
        duration_ms: Some(180_000),
        ids: vec![ExternalId {
            namespace: "mb-recording".to_owned(),
            value: "11111111-1111-4111-8111-111111111111".to_owned(),
        }],
    }
}

/// A fingerprint of `value`.
fn print(value: &str) -> Fingerprint {
    Fingerprint {
        value: value.to_owned(),
        duration_secs: 180,
    }
}

#[tokio::test]
async fn a_listed_fingerprint_is_a_mismatch_with_its_detail() {
    let (_dir, provider) = provider().await;
    assert_eq!(
        provider.health().await.unwrap(),
        "verification-fixture ready"
    );

    let result = provider
        .verify(&expected(), &print(REJECTED))
        .await
        .unwrap();

    assert_eq!(result.verdict, Verdict::Mismatch);
    assert_eq!(result.score, 0.9);
    assert_eq!(
        result.detail.as_deref(),
        Some("fixture rejects this recording")
    );
    assert!(result.identified.is_empty());
}

#[tokio::test]
async fn any_other_fingerprint_is_a_match() {
    let (_dir, provider) = provider().await;

    let result = provider
        .verify(&expected(), &print("AQAAAMSSA0sRF"))
        .await
        .unwrap();

    assert_eq!(result.verdict, Verdict::Match);
    assert_eq!(result.score, 0.95);
    assert_eq!(result.detail, None);
}
