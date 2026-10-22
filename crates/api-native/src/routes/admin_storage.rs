//! `/api/v1/admin/storage`: what the managed roots hold, and a forced pass.
//!
//! The report counts the files under `<storage>/library` and `<storage>/cache`,
//! the files inside scanned folders, the pinned tracks and the free space of
//! the filesystem under the cache root, next to the policy that decides what
//! is evicted and demoted. The forced pass runs the same maintenance as the
//! background worker, once, and answers what it changed. Neither endpoint
//! moves or removes a file itself: the pass is the only writer.

use axum::Json;
use axum::extract::State;
use hearthsound_config::{ConfigError, SettingsStore, keys};
use hearthsound_library::maintenance::{
    AutopinReport, ClassUsage, EvictionReport, FilesystemSpace, MaintenanceReport,
    RelocationReport, StorageUsage,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::AdminAuthed;
use crate::problem::{self, Problem};
use crate::state::NativeState;

/// The files and bytes of one storage class.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassView {
    /// How many files the class holds.
    pub files: u64,
    /// How many bytes those files hold.
    pub bytes: i64,
}

/// The size and free space of the filesystem under the cache root.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemView {
    /// The filesystem's total size in bytes.
    pub total_bytes: i64,
    /// The bytes an unprivileged process may still write.
    pub available_bytes: i64,
}

/// The settings that decide what storage keeps, evicts and demotes.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StoragePolicy {
    /// Upper bound on the cache directory, in bytes; 0 means no bound.
    pub cache_max_bytes: i64,
    /// Free-space floor below which eviction runs, in bytes.
    pub cache_min_free_bytes: i64,
    /// Eviction order: `lru`, `lfu` or `fifo`.
    pub eviction: String,
    /// Age in days above which a cached file is evicted; 0 disables it.
    pub max_age_days: i64,
    /// What happens to a file when its last pin is removed: `demote` or
    /// `delete`.
    pub unpin_action: String,
    /// Play count that pins a track automatically; 0 disables it.
    pub autopin_play_count: i64,
}

/// What the managed roots and the scanned folders hold, with the policy.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StorageReport {
    /// The managed permanent files under `<storage>/library`.
    pub library: ClassView,
    /// The managed evictable files under `<storage>/cache`.
    pub cache: ClassView,
    /// The files inside scanned folders.
    pub folders: ClassView,
    /// How many tracks carry at least one pin reason.
    pub pinned_tracks: u64,
    /// The space of every distinct filesystem the server reads or writes on:
    /// the storage root's and each scanned folder's, one entry per device. The
    /// view adds their totals and free space, because a music library usually
    /// lives on a larger disk than the server's own storage.
    pub filesystems: Vec<FilesystemView>,
    /// The settings that decide what is kept and evicted.
    pub policy: StoragePolicy,
}

/// The play-count pins one maintenance pass changed.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutopinView {
    /// Tracks that gained the automatic pin.
    pub pinned: u64,
    /// Tracks that lost the automatic pin.
    pub released: u64,
}

/// The files one maintenance pass moved or removed.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RelocationView {
    /// Files moved from the cache to the library because their track is pinned.
    pub promoted: u64,
    /// Files moved from the library to the cache after their last pin.
    pub demoted: u64,
    /// Files removed from the library after their last pin, in `delete` mode.
    pub deleted: u64,
}

/// The cache files one maintenance pass evicted.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvictionView {
    /// How many cache files were removed.
    pub files: u64,
    /// How many bytes those files held.
    pub bytes: i64,
}

/// What one forced maintenance pass changed.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceView {
    /// The play-count pins that changed.
    pub autopin: AutopinView,
    /// The files that moved between the roots or were deleted.
    pub relocation: RelocationView,
    /// The cache files that were evicted.
    pub eviction: EvictionView,
}

impl From<ClassUsage> for ClassView {
    fn from(usage: ClassUsage) -> ClassView {
        ClassView {
            files: usage.files,
            bytes: usage.bytes,
        }
    }
}

impl From<FilesystemSpace> for FilesystemView {
    fn from(space: FilesystemSpace) -> FilesystemView {
        FilesystemView {
            total_bytes: space.total_bytes,
            available_bytes: space.available_bytes,
        }
    }
}

impl From<AutopinReport> for AutopinView {
    fn from(report: AutopinReport) -> AutopinView {
        AutopinView {
            pinned: report.pinned,
            released: report.released,
        }
    }
}

impl From<RelocationReport> for RelocationView {
    fn from(report: RelocationReport) -> RelocationView {
        RelocationView {
            promoted: report.promoted,
            demoted: report.demoted,
            deleted: report.deleted,
        }
    }
}

impl From<EvictionReport> for EvictionView {
    fn from(report: EvictionReport) -> EvictionView {
        EvictionView {
            files: report.files,
            bytes: report.bytes,
        }
    }
}

impl From<MaintenanceReport> for MaintenanceView {
    fn from(report: MaintenanceReport) -> MaintenanceView {
        MaintenanceView {
            autopin: report.autopin.into(),
            relocation: report.relocation.into(),
            eviction: report.eviction.into(),
        }
    }
}

/// Reads the storage policy from one settings snapshot.
///
/// # Parameters
///
/// - `settings`: the snapshot to read the storage keys from.
///
/// # Errors
///
/// Returns the [`ConfigError`] of the first key that cannot be read.
fn storage_policy(settings: &SettingsStore) -> Result<StoragePolicy, ConfigError> {
    Ok(StoragePolicy {
        cache_max_bytes: settings.get(keys::STORAGE_CACHE_MAX_BYTES)?,
        cache_min_free_bytes: settings.get(keys::STORAGE_CACHE_MIN_FREE_BYTES)?,
        eviction: settings.get(keys::STORAGE_CACHE_EVICTION)?,
        max_age_days: settings.get(keys::STORAGE_CACHE_MAX_AGE_DAYS)?,
        unpin_action: settings.get(keys::STORAGE_UNPIN_ACTION)?,
        autopin_play_count: settings.get(keys::STORAGE_AUTOPIN_PLAY_COUNT)?,
    })
}

/// Reports what the managed roots and the scanned folders hold.
///
/// # Errors
///
/// Answers `internal` when the usage cannot be counted or a policy setting
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/storage",
    tag = "admin",
    responses(
        (status = 200, description = "The storage usage and policy", body = StorageReport),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn get_storage(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<StorageReport>, Problem> {
    let usage: StorageUsage = state
        .services()
        .maintainer
        .usage()
        .await
        .map_err(|error| problem::internal("counting storage usage", &error))?;
    let policy = storage_policy(&state.settings().current())
        .map_err(|error| problem::internal("reading the storage policy", &error))?;
    Ok(Json(StorageReport {
        library: usage.library.into(),
        cache: usage.cache.into(),
        folders: usage.folders.into(),
        pinned_tracks: usage.pinned_tracks,
        filesystems: usage
            .filesystems
            .into_iter()
            .map(FilesystemView::from)
            .collect(),
        policy,
    }))
}

/// Runs one maintenance pass now: autopin, relocation, then eviction.
///
/// The pass is the same one the background worker runs, so it waits for a
/// pass already in progress and then reports what this one changed.
///
/// # Errors
///
/// Answers `internal` when a step of the pass fails; the steps after the
/// failing one do not run.
#[utoipa::path(
    post,
    path = "/api/v1/admin/storage/evict",
    tag = "admin",
    responses(
        (status = 200, description = "The pass ran; what it changed", body = MaintenanceView),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn evict_storage(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<MaintenanceView>, Problem> {
    let report = state
        .services()
        .maintainer
        .run_once()
        .await
        .map_err(|error| problem::internal("running a storage maintenance pass", &error))?;
    Ok(Json(report.into()))
}
