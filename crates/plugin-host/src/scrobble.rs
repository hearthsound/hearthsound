//! The adapter from the scrobble plugin interface to the core provider traits.
//!
//! This module is the only place where the scrobble world's WIT records become
//! core types and core types become WIT records. Everything above it sees
//! [`ScrobbleProvider`] and its core types; everything below it sees generated
//! bindings.

use std::convert::Infallible;

use async_trait::async_trait;
use hearthsound_core::providers::{
    FeedbackValue, Listen, ProviderLifecycle, ScrobbleLimits, ScrobbleProvider, UserContext,
};
use hearthsound_core::{ProviderError, Slug};

use crate::bindings;
use crate::bindings::hearthsound::plugin::types::{
    ProviderError as WitProviderError, UserContext as WitUserContext,
};
use crate::bindings::scrobble::exports::hearthsound::plugin::scrobble;
use crate::catalog::to_wit_id;
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// A scrobble provider backed by one plugin instance.
pub(crate) struct WasmScrobbleProvider {
    pool: StorePool<bindings::ScrobbleProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmScrobbleProvider {
    /// Wraps a store pool as a scrobble provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::ScrobbleProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmScrobbleProvider {
        WasmScrobbleProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error)
    }
}

#[async_trait]
impl ProviderLifecycle for WasmScrobbleProvider {
    async fn validate_config(&self, server_config: &str) -> Result<Vec<String>, ProviderError> {
        let server_config = server_config.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_lifecycle()
                        .call_validate_config(store, &server_config)
                        .await
                })
            })
            .await?;

        Ok(match outcome {
            Ok(()) => Vec::new(),
            Err(problems) => problems,
        })
    }

    async fn health(&self) -> Result<String, ProviderError> {
        let outcome = self
            .pool
            .call(|store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_lifecycle()
                        .call_health(store)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }
}

#[async_trait]
impl ScrobbleProvider for WasmScrobbleProvider {
    async fn get_limits(&self) -> Result<ScrobbleLimits, ProviderError> {
        let outcome = self
            .pool
            .call(|store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_scrobble()
                        .call_get_limits(store)
                        .await
                        .map(Ok::<_, Infallible>)
                })
            })
            .await?;

        Ok(to_core_limits(outcome.map_err(|never| match never {})?))
    }

    async fn now_playing(&self, user: &UserContext, track: &Listen) -> Result<(), ProviderError> {
        let user = to_wit_user(user);
        let track = to_wit_listen(track);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_scrobble()
                        .call_now_playing(store, &user, &track)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }

    async fn submit_listens(
        &self,
        user: &UserContext,
        listens: &[Listen],
    ) -> Result<(), ProviderError> {
        let user = to_wit_user(user);
        let listens: Vec<scrobble::Listen> = listens.iter().map(to_wit_listen).collect();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_scrobble()
                        .call_submit_listens(store, &user, &listens)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }

    async fn send_feedback(
        &self,
        user: &UserContext,
        track: &Listen,
        value: FeedbackValue,
    ) -> Result<(), ProviderError> {
        let user = to_wit_user(user);
        let track = to_wit_listen(track);
        let value = to_wit_feedback(value);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_scrobble()
                        .call_send_feedback(store, &user, &track, value)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }
}

/// Converts a core user context into the plugin's record.
///
/// # Parameters
///
/// - `user`: the listener the call is made for.
fn to_wit_user(user: &UserContext) -> WitUserContext {
    WitUserContext {
        user_id: user.user_id.clone(),
        config: user.config.clone(),
    }
}

/// Converts a core listen into the plugin's record.
///
/// # Parameters
///
/// - `listen`: the listen to convert.
fn to_wit_listen(listen: &Listen) -> scrobble::Listen {
    scrobble::Listen {
        title: listen.title.clone(),
        artists: listen.artists.clone(),
        album: listen.album.clone(),
        album_artist: listen.album_artist.clone(),
        duration_ms: listen.duration_ms,
        track_number: listen.track_number,
        ids: listen.ids.iter().map(to_wit_id).collect(),
        listened_at: listen.listened_at,
        client: listen.client.clone(),
    }
}

/// Converts the plugin's limits into core limits.
///
/// # Parameters
///
/// - `limits`: the limits the plugin reported.
fn to_core_limits(limits: scrobble::ScrobbleLimits) -> ScrobbleLimits {
    ScrobbleLimits {
        max_batch: limits.max_batch,
        min_interval_ms: limits.min_interval_ms,
        feedback: limits.feedback,
    }
}

/// Converts a core feedback value into the plugin's value.
///
/// # Parameters
///
/// - `value`: the opinion to convert.
fn to_wit_feedback(value: FeedbackValue) -> scrobble::FeedbackValue {
    match value {
        FeedbackValue::Love => scrobble::FeedbackValue::Love,
        FeedbackValue::Hate => scrobble::FeedbackValue::Hate,
        FeedbackValue::Clear => scrobble::FeedbackValue::Clear,
    }
}
