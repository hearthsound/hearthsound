//! `/api/v1/admin/plugins`: the plugin lifecycle over the native API.
//!
//! An administrator lists the installed plugins, installs one from the official
//! index, from a repository an administrator registered or from a repository URL
//! that is registered on the way in, uploads a `.hsplugin` file when sideloading
//! is allowed, and steers the lifecycle: approve the permissions a fresh plugin
//! requests or apply its staged upgrade, switch an approved plugin on or off,
//! discard a staged upgrade, and remove a plugin with its rows and files.
//!
//! Every write goes through [`PluginAdmin`] or [`PluginInstaller`], which record
//! the change and publish a `plugin.status` event; the handler then rebuilds the
//! provider registry so the change is in service before the response is sent.
//! The views report what the last completed update check found, which the
//! update-check job holds in memory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Json;
use axum::extract::multipart::{Field, Multipart};
use axum::extract::{FromRequest, Path as UrlPath, Request, State};
use axum::http::StatusCode;
use hearthsound_config::keys;
use hearthsound_core::{IdPrefix, new_uuid_v7};
use hearthsound_plugin_host::{
    AdminError, AvailablePlugin, AvailableReport, IndexError, InstallError, Manifest,
    PluginRepository, PluginRow, PluginSource, PluginUpdate, RepositoryRef, RequestedPermission,
    Unavailable, default_plugin_ids, requested_permissions,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use utoipa::{IntoParams, ToSchema};

use crate::auth::AdminAuthed;
use crate::extract::{JsonBody, QueryParams};
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{client_id, parse_id};

/// The prefix of the temporary files an uploaded plugin is staged in.
const UPLOAD_PREFIX: &str = ".upload-";

/// One permission a plugin asks for, as shown to the approving administrator.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionView {
    /// The manifest field the permission comes from, such as `http.hosts`.
    pub scope: String,
    /// What is requested, in a form an administrator can read.
    pub detail: String,
    /// How much thought the grant deserves: `low`, `medium` or `high`.
    pub risk: String,
}

impl From<&RequestedPermission> for PermissionView {
    /// Describes one requested permission.
    fn from(permission: &RequestedPermission) -> PermissionView {
        PermissionView {
            scope: permission.scope.to_owned(),
            detail: permission.detail.clone(),
            risk: permission.risk.as_str().to_owned(),
        }
    }
}

/// The newer version a plugin has waiting, when a check found one.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateView {
    /// The version the plugin's index publishes.
    pub version: String,
    /// Whether installing it waits for an approval, which is the case when it
    /// requests a permission the installed version does not hold and is already
    /// staged.
    pub requires_approval: bool,
}

/// The configuration form a plugin's manifest declares.
///
/// The two documents are handed to the client as they are: the JSON Schema
/// describes the values the plugin accepts, and the UI schema says how the form
/// is laid out. A plugin whose manifest declares no configuration has no view.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSectionView {
    /// The draft-07 JSON Schema of the configuration object.
    #[schema(value_type = Object)]
    pub schema: serde_json::Value,
    /// The UI schema that describes the form.
    #[schema(value_type = Object)]
    pub ui_schema: serde_json::Value,
}

/// One installed plugin, as an administrator sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    /// The plugin id from the manifest.
    pub id: String,
    /// The plugin's own name from its manifest.
    pub name: String,
    /// The configuration form the manifest declares for the server scope, or
    /// `null` when the plugin is configured by its host instead.
    pub config: Option<ConfigSectionView>,
    /// Whether this is one of the plugins the server ships with, which the
    /// first-run wizard preselects.
    pub is_default: bool,
    /// The installed version.
    pub version: String,
    /// The plugin API version the component targets.
    pub api: String,
    /// The layers the plugin provides.
    pub provides: Vec<String>,
    /// Where the plugin came from: `official`, `repository`, `sideload` or
    /// `bundled`.
    pub source: String,
    /// The lifecycle state: `pending-approval`, `disabled`, `enabled` or
    /// `upgrade-pending-approval`.
    pub state: String,
    /// Whether the plugin is active. A plugin pending approval is never active.
    pub enabled: bool,
    /// The repository the plugin came from, `pr_...`, when it came from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// The permissions the plugin's manifest requests, with their risk labels.
    pub requested_permissions: Vec<PermissionView>,
    /// Whether an administrator approved the plugin's permissions.
    pub approved: bool,
    /// The newer version the last update check found, or `null`.
    pub update: Option<UpdateView>,
    /// When the plugin was installed, in unix milliseconds.
    pub installed_at: i64,
}

/// Every installed plugin.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PluginList {
    /// The installed plugins, in the order they were installed.
    pub plugins: Vec<PluginView>,
}

/// One plugin an index offers.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailablePluginView {
    /// The plugin id from the manifest.
    pub id: String,
    /// The display name.
    pub name: String,
    /// The one-line description.
    pub description: String,
    /// The layers the plugin provides.
    pub provides: Vec<String>,
    /// Whether this is one of the plugins the server ships with, which the
    /// first-run wizard preselects.
    pub is_default: bool,
    /// The project's home page.
    pub homepage: String,
    /// The license the plugin is distributed under.
    pub license: String,
    /// Whether the index says the project itself publishes the plugin.
    pub official: bool,
    /// Whether the index's signature was verified.
    pub verified: bool,
    /// The repository the plugin comes from, `pr_...`, when it is not the
    /// official index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_id: Option<String>,
    /// The newest version this server can install, when the index offers one.
    pub version: Option<String>,
    /// Why the plugin cannot be installed here, when it cannot.
    pub unavailable: Option<UnavailableView>,
}

/// Why a plugin an index lists cannot be installed here.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnavailableView {
    /// A stable code: `no-compatible-version` or `no-such-version`.
    pub code: String,
    /// Why the plugin is unavailable, in words.
    pub detail: String,
}

/// One index a listing could not read.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailableErrorView {
    /// The URL of the index that could not be read.
    pub source: String,
    /// Why it could not be read.
    pub detail: String,
}

/// Everything an administrator can install right now.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailableView {
    /// The plugins the reachable indexes offer.
    pub plugins: Vec<AvailablePluginView>,
    /// The indexes that could not be read.
    pub errors: Vec<AvailableErrorView>,
}

/// What an install asks for.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallRequest {
    /// `index` to install from an index, `url` for a third-party index URL that
    /// is registered on the way in.
    pub source: String,
    /// The plugin id from the plugin's manifest.
    pub plugin_id: String,
    /// The version to install; the newest compatible one when omitted.
    #[serde(default)]
    pub version: Option<String>,
    /// The registered repository to install from, `pr_...`; the official index
    /// when omitted.
    #[serde(default)]
    pub repository_id: Option<String>,
    /// The URL of a repository's index document, required when `source` is
    /// `url`.
    #[serde(default)]
    pub url: Option<String>,
}

/// The source a listing is limited to.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub struct AvailableQuery {
    /// The repository to list, `pr_...`; every source when omitted.
    pub repository_id: Option<String>,
}

/// Lists the installed plugins.
///
/// Each plugin reports its state, the permissions its manifest requests with
/// their risk labels, whether an administrator approved them, and the newer
/// version the last completed update check found.
///
/// # Errors
///
/// Answers `internal` when the plugin rows or a stored manifest cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins",
    tag = "admin",
    responses(
        (status = 200, description = "The installed plugins", body = PluginList),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_plugins(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<PluginList>, Problem> {
    let repository = state.services().plugin_repository.clone();
    let rows = repository
        .plugins()
        .await
        .map_err(|error| problem::internal("listing the installed plugins", &error))?;
    let mut plugins = Vec::with_capacity(rows.len());
    for row in &rows {
        plugins.push(plugin_view(&state, row, &repository).await?);
    }
    Ok(Json(PluginList { plugins }))
}

/// Installs a plugin from an index or a repository URL.
///
/// The plugin is recorded as pending approval, so an administrator still grants
/// the permissions it requests. An install with `source` `index` reads the
/// official index, or the repository `repositoryId` names; an install with
/// `source` `url` registers the repository at `url` when it is new, which
/// `plugins.allow_third_party_repositories` gates.
///
/// # Errors
///
/// Answers `invalid-request` for a malformed body, `not-found` for an unknown
/// repository or plugin id, `conflict` when the version is already installed or
/// older than the installed one, `repositories-disabled` when a new third-party
/// repository may not be added, `signature-invalid` when an index's signature
/// does not verify, `integrity-failed` when an archive is not what the index
/// declares, `index-unavailable` when an index or archive cannot be
/// downloaded, `invalid-plugin` when an archive is not a plugin this server can
/// host, and `internal` when the tables cannot be read or written.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins",
    tag = "admin",
    request_body = InstallRequest,
    responses(
        (status = 201, description = "The plugin was installed and waits for approval", body = PluginView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Third-party repositories are switched off", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such repository or plugin", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The version is already installed or older", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The archive is not a plugin this server hosts", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The index or the archive could not be used", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn install_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(body): JsonBody<InstallRequest>,
) -> Result<(StatusCode, Json<PluginView>), Problem> {
    let installer = Arc::clone(&state.services().installer);
    let registration = match body.source.as_str() {
        "index" => {
            let repository = body
                .repository_id
                .as_deref()
                .map(|id| parse_id(id, IdPrefix::PluginRepository).map(RepositoryRef::Id))
                .transpose()?;
            installer
                .install_from_index(&body.plugin_id, body.version.as_deref(), repository)
                .await
        }
        "url" => {
            let Some(url) = body.url.as_deref() else {
                return Err(Problem::invalid_request(
                    "An install with `source` `url` needs the repository's `url`.",
                ));
            };
            installer
                .install_from_url(&body.plugin_id, url, body.version.as_deref())
                .await
        }
        other => {
            return Err(Problem::invalid_request(format!(
                "`source` must be `index` or `url`, not `{other}`."
            )));
        }
    }
    .map_err(|error| install_problem(&error))?;
    rebuild(&state).await?;
    let Json(view) = plugin_view_by_id(&state, &registration.id).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Installs a plugin from an uploaded `.hsplugin` file.
///
/// The request is `multipart/form-data` with one `file` part. The file is staged
/// in the plugins directory, installed as a sideloaded plugin and recorded as
/// pending approval, and the staged file is removed whichever way the install
/// ends. `plugins.allow_sideload` gates the whole route.
///
/// # Errors
///
/// Answers `sideload-disabled` when uploading a plugin is switched off,
/// `invalid-request` when the body is not multipart, has no `file` part or is
/// malformed, `conflict` when the version is already installed or older,
/// `invalid-plugin` when the file is not a plugin this server can host, and
/// `internal` when the file cannot be staged or the tables cannot be written.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/upload",
    tag = "admin",
    request_body(content_type = "multipart/form-data", description = "One `file` part holding a `.hsplugin` archive"),
    responses(
        (status = 201, description = "The plugin was installed and waits for approval", body = PluginView),
        (status = 400, description = "The body is not multipart, has no `file` part, or is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Sideloading is switched off", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The version is already installed or older", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The file is not a plugin this server hosts", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn upload_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    request: Request,
) -> Result<(StatusCode, Json<PluginView>), Problem> {
    let allowed = state
        .settings()
        .current()
        .get(keys::PLUGINS_ALLOW_SIDELOAD)
        .map_err(|error| problem::internal("reading plugins.allow_sideload", &error))?;
    if !allowed {
        return Err(Problem::sideload_disabled(
            "Uploading a plugin is turned off on this server.",
        ));
    }

    let mut multipart = Multipart::from_request(request, &state)
        .await
        .map_err(|rejection| Problem::invalid_request(rejection.body_text()))?;
    let mut staged = None;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| Problem::invalid_request(error.body_text()))?
    {
        if field.name() != Some("file") {
            continue;
        }
        staged = Some(stage_upload(&state, &mut field).await?);
        break;
    }
    let Some(path) = staged else {
        return Err(Problem::invalid_request(
            "Send the plugin as a multipart `file` part.",
        ));
    };
    let outcome = state
        .services()
        .admin
        .install_archive(&path, None, PluginSource::Sideload, None, false)
        .await
        .map_err(|error| install_problem(&error));
    remove_staged(&path).await;
    let registration = outcome?;
    rebuild(&state).await?;
    let Json(view) = plugin_view_by_id(&state, &registration.id).await?;
    Ok((StatusCode::CREATED, Json(view)))
}

/// Reports one installed plugin.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `internal` when the row or its
/// stored manifest cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 200, description = "The installed plugin", body = PluginView),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn get_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<PluginView>, Problem> {
    plugin_view_by_id(&state, &id).await
}

/// Approves a plugin's requested permissions, or applies its staged upgrade.
///
/// A plugin waiting for its first approval is enabled once approved; a plugin
/// with a staged upgrade switches to the new version and the files of the old
/// one are removed.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `conflict` when the plugin has
/// neither an approval pending nor a staged upgrade.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/{id}/approve",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 200, description = "The plugin was approved or its upgrade applied", body = PluginView),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The plugin has nothing to approve", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn approve_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<PluginView>, Problem> {
    let row = state
        .services()
        .admin
        .approve(&id)
        .await
        .map_err(|error| admin_problem(&error))?;
    rebuild(&state).await?;
    let repository = state.services().plugin_repository.clone();
    Ok(Json(plugin_view(&state, &row, &repository).await?))
}

/// Switches an approved plugin on.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `conflict` while the plugin
/// awaits approval, which gates enabling.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/{id}/enable",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 200, description = "The plugin is enabled", body = PluginView),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The plugin is pending approval", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn enable_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<PluginView>, Problem> {
    set_enabled(&state, &id, true).await
}

/// Switches an approved plugin off.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `conflict` while the plugin
/// awaits approval.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/{id}/disable",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 200, description = "The plugin is disabled", body = PluginView),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The plugin is pending approval", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn disable_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<PluginView>, Problem> {
    set_enabled(&state, &id, false).await
}

/// Discards a staged upgrade and keeps the installed version in service.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `conflict` when no upgrade is
/// staged.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugins/{id}/decline-upgrade",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 200, description = "The staged upgrade was discarded", body = PluginView),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "No upgrade is staged", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn decline_upgrade(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<PluginView>, Problem> {
    let row = state
        .services()
        .admin
        .decline_upgrade(&id)
        .await
        .map_err(|error| admin_problem(&error))?;
    rebuild(&state).await?;
    let repository = state.services().plugin_repository.clone();
    Ok(Json(plugin_view(&state, &row, &repository).await?))
}

/// Removes a plugin with its rows, assignments and files.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `internal` when its files
/// cannot be removed.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/plugins/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The plugin id from its manifest")),
    responses(
        (status = 204, description = "The plugin was removed"),
        (status = 404, description = "No such plugin is installed", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_plugin(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, Problem> {
    state
        .services()
        .admin
        .remove(&id)
        .await
        .map_err(|error| admin_problem(&error))?;
    rebuild(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists every plugin the reachable indexes offer.
///
/// A source that cannot be read is reported in `errors` and does not stop the
/// listing, so one unreachable repository cannot hide the others.
///
/// # Errors
///
/// Answers `invalid-request` for a malformed repository id, `not-found` for a
/// repository id that names no row, and `internal` when the repository rows
/// cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/plugins/available",
    tag = "admin",
    params(AvailableQuery),
    responses(
        (status = 200, description = "The installable plugins and the sources that failed", body = AvailableView),
        (status = 404, description = "No such repository", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_available(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    QueryParams(query): QueryParams<AvailableQuery>,
) -> Result<Json<AvailableView>, Problem> {
    let repository = query
        .repository_id
        .as_deref()
        .map(|id| parse_id(id, IdPrefix::PluginRepository).map(RepositoryRef::Id))
        .transpose()?;
    let report = state
        .services()
        .installer
        .available(repository)
        .await
        .map_err(|error| install_problem(&error))?;
    Ok(Json(available_view(report)))
}

/// Switches one plugin on or off and reports it.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `id`: the plugin id.
/// - `enabled`: whether to switch the plugin on.
///
/// # Returns
///
/// The plugin view after the change.
///
/// # Errors
///
/// Returns what the enable and disable handlers document.
async fn set_enabled(
    state: &NativeState,
    id: &str,
    enabled: bool,
) -> Result<Json<PluginView>, Problem> {
    state
        .services()
        .admin
        .set_enabled(id, enabled)
        .await
        .map_err(|error| admin_problem(&error))?;
    rebuild(state).await?;
    plugin_view_by_id(state, id).await
}

/// Rebuilds the provider registry so a change is in service.
///
/// A skipped instance is a warning rather than a failure; only a table that
/// cannot be read fails the rebuild.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Errors
///
/// Answers `internal` when the plugin tables cannot be read.
async fn rebuild(state: &NativeState) -> Result<(), Problem> {
    state
        .services()
        .registry
        .rebuild()
        .await
        .map_err(|error| problem::internal("rebuilding the provider registry", &error))?;
    Ok(())
}

/// Reports one installed plugin.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `id`: the plugin id.
///
/// # Returns
///
/// The plugin view.
///
/// # Errors
///
/// Answers `not-found` for an unknown plugin and `internal` when the row or its
/// stored manifest cannot be read.
async fn plugin_view_by_id(state: &NativeState, id: &str) -> Result<Json<PluginView>, Problem> {
    let repository = state.services().plugin_repository.clone();
    let row = repository
        .plugin(id)
        .await
        .map_err(|error| problem::internal("reading a plugin row", &error))?
        .ok_or_else(|| Problem::not_found("No such plugin is installed."))?;
    Ok(Json(plugin_view(state, &row, &repository).await?))
}

/// Builds the view of one installed plugin.
///
/// # Parameters
///
/// - `state`: the shared services, for the settings the manifest is parsed with
///   and the update check's last report.
/// - `row`: the installed plugin row.
/// - `repository`: the plugin tables, for a staged upgrade.
///
/// # Returns
///
/// The view.
///
/// # Errors
///
/// Answers `internal` when the stored manifest or a staged upgrade cannot be
/// read.
async fn plugin_view(
    state: &NativeState,
    row: &PluginRow,
    repository: &PluginRepository,
) -> Result<PluginView, Problem> {
    let settings = state.settings().current();
    let manifest = Manifest::parse(row.manifest.as_bytes(), &settings)
        .map_err(|error| problem::internal("parsing an installed plugin's manifest", &error))?;
    let staged = repository
        .upgrade(&row.id)
        .await
        .map_err(|error| problem::internal("reading a staged plugin upgrade", &error))?;
    let update = state.services().update_job.last_check().and_then(|report| {
        report
            .updates
            .into_iter()
            .find(|update| update.plugin_id == row.id)
    });
    Ok(PluginView {
        id: row.id.clone(),
        name: manifest.name.clone(),
        config: manifest
            .config
            .server
            .as_ref()
            .map(|section| ConfigSectionView {
                schema: section.schema.clone(),
                ui_schema: section.ui_schema.clone(),
            }),
        is_default: default_plugin_ids().contains(&row.id),
        version: row.version.clone(),
        api: row.api.clone(),
        provides: manifest
            .provides()
            .iter()
            .map(|layer| layer.as_str().to_owned())
            .collect(),
        source: row.source.clone(),
        state: row.state(staged.is_some()).as_str().to_owned(),
        enabled: row.enabled,
        repository_id: row
            .repository_id
            .map(|id| client_id(IdPrefix::PluginRepository, id)),
        requested_permissions: requested_permissions(&manifest)
            .iter()
            .map(PermissionView::from)
            .collect(),
        approved: row.approved_permissions.is_some(),
        update: update_view(
            update.as_ref(),
            staged.as_ref().map(|upgrade| upgrade.version.as_str()),
        ),
        installed_at: row.installed_at,
    })
}

/// Describes the update a plugin has waiting, when a check found one.
///
/// # Parameters
///
/// - `update`: the update the last check reported.
/// - `staged`: the version of the upgrade staged for the plugin, when one is.
///
/// # Returns
///
/// The view, or `None` when the check reported no update for the plugin.
fn update_view(update: Option<&PluginUpdate>, staged: Option<&str>) -> Option<UpdateView> {
    let update = update?;
    Some(UpdateView {
        version: update.version.clone(),
        requires_approval: staged == Some(update.version.as_str()),
    })
}

/// Builds the listing of everything an administrator can install.
///
/// # Parameters
///
/// - `report`: what the installer's listing found.
///
/// # Returns
///
/// The view.
fn available_view(report: AvailableReport) -> AvailableView {
    AvailableView {
        plugins: report
            .plugins
            .into_iter()
            .map(available_plugin_view)
            .collect(),
        errors: report
            .errors
            .into_iter()
            .map(|error| AvailableErrorView {
                source: error.source,
                detail: error.detail,
            })
            .collect(),
    }
}

/// Builds the view of one plugin an index offers.
///
/// # Parameters
///
/// - `plugin`: the listing entry.
///
/// # Returns
///
/// The view.
fn available_plugin_view(plugin: AvailablePlugin) -> AvailablePluginView {
    let is_default = default_plugin_ids().contains(&plugin.id);
    AvailablePluginView {
        is_default,
        id: plugin.id,
        name: plugin.name,
        description: plugin.description,
        provides: plugin.provides,
        homepage: plugin.homepage,
        license: plugin.license,
        official: plugin.official,
        verified: plugin.verified,
        repository_id: plugin
            .repository_id
            .map(|id| client_id(IdPrefix::PluginRepository, id)),
        version: plugin.version,
        unavailable: plugin.unavailable.map(|reason| UnavailableView {
            code: unavailable_code(&reason).to_owned(),
            detail: reason.to_string(),
        }),
    }
}

/// Returns the stable code of an unavailable reason.
///
/// # Parameters
///
/// - `reason`: why a plugin cannot be installed.
///
/// # Returns
///
/// `no-compatible-version` or `no-such-version`.
fn unavailable_code(reason: &Unavailable) -> &'static str {
    match reason {
        Unavailable::NoCompatibleVersion { .. } => "no-compatible-version",
        Unavailable::NoSuchVersion { .. } => "no-such-version",
    }
}

/// Stages one uploaded part as a temporary file inside the plugins directory.
///
/// # Parameters
///
/// - `state`: the shared services, for the plugins directory.
/// - `field`: the `file` part to read.
///
/// # Returns
///
/// The staged file's path, which the caller removes.
///
/// # Errors
///
/// Answers `invalid-request` when the part cannot be read and `internal` when
/// the file cannot be written.
async fn stage_upload(state: &NativeState, field: &mut Field<'_>) -> Result<PathBuf, Problem> {
    let dir = state.services().plugins.plugins_dir();
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|error| problem::internal("creating the plugins directory", &error))?;
    let path = dir.join(format!("{UPLOAD_PREFIX}{}", new_uuid_v7()));
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|error| problem::internal("creating a staged plugin upload", &error))?;
    loop {
        let chunk = match field.chunk().await {
            Ok(chunk) => chunk,
            Err(error) => {
                drop(file);
                remove_staged(&path).await;
                return Err(Problem::invalid_request(error.body_text()));
            }
        };
        let Some(chunk) = chunk else {
            break;
        };
        if let Err(error) = file.write_all(&chunk).await {
            drop(file);
            remove_staged(&path).await;
            return Err(problem::internal("writing a staged plugin upload", &error));
        }
    }
    if let Err(error) = file.flush().await {
        drop(file);
        remove_staged(&path).await;
        return Err(problem::internal("writing a staged plugin upload", &error));
    }
    Ok(path)
}

/// Removes a staged upload, logging a failure rather than reporting it.
///
/// # Parameters
///
/// - `path`: the staged file.
async fn remove_staged(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(path = %path.display(), %error, "could not remove a staged plugin upload");
    }
}

/// Maps an installer error onto the problem a client sees.
///
/// # Parameters
///
/// - `error`: the error the installer returned.
///
/// # Returns
///
/// `signature-invalid`, `integrity-failed` or `index-unavailable` for the three
/// index failures, `repositories-disabled` for a refused repository,
/// `not-found` for an unknown repository or plugin, `conflict` for a version
/// that cannot be switched in, `invalid-plugin` when the archive is not a plugin
/// this server hosts, and `internal` for anything else.
fn install_problem(error: &InstallError) -> Problem {
    match error {
        InstallError::IntegrityFailed { .. }
        | InstallError::SizeMismatch { .. }
        | InstallError::UnreadableDigest { .. } => Problem::integrity_failed(error.to_string()),
        InstallError::Index(IndexError::SignatureInvalid { .. } | IndexError::Unsigned { .. }) => {
            Problem::signature_invalid(error.to_string())
        }
        InstallError::Index(IndexError::Unreachable { .. } | IndexError::Malformed { .. })
        | InstallError::Download { .. } => Problem::index_unavailable(error.to_string()),
        InstallError::RepositoriesDisabled => Problem::repositories_disabled(error.to_string()),
        InstallError::UnknownRepository { .. } | InstallError::UnknownPlugin { .. } => {
            Problem::not_found(error.to_string())
        }
        InstallError::AlreadyInstalled { .. }
        | InstallError::Downgrade { .. }
        | InstallError::RepositoryDisabled { .. } => Problem::conflict(error.to_string()),
        InstallError::Archive(_)
        | InstallError::MissingEntry(_)
        | InstallError::EntryTooLarge { .. }
        | InstallError::Manifest(_)
        | InstallError::UnsupportedApi { .. }
        | InstallError::Compile(_)
        | InstallError::DisallowedImport(_)
        | InstallError::MissingExport { .. }
        | InstallError::UndeclaredExport(_)
        | InstallError::UnsupportedCapability(_)
        | InstallError::Incompatible(_)
        | InstallError::Unavailable(_) => Problem::invalid_plugin(error.to_string()),
        InstallError::Io { .. }
        | InstallError::OutsidePluginsDir(_)
        | InstallError::Record(_)
        | InstallError::Settings(_) => problem::internal("the plugin install path", error),
    }
}

/// Maps an administrative plugin error onto the problem a client sees.
///
/// # Parameters
///
/// - `error`: the error the admin path returned.
///
/// # Returns
///
/// `not-found` for an unknown plugin, `conflict` when there is nothing to
/// approve or decline or the plugin is pending approval, the installer's own
/// mapping for an install failure, and `internal` when the tables cannot be
/// read or written.
fn admin_problem(error: &AdminError) -> Problem {
    match error {
        AdminError::NotFound(_) => Problem::not_found(error.to_string()),
        AdminError::PendingApproval(_)
        | AdminError::NothingToApprove { .. }
        | AdminError::NoUpgradeStaged(_) => Problem::conflict(error.to_string()),
        AdminError::Install(error) => install_problem(error),
        AdminError::Repo(_) => problem::internal("the plugin admin path", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_plugin_host::{PluginSource, Risk};
    use utoipa::openapi::RefOr;

    #[test]
    fn the_document_describes_the_plugin_routes() {
        let document = crate::openapi();
        for route in [
            "/api/v1/admin/plugins",
            "/api/v1/admin/plugins/upload",
            "/api/v1/admin/plugins/available",
            "/api/v1/admin/plugins/{id}",
            "/api/v1/admin/plugins/{id}/approve",
            "/api/v1/admin/plugins/{id}/enable",
            "/api/v1/admin/plugins/{id}/disable",
            "/api/v1/admin/plugins/{id}/decline-upgrade",
        ] {
            assert!(
                document.paths.paths.contains_key(route),
                "{route} is missing"
            );
        }
        let available = document
            .paths
            .paths
            .get("/api/v1/admin/plugins/available")
            .unwrap();
        let parameters = available
            .get
            .as_ref()
            .unwrap()
            .parameters
            .iter()
            .flatten()
            .filter_map(|parameter| match parameter {
                RefOr::T(parameter) => Some(parameter.name.as_str()),
                RefOr::Ref(_) => None,
            })
            .collect::<Vec<_>>();
        assert!(parameters.contains(&"repositoryId"), "{parameters:?}");
    }

    #[test]
    fn a_permission_carries_its_scope_detail_and_risk() {
        let permission = RequestedPermission {
            scope: "http.hosts",
            detail: "api.example.com".to_owned(),
            risk: Risk::Low,
        };
        let view = PermissionView::from(&permission);
        assert_eq!(view.scope, "http.hosts");
        assert_eq!(view.detail, "api.example.com");
        assert_eq!(view.risk, "low");
    }

    #[test]
    fn an_update_is_only_flagged_when_its_version_is_staged() {
        let update = PluginUpdate {
            plugin_id: "com.example.plugin".to_owned(),
            current: "0.1.0".to_owned(),
            version: "0.2.0".to_owned(),
            source: PluginSource::Official,
            repository_id: None,
        };
        let view = update_view(Some(&update), Some("0.2.0")).unwrap();
        assert_eq!(view.version, "0.2.0");
        assert!(view.requires_approval);
        let view = update_view(Some(&update), None).unwrap();
        assert!(!view.requires_approval);
        assert!(update_view(None, None).is_none());
    }

    #[test]
    fn an_unavailable_reason_maps_to_a_stable_code() {
        let reason = Unavailable::NoCompatibleVersion {
            supported: "hearthsound:plugin@0.2.0",
            offered: vec!["hearthsound:plugin@0.3.0".to_owned()],
        };
        assert_eq!(unavailable_code(&reason), "no-compatible-version");
        let reason = Unavailable::NoSuchVersion {
            requested: "9.9.9".to_owned(),
            offered: vec!["0.1.0".to_owned()],
        };
        assert_eq!(unavailable_code(&reason), "no-such-version");
    }
}
