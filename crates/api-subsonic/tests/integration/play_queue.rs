//! Integration tests for the play queue endpoints: the round trip a client
//! makes from saving a queue to reading it back, the entry a catalog id
//! projects to, and the ids and indexes the endpoints refuse.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::seed::{Seeded, library};
use crate::common::{Fixture, SNAPSHOT_FILTERS, assert_error, assert_ok, get};

/// Builds a fixture with the seeded library.
///
/// # Returns
///
/// The fixture and the ids of the seeded entities; the fixture keeps its
/// temporary directory alive.
async fn fixture() -> (Fixture, Seeded) {
    let fixture = Fixture::new().await;
    let seeded = library(
        &fixture.database,
        &fixture.subdir("owned"),
        &fixture.subdir("generic"),
        fixture.user_id,
    )
    .await;
    (fixture, seeded)
}

/// `savePlayQueue` stores a queue that `getPlayQueue` and
/// `getPlayQueueByIndex` report.
#[tokio::test]
async fn a_saved_queue_is_reported_in_both_shapes() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    // No queue is saved yet, so neither endpoint carries a member.
    let empty = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&empty);
    assert!(
        !empty.body.contains("playQueue"),
        "an account with no saved queue gets no member: {}",
        empty.body
    );

    let saved = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={}&id={}&current={}&position=3000&c=Rufin&apiKey={}&f=json",
            seeded.alpha_tracks[0], seeded.alpha_tracks[1], seeded.alpha_tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);

    let json = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&json);
    assert!(
        json.body
            .contains(&format!("\"current\":\"{}\"", seeded.alpha_tracks[1])),
        "{}",
        json.body
    );
    assert!(json.body.contains("\"position\":3000"), "{}", json.body);
    assert!(json.body.contains("\"username\":\"echo\""), "{}", json.body);
    assert!(
        json.body.contains("\"changedBy\":\"Rufin\""),
        "{}",
        json.body
    );
    assert!(
        json.body
            .contains(&format!("\"id\":\"{}\"", seeded.alpha_tracks[0])),
        "both entries are listed: {}",
        json.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("play-queue-json", seeded.stabilize(&json.body));
    });

    let indexed = get(
        &app,
        &format!("/rest/getPlayQueueByIndex.view?apiKey={}", fixture.api_key),
    )
    .await;
    assert_ok(&indexed);
    assert!(
        indexed.body.contains("currentIndex=\"1\""),
        "the queue reports the position of its current entry: {}",
        indexed.body
    );
    insta::with_settings!({filters => SNAPSHOT_FILTERS.to_vec()}, {
        insta::assert_snapshot!("play-queue-by-index-xml", seeded.stabilize(&indexed.body));
    });

    // A save with no id clears the queue, and an empty queue reports nothing.
    let cleared = get(
        &app,
        &format!(
            "/rest/savePlayQueue?c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&cleared);
    let after = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&after);
    assert!(
        !after.body.contains("playQueue"),
        "the cleared queue is gone: {}",
        after.body
    );
}

/// `savePlayQueueByIndex` stores the entry the client is on by position.
#[tokio::test]
async fn a_queue_saved_by_index_keeps_its_position() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let saved = get(
        &app,
        &format!(
            "/rest/savePlayQueueByIndex?id={}&id={}&currentIndex=0&position=1000&c=Feishin&apiKey={}&f=json",
            seeded.alpha_tracks[0], seeded.alpha_tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);

    let queue = get(
        &app,
        &format!(
            "/rest/getPlayQueueByIndex?apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&queue);
    assert!(queue.body.contains("\"currentIndex\":0"), "{}", queue.body);
    assert!(queue.body.contains("\"position\":1000"), "{}", queue.body);
    assert!(
        queue.body.contains("\"changedBy\":\"Feishin\""),
        "{}",
        queue.body
    );

    // The queue endpoint that names the current entry by id reports the same
    // entry.
    let by_id = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&by_id);
    assert!(
        by_id
            .body
            .contains(&format!("\"current\":\"{}\"", seeded.alpha_tracks[0])),
        "{}",
        by_id.body
    );
}

/// A catalog entry is projected from what the catalog reports, and one the
/// server already stores is projected as the stored row.
#[tokio::test]
async fn a_catalog_entry_is_projected_from_the_catalog() {
    let (fixture, _seeded) = fixture().await;
    fixture.install_hello_catalog().await;
    let app = fixture.app();

    let found = get(
        &app,
        &format!(
            "/rest/search3?query=cardinals&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&found);
    let start = found
        .body
        .find("x_hello_t_")
        .expect("the search lists a track");
    let rest = &found.body[start..];
    let catalog_song = &rest[..rest.find('"').unwrap()];

    let saved = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={catalog_song}&current={catalog_song}&c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);

    let queue = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&queue);
    assert!(
        queue.body.contains("x_hello_t_"),
        "the catalog entry is projected from the catalog: {}",
        queue.body
    );

    // Once the server stores the track, the same entry reports the stored row.
    let starred = get(
        &app,
        &format!(
            "/rest/star?id={catalog_song}&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&starred);
    let queue = get(
        &app,
        &format!("/rest/getPlayQueue?apiKey={}&f=json", fixture.api_key),
    )
    .await;
    assert_ok(&queue);
    assert!(
        queue.body.contains("\"entry\":[{\"id\":\"tr_"),
        "the stored row is what a materialized entry reports: {}",
        queue.body
    );
}

/// The queue endpoints refuse an id that is not a client id and an index past
/// the end of the queue.
#[tokio::test]
async fn play_queue_requests_are_refused_without_usable_values() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();

    let unknown = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id=nonsense&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&unknown, 70);

    let bad_index = get(
        &app,
        &format!(
            "/rest/savePlayQueueByIndex?id={}&currentIndex=3&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_index, 0);

    let bad_position = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={}&position=soon&apiKey={}&f=json",
            seeded.alpha_tracks[0], fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_position, 0);

    // A current entry the queue does not hold is refused rather than guessed.
    let bad_current = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={}&current={}&apiKey={}&f=json",
            seeded.alpha_tracks[0], seeded.alpha_tracks[1], fixture.api_key
        ),
    )
    .await;
    assert_error(&bad_current, 0);
}

/// Saving a queue keeps only the ids that resolve, moves the current index with
/// them, and refuses a negative position.
#[tokio::test]
async fn an_unresolvable_id_is_dropped_and_the_index_follows() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let stored = seeded.alpha_tracks[0];
    // A track uuid nothing names, an album id, and a catalog track no instance
    // answers: none of them can be served back.
    let gone = hearthsound_core::EntityId::from_uuid(
        hearthsound_core::IdPrefix::Track,
        uuid::Uuid::now_v7(),
    )
    .to_string();
    let album = seeded.alpha.to_string();
    // A catalog id no instance answers: the slug names no configured catalog.
    let catalog = hearthsound_core::CatalogId::new(
        hearthsound_core::Slug::new("nowhere").unwrap(),
        hearthsound_core::EntityKind::Track,
        "ghost",
    )
    .unwrap()
    .to_string();

    let saved = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={gone}&id={stored}&id={album}&id={catalog}&current={stored}&position=2000&c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);

    for call in ["getPlayQueue", "getPlayQueueByIndex"] {
        let queue = get(
            &app,
            &format!("/rest/{call}?c=Rufin&apiKey={}&f=json", fixture.api_key),
        )
        .await;
        assert_ok(&queue);
        assert!(
            queue.body.contains(&format!("\"id\":\"{stored}\"")),
            "{call} dropped the resolvable entry: {}",
            queue.body
        );
        assert!(
            !queue.body.contains(&gone) && !queue.body.contains(catalog.as_str()),
            "{call} served an entry that does not resolve: {}",
            queue.body
        );
        // Each shape reports the current entry its own way.
        if call == "getPlayQueueByIndex" {
            assert!(
                queue.body.contains("\"currentIndex\":0"),
                "{call} did not remap the index: {}",
                queue.body
            );
        } else {
            assert!(
                queue.body.contains(&format!("\"current\":\"{stored}\"")),
                "{call} did not remap the current entry: {}",
                queue.body
            );
        }
        assert!(
            queue.body.contains("\"position\":2000"),
            "{call} lost the position: {}",
            queue.body
        );
    }

    // A negative position is a broken request, and it leaves the saved queue as
    // it was.
    let refused = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={stored}&position=-1&c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_error(&refused, 0);
    let queue = get(
        &app,
        &format!(
            "/rest/getPlayQueue?c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&queue);
    assert!(
        queue.body.contains(&format!("\"current\":\"{stored}\"")),
        "the refused save changed the queue: {}",
        queue.body
    );
}

/// An unknown current id in the middle of the list lands on the next kept
/// entry.
#[tokio::test]
async fn an_unknown_current_after_filtering_lands_on_the_next_entry() {
    let (fixture, seeded) = fixture().await;
    let app = fixture.app();
    let (first, second) = (seeded.alpha_tracks[0], seeded.alpha_tracks[1]);
    let gone = hearthsound_core::EntityId::from_uuid(
        hearthsound_core::IdPrefix::Track,
        uuid::Uuid::now_v7(),
    )
    .to_string();

    let saved = get(
        &app,
        &format!(
            "/rest/savePlayQueue?id={first}&id={gone}&id={second}&current={gone}&c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&saved);

    let queue = get(
        &app,
        &format!(
            "/rest/getPlayQueue?c=Rufin&apiKey={}&f=json",
            fixture.api_key
        ),
    )
    .await;
    assert_ok(&queue);
    assert!(
        queue.body.contains(&format!("\"current\":\"{second}\"")),
        "the index did not move to the next kept entry: {}",
        queue.body
    );
    assert!(
        queue.body.contains(&format!("\"id\":\"{second}\"")),
        "{}",
        queue.body
    );
}
