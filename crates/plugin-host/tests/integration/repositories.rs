//! Integration tests for the plugin repositories an administrator registers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use hearthsound_core::{EntityId, IdPrefix, new_uuid_v7};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::{PluginRepository, RepoError};

/// The repository URL that sorts first.
const ALPHA: &str = "https://alpha.example.com/v1/index.json";

/// The repository URL that sorts second.
const BETA: &str = "https://beta.example.com/v1/index.json";

/// A minisign public key an administrator may have supplied.
const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";

/// One migrated database and the repository over it.
struct Env {
    /// Kept alive so the database file outlives the pool.
    _dir: tempfile::TempDir,
    repository: PluginRepository,
}

impl Env {
    /// Opens a migrated temporary database.
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let database = Database::open(&dir.path().join("hearthsound.db"))
            .await
            .unwrap();
        MIGRATOR.run(database.writer()).await.unwrap();
        Env {
            _dir: dir,
            repository: PluginRepository::new(&database),
        }
    }
}

#[tokio::test]
async fn a_repository_round_trips() {
    let env = Env::new().await;

    let row = env
        .repository
        .insert_repository(ALPHA, Some("Example"), Some(KEY), true)
        .await
        .unwrap();
    assert_eq!(row.url, ALPHA);
    assert_eq!(row.name.as_deref(), Some("Example"));
    assert_eq!(row.public_key.as_deref(), Some(KEY));
    assert!(row.enabled);
    assert_eq!(row.last_fetched_at, None);
    assert_eq!(row.last_error, None);

    // The generated id is a UUIDv7 clients see as a `pr_` id.
    assert_eq!(row.id.get_version_num(), 7);
    let client_id = EntityId::from_uuid(IdPrefix::PluginRepository, row.id);
    assert!(client_id.to_string().starts_with("pr_"));
    assert_eq!(client_id.uuid(), row.id);

    assert_eq!(
        env.repository.repository(row.id).await.unwrap(),
        Some(row.clone())
    );
    assert_eq!(
        env.repository.repository_by_url(ALPHA).await.unwrap(),
        Some(row.clone())
    );
    assert_eq!(
        env.repository.repositories().await.unwrap(),
        vec![row.clone()]
    );

    let updated = env
        .repository
        .update_repository(row.id, None, None, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.id, row.id);
    assert_eq!(updated.url, ALPHA);
    assert_eq!(updated.name, None);
    assert_eq!(updated.public_key, None);
    assert!(!updated.enabled);
    assert_eq!(
        env.repository.repository(row.id).await.unwrap(),
        Some(updated)
    );

    // An id no repository has changes and deletes nothing.
    let unknown = new_uuid_v7();
    assert_eq!(
        env.repository
            .update_repository(unknown, Some("Name"), None, true)
            .await
            .unwrap(),
        None
    );
    assert!(!env.repository.delete_repository(unknown).await.unwrap());

    assert!(env.repository.delete_repository(row.id).await.unwrap());
    assert_eq!(env.repository.repository(row.id).await.unwrap(), None);
    assert_eq!(env.repository.repository_by_url(ALPHA).await.unwrap(), None);
    assert!(env.repository.repositories().await.unwrap().is_empty());
    assert!(!env.repository.delete_repository(row.id).await.unwrap());
}

#[tokio::test]
async fn a_repository_url_is_unique() {
    let env = Env::new().await;

    let first = env
        .repository
        .insert_repository(ALPHA, Some("Example"), None, true)
        .await
        .unwrap();
    let error = env
        .repository
        .insert_repository(ALPHA, Some("Duplicate"), Some(KEY), false)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RepoError::DuplicateUrl(url) if url.as_str() == ALPHA),
        "expected a duplicate-URL error, got {error:?}"
    );
    // The registered row is untouched.
    assert_eq!(env.repository.repositories().await.unwrap(), vec![first]);
}

#[tokio::test]
async fn repositories_are_listed_by_url() {
    let env = Env::new().await;

    let beta = env
        .repository
        .insert_repository(BETA, None, None, true)
        .await
        .unwrap();
    let alpha = env
        .repository
        .insert_repository(ALPHA, Some("Example"), None, false)
        .await
        .unwrap();

    assert_eq!(
        env.repository.repositories().await.unwrap(),
        vec![alpha, beta]
    );
}

#[tokio::test]
async fn a_fetch_records_its_outcome() {
    let env = Env::new().await;
    let row = env
        .repository
        .insert_repository(ALPHA, None, None, true)
        .await
        .unwrap();

    let failed = env
        .repository
        .record_fetch(row.id, 1_700_000_000_000, Some("connection refused"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.last_fetched_at, Some(1_700_000_000_000));
    assert_eq!(failed.last_error.as_deref(), Some("connection refused"));

    // A fetch that read the index clears the error of the one before it.
    let read = env
        .repository
        .record_fetch(row.id, 1_700_000_600_000, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.last_fetched_at, Some(1_700_000_600_000));
    assert_eq!(read.last_error, None);
    assert_eq!(
        env.repository.repository(row.id).await.unwrap(),
        Some(read.clone())
    );

    // A repository that is gone records nothing.
    assert_eq!(
        env.repository
            .record_fetch(new_uuid_v7(), 1_700_000_600_000, None)
            .await
            .unwrap(),
        None
    );
}
