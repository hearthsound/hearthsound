//! Full-stack tests of the folder watcher.
//!
//! A folder is added the way a command-line command adds it - through a second
//! connection to the same database - so these tests also cover the reload that
//! makes the running server pick the folder up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use hearthsound_db::Database;
use hearthsound_library::LibraryRepository;
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};

use crate::common::Fixture;

/// How long the watcher may take to index a change.
const WATCH_BUDGET: Duration = Duration::from_secs(10);

/// Writes the three tracks of one album into `dir`.
async fn write_album(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for index in 1..=3 {
        AudioFixture::new(AudioFormat::Flac)
            .seconds(0.3)
            .file_name(format!("0{index} - Track {index}.flac"))
            .tags(
                TrackTags::new(format!("Track {index}"), "Watched Artist")
                    .album("Watched Album")
                    .album_artist("Watched Artist")
                    .track(index, 3),
            )
            .write_into(dir)
            .await
            .unwrap();
    }
}

/// Copies a directory tree, which is what dropping an album into a watched
/// folder looks like to the filesystem.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Polls `getAlbumList2` until it names an album.
///
/// # Returns
///
/// `Ok(())` once the album appears before the budget ran out, and the last body
/// read otherwise.
async fn wait_for_album(fixture: &Fixture, title: &str) -> Result<(), String> {
    let path = format!(
        "/rest/getAlbumList2?type=alphabeticalByName&apiKey={}&f=json",
        fixture.api_key
    );
    let started = Instant::now();
    let mut body = String::new();
    while started.elapsed() < WATCH_BUDGET {
        body = fixture.get(&path).await.text().await.unwrap();
        if body.contains(&format!("\"name\":\"{title}\"")) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(body)
}

/// A folder added while the server runs is watched after the reload, and an
/// album dropped into it is indexed without a scan being asked for.
#[tokio::test(flavor = "multi_thread")]
async fn a_folder_added_while_the_server_runs_is_watched() {
    let fixture = Fixture::new().await;
    let music = fixture.services.config().paths.data.join("music");
    let staged = fixture.services.config().paths.data.join("staged");
    std::fs::create_dir_all(&music).unwrap();
    write_album(&staged.join("Watched Album")).await;

    // A command-line command adds the folder through its own connection.
    let other = Database::open(fixture.database_path()).await.unwrap();
    let added = LibraryRepository::new(&other)
        .add_library_folder(&music, Some(fixture.user_id))
        .await
        .unwrap();
    assert!(added.watch, "a new folder is watched");

    let started = Instant::now();
    while fixture.services.watcher().watched_folders() == 0 {
        assert!(
            started.elapsed() < WATCH_BUDGET,
            "the reload started a watcher for the added folder"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    copy_tree(&staged.join("Watched Album"), &music.join("Watched Album"));
    let outcome = wait_for_album(&fixture, "Watched Album").await;
    assert!(
        outcome.is_ok(),
        "the watcher indexed the album that appeared: {:?}",
        outcome.err()
    );
}

/// A watched folder is scanned once when the server starts, so what changed
/// while it was stopped is indexed without a filesystem event.
#[tokio::test(flavor = "multi_thread")]
async fn a_watched_folder_is_scanned_when_the_server_starts() {
    let fixture = Fixture::new().await;
    let music = fixture.services.config().paths.data.join("music");
    write_album(&music.join("Watched Album")).await;
    {
        let other = Database::open(fixture.database_path()).await.unwrap();
        LibraryRepository::new(&other)
            .add_library_folder(&music, Some(fixture.user_id))
            .await
            .unwrap();
    }

    let api_key = fixture.api_key.clone();
    let dir = fixture.stop().await;
    let fixture = Fixture::reopen(dir, api_key).await;
    let outcome = wait_for_album(&fixture, "Watched Album").await;
    assert!(
        outcome.is_ok(),
        "the startup scan indexed the album: {:?}",
        outcome.err()
    );
}
