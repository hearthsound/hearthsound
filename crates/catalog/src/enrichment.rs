//! Background enrichment of the catalog extras a page leaves out.
//!
//! A browse page never waits for the catalog: it answers from the library and
//! whatever the catalog layers already cached, and asks for the missing extras
//! through this module. One [`CatalogService::enrich`] run does the provider work
//! the page would have done - the artist, the discography, the top tracks, the
//! biography, the portraits, or the album tracklist - and the request that asked
//! for it is answered by a later visit.
//!
//! The work is requested once per `(entity, listener)` pair: [`ENRICH_DEDUPE_TTL`]
//! keeps the page's burst of reads (the artist, its information, its top songs)
//! from queueing three jobs, and a completed run releases the key so a later
//! visit may ask again once the cached answers have aged out.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hearthsound_core::providers::{ArtistInfoRequest, ArtistSeed, ExternalId, MB_ARTIST};
use hearthsound_core::{ClientId, EntityId, EntityKind, IdPrefix};
use hearthsound_library::query;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CatalogError;
use crate::pages::{MAX_PAGE_ALBUMS, PageDepth};
use crate::service::CatalogService;

/// The job kind that fetches an entity's missing catalog extras.
pub const ENRICH_JOB_KIND: &str = "catalog.enrich";

/// How many top tracks the enrichment run caches; the vendor top-songs reads
/// fall back to this key and truncate to what they were asked for.
pub const TOP_TRACKS_CACHE_LIMIT: u32 = 100;

/// How long a requested enrichment suppresses a second job for the same
/// `(entity, listener)` pair.
const ENRICH_DEDUPE_TTL: Duration = Duration::from_secs(30 * 60);

/// The payload of a `catalog.enrich` job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichJobPayload {
    /// The client id of the entity whose page asked, exactly as it was read.
    pub entity_id: String,
    /// The entity kind: `artist` or `album`.
    pub kind: EntityKind,
    /// The listener whose page asked; a discovery top-tracks warm-up is personal.
    pub user_id: Uuid,
}

/// What one enrichment run established.
#[derive(Debug, Clone, Default)]
pub struct Enrichment {
    /// The artist a discovery warm-up can be seeded with, for an artist target.
    pub artist: Option<ArtistSeed>,
    /// Whether a catalog entity answered, i.e. whether an open page can show more.
    pub enriched: bool,
}

/// The key one enrichment request is deduplicated by.
type EnrichmentKey = (String, String, Uuid);

impl CatalogService {
    /// Fetches the catalog extras one entity's page leaves out.
    ///
    /// The work depends on what the id names: a stored artist is paired with its
    /// catalog artist and that artist's discography, top tracks, biography and
    /// portraits are fetched; a stored album is paired and its tracklist read; a
    /// catalog artist or album is read with the listings its page shows.
    ///
    /// The discography and the album tracklist fail the run so the job retries;
    /// the extras that are additions - top tracks, biography, portraits - are
    /// logged and skipped, because a provider that does not know an artist must
    /// not keep its page from being enriched forever.
    ///
    /// # Parameters
    ///
    /// - `id`: the entity whose page asked.
    ///
    /// # Returns
    ///
    /// What the run established: whether a catalog entity answered, and the seed
    /// a discovery warm-up can use.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read fails and when the discography
    /// or the album tracklist could not be fetched.
    pub async fn enrich(&self, id: &ClientId) -> Result<Enrichment, CatalogError> {
        let layer = self.catalog_layer();
        match id {
            ClientId::Entity(entity) if entity.kind() == IdPrefix::Artist => {
                let Some(catalog) = self
                    .catalog_artist_of_stored(*entity, &layer, PageDepth::Full)
                    .await?
                else {
                    return Ok(Enrichment::default());
                };
                self.artist_albums(&catalog.id, MAX_PAGE_ALBUMS, 0).await?;
                self.warm_artist_extras(*entity, &catalog).await;
                Ok(Enrichment {
                    artist: Some(self.seed_of(*entity, &catalog).await),
                    enriched: true,
                })
            }
            ClientId::Entity(entity) if entity.kind() == IdPrefix::Album => {
                let detail = {
                    let mut conn = self.database().readers().acquire().await?;
                    query::album_detail_any(&mut conn, *entity).await?
                };
                let Some(detail) = detail else {
                    return Ok(Enrichment::default());
                };
                let Some(reference) = self
                    .catalog_album_of_stored(*entity, &detail, &layer, PageDepth::Full)
                    .await?
                else {
                    return Ok(Enrichment::default());
                };
                self.album(&reference.id).await?;
                Ok(Enrichment {
                    artist: None,
                    enriched: true,
                })
            }
            ClientId::Catalog(catalog) if catalog.kind() == EntityKind::Artist => {
                let found = self.artist(catalog).await?;
                self.artist_albums(&found.id, MAX_PAGE_ALBUMS, 0).await?;
                self.artist_top_tracks(&found.id, TOP_TRACKS_CACHE_LIMIT)
                    .await
                    .map(|_| ())
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            artist = %found.id,
                            "the catalog could not list an artist's top tracks"
                        )
                    });
                let request = ArtistInfoRequest {
                    name: found.artist.name.clone(),
                    ids: found.artist.ids.clone(),
                };
                self.artist_info(&request)
                    .await
                    .map(|_| ())
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            artist = %found.id,
                            "the catalog could not read an artist's information"
                        )
                    });
                self.artist_portraits(id)
                    .await
                    .map(|_| ())
                    .unwrap_or_else(|error| {
                        tracing::warn!(
                            %error,
                            artist = %found.id,
                            "the catalog could not describe an artist's portraits"
                        )
                    });
                Ok(Enrichment {
                    artist: Some(ArtistSeed {
                        name: found.artist.name,
                        ids: found.artist.ids,
                    }),
                    enriched: true,
                })
            }
            ClientId::Catalog(catalog) if catalog.kind() == EntityKind::Album => {
                let found = self.album(catalog).await?;
                // The artist page has its own discography: reading it here means
                // the album page that was opened also fills the artist it links
                // to, so that page does not pay for the same listing again.
                let artist = found
                    .album
                    .artists
                    .first()
                    .and_then(hearthsound_core::providers::credit_provider_id)
                    .filter(|external| external.namespace == catalog.slug().as_str())
                    .and_then(|external| {
                        hearthsound_core::CatalogId::new(
                            catalog.slug().clone(),
                            EntityKind::Artist,
                            external.value.clone(),
                        )
                        .ok()
                    });
                if let Some(artist) = artist {
                    self.artist_albums(&artist, MAX_PAGE_ALBUMS, 0)
                        .await
                        .map(|_| ())
                        .unwrap_or_else(|error| {
                            tracing::warn!(
                                %error,
                                artist = %artist,
                                "the catalog could not list an album artist's discography"
                            )
                        });
                }
                Ok(Enrichment {
                    artist: None,
                    enriched: true,
                })
            }
            _ => Ok(Enrichment::default()),
        }
    }

    /// Fetches the extras of a stored artist: top tracks, biography, portraits.
    ///
    /// A failure of any one is logged and skipped; the discography is the only
    /// listing whose failure fails the run, and the caller fetches it.
    ///
    /// # Parameters
    ///
    /// - `artist`: the stored artist.
    /// - `catalog`: the catalog artist it pairs with.
    async fn warm_artist_extras(
        &self,
        artist: EntityId,
        catalog: &crate::service::CatalogArtistResult,
    ) {
        self.artist_top_tracks(&catalog.id, TOP_TRACKS_CACHE_LIMIT)
            .await
            .map(|_| ())
            .unwrap_or_else(|error| {
                tracing::warn!(
                    %error,
                    artist = %catalog.id,
                    "the catalog could not list an artist's top tracks"
                )
            });
        self.artist_info(&self.info_request(artist, catalog).await)
            .await
            .map(|_| ())
            .unwrap_or_else(|error| {
                tracing::warn!(
                    %error,
                    artist = %catalog.id,
                    "the metadata layer could not describe an artist"
                )
            });
        self.artist_portraits(&ClientId::Catalog(catalog.id.clone()))
            .await
            .map(|_| ())
            .unwrap_or_else(|error| {
                tracing::warn!(
                    %error,
                    artist = %catalog.id,
                    "the artwork layer could not describe an artist's portraits"
                )
            });
    }

    /// Builds the information request of a stored artist, merged with its catalog
    /// identifiers.
    ///
    /// # Parameters
    ///
    /// - `artist`: the stored artist.
    /// - `catalog`: the catalog artist it pairs with.
    ///
    /// # Returns
    ///
    /// The name and the identifiers the layer is asked with.
    async fn info_request(
        &self,
        artist: EntityId,
        catalog: &crate::service::CatalogArtistResult,
    ) -> ArtistInfoRequest {
        let stored = {
            let mut conn = match self.database().readers().acquire().await {
                Ok(conn) => conn,
                Err(error) => {
                    tracing::warn!(%error, artist = %artist, "a stored artist could not be read");
                    return ArtistInfoRequest {
                        name: catalog.artist.name.clone(),
                        ids: catalog.artist.ids.clone(),
                    };
                }
            };
            match hearthsound_library::artists::get_artist(&mut conn, artist).await {
                Ok(stored) => stored,
                Err(error) => {
                    tracing::warn!(%error, artist = %artist, "a stored artist could not be read");
                    None
                }
            }
        };
        let mut ids: Vec<ExternalId> = Vec::new();
        if let Some(stored) = &stored
            && let Some(mbid) = stored.mb_artist_id.as_deref().filter(|id| !id.is_empty())
        {
            ids.push(ExternalId {
                namespace: MB_ARTIST.to_owned(),
                value: mbid.to_owned(),
            });
        }
        for id in &catalog.artist.ids {
            if !ids
                .iter()
                .any(|known| known.namespace == id.namespace && known.value == id.value)
            {
                ids.push(id.clone());
            }
        }
        ArtistInfoRequest {
            name: stored
                .map(|stored| stored.name)
                .unwrap_or_else(|| catalog.artist.name.clone()),
            ids,
        }
    }

    /// Returns the discovery seed of a stored artist paired with its catalog one.
    ///
    /// # Parameters
    ///
    /// - `artist`: the stored artist.
    /// - `catalog`: the catalog artist it pairs with.
    async fn seed_of(
        &self,
        artist: EntityId,
        catalog: &crate::service::CatalogArtistResult,
    ) -> ArtistSeed {
        let request = self.info_request(artist, catalog).await;
        ArtistSeed {
            name: request.name,
            ids: request.ids,
        }
    }

    /// Claims the right to enrich one entity for one listener.
    ///
    /// The map is pruned on every call, so an abandoned request cannot suppress
    /// a later one forever, and the lock is held across the map operation only.
    ///
    /// # Parameters
    ///
    /// - `id`: the entity whose page asks.
    /// - `user`: the listener.
    ///
    /// # Returns
    ///
    /// `true` for the caller that claimed the pair, `false` while another request
    /// holds it.
    pub async fn begin_enrichment(&self, id: &ClientId, user: Uuid) -> bool {
        let Some(key) = enrichment_key(id, user) else {
            return false;
        };
        let now = Instant::now();
        let mut enriching = lock(self.enriching());
        enriching.retain(|_, started| now.duration_since(*started) < ENRICH_DEDUPE_TTL);
        if enriching.contains_key(&key) {
            return false;
        }
        enriching.insert(key, now);
        true
    }

    /// Releases a claimed enrichment after a successful run.
    ///
    /// # Parameters
    ///
    /// - `id`: the entity whose page asked.
    /// - `user`: the listener.
    pub async fn finish_enrichment(&self, id: &ClientId, user: Uuid) {
        let Some(key) = enrichment_key(id, user) else {
            return;
        };
        lock(self.enriching()).remove(&key);
    }
}

/// Builds the deduplication key of one enrichment request.
///
/// # Parameters
///
/// - `id`: the entity.
/// - `user`: the listener.
///
/// # Returns
///
/// The key, or `None` when the id names no entity kind a page enriches.
fn enrichment_key(id: &ClientId, user: Uuid) -> Option<EnrichmentKey> {
    let kind = match id {
        ClientId::Entity(entity) => match entity.kind() {
            IdPrefix::Artist => "artist",
            IdPrefix::Album => "album",
            _ => return None,
        },
        ClientId::Catalog(catalog) => match catalog.kind() {
            EntityKind::Artist => "artist",
            EntityKind::Album => "album",
            _ => return None,
        },
    };
    Some((kind.to_owned(), id.to_string(), user))
}

/// Locks a mutex, ignoring poisoning.
///
/// # Parameters
///
/// - `mutex`: the map to lock.
///
/// # Returns
///
/// The guard.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
