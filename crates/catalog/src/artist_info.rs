//! Artist information through the metadata layer.
//!
//! A client asks `getArtistInfo` for an artist, and the server answers with a
//! biography and the pages about them. Nothing in the catalog layer holds that:
//! a metadata provider is asked, in the order the administrator set, and the
//! first instance that knows the artist answers. An instance that fails is
//! logged and skipped, because a biography is an addition to an answer the
//! server can already give.
//!
//! # One answer, several keys
//!
//! The identifiers a caller holds for an artist are not the ones another caller
//! holds, and none of them is required: a stored artist knows its MusicBrainz
//! id, a catalog artist knows whatever its instance reported (Deezer reports
//! none at all), and the page that opens either one asks with what it has. The
//! answer is therefore stored under the request's own key and under one key per
//! identifier plus one key carrying the name alone, and a read tries the same
//! set in that order. Without the aliases a biography cached by the enrichment
//! run - which merges the stored id with the catalog ones - would be invisible
//! to the page that asks with only the stored id, which is exactly the "the
//! description shows once and is gone after a refresh" defect: the write and the
//! read disagreed about the key, not about the artist.

use std::time::Duration;

use std::sync::Arc;

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::Layer;
use hearthsound_core::providers::{ArtistInfo, ArtistInfoRequest};
use hearthsound_plugin_host::registry::RegistryInstance;

use crate::cache::RequestKey;
use crate::error::CatalogError;
use crate::service::CatalogService;
use crate::wire::{WireArtistInfo, WireArtistInfoAnswer};

/// How long an artist-info answer that found something stays fresh.
const ARTIST_INFO_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long an artist-info answer that found nothing stays fresh.
///
/// An empty answer is cached for less time, because an instance that does not
/// know an artist today may know them tomorrow.
const EMPTY_ARTIST_INFO_TTL: Duration = Duration::from_secs(24 * 60 * 60);

impl CatalogService {
    /// Returns what the metadata layer knows about an artist.
    ///
    /// Every instance in the `metadata` layer is asked, in the order the
    /// administrator set, until one answers: the first instance that knows the
    /// artist wins, and an instance that fails is logged and skipped. The answer
    /// is cached, so a second call costs no provider call.
    ///
    /// # Parameters
    ///
    /// - `request`: the artist's name and the identifiers the server has for
    ///   them.
    ///
    /// # Returns
    ///
    /// The information, or `None` when no instance knows the artist.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Setting`] when `metadata.artist_info.timeout_ms`
    /// cannot be read, and [`CatalogError::Database`] when a cached answer cannot
    /// be read or written. A provider failure is never an error here.
    pub async fn artist_info(
        &self,
        request: &ArtistInfoRequest,
    ) -> Result<Option<ArtistInfo>, CatalogError> {
        let instances = self.registry().layers(Layer::Metadata);
        if instances.is_empty() {
            return Ok(None);
        }
        let timeout = artist_info_timeout(&self.settings_snapshot())?;
        let deadline = tokio::time::Instant::now() + timeout;
        let keys = artist_info_keys(request);

        // Every instance is asked at once and the earliest one in the
        // administrator's order that answers wins, so the call costs one
        // instance's deadline rather than the sum of them.
        let answers = futures_util::future::join_all(
            instances
                .iter()
                .map(|instance| self.artist_info_of(instance, &keys, request, deadline)),
        )
        .await;
        Ok(answers.into_iter().flatten().next())
    }

    /// Asks one metadata instance for an artist, or reads its cached answer.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance to ask.
    /// - `keys`: the artist's cache keys, the request's own first.
    /// - `request`: the artist's name and identifiers.
    /// - `deadline`: the instant the whole fan-out stops waiting.
    ///
    /// # Returns
    ///
    /// The information, or `None` when this instance knows nothing, failed, or did
    /// not answer before the deadline.
    async fn artist_info_of(
        &self,
        instance: &Arc<RegistryInstance>,
        keys: &[RequestKey],
        request: &ArtistInfoRequest,
        deadline: tokio::time::Instant,
    ) -> Option<ArtistInfo> {
        let provider = instance.metadata()?;
        if let Some(cached) = self.cached_artist_info_in(instance.id(), keys).await {
            return cached;
        }
        let asked = tokio::time::timeout_at(deadline, provider.artist_info(request)).await;
        match asked {
            Ok(Ok(Some(info))) => {
                self.store_artist_info(instance.id(), keys, Some(&info), ARTIST_INFO_TTL)
                    .await;
                Some(info)
            }
            Ok(Ok(None)) => {
                // The instance does not know the artist; another one may.
                self.store_artist_info(instance.id(), keys, None, EMPTY_ARTIST_INFO_TTL)
                    .await;
                None
            }
            Ok(Err(error)) => {
                tracing::debug!(
                    instance = instance.slug().as_str(),
                    %error,
                    "a metadata instance could not answer an artist lookup"
                );
                None
            }
            Err(_) => {
                tracing::warn!(
                    instance = instance.slug().as_str(),
                    "a metadata instance did not answer in time"
                );
                None
            }
        }
    }

    /// Reads a cached artist-info answer.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `key`: one artist's cache key.
    ///
    /// # Returns
    ///
    /// The answer, or `None` when the cache does not hold one.
    pub(crate) async fn cached_artist_info_of(
        &self,
        instance: uuid::Uuid,
        key: &RequestKey,
    ) -> Option<Option<ArtistInfo>> {
        match self
            .cache()
            .get::<WireArtistInfoAnswer>(instance, key)
            .await
        {
            Ok(Some(answer)) => Some(answer.0.map(ArtistInfo::from)),
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(%error, "a cached artist answer could not be read");
                None
            }
        }
    }

    /// Reads the first cached artist-info answer among an artist's keys.
    ///
    /// The keys are tried in the order [`artist_info_keys`] builds them, so the
    /// answer stored for exactly the identifiers this caller holds wins over one
    /// stored for another caller's set. An empty answer is an answer: it says the
    /// instance does not know the artist, which is not the same as the cache
    /// missing.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `keys`: the artist's cache keys, the request's own first.
    ///
    /// # Returns
    ///
    /// The answer, or `None` when the cache holds none of the keys.
    pub(crate) async fn cached_artist_info_in(
        &self,
        instance: uuid::Uuid,
        keys: &[RequestKey],
    ) -> Option<Option<ArtistInfo>> {
        for key in keys {
            if let Some(answer) = self.cached_artist_info_of(instance, key).await {
                return Some(answer);
            }
        }
        None
    }

    /// Stores an artist-info answer under every key of the artist.
    ///
    /// A failure is logged and ignored: the caller already holds the answer, and
    /// a biography must not fail because its cache could not be written.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `keys`: the artist's cache keys, the request's own first.
    /// - `info`: the answer, or `None` when the instance did not know the
    ///   artist.
    /// - `ttl`: how long the answer stays fresh.
    async fn store_artist_info(
        &self,
        instance: uuid::Uuid,
        keys: &[RequestKey],
        info: Option<&ArtistInfo>,
        ttl: Duration,
    ) {
        let wire = WireArtistInfoAnswer(info.map(WireArtistInfo::from));
        for key in keys {
            self.cache().put(instance, key, &wire, ttl).await;
        }
    }
}

/// Builds every cache key an artist's answer is stored under.
///
/// The request's own key comes first, then one key per identifier the request
/// carries, then the name alone. The identifiers a caller holds differ by path -
/// a stored artist asks with its MusicBrainz id, a catalog artist with whatever
/// its instance reported, and the enrichment run merges the two - so writing all
/// the keys is what lets a later read find the answer with a different set.
///
/// # Parameters
///
/// - `request`: the artist's name and identifiers.
///
/// # Returns
///
/// The keys, most specific first, without duplicates.
pub(crate) fn artist_info_keys(request: &ArtistInfoRequest) -> Vec<RequestKey> {
    let mut keys = vec![RequestKey::artist_info(request)];
    for id in &request.ids {
        let single = ArtistInfoRequest {
            name: request.name.clone(),
            ids: vec![id.clone()],
        };
        let key = RequestKey::artist_info(&single);
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    let named = ArtistInfoRequest {
        name: request.name.clone(),
        ids: Vec::new(),
    };
    let key = RequestKey::artist_info(&named);
    if !keys.contains(&key) {
        keys.push(key);
    }
    keys
}

/// Reads `metadata.artist_info.timeout_ms` from a snapshot.
///
/// # Parameters
///
/// - `settings`: the settings in effect.
///
/// # Returns
///
/// The deadline for one artist-info lookup.
///
/// # Errors
///
/// Returns [`CatalogError::Setting`] when the snapshot no longer holds a valid
/// value for the key.
fn artist_info_timeout(settings: &SettingsStore) -> Result<Duration, CatalogError> {
    let millis = settings.get(keys::METADATA_ARTIST_INFO_TIMEOUT_MS)?;
    Ok(Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
}
