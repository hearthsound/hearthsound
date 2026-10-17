//! Tests for upload ingest: an uploaded file becomes the uploader's permanent
//! library track and is stored for everyone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, settings::Settings};
use hearthsound_core::events::EventBus;
use hearthsound_core::{Availability, PinReason, StorageClass};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_library::query::{Scope, ServerBrowse};
use hearthsound_library::upload::UploadIngest;
use hearthsound_library::{LibraryError, LibraryRepository, StorageRoots};
use hearthsound_test_support::audio::{AudioFixture, AudioFormat, TrackTags};
use uuid::Uuid;

/// An ingest over a temporary data directory with two accounts.
struct Fixture {
    /// Keeps the tree alive for the whole test.
    dir: tempfile::TempDir,
    /// The open database.
    db: Database,
    /// The managed roots.
    roots: StorageRoots,
    /// The ingest under test.
    ingest: UploadIngest,
    /// The uploader.
    uploader: Uuid,
    /// Another account.
    other: Uuid,
}

impl Fixture {
    async fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        db.migrate(&MIGRATOR, &dir.path().join("backups"))
            .await
            .unwrap();
        let roots = StorageRoots::new(dir.path().join("library"), dir.path().join("cache"));
        let settings = Settings::load(
            db.writer().clone(),
            SettingsRegistry::builtin(),
            SettingSources::default(),
        )
        .await
        .unwrap();
        let ingest = UploadIngest::new(&db, roots.clone(), settings, Arc::new(EventBus::new()));
        let uploader = user(&db, "uploader").await;
        let other = user(&db, "other").await;
        Fixture {
            dir,
            db,
            roots,
            ingest,
            uploader,
            other,
        }
    }

    /// Writes a generated FLAC into a staging directory and returns its path.
    async fn staged_flac(&self, title: &str, number: u32) -> std::path::PathBuf {
        let staging = self.dir.path().join("uploads-staging").join(title);
        std::fs::create_dir_all(&staging).unwrap();
        AudioFixture::new(AudioFormat::Flac)
            .seconds(1.0)
            .file_name(format!("{title}.flac"))
            .tags(
                TrackTags::new(title, "Upload Artist")
                    .album("Upload Album")
                    .album_artist("Upload Artist")
                    .track(number, 3),
            )
            .write_into(&staging)
            .await
            .unwrap()
    }
}

/// Inserts an account.
async fn user(db: &Database, name: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, 'x', 0, 0)",
    )
    .bind(id.as_bytes().as_slice())
    .bind(name)
    .bind(name)
    .execute(db.writer())
    .await
    .unwrap();
    id
}

/// Three uploaded FLACs become three permanent library tracks of the uploader,
/// stored under the library root and in the server scope, and nothing of them
/// enters the other account's library.
#[tokio::test]
async fn uploaded_files_join_the_uploaders_library_and_the_server() {
    let fixture = Fixture::new().await;
    let library = LibraryRepository::new(&fixture.db);
    let mut stored = Vec::new();
    for (number, title) in ["First", "Second", "Third"].iter().enumerate() {
        let staged = fixture.staged_flac(title, number as u32 + 1).await;
        let upload = fixture
            .ingest
            .ingest(fixture.uploader, &staged, &format!("{title}.flac"))
            .await
            .unwrap();
        assert!(!staged.exists(), "the staging file is consumed");
        stored.push(upload);
    }

    for (upload, title) in stored.iter().zip(["First", "Second", "Third"]) {
        let path = fixture.roots.library.join(&upload.rel_path);
        assert!(path.is_file(), "{} exists", path.display());
        assert!(
            upload.rel_path.ends_with(&format!("{title}.flac")),
            "{}",
            upload.rel_path
        );
        let file = library
            .get_media_file(upload.media_file_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(file.storage, StorageClass::Library);
        assert_eq!(file.uploaded_by, Some(fixture.uploader));
        let track = library.get_track(upload.track_id).await.unwrap().unwrap();
        assert_eq!(track.title, title);
        assert_eq!(track.availability, Availability::Local);

        let mut conn = fixture.db.readers().acquire().await.unwrap();
        assert_eq!(
            hearthsound_library::retention::pin_reasons(&mut conn, upload.track_id)
                .await
                .unwrap(),
            [PinReason::UserLibrary]
        );
        let mine = Scope::Personal {
            user_id: fixture.uploader,
        };
        let theirs = Scope::Personal {
            user_id: fixture.other,
        };
        let server = Scope::Server(ServerBrowse::PermanentOnly);
        let visible = async |conn: &mut sqlx::SqliteConnection, scope| {
            hearthsound_library::query::track(conn, scope, upload.track_id)
                .await
                .unwrap()
                .is_some()
        };
        assert!(visible(&mut conn, mine).await);
        assert!(!visible(&mut conn, theirs).await);
        assert!(visible(&mut conn, server).await);
    }
}

/// A file that is not audio is refused and its staging file left in place.
#[tokio::test]
async fn a_file_that_is_not_audio_is_refused() {
    let fixture = Fixture::new().await;
    let staging = fixture.dir.path().join("uploads-staging");
    std::fs::create_dir_all(&staging).unwrap();
    let text = staging.join("notes.flac");
    std::fs::write(&text, b"these are not samples").unwrap();
    let named = staging.join("notes.txt");
    std::fs::write(&named, b"these are not samples").unwrap();

    for (path, name) in [(&text, "notes.flac"), (&named, "notes.txt")] {
        let refused = fixture
            .ingest
            .ingest(fixture.uploader, path, name)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, LibraryError::UnsupportedUpload { .. }),
            "{refused:?}"
        );
        assert!(path.exists());
    }
    assert!(!library_root_has_files(&fixture.roots.library));
}

/// `true` when anything was written under a directory.
fn library_root_has_files(root: &Path) -> bool {
    root.exists() && std::fs::read_dir(root).unwrap().next().is_some()
}
