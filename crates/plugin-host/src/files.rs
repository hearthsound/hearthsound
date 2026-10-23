//! The `host-files` import: the directories an acquisition plugin may reach.
//!
//! A plugin never sees a host path. It names a *root*: either the id of an
//! import root the administrator granted this instance, or the reserved name
//! `staging`, which is the current acquisition job's own directory. Both are
//! resolved host-side, so a plugin cannot widen its reach by naming a path.
//!
//! Every path is checked before it is touched:
//!
//! - absolute paths are rejected, because a root is always a directory;
//! - any `..` component is rejected, because a root is a boundary;
//! - the root and the target are canonicalized, so symlinks are resolved, and
//!   the resolved target must still be inside the resolved root, which rejects a
//!   symlink that points out of the root.
//!
//! Import roots are only granted when the plugin's approved permissions include
//! `files.importRoots`; the staging root is always available to an acquisition
//! call, because it exists for that call alone.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::instance::InstanceHostConfig;
use crate::manifest::FilesPermission;

/// The reserved root name of the current acquisition job's staging directory.
pub const STAGING_ROOT: &str = "staging";

/// One file's metadata, as the plugin sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStat {
    /// The file size in bytes.
    pub size: u64,
    /// The last modification time in unix milliseconds UTC.
    pub modified_ms: u64,
    /// Whether the path is a directory.
    pub is_dir: bool,
}

/// Why a file operation failed.
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// The plugin named a root the instance does not grant.
    #[error("no import root `{0}` is granted to this instance")]
    UnknownRoot(String),
    /// The plugin named a path the policy refuses.
    #[error("the path is refused: {0}")]
    Rejected(String),
    /// The filesystem call failed.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

impl FilesError {
    /// Returns the reason a plugin is told about.
    pub fn reason(&self) -> String {
        self.to_string()
    }
}

/// The import roots one instance is granted.
///
/// The map is empty when the plugin was not approved for import roots, so every
/// non-staging root a plugin names is rejected.
#[derive(Debug, Clone, Default)]
pub struct FileAccess {
    roots: BTreeMap<String, PathBuf>,
}

impl FileAccess {
    /// Builds the grant of one instance.
    ///
    /// # Parameters
    ///
    /// - `host_config`: the instance's host-level settings, which carry the
    ///   import roots the administrator configured.
    /// - `permission`: the instance's approved `files` permission. Import roots
    ///   are granted only when it sets `importRoots` to `true`.
    pub fn new(
        host_config: &InstanceHostConfig,
        permission: Option<&FilesPermission>,
    ) -> FileAccess {
        let granted = permission.and_then(|files| files.import_roots) == Some(true);
        let roots = if granted {
            host_config
                .import_roots
                .iter()
                .map(|root| (root.id.clone(), root.path.clone()))
                .collect()
        } else {
            BTreeMap::new()
        };
        FileAccess { roots }
    }

    /// Returns whether `id` names a granted import root.
    ///
    /// # Parameters
    ///
    /// - `id`: the root id a plugin named.
    pub fn grants(&self, id: &str) -> bool {
        self.roots.contains_key(id)
    }

    /// Returns the directory of a granted import root.
    ///
    /// # Parameters
    ///
    /// - `id`: the root id a plugin named.
    pub fn root(&self, id: &str) -> Option<&Path> {
        self.roots.get(id).map(PathBuf::as_path)
    }
}

/// Returns the metadata of `path` inside `root`.
///
/// # Parameters
///
/// - `root`: the resolved directory of the root.
/// - `path`: the plugin's path, relative to the root.
///
/// # Returns
///
/// `None` when nothing exists at the path, otherwise the metadata. A path the
/// policy refuses is an error.
///
/// # Errors
///
/// Returns [`FilesError::Rejected`] for an absolute path, a `..` component, or a
/// symlink escaping the root, and [`FilesError::Io`] for a failed filesystem
/// call.
pub async fn stat(root: &Path, path: &str) -> Result<Option<FileStat>, FilesError> {
    let Some(target) = resolve(root, path).await? else {
        return Ok(None);
    };
    let metadata = tokio::fs::metadata(&target).await?;
    Ok(Some(FileStat {
        size: metadata.len(),
        modified_ms: modified_ms(&metadata),
        is_dir: metadata.is_dir(),
    }))
}

/// Lists the names directly inside `path` within `root`, sorted.
///
/// # Parameters
///
/// - `root`: the resolved directory of the root.
/// - `path`: the plugin's path, relative to the root.
///
/// # Returns
///
/// The entry names, without their parent path, in ascending order.
///
/// # Errors
///
/// Returns [`FilesError::Rejected`] for a refused path or a path that is not a
/// directory, and [`FilesError::Io`] for a failed filesystem call.
pub async fn list_dir(root: &Path, path: &str) -> Result<Vec<String>, FilesError> {
    let Some(target) = resolve(root, path).await? else {
        return Err(FilesError::Rejected(format!("`{path}` does not exist")));
    };
    let metadata = tokio::fs::metadata(&target).await?;
    if !metadata.is_dir() {
        return Err(FilesError::Rejected(format!("`{path}` is not a directory")));
    }

    let mut entries = Vec::new();
    let mut reader = tokio::fs::read_dir(&target).await?;
    while let Some(entry) = reader.next_entry().await? {
        entries.push(entry.file_name().to_string_lossy().into_owned());
    }
    entries.sort();
    Ok(entries)
}

/// Resolves a plugin path against a root, refusing anything outside the root.
///
/// # Parameters
///
/// - `root`: the root directory.
/// - `path`: the plugin's path, relative to the root.
///
/// # Returns
///
/// The canonical path, or `None` when the root or the target does not exist.
///
/// # Errors
///
/// Returns [`FilesError::Rejected`] when the path is absolute, carries a `..`
/// component, or resolves through a symlink to a location outside the root, and
/// [`FilesError::Io`] when canonicalization fails for another reason.
async fn resolve(root: &Path, path: &str) -> Result<Option<PathBuf>, FilesError> {
    let relative = Path::new(path);
    if relative.is_absolute() {
        return Err(FilesError::Rejected(format!("`{path}` is absolute")));
    }
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(FilesError::Rejected(format!(
            "`{path}` contains a component that leaves its root"
        )));
    }

    let base = match tokio::fs::canonicalize(root).await {
        Ok(base) => base,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FilesError::Io(error)),
    };
    let target = match tokio::fs::canonicalize(base.join(relative)).await {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FilesError::Io(error)),
    };
    if !target.starts_with(&base) {
        return Err(FilesError::Rejected(format!(
            "`{path}` resolves to a symlink outside its root"
        )));
    }
    Ok(Some(target))
}

/// Returns a file's modification time in unix milliseconds UTC.
///
/// A filesystem that reports no modification time, or a time before the epoch,
/// reports zero.
fn modified_ms(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::ImportMode;

    use crate::instance::ImportRoot;

    /// Builds a root directory holding a file, a subdirectory, and a symlink.
    pub(super) fn root_dir(dir: &Path) -> PathBuf {
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("song.flac"), b"bytes").unwrap();
        std::fs::write(root.join("sub/nested.mp3"), b"nested").unwrap();
        root
    }

    pub(super) fn access(import_root: &Path, granted: bool) -> FileAccess {
        let host_config = InstanceHostConfig {
            import_roots: vec![ImportRoot {
                id: "downloads".to_owned(),
                path: import_root.to_path_buf(),
                mode: ImportMode::Move,
                read_only: false,
            }],
            ..InstanceHostConfig::default()
        };
        let permission = FilesPermission {
            import_roots: Some(granted),
        };
        FileAccess::new(&host_config, Some(&permission))
    }

    #[tokio::test]
    async fn stats_and_lists_inside_a_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let access = access(&root, true);
        let base = access.root("downloads").unwrap();

        let file = stat(base, "song.flac").await.unwrap().unwrap();
        assert_eq!(file.size, 5);
        assert!(!file.is_dir);
        assert!(file.modified_ms > 0);

        let folder = stat(base, "sub").await.unwrap().unwrap();
        assert!(folder.is_dir);

        assert_eq!(list_dir(base, "").await.unwrap(), ["song.flac", "sub"]);
        assert_eq!(list_dir(base, "sub").await.unwrap(), ["nested.mp3"]);
        assert!(stat(base, "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn refuses_paths_that_leave_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let base = access(&root, true).root("downloads").unwrap().to_path_buf();

        assert!(matches!(
            stat(&base, "/etc/passwd").await,
            Err(FilesError::Rejected(_))
        ));
        assert!(matches!(
            stat(&base, "../outside").await,
            Err(FilesError::Rejected(_))
        ));
        assert!(matches!(
            stat(&base, "sub/../../outside").await,
            Err(FilesError::Rejected(_))
        ));
        assert!(matches!(
            list_dir(&base, "..").await,
            Err(FilesError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn refuses_a_symlink_that_escapes_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        std::fs::write(dir.path().join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), root.join("escape")).unwrap();
        std::os::unix::fs::symlink(root.join("song.flac"), root.join("inside")).unwrap();

        let base = access(&root, true).root("downloads").unwrap().to_path_buf();
        assert!(matches!(
            stat(&base, "escape").await,
            Err(FilesError::Rejected(_))
        ));
        // A symlink that stays inside the root is followed.
        let inside = stat(&base, "inside").await.unwrap().unwrap();
        assert_eq!(inside.size, 5);
    }

    #[tokio::test]
    async fn grants_no_import_root_without_the_permission() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let access = access(&root, false);
        assert!(!access.grants("downloads"));
        assert!(access.root("downloads").is_none());
    }

    #[tokio::test]
    async fn refuses_listing_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let base = access(&root, true).root("downloads").unwrap().to_path_buf();
        assert!(matches!(
            list_dir(&base, "song.flac").await,
            Err(FilesError::Rejected(_))
        ));
    }
}

/// Tests that drive the `host-files` import through the instance state, so root
/// resolution and the staging root are exercised through the real import.
#[cfg(test)]
mod import_tests {
    use std::sync::Arc;

    use hearthsound_core::Slug;

    use crate::bindings::hearthsound::plugin::host_files;
    use crate::http::{HttpClient, HttpGrant};
    use crate::kv::KvStore;
    use crate::log::LogSink;

    use super::tests::{access, root_dir};
    use super::*;

    /// Builds the state of an instance whose `files` grant is `files`.
    fn state(files: FileAccess, staging: Option<PathBuf>) -> crate::state::InstanceState {
        let slug = Slug::new("probe").unwrap();
        let http = Arc::new(
            HttpClient::new(
                "com.example.probe".to_owned(),
                "0.1.0",
                slug.clone(),
                HttpGrant::new(None, "{}"),
            )
            .unwrap(),
        );
        let kv = Arc::new(KvStore::new(
            sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap(),
            KvStore::new_instance_id(),
            crate::kv::DEFAULT_QUOTA_BYTES,
        ));
        let log = Arc::new(LogSink::new("com.example.probe".to_owned(), slug.clone()));
        let mut state = crate::state::InstanceState::new(
            slug.clone(),
            Arc::from("{}"),
            log,
            64,
            crate::state::InstanceServices {
                http,
                kv,
                files: Arc::new(files),
                process: Arc::new(crate::process::ProcessTable::without_tools(
                    "com.example.probe",
                    &slug,
                )),
            },
        );
        state.set_staging(staging);
        state
    }

    #[tokio::test]
    async fn serves_a_granted_import_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let mut state = state(access(&root, true), None);

        let stat =
            host_files::Host::stat(&mut state, "downloads".to_owned(), "song.flac".to_owned())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(stat.size, 5);
        assert!(!stat.is_dir);

        let entries = host_files::Host::list_dir(&mut state, "downloads".to_owned(), String::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entries, ["song.flac", "sub"]);
    }

    #[tokio::test]
    async fn refuses_an_ungranted_root_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let mut state = state(access(&root, false), None);

        assert!(
            host_files::Host::stat(&mut state, "downloads".to_owned(), String::new())
                .await
                .unwrap()
                .is_none()
        );
        let error = host_files::Host::list_dir(&mut state, "downloads".to_owned(), String::new())
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("granted"), "got {error}");
    }

    #[tokio::test]
    async fn resolves_the_staging_root_for_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("staging/ac_1");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("incoming.part"), b"partial").unwrap();

        let mut state = state(FileAccess::default(), Some(staging));
        let stat = host_files::Host::stat(
            &mut state,
            STAGING_ROOT.to_owned(),
            "incoming.part".to_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stat.size, 7);

        // The staging root belongs to one call: without one, the name resolves
        // to nothing.
        state.set_staging(None);
        assert!(
            host_files::Host::stat(&mut state, STAGING_ROOT.to_owned(), String::new())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn refuses_escaping_paths_through_the_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path());
        let mut state = state(access(&root, true), None);

        assert!(
            host_files::Host::stat(&mut state, "downloads".to_owned(), "/etc/passwd".to_owned())
                .await
                .unwrap()
                .is_none()
        );
        let error =
            host_files::Host::list_dir(&mut state, "downloads".to_owned(), "../../etc".to_owned())
                .await
                .unwrap()
                .unwrap_err();
        assert!(error.contains("refused"), "got {error}");
    }
}
