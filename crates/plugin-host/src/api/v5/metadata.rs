//! The adapter from the metadata plugin interface to the core provider traits.
//!
//! This module is the only place where the metadata world's WIT records become
//! core types and core types become WIT records. Everything above it sees
//! [`MetadataProvider`] and its core types; everything below it sees generated
//! bindings.

use async_trait::async_trait;
use hearthsound_core::providers::{
    ArtistInfo, ArtistInfoRequest, ArtistLink, CatalogAlbum, ExternalId, MetadataProvider,
    ProviderLifecycle, RecordingHint, RecordingMatch, ReleaseSummary,
};
use hearthsound_core::{ProviderError, Slug};

use crate::api::v5::bindings;
use crate::api::v5::bindings::hearthsound::plugin::types::{
    IdPair, ProviderError as WitProviderError,
};
use crate::api::v5::bindings::metadata::exports::hearthsound::plugin::metadata;
use crate::api::v5::catalog::{to_core_album, to_core_credit, to_core_id, to_wit_id};
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// A metadata provider backed by one plugin instance.
pub(crate) struct WasmMetadataProvider {
    pool: StorePool<bindings::MetadataProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmMetadataProvider {
    /// Wraps a store pool as a metadata provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::MetadataProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmMetadataProvider {
        WasmMetadataProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error.into())
    }
}

#[async_trait]
impl ProviderLifecycle for WasmMetadataProvider {
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
impl MetadataProvider for WasmMetadataProvider {
    async fn resolve_recording(
        &self,
        hint: &RecordingHint,
    ) -> Result<Vec<RecordingMatch>, ProviderError> {
        let hint = to_wit_hint(hint);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_metadata()
                        .call_resolve_recording(store, &hint)
                        .await
                })
            })
            .await?;

        outcome
            .map(|matches| matches.into_iter().map(to_core_match).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn get_release(&self, ids: &[ExternalId]) -> Result<CatalogAlbum, ProviderError> {
        let ids: Vec<IdPair> = ids.iter().map(to_wit_id).collect();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_metadata()
                        .call_get_release(store, &ids)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_album)
            .map_err(|error| self.map_error(error))
    }

    async fn artist_info(
        &self,
        request: &ArtistInfoRequest,
    ) -> Result<Option<ArtistInfo>, ProviderError> {
        let request = metadata::ArtistInfoRequest {
            name: request.name.clone(),
            ids: request.ids.iter().map(to_wit_id).collect(),
        };
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_metadata()
                        .call_get_artist_info(store, &request)
                        .await
                })
            })
            .await?;

        outcome
            .map(|info| info.map(to_core_artist_info))
            .map_err(|error| self.map_error(error))
    }
}

/// Converts the plugin's artist information into core artist information.
fn to_core_artist_info(info: metadata::ArtistInfo) -> ArtistInfo {
    ArtistInfo {
        ids: info.ids.into_iter().map(to_core_id).collect(),
        biography: info.biography,
        links: info
            .links
            .into_iter()
            .map(|link| ArtistLink {
                kind: link.kind,
                url: link.url,
            })
            .collect(),
    }
}

/// Converts a core recording hint into the plugin's hint record.
fn to_wit_hint(hint: &RecordingHint) -> metadata::RecordingHint {
    metadata::RecordingHint {
        title: hint.title.clone(),
        artists: hint.artists.clone(),
        album: hint.album.clone(),
        duration_ms: hint.duration_ms,
        isrc: hint.isrc.clone(),
    }
}

/// Converts the plugin's recording match into a core recording match.
fn to_core_match(record: metadata::RecordingMatch) -> RecordingMatch {
    RecordingMatch {
        ids: record.ids.into_iter().map(to_core_id).collect(),
        title: record.title,
        artists: record.artists.into_iter().map(to_core_credit).collect(),
        duration_ms: record.duration_ms,
        releases: record.releases.into_iter().map(to_core_release).collect(),
        score: record.score,
    }
}

/// Converts the plugin's release summary into a core release summary.
fn to_core_release(record: metadata::ReleaseSummary) -> ReleaseSummary {
    ReleaseSummary {
        ids: record.ids.into_iter().map(to_core_id).collect(),
        title: record.title,
        date: record.date,
        track_count: record.track_count,
    }
}
