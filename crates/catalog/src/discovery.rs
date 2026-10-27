//! The discovery layer: suggestions, resolved into rows.
//!
//! A discovery provider suggests music - similar artists, similar tracks,
//! recommendations, generated playlists - and never names a row. Every
//! suggestion is resolved here ([`EntityResolver`]) into the row it describes,
//! and the answers are stored in `answer_cache` as ids, so a listener's Home
//! shelf is served from the database and survives a restart or an outage.
//!
//! Nothing in this module calls a provider on the viewer's path when a stored
//! answer exists. The background sync fills the cache; a page reads it.

use std::sync::Arc;

use hearthsound_core::providers::{
    ArtistSeed, GeneratedPlaylist, SuggestedArtist, SuggestedTrack, TrackSeed, UserContext,
};
use hearthsound_core::{EntityId, EntityKind, IdPrefix, Layer};
use hearthsound_db::Database;
use hearthsound_plugin_host::ProviderRegistry;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CatalogError;
use crate::resolve::EntityResolver;

/// How long a discovery answer is reused.
pub const ANSWER_TTL_MS: i64 = 3_600_000;

/// Where a listener's configuration for one instance comes from.
#[async_trait::async_trait]
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

/// The stored account links, as a [`UserContextSource`].
pub struct LinkedUserContexts {
    configs: hearthsound_plugin_host::UserConfigService,
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
    pub fn new(configs: hearthsound_plugin_host::UserConfigService) -> LinkedUserContexts {
        LinkedUserContexts { configs }
    }
}

#[async_trait::async_trait]
impl UserContextSource for LinkedUserContexts {
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

/// A [`UserContextSource`] that reports no linked accounts.
pub struct NoUserContexts;

#[async_trait::async_trait]
impl UserContextSource for NoUserContexts {
    async fn user_context(&self, _instance: Uuid, _user: Uuid) -> Option<UserContext> {
        None
    }
}

/// One generated playlist, with the instance that generated it.
#[derive(Debug, Clone, PartialEq)]
pub struct InstancePlaylist {
    /// The instance that generated the playlist.
    pub instance: hearthsound_core::Slug,
    /// The playlist and its suggestions.
    pub playlist: GeneratedPlaylist,
}

/// One stored discovery answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredAnswer {
    /// Similar artists, by name.
    Artists(Vec<SuggestedArtist>),
    /// Suggested tracks.
    Tracks(Vec<SuggestedTrack>),
    /// Generated playlists.
    Playlists(Vec<GeneratedPlaylist>),
    /// Resolved ids of an earlier answer.
    Ids(Vec<Uuid>),
}

/// The discovery layer over the provider registry.
pub struct DiscoveryService {
    registry: Arc<ProviderRegistry>,
    database: Arc<Database>,
    users: Arc<dyn UserContextSource>,
    resolver: EntityResolver,
}

impl DiscoveryService {
    /// Builds the discovery service.
    ///
    /// # Parameters
    ///
    /// - `registry`: the provider registry.
    /// - `database`: the open database, which stores the answers.
    /// - `users`: where a listener's per-instance configuration comes from.
    /// - `resolver`: the resolver suggestions enter through.
    pub fn new(
        registry: Arc<ProviderRegistry>,
        database: Arc<Database>,
        users: Arc<dyn UserContextSource>,
        resolver: EntityResolver,
    ) -> DiscoveryService {
        DiscoveryService {
            registry,
            database,
            users,
            resolver,
        }
    }

    /// Suggests artists similar to one artist.
    ///
    /// # Parameters
    ///
    /// - `seed`: the artist to find similar artists for.
    /// - `limit`: the most suggestions.
    ///
    /// # Returns
    ///
    /// The first instance's non-empty answer, in its own order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored answer cannot be read.
    pub async fn similar_artists(
        &self,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedArtist>, CatalogError> {
        let arguments = format!("{}|{}", seed.name, limit);
        for instance in self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let key = format!("similar-artists|{}|{arguments}", instance.slug());
            if let Some(StoredAnswer::Artists(cached)) = self.answer(instance.id(), &key).await? {
                if !cached.is_empty() {
                    return Ok(cached);
                }
                continue;
            }
            match provider.similar_artists(None, seed, limit).await {
                Ok(artists) => {
                    self.store(instance.id(), &key, &StoredAnswer::Artists(artists.clone()))
                        .await?;
                    if !artists.is_empty() {
                        return Ok(artists);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a discovery instance could not answer a similar-artists query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Suggests tracks similar to one track.
    ///
    /// # Parameters
    ///
    /// - `seed`: the track to find similar tracks for.
    /// - `limit`: the most suggestions.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored answer cannot be read.
    pub async fn similar_tracks(
        &self,
        seed: &TrackSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, CatalogError> {
        let arguments = format!("{}|{}", seed.title, limit);
        for instance in self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let key = format!("similar-tracks|{}|{arguments}", instance.slug());
            if let Some(StoredAnswer::Tracks(cached)) = self.answer(instance.id(), &key).await? {
                if !cached.is_empty() {
                    return Ok(cached);
                }
                continue;
            }
            match provider.similar_tracks(None, seed, limit).await {
                Ok(tracks) => {
                    self.store(instance.id(), &key, &StoredAnswer::Tracks(tracks.clone()))
                        .await?;
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a discovery instance could not answer a similar-tracks query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Lists the playlists the discovery layer generated for a listener.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored answer cannot be read.
    pub async fn playlists(&self, user: Uuid) -> Result<Vec<InstancePlaylist>, CatalogError> {
        let mut playlists = Vec::new();
        for instance in self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let Some(context) = self.users.user_context(instance.id(), user).await else {
                continue;
            };
            let key = format!("playlists|{}|{user}", instance.slug());
            let generated = match self.answer(instance.id(), &key).await? {
                Some(StoredAnswer::Playlists(cached)) => cached,
                _ => match provider.playlists(&context).await {
                    Ok(generated) => {
                        self.store(
                            instance.id(),
                            &key,
                            &StoredAnswer::Playlists(generated.clone()),
                        )
                        .await?;
                        generated
                    }
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            instance = instance.slug().as_str(),
                            "a discovery instance could not list its playlists"
                        );
                        continue;
                    }
                },
            };
            for playlist in generated {
                playlists.push(InstancePlaylist {
                    instance: instance.slug().clone(),
                    playlist,
                });
            }
        }
        Ok(playlists)
    }

    /// Resolves suggested tracks into stored rows.
    ///
    /// A suggestion that carries a recording identifier resolves by it; one that
    /// carries none is searched for on the catalog layer with its title and
    /// artist, and dropped when no instance names it.
    ///
    /// # Parameters
    ///
    /// - `suggestions`: the suggestions.
    /// - `limit`: the most rows to return.
    ///
    /// # Returns
    ///
    /// One entry per suggestion, in order: the row, or `None` when nothing named
    /// it.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails.
    pub async fn resolve_tracks(
        &self,
        suggestions: &[SuggestedTrack],
        limit: u32,
    ) -> Result<Vec<Option<EntityId>>, CatalogError> {
        let mut resolved = Vec::with_capacity(suggestions.len());
        let mut connection = self.database.writer().acquire().await?;
        for suggestion in suggestions.iter().take(limit as usize) {
            let mut row = None;
            for id in &suggestion.ids {
                if let Some(owner) = hearthsound_library::external_ids::owner_of(
                    &mut connection,
                    EntityKind::Track,
                    &id.namespace,
                    &id.value,
                )
                .await
                .map_err(CatalogError::Library)?
                {
                    row = Some(owner);
                    break;
                }
            }
            if row.is_none() {
                let text = match suggestion.artists.first() {
                    Some(artist) => format!("{artist} {}", suggestion.title),
                    None => suggestion.title.clone(),
                };
                for instance in self.registry.layers(Layer::Catalog) {
                    let Some(provider) = instance.catalog() else {
                        continue;
                    };
                    let query = hearthsound_core::providers::CatalogQuery {
                        text: text.clone(),
                        kinds: vec![EntityKind::Track],
                        limit: 5,
                        offset: 0,
                    };
                    let results = provider.search(&query).await.map_err(|error| {
                        CatalogError::from_provider(instance.slug().as_str(), error)
                    })?;
                    let Some(hit) = results.tracks.first() else {
                        continue;
                    };
                    let row_id = self
                        .resolver
                        .track(&mut connection, instance.id(), instance.slug(), hit)
                        .await?;
                    row = Some(row_id);
                    break;
                }
            }
            resolved.push(row.map(|row| EntityId::from_uuid(IdPrefix::Track, row)));
        }
        Ok(resolved)
    }

    /// Suggests albums for a listener, resolved into rows.
    ///
    /// A discovery service recommends tracks, so the albums are the distinct
    /// albums of its recommendations, in the order it named them.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener.
    /// - `limit`: the most albums.
    ///
    /// # Returns
    ///
    /// The album rows, in the answering instance's order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails.
    pub async fn recommended_albums(
        &self,
        user: Uuid,
        limit: u32,
    ) -> Result<Vec<EntityId>, CatalogError> {
        let mut albums: Vec<Uuid> = Vec::new();
        for instance in self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let Some(context) = self.users.user_context(instance.id(), user).await else {
                continue;
            };
            let key = format!("recommended-albums|{}|{user}|{limit}", instance.slug());
            let ids = match self.answer(instance.id(), &key).await? {
                Some(StoredAnswer::Ids(ids)) => ids,
                _ => {
                    let Ok(suggestions) = provider
                        .recommended_tracks(&context, limit.saturating_mul(3))
                        .await
                    else {
                        continue;
                    };
                    let mut resolved: Vec<Uuid> = Vec::new();
                    let mut connection = self.database.writer().acquire().await?;
                    for suggestion in &suggestions {
                        let Some(title) = suggestion.album.as_ref() else {
                            continue;
                        };
                        let text = match suggestion.artists.first() {
                            Some(artist) => format!("{artist} {title}"),
                            None => title.clone(),
                        };
                        for catalog in self.registry.layers(Layer::Catalog) {
                            let Some(catalog_provider) = catalog.catalog() else {
                                continue;
                            };
                            let query = hearthsound_core::providers::CatalogQuery {
                                text: text.clone(),
                                kinds: vec![EntityKind::Album],
                                limit: 5,
                                offset: 0,
                            };
                            let Ok(results) = catalog_provider.search(&query).await else {
                                continue;
                            };
                            let Some(hit) = results.albums.first() else {
                                continue;
                            };
                            let row = self
                                .resolver
                                .album_ref(&mut connection, catalog.id(), catalog.slug(), hit)
                                .await?;
                            if !resolved.contains(&row) {
                                resolved.push(row);
                            }
                            break;
                        }
                        if resolved.len() >= limit as usize {
                            break;
                        }
                    }
                    drop(connection);
                    self.store(instance.id(), &key, &StoredAnswer::Ids(resolved.clone()))
                        .await?;
                    resolved
                }
            };
            albums.extend(ids);
            if !albums.is_empty() {
                break;
            }
        }
        Ok(albums
            .into_iter()
            .map(|album| EntityId::from_uuid(IdPrefix::Album, album))
            .collect())
    }

    /// Walks the discovery layer until one instance answers a popularity query.
    ///
    /// # Parameters
    ///
    /// - `seed`: the artist whose popular tracks are wanted.
    /// - `limit`: the most tracks.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored answer cannot be read.
    pub async fn artist_top_tracks(
        &self,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, CatalogError> {
        for instance in self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let key = format!(
                "artist-top-tracks|{}|{}|{limit}",
                instance.slug(),
                seed.name
            );
            if let Some(StoredAnswer::Tracks(cached)) = self.answer(instance.id(), &key).await? {
                if !cached.is_empty() {
                    return Ok(cached);
                }
                continue;
            }
            match provider.artist_top_tracks(None, seed, limit).await {
                Ok(tracks) => {
                    self.store(instance.id(), &key, &StoredAnswer::Tracks(tracks.clone()))
                        .await?;
                    if !tracks.is_empty() {
                        return Ok(tracks);
                    }
                }
                Err(error) => {
                    tracing::debug!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a discovery instance could not answer a popularity query"
                    );
                }
            }
        }
        Ok(Vec::new())
    }

    /// Reads a stored answer.
    async fn answer(
        &self,
        instance: Uuid,
        key: &str,
    ) -> Result<Option<StoredAnswer>, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT entity_refs, expires_at FROM answer_cache \
             WHERE instance_id = ? AND request_key = ?",
        )
        .bind(instance.as_bytes().as_slice())
        .bind(key)
        .fetch_optional(&mut *connection)
        .await
        .ok()
        .flatten();
        let Some((refs, expires_at)) = row else {
            return Ok(None);
        };
        if expires_at < unix_millis() {
            return Ok(None);
        }
        Ok(serde_json::from_str(&refs).ok())
    }

    /// Stores an answer.
    async fn store(
        &self,
        instance: Uuid,
        key: &str,
        answer: &StoredAnswer,
    ) -> Result<(), CatalogError> {
        let refs = serde_json::to_string(answer)?;
        let now = unix_millis();
        let mut connection = self.database.writer().acquire().await?;
        let _ = sqlx::query(
            "INSERT INTO answer_cache (instance_id, request_key, entity_refs, fetched_at, expires_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (instance_id, request_key) DO UPDATE SET \
                 entity_refs = excluded.entity_refs, fetched_at = excluded.fetched_at, \
                 expires_at = excluded.expires_at",
        )
        .bind(instance.as_bytes().as_slice())
        .bind(key)
        .bind(refs)
        .bind(now)
        .bind(now + ANSWER_TTL_MS)
        .execute(&mut *connection)
        .await;
        Ok(())
    }
}

/// Returns the current time in unix milliseconds.
fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
