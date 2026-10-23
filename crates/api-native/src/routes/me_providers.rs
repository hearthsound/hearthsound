//! `/api/v1/me/providers`: the listener's links to scrobble and discovery
//! services.
//!
//! A provider instance whose plugin exports `account-link` may be linked by a
//! listener. The plugin decides how: a `config` plugin takes the fields of its
//! user schema through `PUT .../config`, and a `browser` plugin sends the
//! listener to the service's own authorization page through `POST .../link` and
//! finishes the flow at the callback the service redirects to.
//!
//! A link is one row of `provider_user_configs`, sealed with the server's
//! secret key: the account name the service reported and the configuration the
//! plugin needs for every later call. The listing never returns that
//! configuration; it returns the schema the form is rendered from and the
//! account name, so a token the listener entered is never handed back.
//!
//! Outbox counters are reported as zero and no last error is reported until the
//! scrobble outbox exists, which is what fills them.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::HOST;
use axum::http::request::Parts;
use axum::response::Redirect;
use data_encoding::BASE64URL_NOPAD;
use hearthsound_core::providers::{AccountLinkProvider, LinkMethod, UserContext};
use hearthsound_plugin_host::manifest::ConfigSection;
use hearthsound_plugin_host::{
    RegistryInstance, UserConfig, UserConfigError, validate_user_config,
};
use moka::future::Cache;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::{Authed, request_is_https};
use crate::extract::JsonBody;
use crate::problem::{self, Problem};
use crate::state::NativeState;

/// How long a started browser link waits for the service's callback.
const LINK_STATE_TTL: Duration = Duration::from_secs(600);

/// The most browser links one server remembers at a time.
const LINK_STATE_CAPACITY: u64 = 4096;

/// The path the account menu reads a finished link from.
const ACCOUNT_SERVICES_PATH: &str = "/account/services";

/// The browser link flows that are waiting for a service's callback.
///
/// A nonce is stored with the listener it was issued to and the instance it
/// links, lives for [`LINK_STATE_TTL`] and is removed the first time it is
/// presented, so a callback URL cannot be replayed.
#[derive(Clone)]
pub(crate) struct LinkStates {
    /// The outstanding flows, keyed by their nonce.
    states: Cache<String, (Uuid, String)>,
}

impl LinkStates {
    /// Builds an empty store.
    pub(crate) fn new() -> LinkStates {
        LinkStates {
            states: Cache::builder()
                .time_to_live(LINK_STATE_TTL)
                .max_capacity(LINK_STATE_CAPACITY)
                .build(),
        }
    }

    /// Starts a flow for one listener and instance.
    ///
    /// # Parameters
    ///
    /// - `user_id`: the listener the callback must come from.
    /// - `slug`: the instance being linked.
    ///
    /// # Returns
    ///
    /// The single-use nonce the callback carries.
    pub(crate) async fn start(&self, user_id: Uuid, slug: &str) -> String {
        let mut bytes = [0_u8; 32];
        rand::rng().fill(&mut bytes[..]);
        let nonce = BASE64URL_NOPAD.encode(&bytes);
        self.states
            .insert(nonce.clone(), (user_id, slug.to_owned()))
            .await;
        nonce
    }

    /// Removes one flow and returns what it was issued for.
    ///
    /// # Parameters
    ///
    /// - `nonce`: the `state` the callback carried.
    ///
    /// # Returns
    ///
    /// The listener and instance the flow belonged to, or `None` when the nonce
    /// is unknown or already used.
    pub(crate) async fn take(&self, nonce: &str) -> Option<(Uuid, String)> {
        self.states.remove(nonce).await
    }
}

/// The user configuration form a plugin declares.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserConfigSchema {
    /// The draft-07 JSON Schema of the configuration object.
    #[schema(value_type = Object)]
    pub schema: Value,
    /// The UI schema the form is rendered from.
    #[schema(value_type = Object)]
    pub ui_schema: Value,
}

/// One service a listener may link.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderLink {
    /// The provider instance slug.
    pub slug: String,
    /// The name shown in the account menu.
    pub display_name: String,
    /// The plugin the instance is configured from.
    pub plugin_id: String,
    /// The layers the instance takes part in, from `scrobble` and `discovery`.
    pub layers: Vec<String>,
    /// How the account is linked: `config` or `browser`.
    pub link_method: String,
    /// The user configuration form, or `null` when the plugin links through the
    /// service's site.
    #[schema(value_type = Option<Object>)]
    pub user_config: Option<UserConfigSchema>,
    /// Whether the listener has linked the service.
    pub linked: bool,
    /// The name of the linked account, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_name: Option<String>,
    /// When the account was linked, in unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_at: Option<i64>,
    /// Scrobbles waiting to be delivered.
    pub pending_scrobbles: u32,
    /// Scrobbles the outbox gave up on.
    pub failed_scrobbles: u32,
    /// The most recent delivery failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// The services a listener may link.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProviderLinks {
    /// The services, ordered by slug.
    pub providers: Vec<ProviderLink>,
}

/// A listener's configuration for one service.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserConfigUpdate {
    /// The fields of the plugin's user configuration schema.
    #[schema(value_type = Object)]
    pub config: Value,
}

/// The address of the service's authorization page.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProviderLinkUrl {
    /// The URL to open so the listener can approve the link.
    pub url: String,
}

/// Lists the services the caller may link.
///
/// One entry per enabled instance whose plugin exports `account-link`, ordered
/// by slug, with the account the caller linked when there is one.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials and `internal` when the
/// instance or link rows cannot be read. Answers `link-failed` when an instance
/// cannot be asked how it links accounts.
#[utoipa::path(
    get,
    path = "/api/v1/me/providers",
    tag = "me",
    responses(
        (status = 200, description = "The services the caller may link", body = ProviderLinks),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "A service could not be asked how it links accounts", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn list_provider_links(
    authed: Authed,
    State(state): State<NativeState>,
) -> Result<Json<ProviderLinks>, Problem> {
    let user = authed.user_id();
    let mut providers = Vec::new();
    for instance in linkable_instances(&state) {
        let method = link_method(&instance).await?;
        let link = state
            .user_configs()
            .get(instance.id(), user)
            .await
            .map_err(link_storage_problem)?;
        providers.push(provider_link(&instance, method, link));
    }
    Ok(Json(ProviderLinks { providers }))
}

/// Stores the caller's configuration for a service that links by configuration.
///
/// The configuration is checked against the plugin's user schema, with the
/// schema's defaults filled in, and the plugin's `verify-account` names the
/// account before anything is stored. A service that links through its own site
/// answers `wrong-link-method`.
///
/// # Errors
///
/// Answers `not-found` for an unknown or non-linkable slug, `wrong-link-method`
/// for a browser service, `invalid-config` when the configuration does not match
/// the schema, `link-failed` when the plugin rejects the configuration or cannot
/// answer, and `internal` when the link cannot be stored.
#[utoipa::path(
    put,
    path = "/api/v1/me/providers/{slug}/config",
    tag = "me",
    params(("slug" = String, Path, description = "The provider instance slug")),
    request_body = UserConfigUpdate,
    responses(
        (status = 200, description = "The stored link", body = ProviderLink),
        (status = 404, description = "No such linkable service", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The service links through the browser", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The configuration cannot be used", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn put_provider_config(
    authed: Authed,
    State(state): State<NativeState>,
    Path(slug): Path<String>,
    JsonBody(body): JsonBody<UserConfigUpdate>,
) -> Result<Json<ProviderLink>, Problem> {
    let instance = linkable(&state, &slug)?;
    let provider = account_link(&instance)?;
    let method = provider
        .get_link_method()
        .await
        .map_err(|error| Problem::link_failed(StatusCode::BAD_GATEWAY, error.to_string()))?;
    if method == LinkMethod::Browser {
        return Err(Problem::wrong_link_method(
            "This service links accounts through its own site.",
        ));
    }
    let Some(section) = instance.manifest().config.user.as_ref() else {
        return Err(Problem::invalid_config(
            "This service declares no user configuration.",
        ));
    };
    let config =
        validate_user_config(&section.schema, &body.config).map_err(|error| match error {
            UserConfigError::Schema(problems) => Problem::invalid_config(problems.join("; ")),
            other => problem::internal("validating a service configuration", &other),
        })?;
    let account = provider
        .verify_account(&user_context(authed.user_id(), &config))
        .await
        .map_err(|error| {
            Problem::link_failed(StatusCode::UNPROCESSABLE_ENTITY, error.to_string())
        })?;
    let stored = parse_account_config(&account.config)?;
    let link = state
        .user_configs()
        .put(instance.id(), authed.user_id(), Some(account.name), &stored)
        .await
        .map_err(link_storage_problem)?;
    tracing::info!(
        user = %authed.user_id(),
        service = %instance.slug(),
        "linked a service with a configuration"
    );
    Ok(Json(provider_link(&instance, method, Some(link))))
}

/// Starts a browser link and answers the URL to open.
///
/// The callback the service sends the listener back to carries a single-use
/// nonce, and the flow is remembered against the caller and the instance for ten
/// minutes.
///
/// # Errors
///
/// Answers `not-found` for an unknown or non-linkable slug, `wrong-link-method`
/// for a configuration service, and `link-failed` when the plugin cannot answer
/// or refuses to start the flow.
#[utoipa::path(
    post,
    path = "/api/v1/me/providers/{slug}/link",
    tag = "me",
    params(("slug" = String, Path, description = "The provider instance slug")),
    responses(
        (status = 200, description = "The authorization URL to open", body = ProviderLinkUrl),
        (status = 404, description = "No such linkable service", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "The service links with a configuration", body = Problem, content_type = "application/problem+json"),
        (status = 502, description = "The service could not start the flow", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn start_provider_link(
    authed: Authed,
    State(state): State<NativeState>,
    Path(slug): Path<String>,
    parts: Parts,
) -> Result<Json<ProviderLinkUrl>, Problem> {
    let instance = linkable(&state, &slug)?;
    let provider = account_link(&instance)?;
    let method = provider
        .get_link_method()
        .await
        .map_err(|error| Problem::link_failed(StatusCode::BAD_GATEWAY, error.to_string()))?;
    if method == LinkMethod::Config {
        return Err(Problem::wrong_link_method(
            "This service links accounts with a configuration.",
        ));
    }
    let nonce = state
        .link_states()
        .start(authed.user_id(), instance.slug().as_str())
        .await;
    let callback = format!(
        "{}/api/v1/me/providers/{}/link/callback?state={nonce}",
        public_base(&state, &parts),
        instance.slug()
    );
    let context = UserContext {
        user_id: authed.user_id().to_string(),
        config: None,
    };
    let url = provider
        .begin_link(&context, &callback)
        .await
        .map_err(|error| Problem::link_failed(StatusCode::BAD_GATEWAY, error.to_string()))?;
    Ok(Json(ProviderLinkUrl { url }))
}

/// Finishes a browser link from the query the service redirected with.
///
/// The route always answers a redirect to the account menu: `<path>?linked=<slug>`
/// when the link was stored and `<path>?linkError=<slug>` for a missing, expired
/// or foreign nonce, or any failure of the plugin.
///
/// # Errors
///
/// Answers `unauthenticated` without credentials; every other outcome is a
/// redirect, because the address is opened in a browser.
#[utoipa::path(
    get,
    path = "/api/v1/me/providers/{slug}/link/callback",
    tag = "me",
    params(
        ("slug" = String, Path, description = "The provider instance slug"),
        ("state" = String, Query, description = "The single-use nonce the link started with"),
    ),
    responses(
        (status = 303, description = "The account menu, with `linked` or `linkError`"),
        (status = 401, description = "No valid credential", body = Problem, content_type = "application/problem+json"),
    ),
    security(("session" = []))
)]
pub async fn provider_link_callback(
    authed: Authed,
    State(state): State<NativeState>,
    Path(slug): Path<String>,
    RawQuery(raw): RawQuery,
) -> Redirect {
    let Ok(instance) = linkable(&state, &slug) else {
        return link_failure(&slug);
    };
    let query: Vec<(String, String)> = form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
        .into_owned()
        .collect();
    let Some(nonce) = query
        .iter()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.clone())
    else {
        return link_failure(&slug);
    };
    let Some((issued_to, issued_slug)) = state.link_states().take(&nonce).await else {
        return link_failure(&slug);
    };
    if issued_to != authed.user_id() || issued_slug != slug {
        return link_failure(&slug);
    }
    let Ok(provider) = account_link(&instance) else {
        return link_failure(&slug);
    };
    match provider.get_link_method().await {
        Ok(LinkMethod::Browser) => {}
        Ok(LinkMethod::Config) | Err(_) => return link_failure(&slug),
    }
    let query: Vec<(String, String)> = query
        .into_iter()
        .filter(|(name, _)| name != "state")
        .collect();
    let context = UserContext {
        user_id: authed.user_id().to_string(),
        config: None,
    };
    let account = match provider.complete_link(&context, &query).await {
        Ok(account) => account,
        Err(error) => {
            tracing::debug!(
                user = %authed.user_id(),
                service = %slug,
                %error,
                "the service refused a browser link"
            );
            return link_failure(&slug);
        }
    };
    let config = match parse_account_config(&account.config) {
        Ok(config) => config,
        Err(_) => return link_failure(&slug),
    };
    match state
        .user_configs()
        .put(instance.id(), issued_to, Some(account.name), &config)
        .await
    {
        Ok(_) => {
            tracing::info!(
                user = %issued_to,
                service = %slug,
                "linked a service through the browser"
            );
            Redirect::to(&format!("{ACCOUNT_SERVICES_PATH}?linked={slug}"))
        }
        Err(error) => {
            tracing::warn!(%error, "could not store a browser link");
            link_failure(&slug)
        }
    }
}

/// Deletes the caller's link to a service.
///
/// # Errors
///
/// Answers `not-found` for an unknown or non-linkable slug or when the caller
/// has not linked the service, and `internal` when the row cannot be removed.
#[utoipa::path(
    delete,
    path = "/api/v1/me/providers/{slug}/link",
    tag = "me",
    params(("slug" = String, Path, description = "The provider instance slug")),
    responses(
        (status = 204, description = "The link was removed"),
        (status = 404, description = "No such linkable service, or not linked", body = Problem, content_type = "application/problem+json"),
    ),
    security(("bearer" = []), ("session" = []))
)]
pub async fn delete_provider_link(
    authed: Authed,
    State(state): State<NativeState>,
    Path(slug): Path<String>,
) -> Result<StatusCode, Problem> {
    let instance = linkable(&state, &slug)?;
    let removed = state
        .user_configs()
        .delete(instance.id(), authed.user_id())
        .await
        .map_err(link_storage_problem)?;
    if !removed {
        return Err(Problem::not_found(
            "The caller has not linked this service.",
        ));
    }
    tracing::info!(
        user = %authed.user_id(),
        service = %instance.slug(),
        "unlinked a service"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Returns every instance a listener can link, ordered by slug.
///
/// # Parameters
///
/// - `state`: the native state.
fn linkable_instances(state: &NativeState) -> Vec<Arc<RegistryInstance>> {
    state
        .services()
        .registry
        .instances()
        .into_iter()
        .filter(|instance| instance.account_link().is_some())
        .collect()
}

/// Returns one linkable instance or answers `not-found`.
///
/// # Parameters
///
/// - `state`: the native state.
/// - `slug`: the instance slug from the path.
fn linkable(state: &NativeState, slug: &str) -> Result<Arc<RegistryInstance>, Problem> {
    linkable_instances(state)
        .into_iter()
        .find(|instance| instance.slug().as_str() == slug)
        .ok_or_else(|| Problem::not_found("No service with that slug can be linked."))
}

/// Returns an instance's account-link provider.
///
/// # Parameters
///
/// - `instance`: the instance, which must be linkable.
fn account_link(instance: &RegistryInstance) -> Result<&Arc<dyn AccountLinkProvider>, Problem> {
    instance
        .account_link()
        .ok_or_else(|| Problem::not_found("No service with that slug can be linked."))
}

/// Asks an instance how a listener links their account.
///
/// # Parameters
///
/// - `instance`: the instance to ask.
///
/// # Errors
///
/// Answers `link-failed` when the plugin cannot answer.
async fn link_method(instance: &RegistryInstance) -> Result<LinkMethod, Problem> {
    let provider = account_link(instance)?;
    provider
        .get_link_method()
        .await
        .map_err(|error| Problem::link_failed(StatusCode::BAD_GATEWAY, error.to_string()))
}

/// Renders one instance's link as the account menu reads it.
///
/// # Parameters
///
/// - `instance`: the instance.
/// - `method`: how it links accounts.
/// - `link`: the caller's link, when there is one.
fn provider_link(
    instance: &RegistryInstance,
    method: LinkMethod,
    link: Option<UserConfig>,
) -> ProviderLink {
    let mut layers = Vec::new();
    if instance.scrobble().is_some() {
        layers.push("scrobble".to_owned());
    }
    if instance.discovery().is_some() {
        layers.push("discovery".to_owned());
    }
    let user_config = match method {
        LinkMethod::Config => instance.manifest().config.user.as_ref().map(config_schema),
        LinkMethod::Browser => None,
    };
    let (linked, account_name, linked_at) = match link {
        Some(link) => (true, link.account_name, Some(link.linked_at)),
        None => (false, None, None),
    };
    ProviderLink {
        slug: instance.slug().as_str().to_owned(),
        display_name: instance.manifest().name.clone(),
        plugin_id: instance.plugin_id().to_owned(),
        layers,
        link_method: match method {
            LinkMethod::Config => "config".to_owned(),
            LinkMethod::Browser => "browser".to_owned(),
        },
        user_config,
        linked,
        account_name,
        linked_at,
        pending_scrobbles: 0,
        failed_scrobbles: 0,
        last_error: None,
    }
}

/// Renders a manifest's user configuration section.
///
/// # Parameters
///
/// - `section`: the section the plugin declares.
fn config_schema(section: &ConfigSection) -> UserConfigSchema {
    UserConfigSchema {
        schema: section.schema.clone(),
        ui_schema: section.ui_schema.clone(),
    }
}

/// Builds the context a plugin call is made with.
///
/// # Parameters
///
/// - `user_id`: the listener's id.
/// - `config`: the user configuration, as an object.
fn user_context(user_id: Uuid, config: &Value) -> UserContext {
    UserContext {
        user_id: user_id.to_string(),
        config: Some(config.to_string()),
    }
}

/// Reads the configuration a plugin returned for a link.
///
/// # Parameters
///
/// - `text`: the plugin's `config`, as JSON text.
///
/// # Errors
///
/// Answers `link-failed` when the text is not a JSON object.
fn parse_account_config(text: &str) -> Result<Value, Problem> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        Problem::link_failed(
            StatusCode::BAD_GATEWAY,
            format!("The service returned a configuration that is not JSON: {error}"),
        )
    })?;
    if !value.is_object() {
        return Err(Problem::link_failed(
            StatusCode::BAD_GATEWAY,
            "The service returned a configuration that is not an object.",
        ));
    }
    Ok(value)
}

/// Maps a user-config error from storage onto a problem.
///
/// # Parameters
///
/// - `error`: what the service reported.
fn link_storage_problem(error: UserConfigError) -> Problem {
    match error {
        UserConfigError::Config(detail) => problem::internal("opening a stored link", &detail),
        UserConfigError::Schema(problems) => Problem::invalid_config(problems.join("; ")),
        other => problem::internal("reading a provider link", &other),
    }
}

/// Returns the address the service redirects the listener back to.
///
/// `server.base_url` wins when the operator set it; otherwise the request's
/// `Host` header is used, with the scheme HTTPS implies for a trusted proxy or
/// `server.base_url` says.
///
/// # Parameters
///
/// - `state`: the native state, which holds the configured base URL.
/// - `parts`: the request head, which supplies `Host` and the peer address.
fn public_base(state: &NativeState, parts: &Parts) -> String {
    let configured = state.services().base_url.trim().trim_end_matches('/');
    if !configured.is_empty() {
        return configured.to_owned();
    }
    let scheme = if request_is_https(
        parts,
        &state.services().trusted_proxies,
        state.services().base_url_is_https,
    ) {
        "https"
    } else {
        "http"
    };
    let host = parts
        .headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    format!("{scheme}://{host}")
}

/// Answers the account menu with a failed link.
///
/// # Parameters
///
/// - `slug`: the instance whose link failed.
fn link_failure(slug: &str) -> Redirect {
    Redirect::to(&format!("{ACCOUNT_SERVICES_PATH}?linkError={slug}"))
}
