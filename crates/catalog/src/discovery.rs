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

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use hearthsound_core::providers::{
    ArtistSeed, CatalogQuery, SuggestedArtist, SuggestedTrack, TrackSeed, UserContext,
};
use hearthsound_core::{EntityKind, Layer};
use hearthsound_db::Database;
use hearthsound_library::Artist;
use hearthsound_library::query::Scope;
use hearthsound_plugin_host::ProviderRegistry;
use moka::future::Cache as MemoryCache;
use uuid::Uuid;

use crate::dedupe::normalize;
use crate::error::CatalogError;
use crate::service::{CatalogService, CatalogTrackResult};

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
    /// A similar-track or top-track answer.
    Tracks(Vec<SuggestedTrack>),
    /// A similar-artist answer.
    Artists(Vec<SuggestedArtist>),
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
        DiscoveryService {
            inner: Arc::new(Inner {
                registry,
                catalog,
                users,
                cache,
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
        found.tracks.into_iter().find(|result| {
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
        })
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
}
