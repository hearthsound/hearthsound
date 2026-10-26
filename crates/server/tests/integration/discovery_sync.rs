//! Full-stack tests of the generated-playlist sync.
//!
//! The server is built by the composition root, the discovery fixture is
//! installed and assigned to the discovery layer, and the catalog fixture
//! resolves the fixture's suggestions into catalog tracks. A listener links the
//! discovery instance through the native API, which enqueues the sync; the
//! running job runner claims it, so these tests assert the rows the whole
//! pipeline writes rather than calling the handler by hand for the first sync.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use hearthsound_core::Layer;
use hearthsound_library::{AlbumInput, ArtistCredit, LibraryRepository, Origin, TrackInput};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::{ALBUM_CATALOG, HTTP_CATALOG, SCROBBLE_FIXTURE, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::Fixture;

/// How long a test waits for the running job to write its playlist.
const SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Installs a fixture plugin, returning its plugin id.
async fn install_plugin(fixture: &Fixture, archive: &str) -> String {
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(archive))
        .await
        .unwrap();
    PluginRepository::new(fixture.services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap()
        .id
}

/// Installs the catalog and discovery fixtures and assigns both layers.
///
/// The catalog maps the two suggestions the discovery fixture makes onto the
/// two tracks of its album, so both resolve.
async fn install_layers(fixture: &Fixture) {
    install_layers_with_playlists(
        fixture,
        json!([{
            "reference": "weekly-jams",
            "name": "Weekly Jams",
            "description": "A week of listening",
            "tracks": [
                { "title": "Fixture Song 1", "artists": ["Fixture Artist"] },
                { "title": "Fixture Song 2", "artists": ["Fixture Artist"] }
            ]
        }]),
    )
    .await;
}

/// Installs both fixtures with the given discovery playlists.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `playlists`: the `playlists` table the discovery fixture answers with.
async fn install_layers_with_playlists(fixture: &Fixture, playlists: Value) {
    let catalog_plugin = install_plugin(fixture, ALBUM_CATALOG).await;
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "album".to_owned(),
            plugin_id: catalog_plugin,
            display_name: "Album catalog".to_owned(),
            server_config: json!({
                "searchResults": [
                    { "text": "song 1", "track": 1 },
                    { "text": "song 2", "track": 2 }
                ]
            }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    let discovery_plugin = install_plugin(fixture, SCROBBLE_FIXTURE).await;
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "discovery".to_owned(),
            plugin_id: discovery_plugin,
            display_name: "Weekly Jams Service".to_owned(),
            server_config: json!({
                "endpoint": "http://127.0.0.1:9",
                "playlists": playlists
            }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Catalog, &["album".to_owned()])
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Discovery, &["discovery".to_owned()])
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();
}

/// Sends one authenticated request.
async fn send(
    fixture: &Fixture,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = fixture
        .client
        .request(method, format!("{}{path}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key));
    if let Some(body) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(body.to_string());
    }
    request.send().await.unwrap()
}

/// Links the fixture account to the discovery instance through the native API.
async fn link(fixture: &Fixture) {
    let response = send(
        fixture,
        reqwest::Method::PUT,
        "/api/v1/me/providers/discovery/config",
        Some(json!({ "config": { "token": "good" } })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

/// One stored generated playlist.
struct GeneratedPlaylist {
    /// The playlist id bytes.
    id: Vec<u8>,
    /// The playlist name.
    name: String,
    /// Whether it is temporary.
    is_temporary: i64,
    /// The service reference.
    source_ref: Option<String>,
    /// The expiry, in unix milliseconds.
    expires_at: Option<i64>,
    /// How many entries it holds.
    tracks: i64,
}

/// Reads the generated playlist of one listener, when there is one.
async fn generated_playlist(fixture: &Fixture, user: Uuid) -> Option<GeneratedPlaylist> {
    let row = sqlx::query(
        "SELECT p.id, p.name, p.is_temporary, p.source_ref, p.expires_at, \
                (SELECT COUNT(*) FROM playlist_tracks pt WHERE pt.playlist_id = p.id) AS tracks \
         FROM playlists p WHERE p.owner_user_id = ? AND p.source_instance_id IS NOT NULL",
    )
    .bind(user.as_bytes().as_slice())
    .fetch_optional(fixture.services.database().readers())
    .await
    .unwrap()?;
    use sqlx::Row;
    Some(GeneratedPlaylist {
        id: row.get("id"),
        name: row.get("name"),
        is_temporary: row.get("is_temporary"),
        source_ref: row.get("source_ref"),
        expires_at: row.get("expires_at"),
        tracks: row.get("tracks"),
    })
}

/// Waits until one generated playlist exists for the listener.
async fn wait_for_playlist(fixture: &Fixture, user: Uuid) -> GeneratedPlaylist {
    let deadline = tokio::time::Instant::now() + SYNC_TIMEOUT;
    loop {
        if let Some(playlist) = generated_playlist(fixture, user).await {
            return playlist;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the discovery sync wrote no playlist in time"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Linking a discovery service stores its generated playlist, and a second
/// sync keeps the playlist and replaces its tracks.
#[tokio::test]
async fn linking_a_discovery_service_syncs_its_playlists() {
    let fixture = Fixture::new().await;
    install_layers(&fixture).await;
    link(&fixture).await;

    let written = wait_for_playlist(&fixture, fixture.user_id).await;
    assert_eq!(written.name, "Weekly Jams");
    assert_eq!(written.is_temporary, 1);
    assert_eq!(written.source_ref.as_deref(), Some("weekly-jams"));
    assert_eq!(written.tracks, 2);
    assert!(
        written.expires_at.is_some_and(|expires_at| expires_at > 0),
        "a generated playlist carries a default expiry"
    );

    let refreshed = fixture
        .services
        .discovery_sync()
        .sync_user(fixture.user_id)
        .await
        .unwrap();
    assert_eq!(refreshed, 1);
    let after = generated_playlist(&fixture, fixture.user_id)
        .await
        .expect("the playlist is still stored");
    assert_eq!(after.id, written.id, "the refresh keeps the playlist id");
    assert_eq!(after.tracks, 2, "the refresh rewrote the entries");
}

/// A generated playlist the service already retired is skipped, and the copy an
/// earlier sync stored is left to the expiry job.
///
/// A service keeps listing a playlist it retired for a while. Writing it again
/// would store a playlist the expiry job deletes on its next pass, so every
/// sync would recreate and delete it.
#[tokio::test]
async fn an_expired_generated_playlist_is_not_stored() {
    let fixture = Fixture::new().await;
    install_layers_with_playlists(
        &fixture,
        json!([{
            "reference": "weekly-jams",
            "name": "Weekly Jams",
            "expiresAt": 1,
            "tracks": [
                { "title": "Fixture Song 1", "artists": ["Fixture Artist"] }
            ]
        }]),
    )
    .await;
    let instance = fixture.services.instances().get("discovery").await.unwrap();
    let earlier = fixture
        .services
        .activity()
        .playlists()
        .upsert_generated(
            &hearthsound_activity::SyncedPlaylist {
                owner_user_id: fixture.user_id,
                source_instance_id: instance.id,
                source_ref: "weekly-jams",
                name: "Earlier Sync",
                comment: None,
                expires_at: i64::MAX,
                tracks: &[],
            },
            i64::MAX / 2,
        )
        .await
        .unwrap();

    link(&fixture).await;
    let written = fixture
        .services
        .discovery_sync()
        .sync_user(fixture.user_id)
        .await
        .unwrap();

    assert_eq!(written, 0, "an expired playlist is not written");
    let after = generated_playlist(&fixture, fixture.user_id)
        .await
        .expect("the copy an earlier sync stored is still there");
    assert_eq!(
        after.id,
        earlier.id.uuid().as_bytes().to_vec(),
        "the slot kept its id"
    );
    assert_eq!(
        after.name, "Earlier Sync",
        "the sync left the stored copy to the expiry job"
    );
    assert_eq!(after.tracks, 0, "the sync wrote no entries into the slot");
}

/// The daily sync covers every listener linked to a discovery instance and
/// leaves an unlinked account alone.
#[tokio::test]
async fn the_schedule_syncs_every_linked_listener() {
    let fixture = Fixture::new().await;
    install_layers(&fixture).await;
    link(&fixture).await;
    let unlinked = hearthsound_auth::Users::new(fixture.services.database())
        .create(
            &fixture.services.settings().current(),
            &hearthsound_auth::NewUser::new("other", "Other", "hunter22", false),
        )
        .await
        .unwrap();

    fixture.services.discovery_sync().sync_all().await.unwrap();

    assert!(
        generated_playlist(&fixture, fixture.user_id)
            .await
            .is_some(),
        "the linked listener holds the generated playlist"
    );
    assert!(
        generated_playlist(&fixture, unlinked.id).await.is_none(),
        "an unlinked account gets no generated playlist"
    );
}

/// The system schedule runs the sync every morning.
#[tokio::test]
async fn the_discovery_sync_schedule_is_registered() {
    let fixture = Fixture::new().await;

    let cron: String = sqlx::query_scalar(
        "SELECT cron FROM schedules WHERE kind = 'discovery.sync' AND owner_user_id IS NULL",
    )
    .fetch_one(fixture.services.database().readers())
    .await
    .unwrap();

    assert_eq!(cron, "0 4 * * *");
}

/// Inserts one stored track and returns its client id.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn seed_track(fixture: &Fixture) -> hearthsound_core::EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let album = library
        .upsert_album(
            &AlbumInput::new("Sync Album", Origin::Local),
            &[ArtistCredit::new("Sync Artist", "main")],
        )
        .await
        .unwrap();
    let mut input = TrackInput::new(
        "Sync Song",
        Origin::Local,
        hearthsound_core::Availability::Local,
    );
    input.album_id = Some(album);
    input.display_artist = Some("Sync Artist".to_owned());
    input.duration_ms = Some(180_000);
    library
        .insert_track(&input, &[ArtistCredit::new("Sync Artist", "main")], &[])
        .await
        .unwrap()
}

/// A catalog that cannot answer leaves a generated playlist as it was.
///
/// The sync replaces every entry of the slot it writes, so a search that fails
/// must skip the write: writing the shorter list would delete the tracks the
/// failure merely hid until the next pass.
#[tokio::test]
async fn a_catalog_error_keeps_the_previous_entries() {
    let fixture = Fixture::new().await;
    // A catalog whose every search fails, the way a throttled or unreachable
    // provider answers.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let catalog_plugin = install_plugin(&fixture, HTTP_CATALOG).await;
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "broken".to_owned(),
            plugin_id: catalog_plugin,
            display_name: "Broken catalog".to_owned(),
            server_config: json!({ "url": format!("{}/search", server.uri()) }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    let discovery_plugin = install_plugin(&fixture, SCROBBLE_FIXTURE).await;
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: "discovery".to_owned(),
            plugin_id: discovery_plugin,
            display_name: "Weekly Jams Service".to_owned(),
            server_config: json!({
                "endpoint": "http://127.0.0.1:9",
                "playlists": [{
                    "reference": "weekly-jams",
                    "name": "Weekly Jams",
                    "description": "A week of listening",
                    "tracks": [
                        { "title": "Sync Song", "artists": ["Sync Artist"] }
                    ]
                }]
            }),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Catalog, &["broken".to_owned()])
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Discovery, &["discovery".to_owned()])
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();

    // The playlist an earlier, successful sync stored.
    let instance = fixture.services.instances().get("discovery").await.unwrap();
    let track = seed_track(&fixture).await;
    let earlier = fixture
        .services
        .activity()
        .playlists()
        .upsert_generated(
            &hearthsound_activity::SyncedPlaylist {
                owner_user_id: fixture.user_id,
                source_instance_id: instance.id,
                source_ref: "weekly-jams",
                name: "Earlier Sync",
                comment: None,
                expires_at: i64::MAX,
                tracks: &[track],
            },
            i64::MAX / 2,
        )
        .await
        .unwrap();
    let events_before = fixture.services.events().latest_id();

    link(&fixture).await;
    let written = fixture
        .services
        .discovery_sync()
        .sync_user(fixture.user_id)
        .await
        .unwrap();

    assert_eq!(
        written, 0,
        "a playlist that cannot be resolved is not written"
    );
    let after = generated_playlist(&fixture, fixture.user_id)
        .await
        .expect("the playlist an earlier sync stored is still there");
    assert_eq!(after.id, earlier.id.uuid().as_bytes().to_vec());
    assert_eq!(after.name, "Earlier Sync", "the name was left alone");
    assert_eq!(after.tracks, 1, "the entries the failure hid were kept");
    assert_eq!(
        fixture.services.events().latest_id(),
        events_before,
        "a skipped playlist publishes nothing"
    );
}
