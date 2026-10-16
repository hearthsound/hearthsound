//! Integration tests for calling an artwork plugin through the core trait.
//!
//! The artwork fixture answers the images its server configuration lists for the
//! albums and artists it knows, so these tests cover the whole path: the
//! instance an administrator configured, the subject the caller builds, and the
//! images the plugin answers with.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{ArtworkProvider, ArtworkSubject, ExternalId};
use hearthsound_core::{EntityKind, Slug};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{ARTWORK_FIXTURE, fixture_plugin};

/// The albums and artists the fixture is configured with.
const CONFIG: &str = r#"{
  "imageUrl": "https://images.example/",
  "albums": [
    {
      "title": "Hello Album",
      "mbid": "22222222-2222-4222-8222-222222222222",
      "images": [
        { "path": "hello-250.jpg", "width": 250 },
        { "path": "hello-1000.jpg", "width": 1000 }
      ]
    }
  ],
  "artists": [
    {
      "title": "Hello Artist",
      "images": [{ "path": "hello-artist.jpg" }]
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

/// Installs the fixture and returns an artwork provider.
async fn provider() -> (tempfile::TempDir, Arc<dyn ArtworkProvider>) {
    let dir = tempfile::tempdir().unwrap();
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        database(dir.path()).await,
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(ARTWORK_FIXTURE))
        .await
        .unwrap();

    let provider = host
        .artwork_provider(
            &installed,
            InstanceConfig::new(Slug::new("artwork").unwrap()).with_server_config(CONFIG),
        )
        .unwrap();
    (dir, provider)
}

/// A configured album is answered with its images, and an unknown subject with
/// none.
#[tokio::test]
async fn images_are_answered_from_the_config() {
    let (_dir, provider) = provider().await;

    let images = provider
        .find_artwork(&ArtworkSubject {
            kind: EntityKind::Album,
            provider_id: None,
            ids: Vec::new(),
            title: "hello album".to_owned(),
            artists: vec!["Hello Artist".to_owned()],
        })
        .await
        .unwrap();
    let urls: Vec<&str> = images.iter().map(|image| image.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://images.example/hello-250.jpg",
            "https://images.example/hello-1000.jpg",
        ],
        "the configured paths are served from the configured base URL"
    );
    assert_eq!(images[0].width, Some(250));
    assert_eq!(images[1].width, Some(1000));
    assert_eq!(images[0].height, None, "the fixture describes no height");

    // The identifier alone matches, whatever the title says.
    let by_id = provider
        .find_artwork(&ArtworkSubject {
            kind: EntityKind::Album,
            provider_id: None,
            ids: vec![ExternalId {
                namespace: "mb-release".to_owned(),
                value: "22222222-2222-4222-8222-222222222222".to_owned(),
            }],
            title: "Something Else".to_owned(),
            artists: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(by_id.len(), 2, "the identifier matched the album");

    let unknown = provider
        .find_artwork(&ArtworkSubject {
            kind: EntityKind::Album,
            provider_id: None,
            ids: Vec::new(),
            title: "Nobody's Album".to_owned(),
            artists: Vec::new(),
        })
        .await
        .unwrap();
    assert!(unknown.is_empty(), "an unknown album is not an error");
}

/// An artist portrait is answered, and a kind the provider does not serve is
/// answered with nothing.
#[tokio::test]
async fn artists_are_answered_and_other_kinds_are_not() {
    let (_dir, provider) = provider().await;

    let images = provider
        .find_artwork(&ArtworkSubject {
            kind: EntityKind::Artist,
            provider_id: None,
            ids: Vec::new(),
            title: "Hello Artist".to_owned(),
            artists: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].url, "https://images.example/hello-artist.jpg");

    let track = provider
        .find_artwork(&ArtworkSubject {
            kind: EntityKind::Track,
            provider_id: None,
            ids: Vec::new(),
            title: "Hello Album".to_owned(),
            artists: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        track.is_empty(),
        "a kind the provider does not serve is answered with nothing"
    );
}
