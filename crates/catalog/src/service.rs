//! The catalog service: the layer policies over the provider registry.
//!
//! [`CatalogService`] is what the rest of the server calls to reach the catalog
//! layer. It resolves a [`CatalogId`] to the provider instance that issued it,
//! calls that instance through the registry's trait object, and wraps every
//! answer in the id that names it.
//!
//! The catalog policy is deliberately narrow (02-architecture.md, section 5:
//! "first instance primary for search and lookups, later instances only when the
//! primary errors or times out, no result merging, external IDs carry the
//! issuing instance slug"). Search implements exactly that: it walks the layer
//! in the administrator's order, stops at the first instance that answers, and
//! never merges two instances' results into one list. An instance that errors or
//! exceeds its deadline is logged and the next one is tried, because a user is
//! better served by the fallback's results than by an empty list.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hearthsound_config::SettingsStore;
use hearthsound_config::keys;
use hearthsound_config::settings::Settings;
use hearthsound_core::providers::{
    CatalogAlbum, CatalogAlbumRef, CatalogArtist, CatalogProvider, CatalogQuery, CatalogResults,
    CatalogTrack,
};
use hearthsound_core::{CatalogId, EntityKind, Layer, ProviderError, Slug};
use hearthsound_db::Database;
use hearthsound_plugin_host::{ProviderRegistry, RegistryInstance};
use moka::future::Cache as MemoryCache;
use tokio::time::error::Elapsed;

use crate::artwork::REFUSED_IMAGE_TTL;
use crate::cache::{Cache, ENTITY_CACHE_TTL, RequestKey, SEARCH_CACHE_TTL};
use crate::error::CatalogError;
use crate::wire::{WireAlbum, WireAlbumRefs, WireArtist, WireResults, WireTrack, WireTracks};

/// A track a catalog provider returned, with the id that names it.
///
/// The id is the client-facing name of the entity: the issuing instance's slug
/// plus the provider's own track id. Its [`std::fmt::Display`] form is the
/// `x_<slug>_t_<payload>` id a client sends back to stream, save or acquire the
/// track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTrackResult {
    /// The id that names the track.
    pub id: CatalogId,
    /// The track the provider returned.
    pub track: CatalogTrack,
}

/// An album a catalog provider returned, with the id that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAlbumResult {
    /// The id that names the album.
    pub id: CatalogId,
    /// The album and its tracks.
    pub album: CatalogAlbum,
}

/// An album reference a catalog provider returned, with the id that names it.
///
/// A reference is the compact album a search result or an artist listing
/// carries: a title, the credits, a year and the cover images, without the
/// tracklist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAlbumRefResult {
    /// The id that names the album.
    pub id: CatalogId,
    /// The album reference the provider returned.
    pub album: CatalogAlbumRef,
}

/// An artist a catalog provider returned, with the id that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogArtistResult {
    /// The id that names the artist.
    pub id: CatalogId,
    /// The artist the provider returned.
    pub artist: CatalogArtist,
}

/// The catalog half of a search response.
///
/// The results come from one instance only: the primary when it answered, and a
/// fallback instance when it did not. Two instances' results are never merged,
/// so [`CatalogSearch::source`] tells the caller which slug issued every entity
/// in the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogSearch {
    /// Matching tracks, in the provider's order.
    pub tracks: Vec<CatalogTrackResult>,
    /// Matching albums, in the provider's order.
    pub albums: Vec<CatalogAlbumRefResult>,
    /// Matching artists, in the provider's order.
    pub artists: Vec<CatalogArtistResult>,
    /// The instance that answered, or `None` when no instance did.
    pub source: Option<Slug>,
}

impl CatalogSearch {
    /// Returns whether the search produced no candidate at all.
    ///
    /// An empty search is what a caller gets when catalog search is disabled,
    /// when no instance serves the catalog layer, or when every instance failed.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::CatalogSearch;
    ///
    /// assert!(CatalogSearch::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.albums.is_empty() && self.artists.is_empty()
    }
}

/// The catalog layer over the provider registry.
///
/// One service is built by the composition root and shared through an `Arc`.
/// Cloning a service is cheap: every clone shares the same registry, database
/// and settings service.
///
/// # Examples
///
/// ```no_run
/// use std::sync::Arc;
///
/// use hearthsound_catalog::CatalogService;
/// use hearthsound_core::{CatalogId, EntityKind, Slug};
///
/// # async fn example(
/// #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
/// #     database: Arc<hearthsound_db::Database>,
/// #     settings: hearthsound_config::settings::Settings,
/// # ) -> Result<(), Box<dyn std::error::Error>> {
/// let catalog = CatalogService::new(registry, database, settings, "/srv/hearthsound/artwork");
/// let id = CatalogId::new(Slug::new("deezer").unwrap(), EntityKind::Track, "3135556")?;
/// let track = catalog.track(&id).await?;
/// println!("{}", track.track.title);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct CatalogService {
    /// The shared state every clone holds.
    inner: Arc<Inner>,
}

/// The state one [`CatalogService`] shares with its clones.
struct Inner {
    /// The provider instances, ordered by layer assignment.
    registry: Arc<ProviderRegistry>,
    /// The database that holds the alias and cache tables.
    database: Arc<Database>,
    /// The settings service, read once per call.
    settings: Settings,
    /// The two-level cache of provider responses.
    cache: Cache,
    /// The directory cached cover images are written to.
    artwork_dir: PathBuf,
    /// The image URLs an image host answered as missing, and when to retry.
    ///
    /// A missing picture is not a transient refusal: the provider described an
    /// image it cannot deliver. The URL is answered with not-found without
    /// being asked again within the same window.
    missing_images: MemoryCache<String, ()>,
    /// The image URLs an image host refused, and when the refusal expires.
    ///
    /// An image host that answers a burst with a refusal answers the next
    /// request of that burst the same way, so a URL that was refused is not
    /// asked again until its entry expires. A tile grid of a hundred covers
    /// therefore costs one refusal per image, not one per visit.
    refused_images: MemoryCache<String, ()>,
}

/// How many image URLs the refusal window remembers.
///
/// A bound, not a target: the window only has to hold the images one grid asks
/// for at a time, and an evicted entry costs one request.
const REFUSED_IMAGE_CAPACITY: u64 = 4_096;

impl CatalogService {
    /// Builds the service.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry, shared with the plugin host that
    ///   rebuilt it.
    /// - `database`: the open database; the service owns `external_ids` and
    ///   `catalog_cache` in it.
    /// - `settings`: the settings service; every call reads its current
    ///   snapshot so a change takes effect without a restart.
    /// - `artwork_dir`: the directory cached cover images are written to, which
    ///   is `<data>/artwork/` of the server's paths.
    pub fn new(
        registry: Arc<ProviderRegistry>,
        database: Arc<Database>,
        settings: Settings,
        artwork_dir: impl Into<PathBuf>,
    ) -> CatalogService {
        let cache = Cache::new(Arc::clone(&database));
        let missing_images = MemoryCache::builder()
            .max_capacity(REFUSED_IMAGE_CAPACITY)
            .time_to_live(REFUSED_IMAGE_TTL)
            .build();
        let refused_images = MemoryCache::builder()
            .max_capacity(REFUSED_IMAGE_CAPACITY)
            .time_to_live(REFUSED_IMAGE_TTL)
            .build();
        CatalogService {
            inner: Arc::new(Inner {
                registry,
                database,
                settings,
                cache,
                artwork_dir: artwork_dir.into(),
                missing_images,
                refused_images,
            }),
        }
    }

    /// Returns the provider registry the service resolves instances through.
    pub fn registry(&self) -> &ProviderRegistry {
        &self.inner.registry
    }

    /// Returns the open database.
    pub fn database(&self) -> &Database {
        &self.inner.database
    }

    /// Returns the settings service.
    ///
    /// A caller that needs a tunable reads the current snapshot with
    /// [`settings_snapshot`](CatalogService::settings_snapshot).
    pub fn settings(&self) -> &Settings {
        &self.inner.settings
    }

    /// Returns the settings in effect right now.
    pub fn settings_snapshot(&self) -> Arc<SettingsStore> {
        self.inner.settings.current()
    }

    /// Returns the two-level response cache.
    ///
    /// The cache is shared by every call this service makes, so a layer
    /// implemented in another module of this crate resolves through the same two
    /// tiers.
    pub(crate) fn cache(&self) -> &Cache {
        &self.inner.cache
    }

    /// Returns the directory cached cover images are written to.
    pub(crate) fn artwork_dir(&self) -> &Path {
        &self.inner.artwork_dir
    }

    /// Returns the image URLs whose host refused them, with their expiry.
    ///
    /// The artwork layer reads it before it asks an image host for a URL and
    /// writes it when the host answers with a refusal, so a burst of a hundred
    /// cover requests does not ask a throttling host a hundred times.
    pub(crate) fn refused_images(&self) -> &MemoryCache<String, ()> {
        &self.inner.refused_images
    }

    /// Returns the image URLs a host answered as missing, with their expiry.
    ///
    /// A URL in this window is answered with not-found without asking the host
    /// again, which keeps a page of cards from repeating a request the provider
    /// already said it cannot satisfy.
    pub(crate) fn missing_images(&self) -> &MemoryCache<String, ()> {
        &self.inner.missing_images
    }

    /// Looks up one track.
    ///
    /// The call is routed to the instance named by `id`, and to that instance
    /// only: a catalog id carries the slug that issued it, so a lookup never
    /// falls back to another provider.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of a track.
    ///
    /// # Returns
    ///
    /// The track, with the id that names it.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name a track, and
    /// when its slug names an instance that is unknown, disabled, unassigned or
    /// not a catalog provider. Returns the mapped provider failure otherwise.
    pub async fn track(&self, id: &CatalogId) -> Result<CatalogTrackResult, CatalogError> {
        require_kind(id, EntityKind::Track)?;
        let instance = self.instance_for(id.slug())?;
        let provider = Self::catalog_provider(&instance)?;
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let track = self
            .inner
            .cache
            .through::<CatalogTrack, WireTrack, _>(
                instance.id(),
                &RequestKey::track(id.provider_id()),
                ENTITY_CACHE_TTL,
                async {
                    let outcome =
                        tokio::time::timeout(timeout, provider.track(id.provider_id())).await;
                    self.finish(id.slug(), "track", outcome)
                },
            )
            .await?;
        track_result(id.slug(), track)
    }

    /// Looks up one album with its tracklist.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an album.
    ///
    /// # Returns
    ///
    /// The album and the tracks the provider lists for it, with the id that
    /// names the album.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an album and
    /// when its slug does not resolve to a catalog instance, and the mapped
    /// provider failure otherwise.
    pub async fn album(&self, id: &CatalogId) -> Result<CatalogAlbumResult, CatalogError> {
        require_kind(id, EntityKind::Album)?;
        let instance = self.instance_for(id.slug())?;
        let provider = Self::catalog_provider(&instance)?;
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let album = self
            .inner
            .cache
            .through::<CatalogAlbum, WireAlbum, _>(
                instance.id(),
                &RequestKey::album(id.provider_id()),
                ENTITY_CACHE_TTL,
                async {
                    let outcome =
                        tokio::time::timeout(timeout, provider.album(id.provider_id())).await;
                    self.finish(id.slug(), "album", outcome)
                },
            )
            .await?;
        Ok(CatalogAlbumResult {
            id: catalog_id(id.slug(), EntityKind::Album, &album.id)?,
            album,
        })
    }

    /// Looks up one artist.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an artist.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an artist
    /// and when its slug does not resolve to a catalog instance, and the mapped
    /// provider failure otherwise.
    pub async fn artist(&self, id: &CatalogId) -> Result<CatalogArtistResult, CatalogError> {
        require_kind(id, EntityKind::Artist)?;
        let instance = self.instance_for(id.slug())?;
        let provider = Self::catalog_provider(&instance)?;
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let artist = self
            .inner
            .cache
            .through::<CatalogArtist, WireArtist, _>(
                instance.id(),
                &RequestKey::artist(id.provider_id()),
                ENTITY_CACHE_TTL,
                async {
                    let outcome =
                        tokio::time::timeout(timeout, provider.artist(id.provider_id())).await;
                    self.finish(id.slug(), "artist", outcome)
                },
            )
            .await?;
        Ok(CatalogArtistResult {
            id: catalog_id(id.slug(), EntityKind::Artist, &artist.id)?,
            artist,
        })
    }

    /// Lists an artist's albums.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an artist.
    /// - `limit`: the maximum number of albums to return.
    /// - `offset`: the number of albums to skip.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an artist
    /// and when its slug does not resolve to a catalog instance, and the mapped
    /// provider failure otherwise.
    pub async fn artist_albums(
        &self,
        id: &CatalogId,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CatalogAlbumRefResult>, CatalogError> {
        require_kind(id, EntityKind::Artist)?;
        let instance = self.instance_for(id.slug())?;
        let provider = Self::catalog_provider(&instance)?;
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let albums = self
            .inner
            .cache
            .through::<Vec<CatalogAlbumRef>, WireAlbumRefs, _>(
                instance.id(),
                &RequestKey::artist_albums(id.provider_id(), limit, offset),
                ENTITY_CACHE_TTL,
                async {
                    let outcome = tokio::time::timeout(
                        timeout,
                        provider.artist_albums(id.provider_id(), limit, offset),
                    )
                    .await;
                    self.finish(id.slug(), "artist-albums", outcome)
                },
            )
            .await?;
        albums
            .into_iter()
            .map(|album| album_ref_result(id.slug(), album))
            .collect()
    }

    /// Lists an artist's most popular tracks.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an artist.
    /// - `limit`: the maximum number of tracks to return.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an artist
    /// and when its slug does not resolve to a catalog instance, and the mapped
    /// provider failure otherwise.
    pub async fn artist_top_tracks(
        &self,
        id: &CatalogId,
        limit: u32,
    ) -> Result<Vec<CatalogTrackResult>, CatalogError> {
        require_kind(id, EntityKind::Artist)?;
        let instance = self.instance_for(id.slug())?;
        let provider = Self::catalog_provider(&instance)?;
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let tracks = self
            .inner
            .cache
            .through::<Vec<CatalogTrack>, WireTracks, _>(
                instance.id(),
                &RequestKey::artist_top_tracks(id.provider_id(), limit),
                ENTITY_CACHE_TTL,
                async {
                    let outcome = tokio::time::timeout(
                        timeout,
                        provider.artist_top_tracks(id.provider_id(), limit),
                    )
                    .await;
                    self.finish(id.slug(), "artist-top-tracks", outcome)
                },
            )
            .await?;
        tracks
            .into_iter()
            .map(|track| track_result(id.slug(), track))
            .collect()
    }

    /// Looks up every track carrying an ISRC.
    ///
    /// An ISRC names a recording, not a provider entity, so the lookup has no
    /// slug to route by and follows the same policy as search: the primary
    /// catalog instance answers, and a later instance is tried only when the
    /// primary errors or exceeds its deadline. Results are never merged.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the International Standard Recording Code.
    ///
    /// # Returns
    ///
    /// The matching tracks, each with the id that names it, in the answering
    /// instance's order. An ISRC no provider knows is an empty vector; it is not
    /// an error.
    ///
    /// # Errors
    ///
    /// Returns the mapped failure of the last instance that failed. A missing
    /// ISRC is never [`CatalogError::NotFound`].
    pub async fn lookup_isrc(&self, isrc: &str) -> Result<Vec<CatalogTrackResult>, CatalogError> {
        let timeout = catalog_timeout(&self.settings_snapshot())?;
        let instances = self.inner.registry.layers(Layer::Catalog);
        let mut failure: Option<CatalogError> = None;

        for instance in instances {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let outcome = self
                .inner
                .cache
                .through::<Vec<CatalogTrack>, WireTracks, _>(
                    instance.id(),
                    &RequestKey::isrc(isrc),
                    ENTITY_CACHE_TTL,
                    async {
                        let outcome =
                            tokio::time::timeout(timeout, provider.lookup_isrc(isrc)).await;
                        self.finish(instance.slug(), "lookup-isrc", outcome)
                    },
                )
                .await;
            match outcome {
                Ok(tracks) => {
                    let mut results = Vec::with_capacity(tracks.len());
                    for track in tracks {
                        results.push(track_result(instance.slug(), track)?);
                    }
                    return Ok(results);
                }
                Err(error) => failure = Some(error),
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(Vec::new()),
        }
    }

    /// Searches the catalog layer.
    ///
    /// The query goes to the primary catalog instance first. A later instance is
    /// tried only when the one before it failed or exceeded
    /// `search.catalog.timeout_ms`, and the first answer wins: results from two
    /// instances are never merged. The requested limit is capped by
    /// `search.catalog.max_results`, so a client cannot make the server hold an
    /// unbounded provider response.
    ///
    /// A disabled `search.catalog.enabled` short-circuits the whole call: no
    /// instance is contacted and the returned search is empty but successful,
    /// because a client asking for catalog results on a server that turned them
    /// off should get its local results, not an error.
    ///
    /// # Parameters
    ///
    /// - `query`: the search text, the entity kinds to return, and the page
    ///   window. The text goes to the provider unchanged; the limit is capped.
    ///
    /// # Returns
    ///
    /// The catalog half of a search response, with the slug of the instance
    /// that answered.
    ///
    /// # Errors
    ///
    /// Returns the mapped failure of the last instance that failed, including
    /// [`CatalogError::RateLimited`] with its retry delay. A search that no
    /// instance answered because the layer is empty is not an error: it is an
    /// empty [`CatalogSearch`] without a source.
    pub async fn search(&self, query: &CatalogQuery) -> Result<CatalogSearch, CatalogError> {
        let settings = self.settings_snapshot();
        if !settings.get(keys::SEARCH_CATALOG_ENABLED)? {
            return Ok(CatalogSearch::default());
        }
        let cap = settings.get(keys::SEARCH_CATALOG_MAX_RESULTS)?;
        let query = effective_query(query, cap);
        if query.limit == 0 {
            return Ok(CatalogSearch::default());
        }

        let timeout = catalog_timeout(&settings)?;
        let instances = self.inner.registry.layers(Layer::Catalog);
        let mut failure: Option<CatalogError> = None;

        for instance in instances {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let outcome = self
                .inner
                .cache
                .through::<CatalogResults, WireResults, _>(
                    instance.id(),
                    &RequestKey::search(&query),
                    SEARCH_CACHE_TTL,
                    async {
                        let outcome = tokio::time::timeout(timeout, provider.search(&query)).await;
                        self.finish(instance.slug(), "search", outcome)
                    },
                )
                .await;
            match outcome {
                Ok(results) => return search_result(instance.slug(), results),
                Err(error) => failure = Some(error),
            }
        }

        match failure {
            Some(error) => Err(error),
            None => Ok(CatalogSearch::default()),
        }
    }

    /// Deletes the expired rows of the persistent cache tier.
    ///
    /// A stale row is ignored when it is read, so sweeping is housekeeping: it
    /// keeps the table from growing without bound. The composition root calls
    /// this from a scheduled job; nothing in a request path depends on it.
    ///
    /// # Returns
    ///
    /// The number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the delete fails.
    pub async fn sweep_expired_cache(&self) -> Result<u64, CatalogError> {
        self.inner.cache.sweep().await
    }

    /// Returns the registry entry a catalog id names.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the registry serves no enabled,
    /// assigned instance with that slug.
    fn instance_for(&self, slug: &Slug) -> Result<Arc<RegistryInstance>, CatalogError> {
        match self.inner.registry.instance(slug.as_str()) {
            Some(instance) => Ok(instance),
            None => {
                tracing::debug!(
                    instance = slug.as_str(),
                    "no provider instance serves this catalog id"
                );
                Err(CatalogError::NotFound)
            }
        }
    }

    /// Returns the catalog provider of an instance, or a not-found.
    ///
    /// An instance that is assigned to another layer only is not a catalog
    /// provider, which is a not-found for a lookup routed to it.
    ///
    /// # Parameters
    ///
    /// - `instance`: the registry entry the id named.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the instance provides no catalog
    /// layer.
    fn catalog_provider(
        instance: &Arc<RegistryInstance>,
    ) -> Result<&Arc<dyn CatalogProvider>, CatalogError> {
        match instance.catalog() {
            Some(provider) => Ok(provider),
            None => {
                tracing::debug!(
                    instance = instance.slug().as_str(),
                    "the provider instance does not provide the catalog layer"
                );
                Err(CatalogError::NotFound)
            }
        }
    }

    /// Maps one timed provider call into a value or a catalog error.
    ///
    /// # Parameters
    ///
    /// - `slug`: the slug of the instance that was called.
    /// - `operation`: the call that was in flight, for the log line.
    /// - `outcome`: the deadline-wrapped provider result.
    ///
    /// # Returns
    ///
    /// The provider's value.
    ///
    /// # Errors
    ///
    /// Returns the mapped provider failure, and [`CatalogError::Unavailable`]
    /// when the deadline passed before the instance answered.
    pub(crate) fn finish<T>(
        &self,
        slug: &Slug,
        operation: &'static str,
        outcome: Result<Result<T, ProviderError>, Elapsed>,
    ) -> Result<T, CatalogError> {
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                let mapped = CatalogError::from_provider(slug.as_str(), error);
                if mapped.is_transient() {
                    tracing::warn!(
                        instance = slug.as_str(),
                        operation,
                        error = %mapped,
                        "the catalog instance did not answer"
                    );
                } else {
                    tracing::debug!(
                        instance = slug.as_str(),
                        operation,
                        error = %mapped,
                        "the catalog instance rejected the call"
                    );
                }
                Err(mapped)
            }
            Err(_elapsed) => {
                tracing::warn!(
                    instance = slug.as_str(),
                    operation,
                    "the catalog instance exceeded its deadline"
                );
                Err(CatalogError::Unavailable {
                    instance: slug.as_str().to_owned(),
                })
            }
        }
    }
}

/// Reads `search.catalog.timeout_ms` from a snapshot.
///
/// # Errors
///
/// Returns [`CatalogError::Setting`] when the snapshot no longer holds a valid
/// value for the key.
fn catalog_timeout(settings: &SettingsStore) -> Result<Duration, CatalogError> {
    let millis = settings.get(keys::SEARCH_CATALOG_TIMEOUT_MS)?;
    Ok(Duration::from_millis(u64::try_from(millis).unwrap_or(0)))
}

/// Applies the catalog result cap to a query.
///
/// # Parameters
///
/// - `query`: the client's query.
/// - `cap`: `search.catalog.max_results`.
///
/// # Returns
///
/// The query with its limit reduced to the cap. A non-positive cap makes the
/// limit zero, which the caller treats as "do not ask any provider".
fn effective_query(query: &CatalogQuery, cap: i64) -> CatalogQuery {
    let cap = u32::try_from(cap).unwrap_or(0);
    CatalogQuery {
        text: query.text.clone(),
        kinds: query.kinds.clone(),
        limit: query.limit.min(cap),
        offset: query.offset,
    }
}

/// Rejects a catalog id that does not name the expected kind.
///
/// # Parameters
///
/// - `id`: the id the caller passed.
/// - `expected`: the kind the endpoint it was passed to serves.
///
/// # Errors
///
/// Returns [`CatalogError::NotFound`] when the kinds differ, and logs the
/// mismatch: a client address would answer a mismatched id with a not-found,
/// and a caller inside the server has a bug worth a log line.
pub(crate) fn require_kind(id: &CatalogId, expected: EntityKind) -> Result<(), CatalogError> {
    if id.kind() == expected {
        return Ok(());
    }
    tracing::warn!(
        id = %id,
        expected = ?expected,
        found = ?id.kind(),
        "a catalog id was passed to an endpoint of another kind"
    );
    Err(CatalogError::NotFound)
}

/// Builds the id that names a track a provider returned.
///
/// # Errors
///
/// Returns [`CatalogError::InvalidEntity`] when the provider returned an empty
/// track id, which a client id cannot name.
fn track_result(slug: &Slug, track: CatalogTrack) -> Result<CatalogTrackResult, CatalogError> {
    let id = catalog_id(slug, EntityKind::Track, &track.id)?;
    Ok(CatalogTrackResult { id, track })
}

/// Builds the id that names an album reference a provider returned.
///
/// # Errors
///
/// Returns [`CatalogError::InvalidEntity`] when the provider returned an empty
/// album id.
fn album_ref_result(
    slug: &Slug,
    album: CatalogAlbumRef,
) -> Result<CatalogAlbumRefResult, CatalogError> {
    let id = catalog_id(slug, EntityKind::Album, &album.id)?;
    Ok(CatalogAlbumRefResult { id, album })
}

/// Builds a catalog id, reporting an empty provider id as an invalid entity.
///
/// # Errors
///
/// Returns [`CatalogError::InvalidEntity`] when `provider_id` is empty.
fn catalog_id(slug: &Slug, kind: EntityKind, provider_id: &str) -> Result<CatalogId, CatalogError> {
    CatalogId::new(slug.clone(), kind, provider_id).map_err(|_| CatalogError::InvalidEntity {
        instance: slug.as_str().to_owned(),
    })
}

/// Turns a provider's search results into the catalog half of a response.
///
/// # Errors
///
/// Returns [`CatalogError::InvalidEntity`] when the provider returned an entity
/// with an empty id.
fn search_result(slug: &Slug, results: CatalogResults) -> Result<CatalogSearch, CatalogError> {
    let mut search = CatalogSearch {
        source: Some(slug.clone()),
        ..CatalogSearch::default()
    };
    for track in results.tracks {
        search.tracks.push(track_result(slug, track)?);
    }
    for album in results.albums {
        search.albums.push(album_ref_result(slug, album)?);
    }
    for artist in results.artists {
        let id = catalog_id(slug, EntityKind::Artist, &artist.id)?;
        search.artists.push(CatalogArtistResult { id, artist });
    }
    Ok(search)
}

impl std::fmt::Debug for CatalogService {
    /// Reports the service without the provider registry, which holds plugin
    /// configuration.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogService")
            .field("database", &self.inner.database)
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the search query the tests start from.
    fn query() -> CatalogQuery {
        CatalogQuery {
            text: "energy 52".to_owned(),
            kinds: vec![EntityKind::Track],
            limit: 50,
            offset: 10,
        }
    }

    #[test]
    fn the_result_cap_lowers_the_requested_limit() {
        let capped = effective_query(&query(), 20);

        assert_eq!(capped.limit, 20);
        assert_eq!(capped.offset, 10);
        assert_eq!(capped.text, "energy 52");
        assert_eq!(capped.kinds, vec![EntityKind::Track]);
    }

    #[test]
    fn a_limit_below_the_cap_is_left_alone() {
        let mut query = query();
        query.limit = 5;

        assert_eq!(effective_query(&query, 20).limit, 5);
    }

    #[test]
    fn a_non_positive_cap_asks_for_nothing() {
        assert_eq!(effective_query(&query(), 0).limit, 0);
        assert_eq!(effective_query(&query(), -3).limit, 0);
    }

    #[test]
    fn a_failure_keeps_the_instance_that_caused_it() {
        let error = CatalogError::from_provider(
            "deezer",
            ProviderError::RateLimited {
                retry_after_ms: 1_500,
            },
        );

        assert_eq!(error.instance(), Some("deezer"));
        assert_eq!(error.retry_after_ms(), Some(1_500));
        assert!(error.is_transient());
    }
}
