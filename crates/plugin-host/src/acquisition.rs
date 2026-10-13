//! The adapter from the acquisition plugin interface to the core provider
//! traits.
//!
//! This module is the only place where the acquisition world's WIT records
//! become core types and core types become WIT records. Everything above it sees
//! [`AcquisitionProvider`] and its core types; everything below it sees
//! generated bindings.
//!
//! Acquisition is the one layer whose calls carry host state: every call sets the
//! job's staging root on the store before the guest runs, so a `host-files` call
//! under the reserved name `staging` lands in that job's directory. The root is
//! rewritten at the start of every call, and stores never move between layers, so
//! a stale root is never visible.

use std::path::PathBuf;

use async_trait::async_trait;
use hearthsound_core::providers::{
    AcquisitionCapabilities, AcquisitionContext, AcquisitionProvider, AcquisitionQuery, Candidate,
    CandidateFile, DownloadState, OutputFile, Progress, ProviderLifecycle, SearchState, SourceInfo,
    WantedTrack,
};
use hearthsound_core::{Granularity, ProviderError, Slug};

use crate::bindings;
use crate::bindings::acquisition::exports::hearthsound::plugin::acquisition;
use crate::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::error::map_provider_error;
use crate::pool::StorePool;
use crate::state::InstanceState;

/// An acquisition provider backed by one plugin instance.
pub(crate) struct WasmAcquisitionProvider {
    pool: StorePool<bindings::AcquisitionProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmAcquisitionProvider {
    /// Wraps a store pool as an acquisition provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::AcquisitionProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmAcquisitionProvider {
        WasmAcquisitionProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error)
    }
}

/// Marks the store with the staging root of the call about to run.
///
/// # Parameters
///
/// - `store`: the store the guest call will use.
/// - `context`: the job context the call carries.
fn set_staging(store: &mut wasmtime::Store<InstanceState>, context: &AcquisitionContext) {
    store
        .data_mut()
        .set_staging(Some(PathBuf::from(context.staging_root())));
}

#[async_trait]
impl ProviderLifecycle for WasmAcquisitionProvider {
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
impl AcquisitionProvider for WasmAcquisitionProvider {
    async fn get_capabilities(
        &self,
        context: &AcquisitionContext,
    ) -> Result<AcquisitionCapabilities, ProviderError> {
        let context = context.clone();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    match instance
                        .hearthsound_plugin_acquisition()
                        .call_get_capabilities(store)
                        .await
                    {
                        Ok(capabilities) => Ok(Ok(capabilities)),
                        Err(error) => Err(error),
                    }
                })
            })
            .await?;

        outcome
            .map(to_core_capabilities)
            .map_err(|error| self.map_error(error))
    }

    async fn start_search(
        &self,
        context: &AcquisitionContext,
        query: &AcquisitionQuery,
    ) -> Result<String, ProviderError> {
        let context = context.clone();
        let query = to_wit_query(query);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_start_search(store, &query)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }

    async fn poll_search(
        &self,
        context: &AcquisitionContext,
        search_token: &str,
    ) -> Result<SearchState, ProviderError> {
        let context = context.clone();
        let token = search_token.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_poll_search(store, &token)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_search_state)
            .map_err(|error| self.map_error(error))
    }

    async fn cancel_search(&self, context: &AcquisitionContext, search_token: &str) {
        let context = context.clone();
        let token = search_token.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_cancel_search(store, &token)
                        .await?;
                    Ok(Ok::<(), WitProviderError>(()))
                })
            })
            .await;
        if let Err(error) = outcome {
            tracing::debug!(
                target: "hearthsound::plugin",
                plugin = %self.plugin,
                instance = %self.slug,
                error = %error,
                "cancelling a search failed",
            );
        }
    }

    async fn start_download(
        &self,
        context: &AcquisitionContext,
        candidate_token: &str,
        files: &[u32],
    ) -> Result<String, ProviderError> {
        let context = context.clone();
        let token = candidate_token.to_owned();
        let files = files.to_vec();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_start_download(store, &token, &files)
                        .await
                })
            })
            .await?;

        outcome.map_err(|error| self.map_error(error))
    }

    async fn poll_download(
        &self,
        context: &AcquisitionContext,
        download_token: &str,
    ) -> Result<DownloadState, ProviderError> {
        let context = context.clone();
        let token = download_token.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_poll_download(store, &token)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_download_state)
            .map_err(|error| self.map_error(error))
    }

    async fn cancel_download(&self, context: &AcquisitionContext, download_token: &str) {
        let context = context.clone();
        let token = download_token.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    set_staging(store, &context);
                    instance
                        .hearthsound_plugin_acquisition()
                        .call_cancel_download(store, &token)
                        .await?;
                    Ok(Ok::<(), WitProviderError>(()))
                })
            })
            .await;
        if let Err(error) = outcome {
            tracing::debug!(
                target: "hearthsound::plugin",
                plugin = %self.plugin,
                instance = %self.slug,
                error = %error,
                "cancelling a download failed",
            );
        }
    }
}

/// Converts a core query into the plugin's query record.
fn to_wit_query(query: &AcquisitionQuery) -> acquisition::Query {
    acquisition::Query {
        granularity: to_wit_granularity(query.granularity),
        album: query.album.clone(),
        album_artist: query.album_artist.clone(),
        year: query.year,
        tracks: query.tracks.iter().map(to_wit_wanted).collect(),
        preferred_formats: query.preferred_formats.clone(),
    }
}

/// Converts a core granularity into the plugin's enum.
fn to_wit_granularity(granularity: Granularity) -> acquisition::Granularity {
    match granularity {
        Granularity::Track => acquisition::Granularity::Track,
        Granularity::Album => acquisition::Granularity::Album,
    }
}

/// Converts a wanted track into the plugin's record.
fn to_wit_wanted(track: &WantedTrack) -> acquisition::WantedTrack {
    acquisition::WantedTrack {
        title: track.title.clone(),
        artists: track.artists.clone(),
        track_number: track.track_number,
        disc_number: track.disc_number,
        duration_ms: track.duration_ms,
        isrc: track.isrc.clone(),
        ids: track.ids.iter().map(crate::catalog::to_wit_id).collect(),
    }
}

/// Converts the plugin's capabilities into core capabilities.
fn to_core_capabilities(capabilities: acquisition::Capabilities) -> AcquisitionCapabilities {
    AcquisitionCapabilities {
        granularities: capabilities
            .granularities
            .into_iter()
            .map(to_core_granularity)
            .collect(),
        progressive: capabilities.progressive,
    }
}

/// Converts the plugin's granularity into the core enum.
fn to_core_granularity(granularity: acquisition::Granularity) -> Granularity {
    match granularity {
        acquisition::Granularity::Track => Granularity::Track,
        acquisition::Granularity::Album => Granularity::Album,
    }
}

/// Converts the plugin's search state into the core state.
fn to_core_search_state(state: acquisition::SearchState) -> SearchState {
    match state {
        acquisition::SearchState::Running(candidates) => {
            SearchState::Running(candidates.into_iter().map(to_core_candidate).collect())
        }
        acquisition::SearchState::Complete(candidates) => {
            SearchState::Complete(candidates.into_iter().map(to_core_candidate).collect())
        }
    }
}

/// Converts the plugin's candidate into a core candidate.
fn to_core_candidate(candidate: acquisition::Candidate) -> Candidate {
    Candidate {
        token: candidate.token,
        title: candidate.title,
        path: candidate.path,
        files: candidate.files.into_iter().map(to_core_file).collect(),
        source: to_core_source(candidate.source),
    }
}

/// Converts the plugin's candidate file into a core candidate file.
fn to_core_file(file: acquisition::CandidateFile) -> CandidateFile {
    CandidateFile {
        name: file.name,
        size: file.size,
        format: file.format,
        bitrate_kbps: file.bitrate_kbps,
        sample_rate: file.sample_rate,
        bit_depth: file.bit_depth,
        duration_ms: file.duration_ms,
        track_index: file.track_index,
    }
}

/// Converts the plugin's source information into core source information.
fn to_core_source(source: acquisition::SourceInfo) -> SourceInfo {
    SourceInfo {
        key: source.key,
        speed_bps: source.speed_bps,
        queue_length: source.queue_length,
        free_slot: source.free_slot,
    }
}

/// Converts the plugin's download state into the core state.
fn to_core_download_state(state: acquisition::DownloadState) -> DownloadState {
    match state {
        acquisition::DownloadState::Queued(position) => DownloadState::Queued(position),
        acquisition::DownloadState::Running(progress) => {
            DownloadState::Running(to_core_progress(progress))
        }
        acquisition::DownloadState::Complete(files) => DownloadState::Complete(
            files
                .into_iter()
                .map(|file| OutputFile {
                    root: file.root,
                    path: file.path,
                    track_index: file.track_index,
                })
                .collect(),
        ),
        acquisition::DownloadState::Failed(reason) => DownloadState::Failed(reason),
    }
}

/// Converts the plugin's progress into core progress.
fn to_core_progress(progress: acquisition::Progress) -> Progress {
    Progress {
        bytes: progress.bytes,
        total_bytes: progress.total_bytes,
        speed_bps: progress.speed_bps,
    }
}
