//! Integration tests for calling a metadata plugin through the core trait.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{ExternalId, MetadataProvider, RecordingHint};
use hearthsound_core::{ProviderError, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{METADATA_FIXTURE, fixture_plugin};

/// The recordings the fixture is configured with.
const CONFIG: &str = r#"{
  "recordings": [
    {
      "title": "Hello Song",
      "artists": ["Hello Artist"],
      "isrc": "USHW12600001",
      "mbid": "11111111-1111-4111-8111-111111111111",
      "score": 0.97,
      "releases": [
        {
          "mbid": "22222222-2222-4222-8222-222222222222",
          "title": "Hello Album",
          "date": "2026-01-01",
          "trackCount": 2
        }
      ]
    },
    {
      "title": "Other Song",
      "artists": ["Other Artist"],
      "mbid": "33333333-3333-4333-8333-333333333333",
      "score": 0.4,
      "releases": []
    }
  ]
}"#;

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

/// Installs the fixture and returns a metadata provider.
async fn provider() -> (tempfile::TempDir, Arc<dyn MetadataProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(METADATA_FIXTURE))
        .await
        .unwrap();

    let provider = host
        .metadata_provider(
            &installed,
            InstanceConfig::new(Slug::new("metadata").unwrap()).with_server_config(CONFIG),
        )
        .unwrap();
    (dir, provider)
}

fn hint(title: &str, isrc: Option<&str>) -> RecordingHint {
    RecordingHint {
        title: title.to_owned(),
        artists: vec!["Hello Artist".to_owned()],
        album: Some("Hello Album".to_owned()),
        duration_ms: Some(180_000),
        isrc: isrc.map(str::to_owned),
    }
}

#[tokio::test]
async fn resolves_a_recording_by_isrc() {
    let (_dir, provider) = provider().await;
    assert_eq!(provider.health().await.unwrap(), "metadata-fixture ready");

    let matches = provider
        .resolve_recording(&hint("nothing like the title", Some("USHW12600001")))
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    let found = &matches[0];
    assert_eq!(found.title, "Hello Song");
    assert_eq!(found.score, 0.97);
    assert_eq!(found.duration_ms, Some(180_000));
    assert_eq!(
        found.ids,
        [ExternalId {
            namespace: "mb-recording".to_owned(),
            value: "11111111-1111-4111-8111-111111111111".to_owned(),
        }]
    );
    assert_eq!(found.artists[0].name, "Hello Artist");
    assert_eq!(found.releases.len(), 1);
    assert_eq!(found.releases[0].title, "Hello Album");
    assert_eq!(found.releases[0].track_count, Some(2));
    assert_eq!(found.releases[0].ids[0].namespace, "mb-release".to_owned());
}

#[tokio::test]
async fn resolves_a_recording_by_title_case_insensitively() {
    let (_dir, provider) = provider().await;
    let matches = provider
        .resolve_recording(&hint("hello song", None))
        .await
        .unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].title, "Hello Song");

    let none = provider
        .resolve_recording(&hint("unknown", None))
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn materialises_a_release() {
    let (_dir, provider) = provider().await;
    let album = provider
        .get_release(&[ExternalId {
            namespace: "mb-release".to_owned(),
            value: "22222222-2222-4222-8222-222222222222".to_owned(),
        }])
        .await
        .unwrap();
    assert_eq!(album.title, "Hello Album");
    assert_eq!(album.release_date.as_deref(), Some("2026-01-01"));
    assert_eq!(album.album_type.as_deref(), Some("album"));
    assert_eq!(album.tracks.len(), 1);
    assert_eq!(album.tracks[0].title, "Hello Song");
    assert_eq!(album.tracks[0].isrc.as_deref(), Some("USHW12600001"));

    assert_eq!(
        provider
            .get_release(&[ExternalId {
                namespace: "mb-release".to_owned(),
                value: "nope".to_owned(),
            }])
            .await,
        Err(ProviderError::NotFound)
    );
}

#[tokio::test]
async fn reports_configuration_problems() {
    let (_dir, provider) = provider().await;
    assert!(provider.validate_config("not json").await.unwrap().len() == 1);
    assert!(provider.validate_config(CONFIG).await.unwrap().is_empty());
}
