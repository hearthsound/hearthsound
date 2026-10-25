//! The adapter from the artwork plugin interface to the core provider traits.
//!
//! This module is the only place where the artwork world's WIT records become
//! core types and core types become WIT records. Everything above it sees
//! [`ArtworkProvider`] and its core types; everything below it sees generated
//! bindings.

use async_trait::async_trait;
use hearthsound_core::providers::{ArtworkProvider, ArtworkSubject, Image, ProviderLifecycle};
use hearthsound_core::{ProviderError, Slug};

use crate::api::v4::bindings;
use crate::api::v4::bindings::artwork::exports::hearthsound::plugin::artwork;
use crate::api::v4::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::api::v4::catalog::{to_wit_id, to_wit_kind};
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// An artwork provider backed by one plugin instance.
pub(crate) struct WasmArtworkProvider {
    pool: StorePool<bindings::ArtworkProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmArtworkProvider {
    /// Wraps a store pool as an artwork provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::ArtworkProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmArtworkProvider {
        WasmArtworkProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error.into())
    }
}

#[async_trait]
impl ProviderLifecycle for WasmArtworkProvider {
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
impl ArtworkProvider for WasmArtworkProvider {
    async fn find_artwork(&self, subject: &ArtworkSubject) -> Result<Vec<Image>, ProviderError> {
        let subject = artwork::ArtworkSubject {
            kind: to_wit_kind(subject.kind),
            provider_id: subject.provider_id.clone(),
            ids: subject.ids.iter().map(to_wit_id).collect(),
            title: subject.title.clone(),
            artists: subject.artists.clone(),
        };
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_artwork()
                        .call_find_artwork(store, &subject)
                        .await
                })
            })
            .await?;

        outcome
            .map(|images| images.into_iter().map(to_core_artwork_image).collect())
            .map_err(|error| self.map_error(error))
    }
}

/// Converts the plugin's image into a core image.
///
/// # Parameters
///
/// - `image`: the image the provider returned.
///
/// # Returns
///
/// The core image.
fn to_core_artwork_image(
    image: crate::api::v4::bindings::hearthsound::plugin::types::Image,
) -> Image {
    Image {
        url: image.url,
        width: image.width,
        height: image.height,
    }
}
