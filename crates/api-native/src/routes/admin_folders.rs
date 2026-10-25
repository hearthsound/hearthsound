//! `/api/v1/admin/folders`: the scanned library folders.
//!
//! Adding a folder asks the question every pre-existing library raises: whose
//! is it? A request names exactly one of `owner` (the account whose personal
//! library the folder's content joins, pinned as permanent) or
//! `generic: true` (the content is part of the server, in nobody's library,
//! adoptable by anyone). The answer can be changed later; the folder's tracks
//! are re-pinned in the same transaction. Hearthsound only reads a folder: it
//! never writes, moves or deletes anything inside it.

use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use hearthsound_core::events::{LibraryChange, LibraryChanged, ServerEvent};
use hearthsound_core::{EntityKind, IdPrefix};
use hearthsound_library::scan::ScanRequest;
use hearthsound_library::{LibraryFolder, LibraryRepository};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::AdminAuthed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{client_id, parse_id};

/// A scanned library folder.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FolderView {
    /// The folder's client id (`lf_...`).
    pub id: String,
    /// The folder's absolute path on the server.
    pub path: String,
    /// The folder's display name.
    pub name: String,
    /// The account the folder belongs to (`us_...`); absent for a generic
    /// folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Whether the folder is in nobody's personal library.
    pub generic: bool,
    /// Whether changes inside the folder are picked up as they happen.
    pub watch: bool,
    /// When the folder was added, in unix milliseconds.
    pub created_at: i64,
    /// When the folder's last scan finished, in unix milliseconds; a scan that
    /// failed leaves it unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scan_at: Option<i64>,
    /// Why the folder's last scan failed or ended with a warning, when it did.
    /// A complete sentence to render verbatim; `None` after a clean scan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scan_error: Option<String>,
}

impl From<&LibraryFolder> for FolderView {
    fn from(folder: &LibraryFolder) -> FolderView {
        FolderView {
            id: client_id(IdPrefix::LibraryFolder, folder.id),
            path: folder.path.to_string_lossy().into_owned(),
            name: folder.name.clone(),
            owner: folder
                .owner_user_id
                .map(|owner| client_id(IdPrefix::User, owner)),
            generic: folder.owner_user_id.is_none(),
            watch: folder.watch,
            created_at: folder.created_at,
            last_scan_at: folder.last_scan_at,
            last_scan_error: folder.last_scan_error.clone(),
        }
    }
}

/// The scanned library folders.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FolderList {
    /// The folders, by path.
    pub folders: Vec<FolderView>,
}

/// A folder to add.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NewFolder {
    /// The folder's absolute path on the server.
    pub path: String,
    /// The account whose library the folder joins (`us_...`); exactly one of
    /// `owner` and `generic` is required.
    #[serde(default)]
    pub owner: Option<String>,
    /// `true` to keep the folder in nobody's library; exactly one of `owner`
    /// and `generic` is required.
    #[serde(default)]
    pub generic: Option<bool>,
    /// The display name; the directory name when absent.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether to watch the folder for changes; `true` when absent.
    #[serde(default)]
    pub watch: Option<bool>,
}

/// Changes to a folder; an absent field is left as it is.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FolderPatch {
    /// The new display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether to watch the folder for changes.
    #[serde(default)]
    pub watch: Option<bool>,
    /// A new owner (`us_...`); not together with `generic`.
    #[serde(default)]
    pub owner: Option<String>,
    /// `true` to make the folder generic; not together with `owner`.
    #[serde(default)]
    pub generic: Option<bool>,
}

/// What removing a folder would take with it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemovalImpactView {
    /// How many tracks would be deleted from the library.
    pub tracks: i64,
    /// How many track likes would be lost.
    pub likes: i64,
    /// How many playlist entries would be lost.
    pub playlist_entries: i64,
}

impl From<hearthsound_library::FolderRemovalImpact> for RemovalImpactView {
    fn from(impact: hearthsound_library::FolderRemovalImpact) -> RemovalImpactView {
        RemovalImpactView {
            tracks: impact.tracks,
            likes: impact.likes,
            playlist_entries: impact.playlist_entries,
        }
    }
}

/// A scan to start.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScanFolder {
    /// Re-read every file instead of only the changed ones.
    #[serde(default)]
    pub full: bool,
}

/// A queued scan.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ScanQueued {
    /// The queued job's id.
    pub job_id: String,
}

/// Resolves the association a request asks for.
///
/// # Returns
///
/// `Some(Some(owner))` for an owner, `Some(None)` for generic, `None` when the
/// request names neither.
///
/// # Errors
///
/// Answers `invalid-request` when both are named, `generic` is `false`, or the
/// owner is not an existing account.
async fn association(
    state: &NativeState,
    owner: Option<&str>,
    generic: Option<bool>,
) -> Result<Option<Option<Uuid>>, Problem> {
    match (owner, generic) {
        (Some(_), Some(true)) => Err(Problem::invalid_request(
            "Name either an `owner` or `generic: true`, not both.",
        )),
        (None, Some(false)) => Err(Problem::invalid_request(
            "`generic` can only be `true`; name an `owner` instead.",
        )),
        (Some(owner), _) => {
            let id = match parse_id(owner, IdPrefix::User) {
                Ok(id) => id,
                Err(_) => {
                    return Err(Problem::invalid_request(format!(
                        "`{owner}` is not an account id."
                    )));
                }
            };
            let exists = state
                .users()
                .find_by_id(id)
                .await
                .map_err(|error| problem::internal("reading an account", &error))?
                .is_some();
            if !exists {
                return Err(Problem::invalid_request(format!(
                    "`{owner}` names no account."
                )));
            }
            Ok(Some(Some(id)))
        }
        (None, Some(true)) => Ok(Some(None)),
        (None, None) => Ok(None),
    }
}

/// Reads one folder or answers `not-found`.
async fn existing(library: &LibraryRepository, id: Uuid) -> Result<LibraryFolder, Problem> {
    library
        .get_library_folder(id)
        .await
        .map_err(|error| problem::internal("reading a folder", &error))?
        .ok_or_else(|| Problem::not_found("No such folder."))
}

/// Brings the watchers in line with the folders, logging a failure.
async fn reconcile_watchers(state: &NativeState) {
    if let Err(error) = state.services().watcher.reconcile().await {
        tracing::warn!(%error, "could not reconcile the folder watchers");
    }
}

/// Lists the scanned library folders.
///
/// # Errors
///
/// Answers `forbidden` for a caller who is not an administrator.
#[utoipa::path(
    get,
    path = "/api/v1/admin/folders",
    tag = "admin",
    responses(
        (status = 200, description = "The folders; `lastScanAt` and `lastScanError` say how the last scan ended", body = FolderList),
        (status = 403, description = "Not an administrator", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_folders(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<FolderList>, Problem> {
    let mut folders = LibraryRepository::new(state.database())
        .list_library_folders()
        .await
        .map_err(|error| problem::internal("listing folders", &error))?;
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Json(FolderList {
        folders: folders.iter().map(FolderView::from).collect(),
    }))
}

/// Adds a library folder, owned by one account or generic, and scans it.
///
/// # Errors
///
/// Answers `invalid-request` when neither or both of `owner` and `generic` are
/// named, the owner is unknown, the path is not an absolute existing directory,
/// or the path is `/`, is one of the server's own files or directories, contains
/// one, or sits inside one; and `conflict` when the folder is already added.
#[utoipa::path(
    post,
    path = "/api/v1/admin/folders",
    tag = "admin",
    request_body = NewFolder,
    responses(
        (status = 201, description = "The new folder; its first scan is queued, and `lastScanError` says how it ended", body = FolderView),
        (status = 400, description = "No association, an unknown owner, or an unusable path", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The folder is already added", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_folder(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(request): JsonBody<NewFolder>,
) -> Result<(StatusCode, Json<FolderView>), Problem> {
    let owner = association(&state, request.owner.as_deref(), request.generic)
        .await?
        .ok_or_else(|| {
            Problem::invalid_request(
                "Name the folder's `owner`, or set `generic: true` to keep it in nobody's library.",
            )
        })?;
    let path = PathBuf::from(&request.path);
    if !path.is_absolute() {
        return Err(Problem::invalid_request("The path must be absolute."));
    }
    let canonical = canonical_directory(&path)?;
    // A folder that is, contains, or sits inside the server's own files would
    // index the database, the artwork or the plugin files.
    let canonical = hearthsound_library::folders::check_folder_path(&canonical, state.paths())
        .map_err(|rejection| Problem::invalid_request(rejection.to_string()))?;
    let library = LibraryRepository::new(state.database());
    let known = library
        .list_library_folders()
        .await
        .map_err(|error| problem::internal("listing folders", &error))?
        .into_iter()
        .any(|folder| folder.path == canonical);
    if known {
        return Err(Problem::conflict(format!(
            "`{}` is already a library folder.",
            canonical.display()
        )));
    }

    let folder = library
        .add_library_folder(&canonical, owner)
        .await
        .map_err(|error| problem::internal("adding a folder", &error))?;
    library
        .set_library_folder_owner(folder.id, owner)
        .await
        .map_err(|error| problem::internal("associating a folder", &error))?;
    if let Some(name) = request
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        library
            .rename_library_folder(folder.id, name)
            .await
            .map_err(|error| problem::internal("naming a folder", &error))?;
    }
    if request.watch == Some(false) {
        library
            .set_library_folder_watch(folder.id, false)
            .await
            .map_err(|error| problem::internal("changing a folder's watch", &error))?;
    }
    reconcile_watchers(&state).await;
    state
        .services()
        .scanner
        .request_scan(&state.services().jobs, &ScanRequest::folder(folder.id))
        .await
        .map_err(|error| problem::internal("queueing a scan", &error))?;
    tracing::info!(admin = %admin.user_id(), folder = %folder.id, "added a library folder");
    let folder = existing(&library, folder.id).await?;
    Ok((StatusCode::CREATED, Json(FolderView::from(&folder))))
}

/// Resolves a path to the existing directory it names.
///
/// # Errors
///
/// Answers `invalid-request` when the path does not exist or is not a
/// directory.
fn canonical_directory(path: &FsPath) -> Result<PathBuf, Problem> {
    let canonical = path.canonicalize().map_err(|_| {
        Problem::invalid_request(format!(
            "`{}` does not exist on the server.",
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(Problem::invalid_request(format!(
            "`{}` is not a directory.",
            path.display()
        )));
    }
    Ok(canonical)
}

/// Changes a folder's name, watch, or association.
///
/// # Errors
///
/// Answers `not-found` for an unknown folder and `invalid-request` for a
/// contradictory or unknown association.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/folders/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The folder's client id (`lf_...`)")),
    request_body = FolderPatch,
    responses(
        (status = 200, description = "The folder after the change", body = FolderView),
        (status = 400, description = "A contradictory or unknown association", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such folder", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_folder(
    AdminAuthed(admin): AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    JsonBody(patch): JsonBody<FolderPatch>,
) -> Result<Json<FolderView>, Problem> {
    let id = parse_id(&id, IdPrefix::LibraryFolder)?;
    let library = LibraryRepository::new(state.database());
    existing(&library, id).await?;
    let owner = association(&state, patch.owner.as_deref(), patch.generic).await?;
    if let Some(name) = patch.name.as_deref().map(str::trim) {
        if name.is_empty() {
            return Err(Problem::invalid_request("`name` must not be empty."));
        }
        library
            .rename_library_folder(id, name)
            .await
            .map_err(|error| problem::internal("naming a folder", &error))?;
    }
    if let Some(owner) = owner {
        library
            .set_library_folder_owner(id, owner)
            .await
            .map_err(|error| problem::internal("associating a folder", &error))?;
    }
    if let Some(watch) = patch.watch {
        library
            .set_library_folder_watch(id, watch)
            .await
            .map_err(|error| problem::internal("changing a folder's watch", &error))?;
        reconcile_watchers(&state).await;
    }
    tracing::info!(admin = %admin.user_id(), folder = %id, "changed a library folder");
    let folder = existing(&library, id).await?;
    Ok(Json(FolderView::from(&folder)))
}

/// Counts what removing a folder would take with it.
///
/// The counts describe the delete before it happens: the tracks only this folder
/// held, and among them the likes and playlist entries that go with them. The web
/// client asks for them before it sends the delete.
///
/// # Errors
///
/// Answers `not-found` for an unknown folder.
#[utoipa::path(
    get,
    path = "/api/v1/admin/folders/{id}/removal-impact",
    tag = "admin",
    params(("id" = String, Path, description = "The folder's client id (`lf_...`)")),
    responses(
        (status = 200, description = "The counts a removal would take with it", body = RemovalImpactView),
        (status = 404, description = "No such folder", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn folder_removal_impact(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<Json<RemovalImpactView>, Problem> {
    let id = parse_id(&id, IdPrefix::LibraryFolder)?;
    let library = LibraryRepository::new(state.database());
    existing(&library, id).await?;
    let impact = library
        .removal_impact(id)
        .await
        .map_err(|error| problem::internal("previewing a folder removal", &error))?;
    Ok(Json(RemovalImpactView::from(impact)))
}

/// Removes a library folder.
///
/// The folder's file records and the tracks only it held go with it, exactly as
/// a scan's pruning removes a file the folder lost; a track that still has a
/// file in the library or the cache stays. The tracks' likes and playlist entries
/// go with them, and so does their play history. Nothing inside the folder on
/// disk is removed, moved or renamed, and the folder's watcher stops. Preview the
/// loss with `GET /api/v1/admin/folders/{id}/removal-impact` first; the web
/// client does.
///
/// # Errors
///
/// Answers `not-found` for an unknown folder.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/folders/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The folder's client id (`lf_...`)")),
    responses(
        (status = 204, description = "The folder is gone"),
        (status = 404, description = "No such folder", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_folder(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::LibraryFolder)?;
    let library = LibraryRepository::new(state.database());
    existing(&library, id).await?;
    let removed = library
        .remove_library_folder(id)
        .await
        .map_err(|error| problem::internal("removing a folder", &error))?;
    for track in removed {
        state
            .services()
            .events
            .publish(ServerEvent::LibraryChanged(LibraryChanged {
                entity_kind: EntityKind::Track,
                entity_id: track.to_string(),
                change: LibraryChange::Removed,
            }));
    }
    reconcile_watchers(&state).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Queues a scan of one folder.
///
/// # Errors
///
/// Answers `not-found` for an unknown folder.
#[utoipa::path(
    post,
    path = "/api/v1/admin/folders/{id}/scan",
    tag = "admin",
    params(("id" = String, Path, description = "The folder's client id (`lf_...`)")),
    request_body(content = ScanFolder, description = "Whether to re-read every file"),
    responses(
        (status = 202, description = "The scan is queued", body = ScanQueued),
        (status = 404, description = "No such folder", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn scan_folder(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<(StatusCode, Json<ScanQueued>), Problem> {
    let id = parse_id(&id, IdPrefix::LibraryFolder)?;
    existing(&LibraryRepository::new(state.database()), id).await?;
    let mut request = ScanRequest::folder(id);
    let options: ScanFolder = if body.is_empty() {
        ScanFolder::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|error| Problem::invalid_request(error.to_string()))?
    };
    if options.full {
        request = request.full();
    }
    let job = state
        .services()
        .scanner
        .request_scan(&state.services().jobs, &request)
        .await
        .map_err(|error| problem::internal("queueing a scan", &error))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ScanQueued {
            job_id: job.id.to_string(),
        }),
    ))
}
