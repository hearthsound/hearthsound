//! Discovery behind the catalog layer: similarity, popularity and
//! recommendations.
//!
//! A listener asks the server for songs similar to one track, for the popular
//! tracks of an artist, or for the artists related to one they already know.
//! Nothing in the library holds that knowledge, and the catalog layer is the
//! wrong place for it too: a catalog provider answers what exists, while a
//! discovery provider answers what goes together. [`DiscoveryService`] is the
//! layer over the discovery providers, exactly as [`CatalogService`] is the
//! layer over the catalog providers.
//!
//! Two rules shape the calls here (02-architecture.md, section 5):
//!
//! - a similarity or popularity query takes the first instance in the discovery
//!   layer, in the administrator's order, that answers a non-empty list; an
//!   instance that fails is logged and skipped, because an answer from the next
//!   service is better than an empty panel;
//! - an instance is called with the listener's own configuration when they
//!   linked it, and without one otherwise. [`UserContextSource`] supplies that
//!   configuration; the account-linking service is what implements it, and a
//!   process that has no links yet supplies [`NoUserContexts`].
//!
//! A suggestion names a track or an artist the way the service knows it, not
//! the way this server addresses it. Resolution turns a suggestion into a
//! catalog entity through [`CatalogService::search`], accepting only a result
//! whose normalized title and primary artist are the suggestion's; a suggestion
//! no provider can name is dropped rather than shown as something unplayable.
//!
//! Recommendations and generated playlists are the listener's own: an instance
//! is only asked when they linked it, and a playlists answer is the union over
//! every linked discovery instance rather than the first answer, because each
//! service contributes the playlists it generated for that listener. The album
//! helpers on top of them - recommended albums and the albums of the artists
//! similar to what the listener last played - keep the order the services
//! answered in and drop repeated albums.
//!
//! Answers are kept in an in-memory cache for an hour, keyed by the instance,
//! the operation, the arguments and the listener, so a client redrawing a panel
//! does not spend the service's quota again.
//!
//! # Examples
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use hearthsound_catalog::{DiscoveryService, NoUserContexts};
//! use hearthsound_core::providers::TrackSeed;
//!
//! # async fn example(
//! #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
//! #     catalog: Arc<hearthsound_catalog::CatalogService>,
//! #     user: uuid::Uuid,
//! # ) -> Result<(), Box<dyn std::error::Error>> {
//! let discovery = DiscoveryService::new(registry, catalog, Arc::new(NoUserContexts));
//! let seed = TrackSeed {
//!     title: "Creep".to_owned(),
//!     artists: vec!["Radiohead".to_owned()],
//!     ids: Vec::new(),
//! };
//! for suggestion in discovery.similar_tracks(user, &seed, 20).await? {
//!     println!("{}", suggestion.title);
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hearthsound_core::providers::{
    ArtistSeed, CatalogAlbumRef, CatalogQuery, CatalogTrack, GeneratedPlaylist, SuggestedArtist,
    SuggestedTrack, TrackSeed, UserContext,
};
use hearthsound_core::{CatalogId, EntityKind, Layer};
use hearthsound_db::Database;
use hearthsound_library::query::Scope;
use hearthsound_library::{Album, Artist};
use hearthsound_plugin_host::{ProviderRegistry, UserConfigService};
use moka::future::Cache as MemoryCache;
use uuid::Uuid;

use crate::dedupe::normalize;
use crate::error::CatalogError;
use crate::materialize::release_year;
use crate::service::{
    CatalogAlbumRefResult, CatalogArtistResult, CatalogService, CatalogTrackResult,
};

/// How long one discovery answer stays fresh.
///
/// One hour. Recommendations change slowly enough that a listener who returns
/// to the same panel within the hour should see the same suggestions, and an
/// hour is short enough that a service which learned something new today is
/// asked again before the day is over.
const DISCOVERY_CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// How many answers the in-memory cache keeps.
///
/// A bound, not a target: a suggestion list is small, and the cache exists to
/// answer the requests one session repeats rather than to be a store.
const DISCOVERY_CACHE_CAPACITY: u64 = 1_024;

/// How many suggestions are resolved at the same time.
///
/// Four, which is the number of reader connections the database pool holds and
/// also a small enough fan-out that a catalog provider is not asked four
/// searches per suggestion.
const RESOLVE_PARALLELISM: usize = 4;

/// How many catalog results a track resolution reads.
const TRACK_RESOLUTION_LIMIT: u32 = 5;

/// How many catalog results an artist resolution reads.
const ARTIST_RESOLUTION_LIMIT: u32 = 3;

/// How many library artists an artist resolution reads.
const ARTIST_SEARCH_LIMIT: i64 = 5;

/// How many recommended tracks an album listing resolves.
///
/// The album list is what the recommendations say, so a page of albums is
/// derived from more tracks than it shows: two tracks of one album count once,
/// and a suggestion the catalog cannot name contributes nothing.
const RECOMMENDED_TRACK_LIMIT: u32 = 100;

/// How many similar artists seed the similar-artist album listing.
const SIMILAR_ALBUM_ARTISTS: u32 = 10;

/// How many albums a similar artist contributes from the catalog.
///
/// A listener's personal library is worth more than the provider's full
/// discography here: the section exists to help them play something they have,
/// so two albums per artist keep one prolific similar artist from filling the
/// whole list.
const SIMILAR_ALBUM_CATALOG_LIMIT: u32 = 2;

/// Supplies the per-user configuration an instance should be called with.
///
/// The account-linking service owns the stored `provider_user_configs` rows;
/// this trait is the seam that lets the discovery layer ask for one without
/// depending on that service. An instance the listener has not linked is called
/// without a context.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use hearthsound_catalog::{DiscoveryService, NoUserContexts};
///
/// # fn example(
/// #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
/// #     catalog: Arc<hearthsound_catalog::CatalogService>,
/// # ) {
/// let discovery = DiscoveryService::new(registry, catalog, Arc::new(NoUserContexts));
/// # let _ = discovery;
/// # }
/// ```
#[async_trait]
pub trait UserContextSource: Send + Sync {
    /// Returns the listener's configuration for one instance.
    ///
    /// # Parameters
    ///
    /// - `instance`: the provider instance the call is about.
    /// - `user`: the listener the call is made for.
    ///
    /// # Returns
    ///
    /// The listener's context when they linked the instance, and `None`
    /// otherwise.
    async fn user_context(&self, instance: Uuid, user: Uuid) -> Option<UserContext>;
}

/// A [`UserContextSource`] that reports no linked accounts.
///
/// It is what a process without the account-linking service supplies, and what
/// a test uses when it does not care about a listener's configuration.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::NoUserContexts;
/// use hearthsound_catalog::discovery::UserContextSource;
///
/// # async fn example() {
/// let source = NoUserContexts;
/// assert!(
///     source
///         .user_context(uuid::Uuid::nil(), uuid::Uuid::nil())
///         .await
///         .is_none()
/// );
/// # }
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct NoUserContexts;

#[async_trait]
impl UserContextSource for NoUserContexts {
    /// Returns `None` for every instance and listener.
    ///
    /// # Parameters
    ///
    /// - `_instance`: the provider instance the call would be about.
    /// - `_user`: the listener the call would be made for.
    ///
    /// # Returns
    ///
    /// Always `None`.
    async fn user_context(&self, _instance: Uuid, _user: Uuid) -> Option<UserContext> {
        None
    }
}

#[async_trait]
impl<F> UserContextSource for F
where
    F: Fn(Uuid, Uuid) -> Option<UserContext> + Send + Sync,
{
    /// Calls the closure for every instance and listener.
    ///
    /// # Parameters
    ///
    /// - `instance`: the provider instance the call is about.
    /// - `user`: the listener the call is made for.
    ///
    /// # Returns
    ///
    /// What the closure returns.
    async fn user_context(&self, instance: Uuid, user: Uuid) -> Option<UserContext> {
        self(instance, user)
    }
}

/// A [`UserContextSource`] over the listener's stored provider links.
///
/// The account-linking service stores one sealed configuration per
/// (instance, listener) pair; this adapter opens it and reports the context a
/// plugin call is made with. A listener who has not linked the instance is
/// reported as `None`, which is what makes the recommendation and playlist
/// queries skip it. A link that cannot be read is logged and treated the same
/// way: an unreadable link is not a reason to fail a suggestion panel.
///
/// # Examples
///
/// ```
/// use std::sync::Arc;
///
/// use hearthsound_catalog::{DiscoveryService, LinkedUserContexts};
/// use hearthsound_plugin_host::UserConfigService;
///
/// # async fn example(
/// #     registry: Arc<hearthsound_plugin_host::ProviderRegistry>,
/// #     catalog: Arc<hearthsound_catalog::CatalogService>,
/// #     database: &hearthsound_db::Database,
/// #     secrets: hearthsound_config::secret::SecretBox,
/// # ) {
/// let links = LinkedUserContexts::new(UserConfigService::new(database, secrets));
/// let discovery = DiscoveryService::new(registry, catalog, Arc::new(links));
/// # let _ = discovery;
/// # }
/// ```
#[derive(Clone)]
pub struct LinkedUserContexts {
    /// The stored links this adapter opens.
    configs: UserConfigService,
}

impl std::fmt::Debug for LinkedUserContexts {
    /// Reports the adapter without the stored links, whose values are sealed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkedUserContexts")
    }
}

impl LinkedUserContexts {
    /// Builds the adapter over the account-linking service.
    ///
    /// # Parameters
    ///
    /// - `configs`: the service that reads `provider_user_configs`.
    ///
    /// # Returns
    ///
    /// The adapter.
    pub fn new(configs: UserConfigService) -> LinkedUserContexts {
        LinkedUserContexts { configs }
    }
}

#[async_trait]
impl UserContextSource for LinkedUserContexts {
    /// Returns the listener's configuration when they linked the instance.
    ///
    /// # Parameters
    ///
    /// - `instance`: the provider instance the call is about.
    /// - `user`: the listener the call is made for.
    ///
    /// # Returns
    ///
    /// The context the plugin call is made with, or `None` when the listener
    /// has not linked the instance or the stored link cannot be opened.
    async fn user_context(&self, instance: Uuid, user: Uuid) -> Option<UserContext> {
        match self.configs.get(instance, user).await {
            Ok(link) => link.map(|link| link.context(user)),
            Err(error) => {
                tracing::debug!(
                    %error,
                    %instance,
                    %user,
                    "a stored provider link could not be opened"
                );
                None
            }
        }
    }
}

/// An artist a suggestion resolved to.
///
/// A suggestion the caller can see in their library resolves to the stored
/// row; one the catalog knows resolves to the catalog entity. The caller
/// decides which of the two it can show, because a catalog artist is not
/// playable but is still worth naming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedArtist {
    /// An artist of the caller's visible library.
    Stored(Artist),
    /// An artist the catalog layer knows.
    Catalog(crate::service::CatalogArtistResult),
}

/// One generated playlist, with the instance that generated it.
///
/// The sync job stores the instance id beside the playlist so a refresh
/// replaces the playlist the same instance generated rather than creating a
/// second one under the same reference.
#[derive(Debug, Clone, PartialEq)]
pub struct InstancePlaylist {
    /// The instance that generated the playlist.
    pub instance: Uuid,
    /// The playlist and its tracks.
    pub playlist: GeneratedPlaylist,
}

/// An album a recommendation resolved to.
///
/// A similar artist's albums can come from two places: the listener's personal
/// library, which the caller renders from the stored rows, or the catalog, which
/// it renders from the provider's album reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAlbum {
    /// An album of the listener's personal library.
    Stored(Album),
    /// An album the catalog layer knows.
    Catalog(CatalogAlbumRefResult),
}

/// The seed artist and the albums its similar artists contributed.
///
/// The seed is the artist of the listener's most recent play, when they have
/// played anything; the albums are interleaved by artist in the order the
/// discovery layer suggested them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimilarArtistAlbums {
    /// The artist the similar artists were found from.
    pub seed: Option<Artist>,
    /// The albums, one artist at a time.
    pub albums: Vec<ResolvedAlbum>,
}

/// The discovery layer over the provider registry.
///
/// One service is built by the composition root and shared through an `Arc`.
/// Cloning it is cheap: every clone shares the same registry, catalog layer,
/// user-context source and cache.
pub struct DiscoveryService {
    /// The state every clone shares.
    inner: Arc<Inner>,
}

/// The state one [`DiscoveryService`] shares with its clones.
struct Inner {
    /// The provider instances, ordered by layer assignment.
    registry: Arc<ProviderRegistry>,
    /// The catalog layer that resolves a suggestion into an entity.
    catalog: Arc<CatalogService>,
    /// The source of the listener's per-instance configuration.
    users: Arc<dyn UserContextSource>,
    /// The in-memory cache of provider answers.
    cache: MemoryCache<DiscoveryKey, DiscoveryAnswer>,
    /// The catalog entity a suggestion resolved to.
    ///
    /// Resolution is a catalog search per suggestion, and a panel that is
    /// redrawn asks for the same suggestions again; remembering the answer
    /// keeps a recommendation from spending the catalog's quota twice. Only a
    /// found entity is stored: a miss is worth asking again, because a search
    /// that failed may have failed for a transient reason.
    track_resolutions: MemoryCache<String, CatalogTrackResult>,
    /// The catalog artist a normalized name resolved to.
    artist_resolutions: MemoryCache<String, CatalogId>,
}

/// The key of one cached discovery answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiscoveryKey {
    /// The instance that answered.
    instance: Uuid,
    /// The provider function that was called.
    operation: &'static str,
    /// The canonical form of the arguments.
    arguments: String,
    /// The listener the answer was produced for.
    user: Uuid,
}

/// What one cached discovery answer holds.
#[derive(Debug, Clone)]
enum DiscoveryAnswer {
    /// A similar-track, top-track or recommended-track answer.
    Tracks(Vec<SuggestedTrack>),
    /// A similar-artist answer.
    Artists(Vec<SuggestedArtist>),
    /// A generated-playlist answer.
    Playlists(Vec<GeneratedPlaylist>),
}

impl DiscoveryService {
    /// Builds the service.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry, shared with the plugin host that
    ///   rebuilt it.
    /// - `catalog`: the catalog layer the suggestions are resolved through.
    /// - `users`: the source of a listener's per-instance configuration.
    ///
    /// # Returns
    ///
    /// The service, with an empty answer cache.
    pub fn new(
        registry: Arc<ProviderRegistry>,
        catalog: Arc<CatalogService>,
        users: Arc<dyn UserContextSource>,
    ) -> DiscoveryService {
        let cache = MemoryCache::builder()
            .max_capacity(DISCOVERY_CACHE_CAPACITY)
            .time_to_live(DISCOVERY_CACHE_TTL)
            .build();
        let track_resolutions = MemoryCache::builder()
            .max_capacity(DISCOVERY_CACHE_CAPACITY)
            .time_to_live(DISCOVERY_CACHE_TTL)
            .build();
        let artist_resolutions = MemoryCache::builder()
            .max_capacity(DISCOVERY_CACHE_CAPACITY)
            .time_to_live(DISCOVERY_CACHE_TTL)
            .build();
        DiscoveryService {
            inner: Arc::new(Inner {
                registry,
                catalog,
                users,
                cache,
                track_resolutions,
                artist_resolutions,
            }),
        }
    }

    /// Returns the catalog layer suggestions are resolved through.
    pub fn catalog(&self) -> &CatalogService {
        &self.inner.catalog
    }

    /// Suggests tracks similar to a seed track.
    ///
    /// The first discovery instance, in the administrator's order, that answers
    /// a non-empty list wins; an instance that fails is logged and skipped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the request is made for.
    /// - `seed`: the track to find similar tracks for.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Returns
    ///
    /// The suggestions, in the answering instance's order. An empty vector
    /// means no instance had anything to say.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] only when a listener's configuration cannot be
    /// read. A provider failure is logged and skipped.
    pub async fn similar_tracks(
        &self,
        user: Uuid,
        seed: &TrackSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, CatalogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let arguments = track_arguments(seed, limit);
        for instance in self.inner.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let context = self.inner.users.user_context(instance.id(), user).await;
            let key = DiscoveryKey {
                instance: instance.id(),
                operation: "similar-tracks",
                arguments: arguments.clone(),
                user,
            };
            if let Some(DiscoveryAnswer::Tracks(cached)) = self.inner.cache.get(&key).await {
                if cached.is_empty() {
                    continue;
                }
                return Ok(cached);
            }
            match provider.similar_tracks(context.as_ref(), seed, limit).await {
                Ok(tracks) => {
                    self.inner
                        .cache
                        .insert(key, DiscoveryAnswer::Tracks(tracks.clone()))
                        .await;
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a discovery instance could not answer a similar-tracks query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Suggests artists similar to a seed artist.
    ///
    /// The first discovery instance, in the administrator's order, that answers
    /// a non-empty list wins; an instance that fails is logged and skipped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the request is made for.
    /// - `seed`: the artist to find similar artists for.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Returns
    ///
    /// The suggestions, in the answering instance's order. An empty vector
    /// means no instance had anything to say.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] only when a listener's configuration cannot be
    /// read. A provider failure is logged and skipped.
    pub async fn similar_artists(
        &self,
        user: Uuid,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedArtist>, CatalogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let arguments = artist_arguments(seed, limit);
        for instance in self.inner.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let context = self.inner.users.user_context(instance.id(), user).await;
            let key = DiscoveryKey {
                instance: instance.id(),
                operation: "similar-artists",
                arguments: arguments.clone(),
                user,
            };
            if let Some(DiscoveryAnswer::Artists(cached)) = self.inner.cache.get(&key).await {
                if cached.is_empty() {
                    continue;
                }
                return Ok(cached);
            }
            match provider
                .similar_artists(context.as_ref(), seed, limit)
                .await
            {
                Ok(artists) => {
                    self.inner
                        .cache
                        .insert(key, DiscoveryAnswer::Artists(artists.clone()))
                        .await;
                    if !artists.is_empty() {
                        return Ok(artists);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a discovery instance could not answer a similar-artists query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Lists an artist's most popular tracks according to the discovery layer.
    ///
    /// The first discovery instance, in the administrator's order, that answers
    /// a non-empty list wins; an instance that fails is logged and skipped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the request is made for.
    /// - `seed`: the artist whose popular tracks are wanted.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Returns
    ///
    /// The suggestions, in the answering instance's order. An empty vector
    /// means no instance had anything to say.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] only when a listener's configuration cannot be
    /// read. A provider failure is logged and skipped.
    pub async fn artist_top_tracks(
        &self,
        user: Uuid,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, CatalogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let arguments = artist_arguments(seed, limit);
        for instance in self.inner.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let context = self.inner.users.user_context(instance.id(), user).await;
            let key = DiscoveryKey {
                instance: instance.id(),
                operation: "artist-top-tracks",
                arguments: arguments.clone(),
                user,
            };
            if let Some(DiscoveryAnswer::Tracks(cached)) = self.inner.cache.get(&key).await {
                if cached.is_empty() {
                    continue;
                }
                return Ok(cached);
            }
            match provider
                .artist_top_tracks(context.as_ref(), seed, limit)
                .await
            {
                Ok(tracks) => {
                    self.inner
                        .cache
                        .insert(key, DiscoveryAnswer::Tracks(tracks.clone()))
                        .await;
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a discovery instance could not answer a top-tracks query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Suggests tracks recommended for a listener.
    ///
    /// Only instances the listener linked are asked, because a recommendation
    /// is personal: the service has to know whose taste it is describing. The
    /// first such instance, in the administrator's order, that answers a
    /// non-empty list wins; an instance that fails is logged and skipped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the recommendations are for.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Returns
    ///
    /// The suggestions, in the answering instance's order. An empty vector
    /// means no linked instance had anything to say.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] only when a listener's configuration cannot be
    /// read. A provider failure is logged and skipped.
    pub async fn recommended_tracks(
        &self,
        user: Uuid,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, CatalogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let arguments = format!("limit={limit}");
        for instance in self.inner.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let Some(context) = self.inner.users.user_context(instance.id(), user).await else {
                continue;
            };
            let key = DiscoveryKey {
                instance: instance.id(),
                operation: "recommended-tracks",
                arguments: arguments.clone(),
                user,
            };
            if let Some(DiscoveryAnswer::Tracks(cached)) = self.inner.cache.get(&key).await {
                if cached.is_empty() {
                    continue;
                }
                return Ok(cached);
            }
            match provider.recommended_tracks(&context, limit).await {
                Ok(tracks) => {
                    self.inner
                        .cache
                        .insert(key, DiscoveryAnswer::Tracks(tracks.clone()))
                        .await;
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a discovery instance could not answer a recommendations query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Lists the playlists the listener's linked discovery services generated.
    ///
    /// Every linked instance contributes its playlists, in the administrator's
    /// order: a service that generated nothing for this listener contributes
    /// nothing, and the others are not skipped because one was empty. An
    /// instance that fails is logged and skipped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the playlists were generated for.
    ///
    /// # Returns
    ///
    /// One entry per generated playlist, carrying the instance that generated
    /// it so a refresh can replace it in place.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] only when a listener's configuration cannot be
    /// read. A provider failure is logged and skipped.
    pub async fn playlists(&self, user: Uuid) -> Result<Vec<InstancePlaylist>, CatalogError> {
        let mut playlists = Vec::new();
        for instance in self.inner.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let Some(context) = self.inner.users.user_context(instance.id(), user).await else {
                continue;
            };
            let key = DiscoveryKey {
                instance: instance.id(),
                operation: "playlists",
                arguments: String::new(),
                user,
            };
            if let Some(DiscoveryAnswer::Playlists(cached)) = self.inner.cache.get(&key).await {
                playlists.extend(cached.into_iter().map(|playlist| InstancePlaylist {
                    instance: instance.id(),
                    playlist,
                }));
                continue;
            }
            match provider.playlists(&context).await {
                Ok(generated) => {
                    self.inner
                        .cache
                        .insert(key, DiscoveryAnswer::Playlists(generated.clone()))
                        .await;
                    playlists.extend(generated.into_iter().map(|playlist| InstancePlaylist {
                        instance: instance.id(),
                        playlist,
                    }));
                }
                Err(error) => {
                    tracing::debug!(
                        instance = instance.slug().as_str(),
                        %error,
                        "a discovery instance could not list its generated playlists"
                    );
                }
            }
        }
        Ok(playlists)
    }

    /// Lists the distinct albums of the listener's recommended tracks.
    ///
    /// The recommendations are resolved like any other suggestion, and the
    /// albums of the resolved tracks are kept in the order the recommendations
    /// named them, one album per provider album id. A track whose provider
    /// reported no album contributes nothing.
    ///
    /// A recommendation usually names its album with only an id and a title,
    /// so every album reference is completed for the client: its artists come
    /// from the track's credits when the reference carries none, and the
    /// catalog's album lookup adds the album artist, the release year and the
    /// cover when they are still missing.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the recommendations are for.
    /// - `limit`: the maximum number of albums.
    ///
    /// # Returns
    ///
    /// The albums, in recommendation order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a recommendation or a catalog search
    /// cannot be read.
    pub async fn recommended_albums(
        &self,
        user: Uuid,
        limit: u32,
    ) -> Result<Vec<CatalogAlbumRefResult>, CatalogError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let suggestions = self
            .recommended_tracks(user, RECOMMENDED_TRACK_LIMIT)
            .await?;
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut albums = Vec::new();
        let mut seen = HashSet::new();
        // Only as many suggestions are resolved as the shelf needs: tracks of
        // one album count once, and a suggestion no provider names contributes
        // nothing, so resolving all hundred would spend the catalog's quota on
        // entries the answer never shows.
        for chunk in suggestions.chunks(RESOLVE_PARALLELISM) {
            let resolved = self
                .resolve_tracks(chunk, u32::try_from(chunk.len()).unwrap_or(u32::MAX))
                .await?;
            for result in resolved {
                let Some(album) = result.track.album.clone() else {
                    continue;
                };
                let Ok(id) = CatalogId::new(
                    result.id.slug().clone(),
                    EntityKind::Album,
                    album.id.clone(),
                ) else {
                    continue;
                };
                if !seen.insert(id.to_string()) {
                    continue;
                }
                let album = self.completed_album(&result.track, album, &id).await;
                albums.push(CatalogAlbumRefResult { id, album });
                if albums.len() >= wanted {
                    return Ok(albums);
                }
            }
        }
        Ok(albums)
    }

    /// Fills in an album reference a recommended track named.
    ///
    /// A recommendation can name its album with only an id and a title, which
    /// leaves a shelf entry without an artist and often without a year or a
    /// cover. The resolved track already carries the credit its album is filed
    /// under, so that credit is used first; the catalog's album lookup, which
    /// is cached, then supplies the album artist, the release year and the
    /// cover when they are still missing. A failed lookup is logged and the
    /// reference is returned with what the track carried, because an entry
    /// without a cover is better than a section that failed.
    ///
    /// # Parameters
    ///
    /// - `track`: the resolved catalog track that named the album.
    /// - `album`: the album reference the track carried.
    /// - `id`: the catalog id of the album.
    ///
    /// # Returns
    ///
    /// The album reference, with as many fields filled as the data allowed.
    async fn completed_album(
        &self,
        track: &CatalogTrack,
        mut album: CatalogAlbumRef,
        id: &CatalogId,
    ) -> CatalogAlbumRef {
        if album.artists.is_empty()
            && let Some(primary) = track.artists.first()
        {
            album.artists = vec![primary.clone()];
        }
        // The resolved track already names the artist the album is filed under,
        // so a lookup is only needed when the reference names no credit at all.
        // Missing year and cover entries do not justify one: the catalog album
        // id the entry carries serves the cover, and a shelf that resolves ten
        // albums should cost ten searches, not ten more album lookups.
        if album.artists.is_empty() {
            match self.inner.catalog.album(id).await {
                Ok(full) => {
                    if album.artists.is_empty() {
                        album.artists = full.album.artists;
                    }
                    if album.year.is_none() {
                        album.year = release_year(full.album.release_date.as_deref())
                            .and_then(|year| u16::try_from(year).ok());
                    }
                    if album.images.is_empty() {
                        album.images = full.album.images;
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        album = %id,
                        "a recommended album is served without its catalog details"
                    );
                }
            }
        }
        album
    }

    /// Lists the albums of the artists similar to the listener's last play.
    ///
    /// The seed is the artist credited first on the listener's most recent
    /// play, so the section describes what they are listening to now. Each of
    /// the similar artists contributes the albums it has in the listener's
    /// personal library, and otherwise up to two albums from the catalog; the
    /// lists are interleaved one artist at a time and repeated albums are
    /// dropped.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the section is for.
    /// - `scopes`: the scopes the caller can see, in the order a similar artist
    ///   is preferred.
    /// - `size`: the maximum number of albums.
    ///
    /// # Returns
    ///
    /// The seed artist, when the listener has played something, and the
    /// albums.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a discovery query, a library read or a
    /// catalog lookup fails.
    pub async fn similar_artist_albums(
        &self,
        user: Uuid,
        scopes: &[Scope],
        size: u32,
    ) -> Result<SimilarArtistAlbums, CatalogError> {
        if size == 0 {
            return Ok(SimilarArtistAlbums {
                seed: None,
                albums: Vec::new(),
            });
        }
        let database = self.inner.catalog.database();
        let seed = {
            let mut conn = database.readers().acquire().await?;
            hearthsound_library::query::most_recent_played_artist(&mut conn, user).await?
        };
        let Some(seed) = seed else {
            return Ok(SimilarArtistAlbums {
                seed: None,
                albums: Vec::new(),
            });
        };
        let suggestions = self
            .similar_artists(user, &artist_seed(&seed), SIMILAR_ALBUM_ARTISTS)
            .await?;
        let resolved = self
            .resolve_artists(scopes, &suggestions, SIMILAR_ALBUM_ARTISTS)
            .await?;
        let personal = Scope::Personal { user_id: user };
        let wanted = usize::try_from(size).unwrap_or(usize::MAX);
        let mut lists = Vec::new();
        for artist in resolved {
            let albums = match artist {
                ResolvedArtist::Stored(artist) => {
                    let stored = {
                        let mut conn = database.readers().acquire().await?;
                        hearthsound_library::query::list_albums_for_artist(
                            &mut conn, personal, artist.id,
                        )
                        .await?
                    };
                    if stored.is_empty() {
                        match self.catalog_artist(&artist.name).await? {
                            Some(catalog) => self.catalog_albums(&catalog).await?,
                            None => Vec::new(),
                        }
                    } else {
                        stored.into_iter().map(ResolvedAlbum::Stored).collect()
                    }
                }
                ResolvedArtist::Catalog(catalog) => self.catalog_albums(&catalog).await?,
            };
            if !albums.is_empty() {
                lists.push(albums);
            }
            // The interleave cannot shrink when another artist joins it, so the
            // artists that could not contribute to a full shelf are never asked
            // for their albums.
            if interleave_albums(lists.clone(), size).len() >= wanted {
                break;
            }
        }
        Ok(SimilarArtistAlbums {
            seed: Some(seed),
            albums: interleave_albums(lists, size),
        })
    }

    /// Finds the catalog artist one name names.
    ///
    /// This is the fallback for a similar artist the listener has in their
    /// library but whose albums they do not: the catalog can still name what
    /// that artist released.
    ///
    /// # Parameters
    ///
    /// - `name`: the artist's name.
    ///
    /// # Returns
    ///
    /// The catalog artist whose normalized name is equal, when one exists.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the catalog search reports a failure the
    /// layer raises; a failure the provider answers is logged and reported as
    /// `None`.
    async fn catalog_artist(
        &self,
        name: &str,
    ) -> Result<Option<CatalogArtistResult>, CatalogError> {
        let wanted = normalize(name);
        if wanted.is_empty() {
            return Ok(None);
        }
        if let Some(cached) = self.inner.artist_resolutions.get(&wanted).await {
            match self.inner.catalog.artist(&cached).await {
                Ok(artist) => return Ok(Some(artist)),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        name,
                        "a cached artist resolution could not be read back"
                    );
                }
            }
        }
        let query = CatalogQuery {
            text: name.to_owned(),
            kinds: vec![EntityKind::Artist],
            limit: ARTIST_RESOLUTION_LIMIT,
            offset: 0,
        };
        match self.inner.catalog.search(&query).await {
            Ok(found) => {
                let resolved = found
                    .artists
                    .into_iter()
                    .find(|result| normalize(&result.artist.name) == wanted);
                if let Some(resolved) = &resolved {
                    self.inner
                        .artist_resolutions
                        .insert(wanted, resolved.id.clone())
                        .await;
                }
                Ok(resolved)
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    name,
                    "a similar artist could not be found in the catalog"
                );
                Ok(None)
            }
        }
    }

    /// Lists up to [`SIMILAR_ALBUM_CATALOG_LIMIT`] catalog albums of an artist.
    ///
    /// A failure is logged and reported as no albums: an artist the catalog
    /// cannot list is not a reason to fail the whole section.
    ///
    /// # Parameters
    ///
    /// - `artist`: the catalog artist.
    ///
    /// # Returns
    ///
    /// The albums, in the provider's order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the catalog lookup fails in a way the
    /// layer raises; a failure the provider answers is logged and reported as
    /// an empty list.
    async fn catalog_albums(
        &self,
        artist: &CatalogArtistResult,
    ) -> Result<Vec<ResolvedAlbum>, CatalogError> {
        match self
            .inner
            .catalog
            .artist_albums(&artist.id, SIMILAR_ALBUM_CATALOG_LIMIT, 0)
            .await
        {
            Ok(albums) => Ok(albums
                .into_iter()
                .map(ResolvedAlbum::Catalog)
                .collect::<Vec<_>>()),
            Err(error) => {
                tracing::debug!(
                    %error,
                    artist = artist.artist.name.as_str(),
                    "a similar artist's albums could not be listed"
                );
                Ok(Vec::new())
            }
        }
    }

    /// Resolves suggested tracks to catalog entities.
    ///
    /// A suggestion is searched for as `"<primary artist> <title>"` in the
    /// track kind, and the first result whose normalized title and primary
    /// artist equal the suggestion's is accepted. A suggestion with no artist
    /// is matched on its normalized title alone. Suggestions no provider names
    /// are dropped.
    ///
    /// # Parameters
    ///
    /// - `suggestions`: the suggestions to resolve.
    /// - `limit`: the maximum number of suggestions to resolve.
    ///
    /// # Returns
    ///
    /// The resolved entities, in the suggestions' order, without the ones that
    /// could not be resolved.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a catalog search fails in a way the layer
    /// reports; a failure is logged and the suggestion is dropped, so this is
    /// a defensive signature rather than a normal outcome.
    pub async fn resolve_tracks(
        &self,
        suggestions: &[SuggestedTrack],
        limit: u32,
    ) -> Result<Vec<CatalogTrackResult>, CatalogError> {
        let wanted = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(suggestions.len());
        let mut resolved = Vec::new();
        for chunk in suggestions[..wanted].chunks(RESOLVE_PARALLELISM) {
            let answers =
                futures_util::future::join_all(chunk.iter().map(|seed| self.resolve_track(seed)))
                    .await;
            resolved.extend(answers.into_iter().flatten());
        }
        Ok(resolved)
    }

    /// Resolves suggested artists to library or catalog entities.
    ///
    /// A suggestion resolves to a stored artist when the caller can see one with
    /// an equal MusicBrainz artist id or an equal normalized name. Otherwise it
    /// resolves to a catalog artist found by name. The stored artists come
    /// first in the returned vector, then the catalog ones, each group in the
    /// suggestions' order.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller can see, in the order a match is
    ///   preferred.
    /// - `suggestions`: the suggestions to resolve.
    /// - `limit`: the maximum number of suggestions to resolve.
    ///
    /// # Returns
    ///
    /// The resolved artists, without the suggestions that could not be
    /// resolved.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a library or database read fails.
    pub async fn resolve_artists(
        &self,
        scopes: &[Scope],
        suggestions: &[SuggestedArtist],
        limit: u32,
    ) -> Result<Vec<ResolvedArtist>, CatalogError> {
        let wanted = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(suggestions.len());
        let mut stored = Vec::new();
        let mut catalog = Vec::new();
        for suggestion in &suggestions[..wanted] {
            match self.resolve_artist(scopes, suggestion).await? {
                Some(ResolvedArtist::Stored(artist)) => stored.push(ResolvedArtist::Stored(artist)),
                Some(resolved @ ResolvedArtist::Catalog(_)) => catalog.push(resolved),
                None => {}
            }
        }
        stored.extend(catalog);
        Ok(stored)
    }

    /// Resolves one suggested track through catalog search.
    ///
    /// # Parameters
    ///
    /// - `suggestion`: the suggestion to resolve.
    ///
    /// # Returns
    ///
    /// The catalog entity the suggestion names, or `None` when the search did
    /// not name it.
    async fn resolve_track(&self, suggestion: &SuggestedTrack) -> Option<CatalogTrackResult> {
        let key = track_resolution_key(suggestion);
        if let Some(cached) = self.inner.track_resolutions.get(&key).await {
            return Some(cached);
        }
        let primary = suggestion
            .artists
            .first()
            .map(String::as_str)
            .unwrap_or_default();
        let text = if primary.is_empty() {
            suggestion.title.clone()
        } else {
            format!("{primary} {}", suggestion.title)
        };
        let query = CatalogQuery {
            text,
            kinds: vec![EntityKind::Track],
            limit: TRACK_RESOLUTION_LIMIT,
            offset: 0,
        };
        let found = match self.inner.catalog.search(&query).await {
            Ok(found) => found,
            Err(error) => {
                tracing::debug!(
                    %error,
                    title = suggestion.title.as_str(),
                    "a suggested track could not be resolved"
                );
                return None;
            }
        };
        let title = normalize(&suggestion.title);
        let resolved = found.tracks.into_iter().find(|result| {
            if normalize(&result.track.title) != title {
                return false;
            }
            primary.is_empty()
                || result
                    .track
                    .artists
                    .first()
                    .map(|credit| normalize(&credit.name) == normalize(primary))
                    .unwrap_or(false)
        });
        if let Some(resolved) = &resolved {
            self.inner
                .track_resolutions
                .insert(key, resolved.clone())
                .await;
        }
        resolved
    }

    /// Resolves one suggested artist to a visible stored artist or a catalog
    /// entity.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller can see.
    /// - `suggestion`: the suggestion to resolve.
    ///
    /// # Returns
    ///
    /// The resolved artist, or `None` when neither the library nor the catalog
    /// names one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a library read fails.
    async fn resolve_artist(
        &self,
        scopes: &[Scope],
        suggestion: &SuggestedArtist,
    ) -> Result<Option<ResolvedArtist>, CatalogError> {
        let database: &Database = self.inner.catalog.database();
        let mut conn = database.readers().acquire().await?;
        let mbid = suggestion
            .ids
            .iter()
            .find(|id| id.namespace == hearthsound_core::providers::MB_ARTIST)
            .filter(|id| !id.value.is_empty());
        if let Some(mbid) = mbid {
            let found =
                hearthsound_library::artists::find_artist_by_mb_id(&mut conn, &mbid.value).await?;
            if let Some(artist) = found
                && visible_in(&mut conn, scopes, artist.id).await?
            {
                return Ok(Some(ResolvedArtist::Stored(artist)));
            }
        }
        let wanted = normalize(&suggestion.name);
        if wanted.is_empty() {
            return Ok(None);
        }
        for scope in scopes {
            let found = hearthsound_library::query::search(
                &mut conn,
                *scope,
                &suggestion.name,
                &[EntityKind::Artist],
                ARTIST_SEARCH_LIMIT,
                0,
            )
            .await?;
            if let Some(artist) = found
                .artists
                .into_iter()
                .find(|artist| normalize(&artist.name) == wanted)
            {
                return Ok(Some(ResolvedArtist::Stored(artist)));
            }
        }
        drop(conn);
        let query = CatalogQuery {
            text: suggestion.name.clone(),
            kinds: vec![EntityKind::Artist],
            limit: ARTIST_RESOLUTION_LIMIT,
            offset: 0,
        };
        match self.inner.catalog.search(&query).await {
            Ok(found) => Ok(found
                .artists
                .into_iter()
                .find(|result| normalize(&result.artist.name) == wanted)
                .map(ResolvedArtist::Catalog)),
            Err(error) => {
                tracing::debug!(
                    %error,
                    name = suggestion.name.as_str(),
                    "a suggested artist could not be resolved"
                );
                Ok(None)
            }
        }
    }
}

impl std::fmt::Debug for DiscoveryService {
    /// Reports the service without the registry or the per-user source, which
    /// hold plugin configuration.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryService")
            .field("catalog", &self.inner.catalog)
            .finish_non_exhaustive()
    }
}

/// Returns whether a stored artist is visible in any of the scopes.
///
/// # Parameters
///
/// - `conn`: the connection to read on.
/// - `scopes`: the scopes the caller can see.
/// - `artist`: the artist to check.
///
/// # Returns
///
/// `true` when at least one scope contains the artist.
///
/// # Errors
///
/// Returns [`CatalogError`] when a read fails.
async fn visible_in(
    conn: &mut sqlx::SqliteConnection,
    scopes: &[Scope],
    artist: hearthsound_core::EntityId,
) -> Result<bool, CatalogError> {
    for scope in scopes {
        if hearthsound_library::query::artist_detail(conn, *scope, artist)
            .await?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Builds the cache key of one resolved suggestion.
///
/// The key names the suggestion the way the resolution decides it: the title,
/// the artist names, and the identifiers the service attached. Two suggestions
/// that differ in any of them are different questions and get different keys.
///
/// # Parameters
///
/// - `suggestion`: the suggestion to key.
///
/// # Returns
///
/// The canonical key.
fn track_resolution_key(suggestion: &SuggestedTrack) -> String {
    let mut key = format!(
        "title={}|artists={}",
        encode(&suggestion.title),
        suggestion.artists.len()
    );
    for artist in &suggestion.artists {
        key.push_str("|artist=");
        key.push_str(&encode(artist));
    }
    for id in &suggestion.ids {
        key.push_str("|id=");
        key.push_str(&encode(&id.namespace));
        key.push('=');
        key.push_str(&encode(&id.value));
    }
    key
}

/// Builds the canonical argument string of a track query.
///
/// Every argument is length-prefixed, so two different seeds never produce the
/// same string.
///
/// # Parameters
///
/// - `seed`: the track the query is seeded with.
/// - `limit`: the maximum number of suggestions.
///
/// # Returns
///
/// The canonical string.
fn track_arguments(seed: &TrackSeed, limit: u32) -> String {
    let mut out = format!("limit={limit}|title={}", encode(&seed.title));
    for artist in &seed.artists {
        out.push_str("|artist=");
        out.push_str(&encode(artist));
    }
    for id in &seed.ids {
        out.push_str("|id=");
        out.push_str(&encode(&id.namespace));
        out.push('=');
        out.push_str(&encode(&id.value));
    }
    out
}

/// Builds the canonical argument string of an artist query.
///
/// Every argument is length-prefixed, so two different seeds never produce the
/// same string.
///
/// # Parameters
///
/// - `seed`: the artist the query is seeded with.
/// - `limit`: the maximum number of suggestions.
///
/// # Returns
///
/// The canonical string.
fn artist_arguments(seed: &ArtistSeed, limit: u32) -> String {
    let mut out = format!("limit={limit}|name={}", encode(&seed.name));
    for id in &seed.ids {
        out.push_str("|id=");
        out.push_str(&encode(&id.namespace));
        out.push('=');
        out.push_str(&encode(&id.value));
    }
    out
}

/// Builds the seed a similar-artist query is made with from a stored artist.
///
/// # Parameters
///
/// - `artist`: the stored artist.
///
/// # Returns
///
/// The artist's name and its MusicBrainz artist id, when the row carries one.
fn artist_seed(artist: &Artist) -> ArtistSeed {
    ArtistSeed {
        name: artist.name.clone(),
        ids: artist
            .mb_artist_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(|value| hearthsound_core::providers::ExternalId {
                namespace: hearthsound_core::providers::MB_ARTIST.to_owned(),
                value: value.to_owned(),
            })
            .into_iter()
            .collect(),
    }
}

/// Interleaves one list of albums per artist and drops repeated albums.
///
/// # Parameters
///
/// - `lists`: the per-artist lists, in the order the artists were suggested.
/// - `size`: the maximum number of albums.
///
/// # Returns
///
/// The first album of every artist, then the second of every artist, and so
/// on, up to `size` distinct albums. Two albums are the same when their
/// normalized title and primary artist agree, whether they came from the
/// library or the catalog.
fn interleave_albums(lists: Vec<Vec<ResolvedAlbum>>, size: u32) -> Vec<ResolvedAlbum> {
    let limit = usize::try_from(size).unwrap_or(usize::MAX);
    let mut albums = Vec::new();
    let mut seen = HashSet::new();
    let mut index = 0;
    loop {
        let mut advanced = false;
        for list in &lists {
            if let Some(album) = list.get(index) {
                advanced = true;
                if seen.insert(album_key(album)) {
                    albums.push(album.clone());
                    if albums.len() >= limit {
                        return albums;
                    }
                }
            }
        }
        if !advanced {
            break;
        }
        index += 1;
    }
    albums
}

/// Returns the normalized identity two album lists are deduplicated by.
///
/// # Parameters
///
/// - `album`: the album.
///
/// # Returns
///
/// The normalized title and primary artist.
fn album_key(album: &ResolvedAlbum) -> (String, String) {
    match album {
        ResolvedAlbum::Stored(album) => (
            normalize(&album.title),
            normalize(album.display_artist.as_deref().unwrap_or_default()),
        ),
        ResolvedAlbum::Catalog(result) => (
            normalize(&result.album.title),
            normalize(
                result
                    .album
                    .artists
                    .first()
                    .map(|credit| credit.name.as_str())
                    .unwrap_or_default(),
            ),
        ),
    }
}

/// Length-prefixes one free-form argument.
///
/// # Parameters
///
/// - `value`: the argument.
///
/// # Returns
///
/// The value's byte length, a colon, and the value.
fn encode(value: &str) -> String {
    format!("{}:{value}", value.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hearthsound_core::providers::ExternalId;

    /// Builds a seed track for the key tests.
    fn track() -> TrackSeed {
        TrackSeed {
            title: "Creep".to_owned(),
            artists: vec!["Radiohead".to_owned()],
            ids: vec![ExternalId {
                namespace: "mb-recording".to_owned(),
                value: "recording-1".to_owned(),
            }],
        }
    }

    #[test]
    fn a_track_key_names_every_argument() {
        let key = track_arguments(&track(), 20);

        assert_eq!(
            key,
            "limit=20|title=5:Creep|artist=9:Radiohead|id=12:mb-recording=11:recording-1"
        );
    }

    #[test]
    fn different_limits_produce_different_keys() {
        assert_ne!(track_arguments(&track(), 20), track_arguments(&track(), 21));
    }

    #[test]
    fn an_artist_key_names_every_argument() {
        let seed = ArtistSeed {
            name: "Radiohead".to_owned(),
            ids: vec![ExternalId {
                namespace: "mb-artist".to_owned(),
                value: "artist-1".to_owned(),
            }],
        };

        assert_eq!(
            artist_arguments(&seed, 5),
            "limit=5|name=9:Radiohead|id=9:mb-artist=8:artist-1"
        );
    }

    /// Builds a stored album for the interleave tests.
    ///
    /// # Parameters
    ///
    /// - `title`: the album title.
    /// - `artist`: the album's display artist.
    fn stored_album(title: &str, artist: &str) -> ResolvedAlbum {
        ResolvedAlbum::Stored(Album {
            id: hearthsound_core::EntityId::new(hearthsound_core::IdPrefix::Album),
            title: title.to_owned(),
            sort_title: title.to_lowercase(),
            display_artist: Some(artist.to_owned()),
            year: None,
            release_date: None,
            album_type: None,
            mb_release_id: None,
            mb_release_group_id: None,
            upc: None,
            origin: hearthsound_library::Origin::Catalog,
            created_at: 0,
            updated_at: 0,
        })
    }

    /// Returns the titles of a resolved album list.
    ///
    /// # Parameters
    ///
    /// - `albums`: the albums to name.
    fn titles(albums: &[ResolvedAlbum]) -> Vec<&str> {
        albums
            .iter()
            .map(|album| match album {
                ResolvedAlbum::Stored(album) => album.title.as_str(),
                ResolvedAlbum::Catalog(result) => result.album.title.as_str(),
            })
            .collect()
    }

    #[test]
    fn albums_are_interleaved_one_artist_at_a_time() {
        let lists = vec![
            vec![stored_album("First", "One"), stored_album("Third", "One")],
            vec![stored_album("Second", "Two")],
        ];

        assert_eq!(
            titles(&interleave_albums(lists, 10)),
            ["First", "Second", "Third"]
        );
    }

    #[test]
    fn a_repeated_album_is_listed_once() {
        let lists = vec![
            vec![stored_album("Same", "One")],
            vec![stored_album("same", "one")],
        ];

        assert_eq!(titles(&interleave_albums(lists, 10)), ["Same"]);
    }

    #[test]
    fn the_album_list_stops_at_the_requested_size() {
        let lists = vec![
            vec![stored_album("First", "One"), stored_album("Third", "One")],
            vec![stored_album("Second", "Two")],
        ];

        assert_eq!(titles(&interleave_albums(lists, 2)), ["First", "Second"]);
    }
}
