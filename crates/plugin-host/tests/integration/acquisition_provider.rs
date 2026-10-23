//! Integration tests for calling an acquisition plugin through the core trait.
//!
//! The acquisition fixture simulates a downloader through `host-kv` and
//! `host-files`, so these tests cover the whole path: a search that completes
//! over several polls, a download that reports queued and running progress, and
//! the files it reports, which it discovered by listing a root through
//! `host-files`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hearthsound_config::{SettingSources, SettingsRegistry, SettingsStore};
use hearthsound_core::providers::{
    AcquisitionContext, AcquisitionProvider, AcquisitionQuery, DownloadState, SearchState,
    WantedTrack,
};
use hearthsound_core::{Granularity, ImportMode, ProviderError, Slug, new_uuid_v7};
use hearthsound_db::{Database, MIGRATOR};
use hearthsound_plugin_host::manifest::{FilesPermission, KvPermission, Permissions};
use hearthsound_plugin_host::{ImportRoot, InstanceConfig, InstanceHostConfig, PluginHost};
use hearthsound_test_support::{ACQUISITION_FIXTURE, fixture_plugin};
use uuid::Uuid;

fn settings() -> Arc<SettingsStore> {
    Arc::new(
        SettingsStore::resolve(&SettingsRegistry::builtin(), &SettingSources::default()).unwrap(),
    )
}

/// Opens a migrated temporary database for a test.
async fn database(dir: &Path) -> Arc<Database> {
    let database = Database::open(&dir.join("hearthsound.db")).await.unwrap();
    MIGRATOR.run(database.writer()).await.unwrap();
    Arc::new(database)
}

/// Registers the plugin and instance rows the fixture's storage refers to.
///
/// The key-value table is keyed by `provider_instances.id`, so an instance a
/// test configures directly has to exist in the database before its plugin can
/// store anything.
async fn register(database: &Database, instance: Uuid, plugin_id: &str, slug: &str) {
    sqlx::query!(
        "INSERT INTO plugins (id, version, api, manifest, sha256, source, enabled, installed_at)
         VALUES (?, '0.1.0', 'hearthsound:plugin@0.2.0', '{}', X'', 'sideload', 1, 0)",
        plugin_id,
    )
    .execute(database.writer())
    .await
    .unwrap();
    sqlx::query!(
        "INSERT INTO provider_instances
           (id, plugin_id, slug, display_name, enabled, created_at, updated_at)
         VALUES (?, ?, ?, ?, 1, 0, 0)",
        instance.as_bytes().as_slice(),
        plugin_id,
        slug,
        slug,
    )
    .execute(database.writer())
    .await
    .unwrap();
}

/// The permissions a test approves for the fixture.
fn permissions() -> Permissions {
    Permissions {
        kv: Some(KvPermission {
            max_bytes: Some(1024 * 1024),
        }),
        files: Some(FilesPermission {
            import_roots: Some(true),
        }),
        ..Permissions::default()
    }
}

/// A service the fixture can be pointed at, with its directories.
struct Fixture {
    _dir: tempfile::TempDir,
    /// The instance's import root, which is also the staging root.
    downloads: PathBuf,
    /// The staging root a job's context names.
    staging: PathBuf,
    provider: Arc<dyn AcquisitionProvider>,
}

/// Installs the fixture with `config`; `grant_import_root` decides whether the
/// instance is granted the `downloads` import root.
async fn fixture(config: &str, grant_import_root: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let staging = dir.path().join("staging/ac_1");
    std::fs::create_dir_all(&staging).unwrap();

    let host_config = if grant_import_root {
        host_config(&downloads)
    } else {
        InstanceHostConfig::default()
    };

    let database = database(dir.path()).await;
    let host = PluginHost::new(
        dir.path().join("plugins"),
        settings(),
        Arc::clone(&database),
    )
    .unwrap();
    let installed = host
        .install(&fixture_plugin(ACQUISITION_FIXTURE))
        .await
        .unwrap();

    let instance_id = new_uuid_v7();
    register(&database, instance_id, &installed.manifest().id, "acq").await;

    let provider = host
        .acquisition_provider(
            &installed,
            InstanceConfig::new(Slug::new("acq").unwrap())
                .with_id(instance_id)
                .with_server_config(config)
                .with_permissions(permissions())
                .with_host_config(host_config),
        )
        .unwrap();

    Fixture {
        _dir: dir,
        downloads,
        staging,
        provider,
    }
}

/// The host configuration that grants `downloads` as an import root.
fn host_config(downloads: &Path) -> InstanceHostConfig {
    InstanceHostConfig {
        import_roots: vec![ImportRoot {
            id: "downloads".to_owned(),
            path: downloads.to_path_buf(),
            mode: ImportMode::Move,
            read_only: false,
        }],
        ..InstanceHostConfig::default()
    }
}

/// The context of a job whose staging root is `staging`.
fn context(staging: &Path) -> AcquisitionContext {
    AcquisitionContext::new(staging)
}

/// The query a test searches with.
fn query() -> AcquisitionQuery {
    AcquisitionQuery {
        granularity: Granularity::Track,
        album: None,
        album_artist: None,
        year: None,
        tracks: vec![WantedTrack {
            title: "Hello Song".to_owned(),
            artists: vec!["Hello Artist".to_owned()],
            track_number: Some(1),
            disc_number: Some(1),
            duration_ms: Some(180_000),
            isrc: Some("USHW12600001".to_owned()),
            ids: vec![],
        }],
        preferred_formats: vec!["flac".to_owned()],
    }
}

/// The configuration of a normal run whose output root is `root`.
fn config(root: &str) -> String {
    format!(
        r#"{{
  "candidates": [
    {{
      "token": "cand-1",
      "title": "Hello Song",
      "path": "peer/hello.flac",
      "files": [
        {{
          "name": "hello.flac",
          "size": 42000,
          "format": "flac",
          "bitrateKbps": 987,
          "sampleRate": 44100,
          "bitDepth": 16,
          "durationMs": 180000,
          "trackIndex": 0
        }}
      ],
      "source": {{ "key": "peer-1", "speedBps": 1250000, "queueLength": 0, "freeSlot": true }}
    }}
  ],
  "searchPolls": 1,
  "downloadQueuedPolls": 1,
  "downloadRunningPolls": 2,
  "downloadBytesPerPoll": 5000,
  "outputRoot": "{root}",
  "matchSuffix": ".flac"
}}"#
    )
}

/// Polls a search until it completes, returning the candidates.
async fn complete_search(
    fixture: &Fixture,
    context: &AcquisitionContext,
) -> Vec<hearthsound_core::providers::Candidate> {
    let token = fixture
        .provider
        .start_search(context, &query())
        .await
        .unwrap();

    let mut running = false;
    for _ in 0..10 {
        match fixture.provider.poll_search(context, &token).await.unwrap() {
            SearchState::Running(candidates) => {
                assert!(candidates.is_empty());
                running = true;
            }
            SearchState::Complete(candidates) => {
                assert!(running, "the first poll should not complete the search");
                return candidates;
            }
        }
    }
    panic!("the search never completed");
}

#[tokio::test]
async fn searches_and_downloads_into_an_import_root() {
    let fixture = fixture(&config("downloads"), true).await;
    let staging = fixture.staging.clone();
    let context = context(&staging);

    let capabilities = fixture.provider.get_capabilities(&context).await.unwrap();
    assert_eq!(capabilities.granularities, [Granularity::Track]);
    assert!(!capabilities.progressive);

    let candidates = complete_search(&fixture, &context).await;
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.token, "cand-1");
    assert_eq!(candidate.path, "peer/hello.flac");
    assert_eq!(candidate.files.len(), 1);
    assert_eq!(candidate.files[0].name, "hello.flac");
    assert_eq!(candidate.files[0].format.as_deref(), Some("flac"));
    assert_eq!(candidate.source.key, "peer-1");
    assert_eq!(candidate.source.speed_bps, Some(1_250_000));

    // The test places the file where the backend would have downloaded it.
    std::fs::write(fixture.downloads.join("hello.flac"), b"audio").unwrap();

    let download = fixture
        .provider
        .start_download(&context, "cand-1", &[0])
        .await
        .unwrap();
    assert_eq!(
        fixture
            .provider
            .poll_download(&context, &download)
            .await
            .unwrap(),
        DownloadState::Queued(Some(1))
    );

    let mut running = Vec::new();
    let mut complete = None;
    for _ in 0..10 {
        match fixture
            .provider
            .poll_download(&context, &download)
            .await
            .unwrap()
        {
            DownloadState::Running(progress) => running.push(progress.bytes),
            other => {
                complete = Some(other);
                break;
            }
        }
    }
    assert_eq!(running, [5_000, 10_000], "progress must grow");
    let complete = complete.unwrap();
    let DownloadState::Complete(files) = complete else {
        panic!("the download should have completed, got {complete:?}");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].root, "downloads");
    assert_eq!(files[0].path, "hello.flac");
}

#[tokio::test]
async fn lists_the_staging_root_of_the_call() {
    let fixture = fixture(&config("staging"), false).await;
    let context = context(&fixture.staging.clone());
    std::fs::write(fixture.staging.join("incoming.flac"), b"audio").unwrap();

    let candidates = complete_search(&fixture, &context).await;
    assert_eq!(candidates.len(), 1);

    let download = fixture
        .provider
        .start_download(&context, "cand-1", &[0])
        .await
        .unwrap();
    let mut state = DownloadState::Queued(None);
    for _ in 0..10 {
        state = fixture
            .provider
            .poll_download(&context, &download)
            .await
            .unwrap();
        if !matches!(state, DownloadState::Queued(_) | DownloadState::Running(_)) {
            break;
        }
    }
    let DownloadState::Complete(files) = state else {
        panic!("the download should have completed, got {state:?}");
    };
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].root, "staging");
    assert_eq!(files[0].path, "incoming.flac");
}

#[tokio::test]
async fn reports_an_empty_search() {
    let fixture = fixture(
        r#"{ "candidates": [], "searchPolls": 0, "scenario": "no-candidates" }"#,
        false,
    )
    .await;
    let context = context(&fixture.staging.clone());

    let token = fixture
        .provider
        .start_search(&context, &query())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .provider
            .poll_search(&context, &token)
            .await
            .unwrap(),
        SearchState::Complete(Vec::new())
    );
}

#[tokio::test]
async fn reports_a_download_failure() {
    let fixture = fixture(
        r#"{ "candidates": [], "downloadQueuedPolls": 0, "downloadRunningPolls": 0, "scenario": "download-failure", "outputRoot": "staging" }"#,
        false,
    )
    .await;
    let context = context(&fixture.staging.clone());

    let download = fixture
        .provider
        .start_download(&context, "cand-1", &[0])
        .await
        .unwrap();
    let state = fixture
        .provider
        .poll_download(&context, &download)
        .await
        .unwrap();
    let DownloadState::Failed(reason) = state else {
        panic!("the download should have failed, got {state:?}");
    };
    assert!(reason.contains("peer"), "got {reason}");
}

#[tokio::test]
async fn reports_a_stalled_download() {
    let fixture = fixture(
        r#"{ "scenario": "stalled", "downloadBytesPerPoll": 0, "outputRoot": "staging" }"#,
        false,
    )
    .await;
    let context = context(&fixture.staging.clone());

    let download = fixture
        .provider
        .start_download(&context, "cand-1", &[0])
        .await
        .unwrap();
    let mut bytes = Vec::new();
    for _ in 0..5 {
        let state = fixture
            .provider
            .poll_download(&context, &download)
            .await
            .unwrap();
        let DownloadState::Running(progress) = state else {
            panic!("a stalled download must keep running, got {state:?}");
        };
        bytes.push(progress.bytes);
    }
    assert_eq!(
        bytes,
        [0, 0, 0, 0, 0],
        "a stalled download makes no progress"
    );
}

#[tokio::test]
async fn reports_an_unknown_token_as_not_found() {
    let fixture = fixture(&config("staging"), false).await;
    let context = context(&fixture.staging.clone());

    assert_eq!(
        fixture.provider.poll_search(&context, "search-999").await,
        Err(ProviderError::NotFound)
    );
    assert_eq!(
        fixture
            .provider
            .poll_download(&context, "download-999")
            .await,
        Err(ProviderError::NotFound)
    );
}

#[tokio::test]
async fn keeps_state_across_calls_through_the_store() {
    let fixture = fixture(&config("staging"), false).await;
    let context = context(&fixture.staging.clone());

    let token = fixture
        .provider
        .start_search(&context, &query())
        .await
        .unwrap();
    assert_eq!(token, "search-1");
    // A second search gets a fresh token, which proves the counter lives in the
    // instance's key-value store rather than in guest memory.
    let second = fixture
        .provider
        .start_search(&context, &query())
        .await
        .unwrap();
    assert_eq!(second, "search-2");

    fixture.provider.cancel_search(&context, &token).await;
    assert_eq!(
        fixture.provider.poll_search(&context, &token).await,
        Err(ProviderError::NotFound)
    );
}
