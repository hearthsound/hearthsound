//! Placing files at a name the caller picked free.
//!
//! Every writer that puts a file under a managed root or into a scanned folder
//! goes through these two primitives: the collision walk that never replaces a
//! file, and the move that leaves the destination with an inode of its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use hearthsound_library::placement;

/// Writes a file with the given bytes.
fn write(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn move_file_unshared_copies_a_hardlinked_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.flac");
    let other = dir.path().join("other.flac");
    write(&source, b"the bytes");
    std::fs::hard_link(&source, &other).unwrap();
    let destination = dir.path().join("folder/album/song.flac");

    placement::move_file_unshared(&source, &destination)
        .await
        .unwrap();

    assert!(!source.exists(), "the source name is gone");
    assert_eq!(std::fs::read(&destination).unwrap(), b"the bytes");
    assert_eq!(
        std::fs::read(&other).unwrap(),
        b"the bytes",
        "the other name keeps its bytes"
    );
    let moved = std::fs::metadata(&destination).unwrap();
    let kept = std::fs::metadata(&other).unwrap();
    assert_ne!(
        moved.ino(),
        kept.ino(),
        "the destination must not share the other name's inode"
    );
    assert_eq!(
        kept.nlink(),
        1,
        "the other name is the only link of its inode now"
    );
}

#[tokio::test]
async fn move_file_unshared_renames_a_single_link() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.flac");
    write(&source, b"the bytes");
    let inode = std::fs::metadata(&source).unwrap().ino();
    let destination = dir.path().join("folder/album/song.flac");

    placement::move_file_unshared(&source, &destination)
        .await
        .unwrap();

    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), b"the bytes");
    assert_eq!(
        std::fs::metadata(&destination).unwrap().ino(),
        inode,
        "a file with one name is renamed, not copied"
    );
}

#[tokio::test]
async fn free_destination_never_replaces_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let taken = dir.path().join("Artist/Album/01 - Song.flac");
    write(&taken, b"the original bytes");

    let free = placement::free_destination(&taken, &HashSet::new())
        .await
        .unwrap();

    assert_eq!(
        free.file_name().unwrap().to_str().unwrap(),
        "01 - Song (2).flac"
    );
    assert_eq!(
        std::fs::read(&taken).unwrap(),
        b"the original bytes",
        "the file that was already there is untouched"
    );
}
