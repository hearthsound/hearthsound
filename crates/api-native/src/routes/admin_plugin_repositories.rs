//! `/api/v1/admin/plugin-repositories`: the plugin indexes an administrator trusts.
//!
//! A repository is an index of `.hsplugin` releases: the URL of its index
//! document, the name to show for it, the minisign public key its index is
//! signed with, and whether it is queried. A repository with a key must sign its
//! index, and one without a key is read unsigned and reported unverified.
//!
//! Registering a repository is gated by
//! `plugins.allow_third_party_repositories`, because an index is a plugin
//! source an administrator trusts. Reading an index is the installer's job:
//! every fetch records when it ran and what went wrong on the repository's row,
//! which is what the views below report.

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use hearthsound_config::keys;
use hearthsound_core::IdPrefix;
use hearthsound_plugin_host::{RepoError, RepositoryRow};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::AdminAuthed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;
use crate::views::{client_id, parse_id};

/// One plugin repository, as an administrator sees it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryView {
    /// The repository's client id (`pr_...`).
    pub id: String,
    /// The URL of the repository's index document.
    pub url: String,
    /// The name shown to administrators.
    pub name: Option<String>,
    /// The minisign public key the repository signs its index with, when it is
    /// signed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Whether the repository is queried for its index.
    pub enabled: bool,
    /// Whether the index is verified: a repository with a configured key
    /// refuses an index whose signature does not match, and one without a key
    /// is read unsigned.
    pub verified: bool,
    /// When the index was last fetched, in unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fetched_at: Option<i64>,
    /// What went wrong on the last fetch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl From<&RepositoryRow> for RepositoryView {
    /// Describes one repository row.
    fn from(row: &RepositoryRow) -> RepositoryView {
        RepositoryView {
            id: client_id(IdPrefix::PluginRepository, row.id),
            url: row.url.clone(),
            name: row.name.clone(),
            public_key: row.public_key.clone(),
            enabled: row.enabled,
            verified: row.public_key.is_some(),
            last_fetched_at: row.last_fetched_at,
            last_error: row.last_error.clone(),
        }
    }
}

/// The registered plugin repositories, by URL.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RepositoryList {
    /// The repositories.
    pub repositories: Vec<RepositoryView>,
}

/// A repository to register.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewRepository {
    /// The URL of the repository's index document.
    pub url: String,
    /// The name to show to administrators.
    #[serde(default)]
    pub name: Option<String>,
    /// The minisign public key the repository signs its index with; omitted for
    /// a repository that is read unsigned.
    #[serde(default)]
    pub public_key: Option<String>,
}

/// Changes to a repository; an absent field is left as it is.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RepositoryPatch {
    /// The name to show; `null` clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub name: Option<Option<String>>,
    /// The minisign public key to verify the index with; `null` clears it and
    /// makes the repository unsigned.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    pub public_key: Option<Option<String>>,
    /// Whether the repository is queried for its index.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Reads a field that tells an absent value from an explicit `null`.
///
/// Without this, `null` and an omitted field deserialize the same way, and a
/// patch could not clear the name or the public key of a repository.
///
/// # Parameters
///
/// - `deserializer`: the field's deserializer.
///
/// # Returns
///
/// `Some(None)` for `null`, `Some(Some(value))` otherwise, the outer `None`
/// coming from serde's `#[serde(default)]` for an absent field.
///
/// # Errors
///
/// Returns the deserializer's error when the field is not a string.
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// Lists the registered plugin repositories.
///
/// Each repository reports whether it is queried, whether its index is verified
/// against a configured public key, and the time and error of its last fetch.
///
/// # Errors
///
/// Answers `internal` when the repository rows cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/plugin-repositories",
    tag = "admin",
    responses(
        (status = 200, description = "The registered repositories", body = RepositoryList),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_repositories(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<RepositoryList>, Problem> {
    let rows = state
        .services()
        .plugin_repository
        .repositories()
        .await
        .map_err(|error| problem::internal("listing the plugin repositories", &error))?;
    Ok(Json(RepositoryList {
        repositories: rows.iter().map(RepositoryView::from).collect(),
    }))
}

/// Registers a plugin repository.
///
/// The repository is queried for its index from then on, and its index is
/// verified against `publicKey` when one is given.
///
/// # Errors
///
/// Answers `invalid-request` for a malformed body, `repositories-disabled` when
/// `plugins.allow_third_party_repositories` is off, `conflict` when another
/// repository is registered under the same URL, and `internal` when the row
/// cannot be written.
#[utoipa::path(
    post,
    path = "/api/v1/admin/plugin-repositories",
    tag = "admin",
    request_body = NewRepository,
    responses(
        (status = 201, description = "The repository was registered", body = RepositoryView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "Third-party repositories are switched off", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "A repository is registered under this URL already", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_repository(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(body): JsonBody<NewRepository>,
) -> Result<(StatusCode, Json<RepositoryView>), Problem> {
    let allowed = state
        .settings()
        .current()
        .get(keys::PLUGINS_ALLOW_THIRD_PARTY_REPOSITORIES)
        .map_err(|error| {
            problem::internal("reading plugins.allow_third_party_repositories", &error)
        })?;
    if !allowed {
        return Err(Problem::repositories_disabled(
            "Adding a plugin repository is turned off on this server.",
        ));
    }
    let row = state
        .services()
        .plugin_repository
        .insert_repository(
            &body.url,
            body.name.as_deref(),
            body.public_key.as_deref(),
            true,
        )
        .await
        .map_err(|error| repository_problem(&error))?;
    Ok((StatusCode::CREATED, Json(RepositoryView::from(&row))))
}

/// Changes a plugin repository.
///
/// The URL is not mutable: a different URL is a different repository. An absent
/// field keeps its stored value; `null` clears the name or the public key.
///
/// # Errors
///
/// Answers `not-found` for an unknown repository, `invalid-request` for a
/// malformed body, and `internal` when the row cannot be read or written.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/plugin-repositories/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The repository's client id (`pr_...`)")),
    request_body = RepositoryPatch,
    responses(
        (status = 200, description = "The repository was changed", body = RepositoryView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such repository", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_repository(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
    JsonBody(body): JsonBody<RepositoryPatch>,
) -> Result<Json<RepositoryView>, Problem> {
    let repository = state.services().plugin_repository.clone();
    let id = parse_id(&id, IdPrefix::PluginRepository)?;
    let row = repository
        .repository(id)
        .await
        .map_err(|error| problem::internal("reading a plugin repository", &error))?
        .ok_or_else(|| Problem::not_found("No such plugin repository."))?;

    let name = match body.name {
        Some(name) => name,
        None => row.name.clone(),
    };
    let public_key = match body.public_key {
        Some(public_key) => public_key,
        None => row.public_key.clone(),
    };
    let enabled = body.enabled.unwrap_or(row.enabled);

    let row = repository
        .update_repository(id, name.as_deref(), public_key.as_deref(), enabled)
        .await
        .map_err(|error| problem::internal("changing a plugin repository", &error))?
        .ok_or_else(|| Problem::not_found("No such plugin repository."))?;
    Ok(Json(RepositoryView::from(&row)))
}

/// Removes a plugin repository.
///
/// Plugins installed from the repository keep their rows; only the repository
/// itself goes.
///
/// # Errors
///
/// Answers `not-found` for an unknown repository and `internal` when the row
/// cannot be removed.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/plugin-repositories/{id}",
    tag = "admin",
    params(("id" = String, Path, description = "The repository's client id (`pr_...`)")),
    responses(
        (status = 204, description = "The repository was removed"),
        (status = 404, description = "No such repository", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_repository(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(id): UrlPath<String>,
) -> Result<StatusCode, Problem> {
    let id = parse_id(&id, IdPrefix::PluginRepository)?;
    let removed = state
        .services()
        .plugin_repository
        .delete_repository(id)
        .await
        .map_err(|error| problem::internal("removing a plugin repository", &error))?;
    if !removed {
        return Err(Problem::not_found("No such plugin repository."));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Maps a repository write failure onto the problem a client sees.
///
/// # Parameters
///
/// - `error`: the error the repository tables returned.
///
/// # Returns
///
/// `conflict` for a URL that is registered already, and `internal` for anything
/// else.
fn repository_problem(error: &RepoError) -> Problem {
    match error {
        RepoError::DuplicateUrl(_) => Problem::conflict(error.to_string()),
        _ => problem::internal("registering a plugin repository", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_describes_the_repository_routes() {
        let document = crate::openapi();
        for route in [
            "/api/v1/admin/plugin-repositories",
            "/api/v1/admin/plugin-repositories/{id}",
        ] {
            assert!(
                document.paths.paths.contains_key(route),
                "{route} is missing"
            );
        }
    }

    #[test]
    fn a_patch_tells_an_absent_field_from_a_null_one() {
        let patch: RepositoryPatch = serde_json::from_str("{}").unwrap();
        assert_eq!(patch.name, None);
        assert_eq!(patch.public_key, None);
        assert_eq!(patch.enabled, None);

        let patch: RepositoryPatch =
            serde_json::from_str(r#"{"name": null, "publicKey": null, "enabled": false}"#).unwrap();
        assert_eq!(patch.name, Some(None));
        assert_eq!(patch.public_key, Some(None));
        assert_eq!(patch.enabled, Some(false));

        let patch: RepositoryPatch =
            serde_json::from_str(r#"{"name": "local", "publicKey": "RWQ"}"#).unwrap();
        assert_eq!(patch.name, Some(Some("local".to_owned())));
        assert_eq!(patch.public_key, Some(Some("RWQ".to_owned())));
        assert_eq!(patch.enabled, None);
    }

    #[test]
    fn a_repository_view_reports_a_configured_key_as_verified() {
        let id = hearthsound_core::new_uuid_v7();
        let row = RepositoryRow {
            id,
            url: "https://plugins.example/v1/index.json".to_owned(),
            name: Some("Example".to_owned()),
            public_key: Some("RWQ".to_owned()),
            enabled: true,
            last_fetched_at: Some(1_700_000_000_000),
            last_error: Some("the index does not answer".to_owned()),
        };
        let view = RepositoryView::from(&row);
        assert_eq!(view.id, client_id(IdPrefix::PluginRepository, id));
        assert!(view.verified);
        assert!(view.enabled);
        assert_eq!(
            view.last_error.as_deref(),
            Some("the index does not answer")
        );

        let unsigned = RepositoryRow {
            public_key: None,
            ..row
        };
        assert!(!RepositoryView::from(&unsigned).verified);
    }
}
