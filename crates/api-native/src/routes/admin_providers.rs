//! `/api/v1/admin/providers`: the configured provider instances.
//!
//! An instance is a plugin plus the configuration it runs with: the slug it is
//! addressed by, the plugin it is configured from, the plugin's server-scoped
//! configuration, the host-level settings (import roots and concurrency), and
//! whether it takes part in its layers. Configuring one validates the
//! configuration against the plugin's schema, fills in the schema's defaults,
//! seals its write-only fields, and asks the plugin whether the configuration
//! works, so only a configuration that passes all of that is stored.
//!
//! Secrets are never handed back. A read carries the sealed placeholder in
//! place of every write-only field, and a write that echoes the placeholder
//! back keeps the stored secret. A health check runs the plugin's own
//! `lifecycle.health` and records its outcome on the instance, so the result
//! and the time it was taken are visible afterwards.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use hearthsound_plugin_host::{InstanceError, InstanceHostConfig, InstanceSpec, InstanceView};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use utoipa::ToSchema;

use crate::auth::AdminAuthed;
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;

/// The outcome of one health check.
///
/// The object is what the health route answers and what the instance's row
/// stores, so a later read reports the check as it was taken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthView {
    /// The instance the check ran against.
    pub slug: String,
    /// Whether the instance answered that it can serve requests.
    pub ok: bool,
    /// What the probe reported: the plugin's own status line when the instance
    /// is healthy, and the failure when it is not.
    pub detail: String,
    /// When the probe ran, in unix milliseconds.
    pub checked_at: i64,
}

/// One configured provider instance.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderView {
    /// The instance slug.
    pub slug: String,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// The name shown in the admin UI.
    pub display_name: String,
    /// Whether the instance takes part in its layers.
    pub enabled: bool,
    /// The plugin's server-scoped configuration, carrying the sealed
    /// placeholder in place of every write-only field.
    #[schema(value_type = Object)]
    pub server_config: Value,
    /// The host-level settings: the import roots and the concurrency limits.
    #[schema(value_type = Object)]
    pub host_config: InstanceHostConfig,
    /// The last health check, when one has run.
    pub health: Option<HealthView>,
    /// When the instance was created, in unix milliseconds.
    pub created_at: i64,
    /// When the instance was last changed, in unix milliseconds.
    pub updated_at: i64,
}

/// The configured provider instances, by slug.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProviderList {
    /// The instances.
    pub providers: Vec<ProviderView>,
}

/// An instance to configure.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewProvider {
    /// The instance slug, `^[a-z0-9]{2,24}$`, unique among the server's
    /// instances and immutable once created.
    pub slug: String,
    /// The installed, enabled plugin to configure the instance from.
    pub plugin_id: String,
    /// The name shown in the admin UI; the slug when absent.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The plugin's server-scoped configuration; an empty object when absent.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub server_config: Option<Value>,
    /// The host-level settings; the plugin host's defaults when absent.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub host_config: Option<Value>,
}

/// Changes to an instance; an absent field is left as it is.
///
/// The slug and the plugin are not mutable: a different plugin is a different
/// instance. Fields this route does not change are ignored, so an instance read
/// from [`ProviderView`] can be sent back with its changes made.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPatch {
    /// The new name shown in the admin UI.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The plugin's server-scoped configuration, which replaces the stored one.
    /// A write-only field that carries the sealed placeholder keeps the secret
    /// the instance stores.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub server_config: Option<Value>,
    /// The host-level settings, which replace the stored ones.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub host_config: Option<Value>,
    /// Whether the instance takes part in its layers.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Lists the configured provider instances.
///
/// Each instance reports its plugin, its configuration (with the sealed
/// placeholder in place of every secret), whether it is enabled, and the
/// outcome of the last health check.
///
/// # Errors
///
/// Answers `internal` when the instance rows cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/providers",
    tag = "admin",
    responses(
        (status = 200, description = "The configured provider instances", body = ProviderList),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_providers(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
) -> Result<Json<ProviderList>, Problem> {
    let views = state
        .services()
        .instances
        .list()
        .await
        .map_err(|error| instance_problem(&error))?;
    let health = instance_health(&state).await?;
    let mut providers = Vec::with_capacity(views.len());
    for view in views {
        let health = health.get(&view.slug).cloned();
        providers.push(provider_view(view, health));
    }
    Ok(Json(ProviderList { providers }))
}

/// Configures a provider instance from an installed plugin.
///
/// The plugin must be installed and enabled, the slug must be free, the
/// server-scoped configuration must satisfy the plugin's schema, and the plugin
/// must accept it in `lifecycle.validate-config`. The instance is enabled, so
/// it takes part in the layers it is assigned to.
///
/// # Errors
///
/// Answers `invalid-request` for a malformed body or a slug that is not a
/// legal slug, `conflict` when the slug is taken, `invalid-provider` when the
/// plugin is unknown or disabled, provides no layer this server drives, or
/// rejects the configuration, and `internal` when the plugin cannot be loaded
/// or the row cannot be written.
#[utoipa::path(
    post,
    path = "/api/v1/admin/providers",
    tag = "admin",
    request_body = NewProvider,
    responses(
        (status = 201, description = "The instance was configured and enabled", body = ProviderView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The slug is taken", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The plugin or the configuration cannot be used", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn create_provider(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    JsonBody(body): JsonBody<NewProvider>,
) -> Result<(StatusCode, Json<ProviderView>), Problem> {
    let spec = InstanceSpec {
        display_name: body.display_name.unwrap_or_else(|| body.slug.clone()),
        slug: body.slug,
        plugin_id: body.plugin_id,
        server_config: body
            .server_config
            .unwrap_or_else(|| Value::Object(Map::new())),
        host_config: body.host_config.unwrap_or(Value::Null),
    };
    let view = state
        .services()
        .instances
        .create(&spec)
        .await
        .map_err(|error| instance_problem(&error))?;
    rebuild(&state).await?;
    // A new instance must not show "Not checked yet": the probe runs beside the
    // request and its result is recorded on the instance.
    check_health_in_background(&state, view.slug.clone());
    Ok((StatusCode::CREATED, Json(provider_view(view, None))))
}

/// Reports one configured provider instance.
///
/// # Errors
///
/// Answers `not-found` for an unknown instance and `internal` when the row or
/// its configuration cannot be read.
#[utoipa::path(
    get,
    path = "/api/v1/admin/providers/{slug}",
    tag = "admin",
    params(("slug" = String, Path, description = "The instance slug")),
    responses(
        (status = 200, description = "The configured instance", body = ProviderView),
        (status = 404, description = "No such instance", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn get_provider(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(slug): UrlPath<String>,
) -> Result<Json<ProviderView>, Problem> {
    let health = instance_health(&state).await?.remove(&slug);
    let view = state
        .services()
        .instances
        .get(&slug)
        .await
        .map_err(|error| instance_problem(&error))?;
    Ok(Json(provider_view(view, health)))
}

/// Changes a provider instance.
///
/// The configuration is validated and sealed exactly as it is on creation, and
/// it is applied as a patch: a server-config field the request leaves out keeps
/// its stored value, secrets included, a write-only field sent as `[sealed]`
/// also keeps it, and a write-only field sent as an empty string removes it.
/// A configuration that does not satisfy the plugin's schema is answered with
/// `invalid-provider`, and the detail names the field without ever printing a
/// stored secret.
///
/// The registry is rebuilt so the change is in service before the answer is
/// sent. Enabling an instance also runs its health check, so a switched-on
/// instance never shows a result from before it was switched off.
///
/// # Errors
///
/// Answers `invalid-request` for a malformed body, `not-found` for an unknown
/// instance, `invalid-provider` when the plugin rejects the configuration or
/// cannot serve it, and `unavailable` when the change was saved but the registry
/// could not be rebuilt yet.
#[utoipa::path(
    patch,
    path = "/api/v1/admin/providers/{slug}",
    tag = "admin",
    params(("slug" = String, Path, description = "The instance slug")),
    request_body = ProviderPatch,
    responses(
        (status = 200, description = "The instance was changed", body = ProviderView),
        (status = 400, description = "The request body is malformed", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "No such instance", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The configuration cannot be used", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "The change was saved but not applied yet", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn patch_provider(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(slug): UrlPath<String>,
    JsonBody(body): JsonBody<ProviderPatch>,
) -> Result<Json<ProviderView>, Problem> {
    let instances = state.services().instances.clone();
    let mut current = instances
        .get(&slug)
        .await
        .map_err(|error| instance_problem(&error))?;

    if body.display_name.is_some() || body.server_config.is_some() || body.host_config.is_some() {
        let host_config = match body.host_config {
            Some(host_config) => host_config,
            None => serde_json::to_value(&current.host_config)
                .map_err(|error| problem::internal("encoding the host settings", &error))?,
        };
        let spec = InstanceSpec {
            display_name: body
                .display_name
                .unwrap_or_else(|| current.display_name.clone()),
            slug: current.slug.clone(),
            plugin_id: current.plugin_id.clone(),
            // A configuration field the request leaves out keeps its stored
            // value; the service merges the patch onto what the instance holds.
            server_config: body.server_config.unwrap_or_else(|| serde_json::json!({})),
            host_config,
        };
        current = instances
            .update(&slug, &spec)
            .await
            .map_err(|error| instance_problem(&error))?;
        // The edit already landed, so a cache that cannot be cleared is a log
        // line rather than a failed request: the next fetch overwrites the entry.
        if let Err(error) = state
            .services()
            .catalog
            .invalidate_instance(current.id)
            .await
        {
            tracing::warn!(
                %error,
                instance = %current.slug,
                "the cached answers of an edited instance could not be dropped"
            );
        }
    }

    // The enable write is not allowed to skip the rebuild: a failure of either
    // is reported, but the registry is always brought in line with what was
    // written.
    let enabled_result = match body.enabled {
        Some(enabled) if enabled != current.enabled => Some(
            instances
                .set_enabled(&slug, enabled)
                .await
                .map_err(|error| instance_problem(&error))?,
        ),
        _ => None,
    };
    if let Some(view) = enabled_result {
        current = view;
    }
    let rebuild_result = rebuild(&state).await;
    // Both writes are committed now; the answer reports the first failure, and
    // a failed rebuild has already marked the registry stale for the retry.
    if let Some(enabled) = body.enabled
        && enabled
        && current.enabled
    {
        check_health_in_background(&state, current.slug.clone());
    }
    rebuild_result?;
    let health = instance_health(&state).await?.remove(&slug);
    Ok(Json(provider_view(current, health)))
}

/// Removes a provider instance.
///
/// The instance's key-value rows and its layer assignments go with it, and the
/// registry is rebuilt so it stops serving.
///
/// # Errors
///
/// Answers `not-found` for an unknown instance and `internal` when the row
/// cannot be removed.
#[utoipa::path(
    delete,
    path = "/api/v1/admin/providers/{slug}",
    tag = "admin",
    params(("slug" = String, Path, description = "The instance slug")),
    responses(
        (status = 204, description = "The instance was removed"),
        (status = 404, description = "No such instance", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_provider(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(slug): UrlPath<String>,
) -> Result<StatusCode, Problem> {
    state
        .services()
        .instances
        .delete(&slug)
        .await
        .map_err(|error| instance_problem(&error))?;
    rebuild(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Runs one instance's health check.
///
/// The probe is the plugin's own `lifecycle.health`, run through the first
/// provider the instance holds. Its outcome is recorded on the instance, so the
/// provider views report it until the next check.
///
/// # Errors
///
/// Answers `not-found` for an unknown instance, `conflict` when the instance is
/// not in service - it is disabled, or no layer it serves has been assigned to
/// it - and `internal` when the probe's outcome cannot be recorded.
#[utoipa::path(
    get,
    path = "/api/v1/admin/providers/{slug}/health",
    tag = "admin",
    params(("slug" = String, Path, description = "The instance slug")),
    responses(
        (status = 200, description = "What the health check reported", body = HealthView),
        (status = 404, description = "No such instance", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The instance is not in service", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn provider_health(
    _admin: AdminAuthed,
    State(state): State<NativeState>,
    UrlPath(slug): UrlPath<String>,
) -> Result<Json<HealthView>, Problem> {
    state
        .services()
        .plugin_repository
        .instance_by_slug(&slug)
        .await
        .map_err(|error| problem::internal("reading a provider instance", &error))?
        .ok_or_else(|| Problem::not_found("No such provider instance."))?;
    let outcome = state
        .services()
        .registry
        .check_health(&slug)
        .await
        .map_err(|error| problem::internal("recording a health result", &error))?
        .ok_or_else(|| {
            Problem::conflict(
                "The provider instance is not in service; enable it and assign it to a layer.",
            )
        })?;
    Ok(Json(HealthView {
        slug: outcome.slug,
        ok: outcome.ok,
        detail: outcome.detail,
        checked_at: outcome.checked_at,
    }))
}

/// Rebuilds the provider registry so a change is in service.
///
/// A skipped instance is a warning rather than a failure; only a table that
/// cannot be read makes the rebuild fail, and the write that preceded it has
/// already committed. That is answered as `unavailable` rather than `internal`:
/// the change is stored, the running server could not apply it yet, and the
/// reload worker retries while the registry is marked stale.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Errors
///
/// Answers `unavailable` when the plugin tables cannot be read.
async fn rebuild(state: &NativeState) -> Result<(), Problem> {
    state
        .services()
        .registry
        .rebuild_or_mark_stale()
        .await
        .map_err(|error| {
            tracing::error!(%error, "could not rebuild the provider registry");
            Problem::unavailable(
                "The change was saved, but the running server could not apply it yet; it retries automatically.",
            )
        })?;
    if let Err(error) = state.services().orchestrator.providers_changed().await {
        tracing::error!(%error, "could not bring the acquisitions in line with the registry");
    }
    Ok(())
}

/// Runs one instance's health check in the background.
///
/// Creating and enabling an instance are answered before the plugin is asked
/// anything, because a probe is an outbound request that can take the plugin's
/// whole call timeout. The outcome is recorded on the instance, which the
/// Providers and Layers pages read.
///
/// # Parameters
///
/// - `state`: the shared services.
/// - `slug`: the instance to check.
fn check_health_in_background(state: &NativeState, slug: String) {
    let registry = std::sync::Arc::clone(&state.services().registry);
    tokio::spawn(async move {
        match registry.check_health(&slug).await {
            Ok(_) => {}
            Err(error) => tracing::warn!(
                instance = %slug,
                %error,
                "could not record a provider health result"
            ),
        }
    });
}

/// Builds the view of one instance.
///
/// # Parameters
///
/// - `view`: the instance as the service reads it.
/// - `health`: the last health check of the instance, when one has run.
///
/// # Returns
///
/// The view.
fn provider_view(view: InstanceView, health: Option<HealthView>) -> ProviderView {
    ProviderView {
        slug: view.slug,
        plugin_id: view.plugin_id,
        display_name: view.display_name,
        enabled: view.enabled,
        server_config: view.server_config,
        host_config: view.host_config,
        health,
        created_at: view.created_at,
        updated_at: view.updated_at,
    }
}

/// Reads the last health check of every instance, keyed by slug.
///
/// # Parameters
///
/// - `state`: the shared services.
///
/// # Errors
///
///Answers `internal` when the instance rows cannot be read or a stored health
/// result is not the object the health route wrote.
async fn instance_health(state: &NativeState) -> Result<HashMap<String, HealthView>, Problem> {
    let rows = state
        .services()
        .plugin_repository
        .instances()
        .await
        .map_err(|error| problem::internal("reading the provider instances", &error))?;
    let mut health = HashMap::with_capacity(rows.len());
    for row in rows {
        let Some(text) = row.health.as_deref() else {
            continue;
        };
        let recorded: HealthView = serde_json::from_str(text)
            .map_err(|error| problem::internal("reading a stored health result", &error))?;
        health.insert(row.slug, recorded);
    }
    Ok(health)
}

/// Maps a provider configuration failure onto the problem a client sees.
///
/// # Parameters
///
/// - `error`: the error the instance service returned.
///
/// # Returns
///
/// `invalid-request` for a slug that is not a legal slug, `not-found` for an
/// unknown instance, `conflict` for a taken slug or a changed plugin,
/// `invalid-provider` for a plugin or configuration this server cannot use, and
/// `internal` for the failures behind the host's back.
fn instance_problem(error: &InstanceError) -> Problem {
    match error {
        InstanceError::InvalidSlug(_) => Problem::invalid_request(error.to_string()),
        InstanceError::UnknownInstance(_) => Problem::not_found(error.to_string()),
        InstanceError::SlugTaken(_) | InstanceError::PluginMismatch { .. } => {
            Problem::conflict(error.to_string())
        }
        InstanceError::UnknownPlugin(_)
        | InstanceError::PluginDisabled(_)
        | InstanceError::ServerConfig(_)
        | InstanceError::Schema(_)
        | InstanceError::ValidateConfig(_)
        | InstanceError::HostConfig(_)
        | InstanceError::DuplicateInstance(_)
        | InstanceError::LayerNotProvided { .. }
        | InstanceError::UnsupportedCapability(_)
        | InstanceError::NoHostedLayer(_) => Problem::invalid_provider(error.to_string()),
        InstanceError::Version { .. }
        | InstanceError::StoredManifest { .. }
        | InstanceError::Load(_)
        | InstanceError::Host(_)
        | InstanceError::ValidateFailed(_)
        | InstanceError::Secret(_)
        | InstanceError::Repository(_) => problem::internal("the provider configuration", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_describes_the_provider_routes() {
        let document = crate::openapi();
        for route in [
            "/api/v1/admin/providers",
            "/api/v1/admin/providers/{slug}",
            "/api/v1/admin/providers/{slug}/health",
        ] {
            assert!(
                document.paths.paths.contains_key(route),
                "{route} is missing"
            );
        }
    }

    #[test]
    fn a_health_result_round_trips_through_its_stored_form() {
        let health = HealthView {
            slug: "hello".to_owned(),
            ok: true,
            detail: "ready".to_owned(),
            checked_at: 1_700_000_000_000,
        };
        let text = serde_json::to_string(&health).unwrap();
        assert_eq!(
            text,
            r#"{"slug":"hello","ok":true,"detail":"ready","checkedAt":1700000000000}"#
        );
        assert_eq!(serde_json::from_str::<HealthView>(&text).unwrap(), health);
    }

    #[test]
    fn an_instance_error_maps_to_its_problem() {
        let problem = instance_problem(&InstanceError::UnknownInstance("hello".to_owned()));
        assert_eq!(problem.status, 404);
        assert_eq!(problem.code, "not-found");

        let problem = instance_problem(&InstanceError::PluginDisabled("com.example".to_owned()));
        assert_eq!(problem.status, 422);
        assert_eq!(problem.code, "invalid-provider");

        let problem = instance_problem(&InstanceError::Schema(vec!["url is required".to_owned()]));
        assert_eq!(problem.status, 422);

        let problem = instance_problem(&InstanceError::SlugTaken("hello".to_owned()));
        assert_eq!(problem.status, 409);
        assert_eq!(problem.code, "conflict");
    }
}
