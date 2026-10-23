//! Integration tests for calling a discovery plugin through the core trait.
//!
//! The fixture answers from the tables its server configuration holds, so these
//! tests cover each function of the interface and the limit the caller sets.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::Slug;
use hearthsound_core::providers::{ArtistSeed, DiscoveryProvider, TrackSeed, UserContext};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{InstanceConfig, PluginHost};
use hearthsound_test_support::{SCROBBLE_FIXTURE, fixture_plugin};
use serde_json::json;

/// The suggestions the fixture is configured with.
const CONFIG: &str = r#"{
  "endpoint": "https://service.example/",
  "similarTracks": [
    { "title": "Similar One", "artists": ["Hello Artist"] },
    { "title": "Similar Two", "artists": ["Hello Artist"] }
  ],
  "similarArtists": [{ "name": "Neighbour" }, { "name": "Second Neighbour" }],
  "topTracks": [{ "title": "Popular Song", "artists": ["Hello Artist"] }],
  "recommended": [{ "title": "For You", "artists": ["Hello Artist"] }],
  "playlists": [
    {
      "reference": "weekly-jams",
      "name": "Weekly Jams",
      "description": "This week",
      "expiresAt": 1790000000000,
      "tracks": [{ "title": "Playlist Song", "artists": ["Hello Artist"] }]
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

/// Installs the fixture and returns a discovery provider.
async fn provider() -> (tempfile::TempDir, Arc<dyn DiscoveryProvider>) {
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

    let provider = host
        .discovery_provider(
            &installed,
            InstanceConfig::new(Slug::new("discovery").unwrap()).with_server_config(CONFIG),
        )
        .unwrap();
    (dir, provider)
}

/// The listener the personalized queries are made for.
fn listener() -> UserContext {
    UserContext {
        user_id: "0194f3b0-2b3c-7d4e-8f00-000000000001".to_owned(),
        config: Some(r#"{"token":"good"}"#.to_owned()),
    }
}

#[tokio::test]
async fn similar_tracks_come_from_the_configured_table() {
    let (_dir, provider) = provider().await;
    assert_eq!(provider.health().await.unwrap(), "scrobble-fixture ready");

    let seed = TrackSeed {
        title: "Hello Song".to_owned(),
        artists: vec!["Hello Artist".to_owned()],
        ids: Vec::new(),
    };
    let tracks = provider.similar_tracks(None, &seed, 10).await.unwrap();
    let titles: Vec<&str> = tracks.iter().map(|track| track.title.as_str()).collect();
    assert_eq!(titles, ["Similar One", "Similar Two"]);
    assert_eq!(tracks[0].artists, ["Hello Artist"]);

    let limited = provider.similar_tracks(None, &seed, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].title, "Similar One");
}

#[tokio::test]
async fn similar_artists_come_from_the_configured_table() {
    let (_dir, provider) = provider().await;
    let seed = ArtistSeed {
        name: "Hello Artist".to_owned(),
        ids: Vec::new(),
    };

    let artists = provider.similar_artists(None, &seed, 10).await.unwrap();
    let names: Vec<&str> = artists.iter().map(|artist| artist.name.as_str()).collect();
    assert_eq!(names, ["Neighbour", "Second Neighbour"]);

    let limited = provider.similar_artists(None, &seed, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
}

#[tokio::test]
async fn popular_and_recommended_tracks_come_from_the_configured_tables() {
    let (_dir, provider) = provider().await;
    let artist = ArtistSeed {
        name: "Hello Artist".to_owned(),
        ids: Vec::new(),
    };

    let popular = provider
        .artist_top_tracks(Some(&listener()), &artist, 10)
        .await
        .unwrap();
    assert_eq!(popular.len(), 1);
    assert_eq!(popular[0].title, "Popular Song");

    let recommended = provider.recommended_tracks(&listener(), 10).await.unwrap();
    assert_eq!(recommended.len(), 1);
    assert_eq!(recommended[0].title, "For You");
}

#[tokio::test]
async fn generated_playlists_carry_their_tracks_and_expiry() {
    let (_dir, provider) = provider().await;

    let playlists = provider.playlists(&listener()).await.unwrap();

    assert_eq!(playlists.len(), 1);
    let playlist = &playlists[0];
    assert_eq!(playlist.summary.reference, "weekly-jams");
    assert_eq!(playlist.summary.name, "Weekly Jams");
    assert_eq!(playlist.summary.description.as_deref(), Some("This week"));
    assert_eq!(playlist.summary.expires_at, Some(1_790_000_000_000));
    assert_eq!(playlist.tracks.len(), 1);
    assert_eq!(playlist.tracks[0].title, "Playlist Song");
}

#[tokio::test]
async fn an_empty_table_is_an_empty_answer_not_an_error() {
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
    let provider = host
        .discovery_provider(
            &installed,
            InstanceConfig::new(Slug::new("discovery").unwrap())
                .with_server_config(json!({ "endpoint": "https://service.example/" }).to_string()),
        )
        .unwrap();

    assert!(
        provider
            .recommended_tracks(&listener(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}
