//! The adapter from the verification plugin interface to the core provider
//! traits.
//!
//! This module is the only place where the verification world's WIT records
//! become core types and core types become WIT records. Everything above it sees
//! [`VerificationProvider`] and its core types; everything below it sees
//! generated bindings.

use async_trait::async_trait;
use hearthsound_core::providers::{
    ExpectedTrack, Fingerprint, ProviderLifecycle, Verdict, VerificationProvider,
    VerificationResult,
};
use hearthsound_core::{ProviderError, Slug};

use crate::api::v5::bindings;
use crate::api::v5::bindings::hearthsound::plugin::types::ProviderError as WitProviderError;
use crate::api::v5::bindings::verification::exports::hearthsound::plugin::verification;
use crate::api::v5::catalog::{to_core_id, to_wit_id};
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// A verification provider backed by one plugin instance.
pub(crate) struct WasmVerificationProvider {
    pool: StorePool<bindings::VerificationProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmVerificationProvider {
    /// Wraps a store pool as a verification provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::VerificationProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmVerificationProvider {
        WasmVerificationProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error.into())
    }
}

#[async_trait]
impl ProviderLifecycle for WasmVerificationProvider {
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
impl VerificationProvider for WasmVerificationProvider {
    async fn verify(
        &self,
        expected: &ExpectedTrack,
        print: &Fingerprint,
    ) -> Result<VerificationResult, ProviderError> {
        let expected = to_wit_expected(expected);
        let print = verification::Fingerprint {
            value: print.value.clone(),
            duration_secs: print.duration_secs,
        };
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_verification()
                        .call_verify(store, &expected, &print)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_result)
            .map_err(|error| self.map_error(error))
    }
}

/// Converts a core expected track into the plugin's record.
///
/// # Parameters
///
/// - `expected`: the recording the server asked for.
fn to_wit_expected(expected: &ExpectedTrack) -> verification::ExpectedTrack {
    verification::ExpectedTrack {
        title: expected.title.clone(),
        artists: expected.artists.clone(),
        album: expected.album.clone(),
        duration_ms: expected.duration_ms,
        ids: expected.ids.iter().map(to_wit_id).collect(),
    }
}

/// Converts the plugin's verification result into a core result.
///
/// # Parameters
///
/// - `result`: the result the plugin returned.
fn to_core_result(result: verification::VerificationResult) -> VerificationResult {
    VerificationResult {
        verdict: match result.verdict {
            verification::Verdict::Match => Verdict::Match,
            verification::Verdict::Mismatch => Verdict::Mismatch,
            verification::Verdict::Unknown => Verdict::Unknown,
        },
        score: result.score,
        detail: result.detail,
        identified: result.identified.into_iter().map(to_core_id).collect(),
    }
}
