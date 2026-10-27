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
