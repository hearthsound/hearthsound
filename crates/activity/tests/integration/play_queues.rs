//! Tests for the saved play queue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_activity::{ActivityError, PlayQueues};

use crate::common::{insert_user, open_database};

/// The ids one client sent, in queue order.
fn ids() -> Vec<String> {
    vec![
        "tr_one".to_owned(),
        "tr_two".to_owned(),
        "tr_three".to_owned(),
    ]
}

/// The most recently saved queue is the one that is read back, whichever client
/// saved it.
#[tokio::test]
async fn the_latest_saved_queue_is_the_one_that_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queues = PlayQueues::new(&database);
    let user = insert_user(&database).await;

    assert!(queues.latest(user).await.unwrap().is_none());

    queues.save(user, "phone", &ids(), 1, 4_000).await.unwrap();
    let saved = queues.latest(user).await.unwrap().unwrap();
    assert_eq!(saved.client, "phone");
    assert_eq!(saved.track_ids, ids());
    assert_eq!(saved.current_index, 1);
    assert_eq!(saved.position_ms, 4_000);

    // Another client's save replaces what the queue endpoint answers, even
    // though the first client's row is still stored.
    queues
        .save(user, "desktop", &ids()[..2], 0, 250)
        .await
        .unwrap();
    let saved = queues.latest(user).await.unwrap().unwrap();
    assert_eq!(saved.client, "desktop");
    assert_eq!(saved.track_ids, ids()[..2]);
    assert_eq!(saved.current_index, 0);

    // The same client saving again updates its own row.
    queues
        .save(user, "desktop", &ids(), 2, 1_000)
        .await
        .unwrap();
    let saved = queues.latest(user).await.unwrap().unwrap();
    assert_eq!(saved.client, "desktop");
    assert_eq!(saved.current_index, 2);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM play_queues WHERE user_id = ?")
        .bind(user.as_bytes().as_slice())
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(rows, 2, "one row per client");
}

/// Saving an empty queue clears the saved one.
#[tokio::test]
async fn saving_an_empty_queue_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queues = PlayQueues::new(&database);
    let user = insert_user(&database).await;

    queues.save(user, "phone", &ids(), 0, 0).await.unwrap();
    assert!(queues.latest(user).await.unwrap().is_some());

    queues.save(user, "phone", &[], 0, 0).await.unwrap();
    assert!(queues.latest(user).await.unwrap().is_none());
}

/// One client's queue does not hide another's.
#[tokio::test]
async fn queues_are_per_user_and_per_client() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queues = PlayQueues::new(&database);
    let (first, second) = (insert_user(&database).await, insert_user(&database).await);

    queues.save(first, "phone", &ids(), 0, 0).await.unwrap();
    assert!(queues.latest(second).await.unwrap().is_none());

    queues
        .save(second, "phone", &ids()[..1], 0, 0)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM play_queues")
        .fetch_one(database.readers())
        .await
        .unwrap();
    assert_eq!(rows, 2);
}

/// An index outside the queue is refused.
#[tokio::test]
async fn an_index_outside_the_queue_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let database = open_database(dir.path()).await;
    let queues = PlayQueues::new(&database);
    let user = insert_user(&database).await;

    let refused = queues.save(user, "phone", &ids(), 3, 0).await.unwrap_err();
    assert!(
        matches!(
            refused,
            ActivityError::InvalidQueueIndex { index: 3, len: 3 }
        ),
        "{refused:?}"
    );
    assert!(queues.latest(user).await.unwrap().is_none());
}
