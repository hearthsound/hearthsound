//! Tests for watching `PRAGMA data_version`: a commit from another connection
//! notifies a subscriber exactly once, the server's own writes notify nobody,
//! and shutting the watcher down stops the polling task.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use hearthsound_db::Database;
use sqlx::SqlitePool;

/// The interval the tests poll at: short, so notifications arrive quickly.
const POLL: Duration = Duration::from_millis(20);

/// How long a test waits before it concludes that no notification arrives.
const QUIET: Duration = Duration::from_millis(500);

/// How long a test waits for a notification it expects.
const EXPECTED: Duration = Duration::from_secs(5);

async fn open_temp(dir: &Path) -> Database {
    Database::open(&dir.join("hearthsound.db")).await.unwrap()
}

/// Opens a second pool on the same file, the way a CLI command does.
///
/// The pool connects before the watcher starts, so connection setup is never
/// mistaken for a change.
async fn cli_pool(dir: &Path) -> SqlitePool {
    let url = format!("sqlite://{}", dir.join("hearthsound.db").display());
    SqlitePool::connect(&url).await.unwrap()
}

#[tokio::test]
async fn a_commit_from_another_connection_notifies_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;
    let other = cli_pool(dir.path()).await;

    let watcher = db.watch_data_version(POLL).await;
    let mut changes = watcher.subscribe();

    sqlx::query("CREATE TABLE a (id INTEGER PRIMARY KEY)")
        .execute(&other)
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(EXPECTED, changes.changed())
            .await
            .expect("a commit from another connection must notify"),
        "a commit from another connection must notify"
    );
    assert!(
        tokio::time::timeout(QUIET, changes.changed())
            .await
            .is_err(),
        "one commit must notify exactly once"
    );

    watcher.shutdown().await;
    other.close().await;
    db.close().await;
}

#[tokio::test]
async fn the_servers_own_writes_do_not_notify() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;

    let watcher = db.watch_data_version(POLL).await;
    let mut changes = watcher.subscribe();

    sqlx::query("CREATE TABLE a (id INTEGER PRIMARY KEY)")
        .execute(db.writer())
        .await
        .unwrap();
    sqlx::query("INSERT INTO a (id) VALUES (1)")
        .execute(db.writer())
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(QUIET, changes.changed())
            .await
            .is_err(),
        "writes through the writer pool must not notify"
    );

    watcher.shutdown().await;
    db.close().await;
}

#[tokio::test]
async fn shutdown_stops_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;
    let other = cli_pool(dir.path()).await;

    let watcher = db.watch_data_version(POLL).await;
    let mut changes = watcher.subscribe();

    sqlx::query("CREATE TABLE a (id INTEGER PRIMARY KEY)")
        .execute(&other)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(EXPECTED, changes.changed())
            .await
            .expect("the watcher must run before it is shut down"),
        "the watcher must run before it is shut down"
    );

    watcher.shutdown().await;

    // Shutdown waits for the polling task and drops the watcher's own sender, so
    // the subscription ending proves the task exited.
    assert!(
        !changes.changed().await,
        "a subscription must end when the watcher stops"
    );

    // A later commit reaches nobody.
    sqlx::query("CREATE TABLE b (id INTEGER PRIMARY KEY)")
        .execute(&other)
        .await
        .unwrap();
    assert!(
        !changes.changed().await,
        "a stopped watcher must not notify"
    );

    other.close().await;
    db.close().await;
}

/// A poll notices a foreign commit while the writer connection is busy.
///
/// The poll used to read the pragma on the writer connection, so a long write
/// transaction delayed every notification - and every write that arrived while a
/// poll waited queued behind it. The poll now owns a read-only connection, and a
/// change it cannot attribute because the writer is busy is still reported.
#[tokio::test]
async fn a_poll_does_not_wait_for_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp(dir.path()).await;
    let other = cli_pool(dir.path()).await;

    let watcher = db.watch_data_version(POLL).await;
    let mut changes = watcher.subscribe();

    // The writer connection is held for as long as the assertion runs. A
    // deferred transaction holds the connection without blocking the other
    // connection's write, which an immediate one would.
    let mut held = db.writer().acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *held).await.unwrap();

    sqlx::query("CREATE TABLE a (id INTEGER PRIMARY KEY)")
        .execute(&other)
        .await
        .unwrap();

    assert!(
        tokio::time::timeout(EXPECTED, changes.changed())
            .await
            .expect("a foreign commit is noticed while the writer is busy"),
        "a foreign commit is noticed while the writer is busy"
    );

    sqlx::query("COMMIT").execute(&mut *held).await.unwrap();
    drop(held);
    watcher.shutdown().await;
    other.close().await;
    db.close().await;
}
