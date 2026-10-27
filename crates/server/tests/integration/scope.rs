//! The viewer scope of a stored artist page.
//!
//! Every artist page is one stored row: its discography is the albums other
//! clients may not see, and a third-party Subsonic client sees only the albums
//! its own library holds. This server's own client, which asks with
//! `hearthsound=1`, sees the whole discography.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::providers::ExternalId;
use hearthsound_core::{Availability, EntityId, IdPrefix, Layer};
use hearthsound_library::{AlbumInput, ArtistCredit, ArtistInput, LibraryRepository, TrackInput};
use hearthsound_plugin_host::{InstanceSpec, PluginRepository, PluginSource};
use hearthsound_test_support::{HELLO_CATALOG, fixture_plugin};
use reqwest::StatusCode;
use reqwest::header::AUTHORIZATION;
use serde_json::{Value, json};

use crate::common::Fixture;

/// The manifest id of the hello catalog fixture.
const HELLO_ID: &str = "com.example.hello-catalog";

/// The slug of the hello catalog instance.
const HELLO_SLUG: &str = "hello";

/// The MusicBrainz id the hello catalog names its artist by.
const HELLO_ARTIST_MBID: &str = "hello-artist-1";

/// The album the server's own library holds.
const OWNED: &str = "Owned Album";

/// The album only the provider knows.
const REMOTE: &str = "Hello Album";

/// Installs the hello catalog fixture and assigns it to the catalog layer.
///
/// # Parameters
///
/// - `fixture`: the running server.
async fn install_catalog(fixture: &Fixture) {
    let installed = fixture
        .services
        .plugins()
        .install(&fixture_plugin(HELLO_CATALOG))
        .await
        .unwrap();
    let registration = PluginRepository::new(fixture.services.database())
        .register(&installed, PluginSource::Sideload)
        .await
        .unwrap();
    assert_eq!(registration.id, HELLO_ID);
    fixture
        .services
        .instances()
        .create(&InstanceSpec {
            slug: HELLO_SLUG.to_owned(),
            plugin_id: HELLO_ID.to_owned(),
            display_name: "Hello catalog".to_owned(),
            server_config: json!({}),
            host_config: Value::Null,
        })
        .await
        .unwrap();
    fixture
        .services
        .instances()
        .set_layer(Layer::Catalog, &[HELLO_SLUG.to_owned()])
        .await
        .unwrap();
    fixture.services.registry().rebuild().await.unwrap();
}

/// Writes one library album with a track and saves it for the test account.
///
/// # Parameters
///
/// - `fixture`: the running server.
///
/// # Returns
///
/// The album's id.
async fn owned_album(fixture: &Fixture) -> EntityId {
    let library = LibraryRepository::new(fixture.services.database());
    let mut artist = ArtistInput::new("Scope Artist");
    artist.ids = vec![ExternalId {
        namespace: "mb-artist".to_owned(),
        value: HELLO_ARTIST_MBID.to_owned(),
    }];
    let stored = library
        .upsert_album(
            &AlbumInput::new(OWNED),
            &[ArtistCredit {
                artist,
                role: "main".to_owned(),
            }],
        )
        .await
        .unwrap();
    let mut track = TrackInput::new("Owned Song", Availability::Local);
    track.album_id = Some(stored);
    library.insert_track(&track, &[], &[]).await.unwrap();
    let mut connection = fixture
        .services
        .database()
        .writer()
        .acquire()
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_library (user_id, entity_kind, entity_id, origin, added_at) \
         VALUES (?, 'album', ?, 'saved', 0)",
    )
    .bind(fixture.user_id.as_bytes().as_slice())
    .bind(stored.uuid().as_bytes().as_slice())
    .execute(&mut *connection)
    .await
    .unwrap();
    stored
}

/// Returns the album titles a `getArtist` answer lists.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `artist`: the artist's id.
/// - `vendor`: whether the request carries `hearthsound=1`.
///
/// # Returns
///
/// The titles, in the order the answer lists them.
async fn artist_albums(fixture: &Fixture, artist: EntityId, vendor: bool) -> Vec<String> {
    let flag = if vendor { "&hearthsound=1" } else { "" };
    let response = fixture
        .get(&format!(
            "/rest/getArtist?id={artist}&apiKey={}&f=json{flag}",
            fixture.api_key
        ))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_str(&response.text().await.unwrap()).unwrap();
    let albums = &body["subsonic-response"]["artist"]["album"];
    albums
        .as_array()
        .map(|albums| {
            albums
                .iter()
                .filter_map(|album| album["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the artist page as this server's own client does, and runs the indexer
/// until the work the page queued is stored.
///
/// A held artist's page is queued rather than fetched while it is being viewed,
/// so the discography arrives one index pass later - which is what a running
/// server does with its own worker.
///
/// # Parameters
///
/// - `fixture`: the running server.
/// - `artist`: the artist's id.
async fn load_page(fixture: &Fixture, artist: EntityId) {
    let response = fixture
        .client
        .get(format!("{}/api/v1/artists/{artist}", fixture.base_url()))
        .header(AUTHORIZATION, format!("Bearer {}", fixture.api_key))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let indexer = std::sync::Arc::clone(fixture.services.catalog().indexer());
    let worker_indexer = std::sync::Arc::clone(&indexer);
    let shutdown = hearthsound_jobs::Shutdown::new();
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { worker_indexer.run(shutdown).await })
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let (queued, failed) = indexer.queue_counts().await.unwrap();
        if queued == 0 && failed == 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the index queue did not drain: {queued} queued, {failed} failed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    shutdown.trigger();
    worker.await.unwrap();
}

/// A third-party client sees only the albums its own library holds, and this
/// server's own client sees the stored discography beside them.
#[tokio::test]
async fn a_third_party_client_sees_only_its_own_albums() {
    let fixture = Fixture::new().await;
    install_catalog(&fixture).await;
    let album = owned_album(&fixture).await;
    let artist = {
        let mut connection = fixture
            .services
            .database()
            .readers()
            .acquire()
            .await
            .unwrap();
        let row: Vec<u8> =
            sqlx::query_scalar("SELECT artist_id FROM album_artists WHERE album_id = ? LIMIT 1")
                .bind(album.uuid().as_bytes().as_slice())
                .fetch_one(&mut *connection)
                .await
                .unwrap();
        EntityId::from_uuid(IdPrefix::Artist, uuid::Uuid::from_slice(&row).unwrap())
    };

    load_page(&fixture, artist).await;

    let own = artist_albums(&fixture, artist, true).await;
    assert!(
        own.contains(&OWNED.to_owned()) && own.contains(&REMOTE.to_owned()),
        "the server's own client sees the stored discography: {own:?}"
    );
    let third_party = artist_albums(&fixture, artist, false).await;
    assert_eq!(
        third_party,
        vec![OWNED.to_owned()],
        "a third-party client sees only the albums its library holds"
    );
}
