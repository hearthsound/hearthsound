//! The server information vendor endpoint.
//!
//! `hsGetServerInfo` is how a client feature-detects: it reports the pending
//! mode the server would answer a stream with, whether catalog search is on,
//! whether any acquisition provider is enabled, which provider instances serve
//! each layer, and the vendor features the server implements. Everything it
//! reports is read from the settings and the provider registry at request time,
//! so a change takes effect without a restart.
//!
//! The endpoint belongs to the `hearthsound` vendor extension, so it is answered
//! only when the request carries `hearthsound=1` and answers Subsonic error 70
//! without it.

use hearthsound_config::keys;
use hearthsound_core::Layer;

use crate::auth::Call;
use crate::response::{Encoded, Response, SubsonicError};
use crate::value::{Array, Object};

/// The feature reported while the prefetch scheduler is on.
const PREFETCH_FEATURE: &str = "prefetch";

/// The feature every server of this vendor-extension version reports.
const VENDOR_FEATURE: &str = "vendor-v1";

/// Answers `hsGetServerInfo`: what this server can do.
///
/// # Parameters
///
/// - `call`: the authenticated call; the request must carry `hearthsound=1`.
///
/// # Returns
///
/// A successful envelope carrying `serverInfo`, or a failed envelope.
///
/// # Errors
///
/// Answers Subsonic error 70 when the request does not carry `hearthsound=1`,
/// and a generic error when a setting cannot be read.
pub async fn hs_get_server_info(call: Call) -> Encoded {
    if !call.vendor_enabled() {
        return call.encode(Response::failed(SubsonicError::not_found()));
    }
    let settings = call.state().settings_snapshot();
    let pending_mode = match settings.get(keys::STREAM_PENDING_MODE) {
        Ok(mode) => mode,
        Err(error) => {
            tracing::error!(error = %error, "could not read stream.pending.mode");
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    };
    let catalog_enabled = match settings.get(keys::SEARCH_CATALOG_ENABLED) {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::error!(error = %error, "could not read search.catalog.enabled");
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    };
    let prefetch_enabled = match settings.get(keys::PREFETCH_ENABLED) {
        Ok(enabled) => enabled,
        Err(error) => {
            tracing::error!(error = %error, "could not read prefetch.enabled");
            return call.encode(Response::failed(SubsonicError::generic()));
        }
    };

    let registry = call.state().registry();
    let acquisition_enabled =
        registry.is_some_and(|registry| !registry.layers(Layer::Acquisition).is_empty());
    let layers = match registry {
        Some(registry) => assignments(registry),
        None => Vec::new(),
    };
    let mut features = Vec::new();
    if prefetch_enabled {
        features.push(PREFETCH_FEATURE);
    }
    features.push(VENDOR_FEATURE);

    call.encode(
        Response::ok().member(
            "serverInfo",
            Object::new()
                .set("pendingMode", pending_mode)
                .set("catalogEnabled", catalog_enabled)
                .set("acquisitionEnabled", acquisition_enabled)
                .set("layers", Object::new().set("layer", Array::new(layers)))
                .set("features", Array::named("feature", features)),
        ),
    )
}

/// Builds one entry per layer the registry serves an instance for.
///
/// # Parameters
///
/// - `registry`: the provider registry.
///
/// # Returns
///
/// The layers, in the fixed order of [`Layer::ALL`], each with the slugs of the
/// enabled instances assigned to it in the order the layer tries them. A layer
/// nothing is assigned to is not listed: an empty entry would tell a client that
/// this server can do something it cannot.
fn assignments(registry: &hearthsound_plugin_host::ProviderRegistry) -> Vec<Object> {
    let mut layers = Vec::new();
    for layer in Layer::ALL {
        let instances = registry.layers(layer);
        if instances.is_empty() {
            continue;
        }
        let slugs: Vec<String> = instances
            .iter()
            .map(|instance| instance.slug().as_str().to_owned())
            .collect();
        layers.push(Object::new().set("name", layer.as_str()).set(
            "instances",
            Object::new().set("instance", Array::new(slugs)),
        ));
    }
    layers
}
