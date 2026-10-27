//! Views: a page whose data is being fetched answers partial once and completes
//! when the walk lands and `entity.updated` is published.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use hearthsound_catalog::{Completeness, Viewer};
use hearthsound_config::SettingValue;
use hearthsound_core::EntityKind;
use hearthsound_core::events::ServerEvent;
use hearthsound_core::providers::MB_RELEASE;
use hearthsound_jobs::Shutdown;
use hearthsound_library::index_queue::Depth;
use hearthsound_library::{AlbumInput, ArtistCredit};
use hearthsound_test_support::METADATA_FIXTURE;
use serde_json::json;

use crate::common::{self, Env, METADATA_ID};

/// The MusicBrainz release id the fixture answers.
const RELEASE: &str = "delay-release-1";

#[tokio::test]
async fn a_cold_album_page_answers_partial_while_its_release_answer_is_late() {
    let env = Env::new().await;
    env.instance(
        "meta",
        METADATA_FIXTURE,
        METADATA_ID,
        common::delayed_metadata_config(
            json!([{
                "title": "Cold Song",
                "artists": ["Cold Artist"],
                "mbid": "delay-recording-1",
                "releases": [{
                    "mbid": RELEASE,
                    "title": "Cold Album",
                    "date": "2001-02-03",
                    "trackCount": 1,
                }],
            }]),
            json!([]),
            3_000,
        ),
    )
    .await;
    env.assign(hearthsound_core::Layer::Metadata, &["meta"])
        .await;
    env.set("index.view_timeout_ms", SettingValue::Integer(500))
        .await;

    let library = env.library();
    let mut album = AlbumInput::new("Cold Album");
    album.year = Some(2001);
    let album = library
        .upsert_album(&album, &[ArtistCredit::new("Cold Artist", "main")])
        .await
        .unwrap();
    env.attach(
        EntityKind::Album,
        album,
        &[common::external_id(MB_RELEASE, RELEASE)],
    )
    .await;

    let service = env.service();
    let user = env.user().await;
    let viewer = Viewer {
        user,
        vendor: false,
    };
    let mut receiver = env.events.subscribe();

    let first = service
        .views()
        .album(album, viewer)
        .await
        .unwrap()
        .expect("the album row exists");
    assert!(
        !first.complete,
        "a page whose release answer outlives the deadline answers partial"
    );

    wait_for_updated(&mut receiver, &album.to_string()).await;

    let second = service
        .views()
        .album(album, viewer)
        .await
        .unwrap()
        .expect("the album row exists");
    assert!(
        second.complete,
        "the page the background walk stored answers complete"
    );
    assert_eq!(
        second.tracks.len(),
        1,
        "the stored release's tracklist is the page's tracklist"
    );
}

#[tokio::test]
async fn an_inline_walk_with_no_time_answers_partial() {
    let env = Env::new().await;
    let album = env
        .library()
        .upsert_album(
            &AlbumInput::new("Instant Album"),
            &[ArtistCredit::new("Instant Artist", "main")],
        )
        .await
        .unwrap();
    let status = env
        .service()
        .indexer()
        .ensure(EntityKind::Album, album.uuid(), Depth::Page, Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(status, Completeness::Partial);
}

/// Waits for the `entity.updated` event of one id.
async fn wait_for_updated(
    receiver: &mut tokio::sync::broadcast::Receiver<hearthsound_core::events::EventEnvelope>,
    id: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let envelope = tokio::time::timeout(remaining, receiver.recv())
            .await
            .expect("the walk publishes entity.updated")
            .expect("the bus is alive");
        if let ServerEvent::EntityUpdated(update) = envelope.event
            && update.id == id
        {
            return;
        }
    }
}

/// A held album's page answers at once and queues itself instead of walking.
///
/// The page used to walk the provider inline whenever its data was missing, so a
/// library album took as long as the provider did - seconds, not milliseconds -
/// and the read held the writer the indexer needed. A held album has nothing to
/// gain from that wait: it is queued at the front of the queue and answered from
/// what is stored, and the page re-reads when the walk lands.
#[tokio::test]
async fn a_held_album_page_answers_at_once_and_queues_itself() {
    let env = Env::new().await;
    env.instance(
        "meta",
        METADATA_FIXTURE,
        METADATA_ID,
        common::delayed_metadata_config(
            json!([{
                "title": "Held Song",
                "artists": ["Held Artist"],
                "mbid": "held-recording-1",
                "releases": [{
                    "mbid": RELEASE,
                    "title": "Held Album",
                    "date": "2001-02-03",
                    "trackCount": 1,
                }],
            }]),
            json!([]),
            3_000,
        ),
    )
    .await;
    env.assign(hearthsound_core::Layer::Metadata, &["meta"])
        .await;

    let library = env.library();
    let mut album = AlbumInput::new("Held Album");
    album.year = Some(2001);
    let album = library
        .upsert_album(&album, &[ArtistCredit::new("Held Artist", "main")])
        .await
        .unwrap();
    env.attach(
        EntityKind::Album,
        album,
        &[common::external_id(MB_RELEASE, RELEASE)],
    )
    .await;
    let user = env.user().await;
    env.save(user, EntityKind::Album, album).await;

    let service = env.service();
    let viewer = Viewer {
        user,
        vendor: false,
    };

    let started = std::time::Instant::now();
    let first = service
        .views()
        .album(album, viewer)
        .await
        .unwrap()
        .expect("the album row exists");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "a held page answers from stored data, took {:?}",
        started.elapsed()
    );
    assert!(
        !first.complete,
        "the stored page is not complete until the walk lands"
    );

    let mut connection = env.database.writer().acquire().await.unwrap();
    let (depth, priority): (String, i64) = sqlx::query_as(
        "SELECT depth, priority FROM index_queue WHERE entity_kind = 'album' AND entity_id = ?",
    )
    .bind(album.uuid().as_bytes().as_slice())
    .fetch_one(&mut *connection)
    .await
    .expect("the page queued itself");
    drop(connection);
    assert_eq!(
        priority, 0,
        "a viewer's page goes to the front of the queue"
    );
    assert_eq!(depth, "page", "an album's page is what the view needs");

    let shutdown = Shutdown::new();
    let indexer = Arc::clone(service.indexer());
    let worker = {
        let shutdown = shutdown.clone();
        tokio::spawn(async move { indexer.run(shutdown).await })
    };
    env.wait_for_queue(Duration::from_secs(120)).await;
    shutdown.trigger();
    worker.await.unwrap();

    let second = service
        .views()
        .album(album, viewer)
        .await
        .unwrap()
        .expect("the album row exists");
    assert!(second.complete, "the stored page answers complete");
    assert_eq!(
        second.tracks.len(),
        1,
        "the stored release's tracklist is the page's tracklist"
    );
}
