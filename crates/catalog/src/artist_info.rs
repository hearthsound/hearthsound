//! Artist information through the metadata layer.
//!
//! A client asks `getArtistInfo` for an artist, and the server answers with a
//! biography and the pages about them. Nothing in the catalog layer holds that:
//! a metadata provider is asked, in the order the administrator set, and the
//! first instance that knows the artist answers. An instance that fails is
//! logged and skipped, because a biography is an addition to an answer the
//! server can already give.

use std::time::Duration;

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_core::Layer;
use hearthsound_core::providers::{ArtistInfo, ArtistInfoRequest};

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
        let key = RequestKey::artist_info(request);

        for instance in instances {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            if let Some(cached) = self.cached_artist_info(instance.id(), &key).await {
                match cached {
                    Some(info) => return Ok(Some(info)),
                    None => continue,
                }
            }
            let asked = tokio::time::timeout(timeout, provider.artist_info(request)).await;
            let info = match asked {
                Ok(Ok(Some(info))) => info,
                Ok(Ok(None)) => {
                    // The instance does not know the artist; the next one may.
                    self.store_artist_info(instance.id(), &key, None, EMPTY_ARTIST_INFO_TTL)
                        .await;
                    continue;
                }
                Ok(Err(error)) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a metadata instance could not answer an artist lookup"
                    );
                    continue;
                }
                Err(_) => {
                    tracing::warn!(
                        instance = instance.slug().as_str(),
                        "a metadata instance did not answer in time"
                    );
                    continue;
                }
            };
            self.store_artist_info(instance.id(), &key, Some(&info), ARTIST_INFO_TTL)
                .await;
            return Ok(Some(info));
        }
        Ok(None)
    }

    /// Reads a cached artist-info answer.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `key`: the artist's cache key.
    ///
    /// # Returns
    ///
    /// The answer, or `None` when the cache does not hold one.
    async fn cached_artist_info(
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

    /// Stores an artist-info answer.
    ///
    /// A failure is logged and ignored: the caller already holds the answer, and
    /// a biography must not fail because its cache could not be written.
    ///
    /// # Parameters
    ///
    /// - `instance`: the instance that answered.
    /// - `key`: the artist's cache key.
    /// - `info`: the answer, or `None` when the instance did not know the
    ///   artist.
    /// - `ttl`: how long the answer stays fresh.
    async fn store_artist_info(
        &self,
        instance: uuid::Uuid,
        key: &RequestKey,
        info: Option<&ArtistInfo>,
        ttl: Duration,
    ) {
        let wire = WireArtistInfoAnswer(info.map(WireArtistInfo::from));
        self.cache().put(instance, key, &wire, ttl).await;
    }
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
