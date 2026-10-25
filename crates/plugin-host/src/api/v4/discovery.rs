//! The adapter from the discovery plugin interface to the core provider
//! traits.
//!
//! This module is the only place where the discovery world's WIT records become
//! core types and core types become WIT records. Everything above it sees
//! [`DiscoveryProvider`] and its core types; everything below it sees generated
//! bindings.

use async_trait::async_trait;
use hearthsound_core::providers::{
    ArtistSeed, DiscoveryProvider, GeneratedPlaylist, PlaylistSummary, ProviderLifecycle,
    SuggestedArtist, SuggestedTrack, TrackSeed, UserContext,
};
use hearthsound_core::{ProviderError, Slug};

use crate::api::v4::bindings;
use crate::api::v4::bindings::discovery::exports::hearthsound::plugin::discovery;
use crate::api::v4::bindings::hearthsound::plugin::types::{
    ProviderError as WitProviderError, UserContext as WitUserContext,
};
use crate::api::v4::catalog::{to_core_id, to_wit_id};
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// A discovery provider backed by one plugin instance.
pub(crate) struct WasmDiscoveryProvider {
    pool: StorePool<bindings::DiscoveryProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmDiscoveryProvider {
    /// Wraps a store pool as a discovery provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::DiscoveryProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmDiscoveryProvider {
        WasmDiscoveryProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: WitProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error.into())
    }
}

#[async_trait]
impl ProviderLifecycle for WasmDiscoveryProvider {
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
impl DiscoveryProvider for WasmDiscoveryProvider {
    async fn similar_tracks(
        &self,
        user: Option<&UserContext>,
        seed: &TrackSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError> {
        let user = user.map(to_wit_user);
        let seed = to_wit_track_seed(seed);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_discovery()
                        .call_similar_tracks(store, user.as_ref(), &seed, limit)
                        .await
                })
            })
            .await?;

        outcome
            .map(|tracks| tracks.into_iter().map(to_core_suggested_track).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn similar_artists(
        &self,
        user: Option<&UserContext>,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedArtist>, ProviderError> {
        let user = user.map(to_wit_user);
        let seed = to_wit_artist_seed(seed);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_discovery()
                        .call_similar_artists(store, user.as_ref(), &seed, limit)
                        .await
                })
            })
            .await?;

        outcome
            .map(|artists| artists.into_iter().map(to_core_suggested_artist).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn artist_top_tracks(
        &self,
        user: Option<&UserContext>,
        artist: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError> {
        let user = user.map(to_wit_user);
        let artist = to_wit_artist_seed(artist);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_discovery()
                        .call_artist_top_tracks(store, user.as_ref(), &artist, limit)
                        .await
                })
            })
            .await?;

        outcome
            .map(|tracks| tracks.into_iter().map(to_core_suggested_track).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn recommended_tracks(
        &self,
        user: &UserContext,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError> {
        let user = to_wit_user(user);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_discovery()
                        .call_recommended_tracks(store, &user, limit)
                        .await
                })
            })
            .await?;

        outcome
            .map(|tracks| tracks.into_iter().map(to_core_suggested_track).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn playlists(&self, user: &UserContext) -> Result<Vec<GeneratedPlaylist>, ProviderError> {
        let user = to_wit_user(user);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_discovery()
                        .call_playlists(store, &user)
                        .await
                })
            })
            .await?;

        outcome
            .map(|playlists| {
                playlists
                    .into_iter()
                    .map(to_core_generated_playlist)
                    .collect()
            })
            .map_err(|error| self.map_error(error))
    }
}

/// Converts a core user context into the plugin's record.
///
/// # Parameters
///
/// - `user`: the listener the call is made for.
fn to_wit_user(user: &UserContext) -> WitUserContext {
    WitUserContext {
        user_id: user.user_id.clone(),
        config: user.config.clone(),
    }
}

/// Converts a core track seed into the plugin's record.
///
/// # Parameters
///
/// - `seed`: the seed track.
fn to_wit_track_seed(seed: &TrackSeed) -> discovery::TrackSeed {
    discovery::TrackSeed {
        title: seed.title.clone(),
        artists: seed.artists.clone(),
        ids: seed.ids.iter().map(to_wit_id).collect(),
    }
}

/// Converts a core artist seed into the plugin's record.
///
/// # Parameters
///
/// - `seed`: the seed artist.
fn to_wit_artist_seed(seed: &ArtistSeed) -> discovery::ArtistSeed {
    discovery::ArtistSeed {
        name: seed.name.clone(),
        ids: seed.ids.iter().map(to_wit_id).collect(),
    }
}

/// Converts a suggested track the plugin returned.
///
/// # Parameters
///
/// - `track`: the suggested track.
fn to_core_suggested_track(track: discovery::SuggestedTrack) -> SuggestedTrack {
    SuggestedTrack {
        title: track.title,
        artists: track.artists,
        album: track.album,
        duration_ms: track.duration_ms,
        ids: track.ids.into_iter().map(to_core_id).collect(),
        score: track.score,
    }
}

/// Converts a suggested artist the plugin returned.
///
/// # Parameters
///
/// - `artist`: the suggested artist.
fn to_core_suggested_artist(artist: discovery::SuggestedArtist) -> SuggestedArtist {
    SuggestedArtist {
        name: artist.name,
        ids: artist.ids.into_iter().map(to_core_id).collect(),
        score: artist.score,
    }
}

/// Converts a generated playlist the plugin returned.
///
/// # Parameters
///
/// - `playlist`: the generated playlist.
fn to_core_generated_playlist(playlist: discovery::GeneratedPlaylist) -> GeneratedPlaylist {
    GeneratedPlaylist {
        summary: PlaylistSummary {
            reference: playlist.summary.reference,
            name: playlist.summary.name,
            description: playlist.summary.description,
            expires_at: playlist.summary.expires_at,
        },
        tracks: playlist
            .tracks
            .into_iter()
            .map(to_core_suggested_track)
            .collect(),
    }
}
