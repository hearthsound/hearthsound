//! Search: the library's hit and the catalog's hit for one title are one album,
//! and the answer is served from the stored id list on the second call.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{EntityKind, Layer};
use hearthsound_library::{AlbumInput, ArtistCredit};
use hearthsound_test_support::HTTP_CATALOG;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{self, Env, HTTP_CATALOG_ID};

#[tokio::test]
async fn a_library_hit_and_a_provider_hit_of_one_title_answer_once() {
    let env = Env::new().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "albums": [{
                "id": "remote-album-1",
                "title": "Shared Album",
                "artists": ["Shared Artist"],
                "year": 2026,
            }],
            "tracks": [],
            "artists": [],
        })))
        .mount(&server)
        .await;
    env.instance(
        "remote",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        common::http_config(&server.uri()),
    )
    .await;
    env.assign(Layer::Catalog, &["remote"]).await;

    let library = env.library();
    let mut album = AlbumInput::new("Shared Album");
    album.year = Some(2026);
    let stored = library
        .upsert_album(&album, &[ArtistCredit::new("Shared Artist", "main")])
        .await
        .unwrap();
    let user = env.user().await;
    env.save(user, EntityKind::Album, stored).await;

    let service = env.service();
    let answer = service
        .search(user, "Shared", &[EntityKind::Album], 10, 0)
        .await
        .unwrap();

    assert_eq!(
        answer.albums.len(),
        1,
        "the library's album and the provider's album are one hit"
    );
    assert_eq!(answer.albums[0].album.id, stored);
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 1, "the provider was asked once");
    assert_eq!(env.count_rows("answer_cache").await, 1);

    let again = service
        .search(user, "Shared", &[EntityKind::Album], 10, 0)
        .await
        .unwrap();
    assert_eq!(again.albums.len(), 1);
    assert_eq!(again.albums[0].album.id, stored);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "the stored answer is served without a second provider call"
    );
}

/// A slow catalog instance does not spend the whole budget, so the instance
/// behind it still answers.
///
/// The fan-out used to share one deadline across the layer: an instance that is
/// slow - a rate-limited provider waits inside its own call - held the fan-out
/// until the deadline was gone, and the instance behind it was asked with a
/// deadline already in the past. A search then answered the library alone while a
/// healthy catalog instance was never consulted, which reads as "nothing
/// matched".
#[tokio::test]
async fn a_slow_catalog_instance_does_not_starve_the_next_one() {
    let env = Env::new().await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(3))
                .set_body_json(json!({
                    "albums": [{
                        "id": "slow-album",
                        "title": "The Slow Instance Answer",
                        "artists": ["Slow Artist"],
                        "year": 2026,
                    }],
                    "tracks": [],
                    "artists": [],
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/fast"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "albums": [{
                "id": "remote-album-2",
                "title": "Katy Perry Album",
                "artists": ["Katy Perry"],
                "year": 2026,
            }],
            "tracks": [],
            "artists": [],
        })))
        .mount(&server)
        .await;
    env.instance(
        "slow",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        common::http_config(&format!("{}/slow", server.uri())),
    )
    .await;
    env.instance(
        "fast",
        HTTP_CATALOG,
        HTTP_CATALOG_ID,
        common::http_config(&format!("{}/fast", server.uri())),
    )
    .await;
    env.assign(Layer::Catalog, &["slow", "fast"]).await;
    env.set(
        "search.catalog.timeout_ms",
        hearthsound_config::SettingValue::Integer(2_000),
    )
    .await;

    let user = env.user().await;
    let service = env.service();
    let started = std::time::Instant::now();
    let answer = service
        .search(user, "Katy Perry", &[EntityKind::Album], 10, 0)
        .await
        .unwrap();
    let elapsed = started.elapsed();

    assert_eq!(
        answer.albums.len(),
        1,
        "the second instance answered after the first one ran out of its slice"
    );
    assert_eq!(answer.albums[0].album.title, "Katy Perry Album");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the search stayed inside its budget, took {elapsed:?}"
    );
}
