//! The adapter from the catalog plugin interface to the core provider traits.
//!
//! This module is the only place where WIT records become core types and core
//! types become WIT records. Everything above it sees [`CatalogProvider`] and
//! its core types; everything below it sees generated bindings.

use async_trait::async_trait;
use hearthsound_core::providers::{
    ArtistCredit, CatalogAlbum, CatalogAlbumRef, CatalogArtist, CatalogProvider, CatalogQuery,
    CatalogResults, CatalogTrack, ExternalId, Image, ProviderLifecycle,
};
use hearthsound_core::{EntityKind, ProviderError, Slug};

use crate::bindings;
use crate::bindings::exports::hearthsound::plugin::catalog::{SearchQuery, SearchResults};
use crate::bindings::hearthsound::plugin::types as wit;
use crate::error::map_provider_error;
use crate::pool::StorePool;

/// A catalog provider backed by one plugin instance.
pub(crate) struct WasmCatalogProvider {
    pool: StorePool<bindings::CatalogProvider>,
    plugin: String,
    slug: Slug,
}

impl WasmCatalogProvider {
    /// Wraps a store pool as a catalog provider.
    ///
    /// # Parameters
    ///
    /// - `pool`: the instance's store pool.
    /// - `plugin`: the plugin id, used in log fields.
    /// - `slug`: the instance slug.
    pub(crate) fn new(
        pool: StorePool<bindings::CatalogProvider>,
        plugin: String,
        slug: Slug,
    ) -> WasmCatalogProvider {
        WasmCatalogProvider { pool, plugin, slug }
    }

    /// Maps a guest error to the core error, logging the guest's message.
    fn map_error(&self, error: wit::ProviderError) -> ProviderError {
        map_provider_error(&self.plugin, &self.slug, error)
    }
}

#[async_trait]
impl ProviderLifecycle for WasmCatalogProvider {
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
impl CatalogProvider for WasmCatalogProvider {
    async fn search(&self, query: &CatalogQuery) -> Result<CatalogResults, ProviderError> {
        let query = to_wit_query(query);
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_search(store, &query)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_results)
            .map_err(|error| self.map_error(error))
    }

    async fn track(&self, id: &str) -> Result<CatalogTrack, ProviderError> {
        let id = id.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_get_track(store, &id)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_track)
            .map_err(|error| self.map_error(error))
    }

    async fn album(&self, id: &str) -> Result<CatalogAlbum, ProviderError> {
        let id = id.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_get_album(store, &id)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_album)
            .map_err(|error| self.map_error(error))
    }

    async fn artist(&self, id: &str) -> Result<CatalogArtist, ProviderError> {
        let id = id.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_get_artist(store, &id)
                        .await
                })
            })
            .await?;

        outcome
            .map(to_core_artist)
            .map_err(|error| self.map_error(error))
    }

    async fn artist_albums(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CatalogAlbumRef>, ProviderError> {
        let id = id.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_artist_albums(store, &id, limit, offset)
                        .await
                })
            })
            .await?;

        outcome
            .map(|albums| albums.into_iter().map(to_core_album_ref).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn artist_top_tracks(
        &self,
        id: &str,
        limit: u32,
    ) -> Result<Vec<CatalogTrack>, ProviderError> {
        let id = id.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_artist_top_tracks(store, &id, limit)
                        .await
                })
            })
            .await?;

        outcome
            .map(|tracks| tracks.into_iter().map(to_core_track).collect())
            .map_err(|error| self.map_error(error))
    }

    async fn lookup_isrc(&self, isrc: &str) -> Result<Vec<CatalogTrack>, ProviderError> {
        let isrc = isrc.to_owned();
        let outcome = self
            .pool
            .call(move |store, instance| {
                Box::pin(async move {
                    instance
                        .hearthsound_plugin_catalog()
                        .call_lookup_isrc(store, &isrc)
                        .await
                })
            })
            .await?;

        outcome
            .map(|tracks| tracks.into_iter().map(to_core_track).collect())
            .map_err(|error| self.map_error(error))
    }
}

/// Converts a core query into the plugin's query record.
fn to_wit_query(query: &CatalogQuery) -> SearchQuery {
    SearchQuery {
        text: query.text.clone(),
        kinds: query.kinds.iter().copied().map(to_wit_kind).collect(),
        limit: query.limit,
        offset: query.offset,
    }
}

/// Converts a core entity kind into the plugin's enum.
fn to_wit_kind(kind: EntityKind) -> wit::EntityKind {
    match kind {
        EntityKind::Track => wit::EntityKind::Track,
        EntityKind::Album => wit::EntityKind::Album,
        EntityKind::Artist => wit::EntityKind::Artist,
        EntityKind::Playlist => wit::EntityKind::Playlist,
    }
}

/// Converts the plugin's search results into core results.
fn to_core_results(results: SearchResults) -> CatalogResults {
    CatalogResults {
        tracks: results.tracks.into_iter().map(to_core_track).collect(),
        albums: results.albums.into_iter().map(to_core_album_ref).collect(),
        artists: results.artists.into_iter().map(to_core_artist).collect(),
    }
}

/// Converts a plugin track into a core track.
pub(crate) fn to_core_track(track: wit::Track) -> CatalogTrack {
    CatalogTrack {
        id: track.id,
        title: track.title,
        artists: track.artists.into_iter().map(to_core_credit).collect(),
        album: track.album.map(to_core_album_ref),
        duration_ms: track.duration_ms,
        disc_number: track.disc_number,
        track_number: track.track_number,
        isrc: track.isrc,
        explicit: track.explicit,
        ids: track.ids.into_iter().map(to_core_id).collect(),
    }
}

/// Converts a plugin album reference into a core album reference.
fn to_core_album_ref(album: wit::AlbumRef) -> CatalogAlbumRef {
    CatalogAlbumRef {
        id: album.id,
        title: album.title,
        artists: album.artists.into_iter().map(to_core_credit).collect(),
        year: album.year,
        images: album.images.into_iter().map(to_core_image).collect(),
    }
}

/// Converts a plugin album into a core album.
pub(crate) fn to_core_album(album: wit::Album) -> CatalogAlbum {
    CatalogAlbum {
        id: album.id,
        title: album.title,
        artists: album.artists.into_iter().map(to_core_credit).collect(),
        release_date: album.release_date,
        album_type: album.album_type,
        upc: album.upc,
        genres: album.genres,
        images: album.images.into_iter().map(to_core_image).collect(),
        tracks: album.tracks.into_iter().map(to_core_track).collect(),
        ids: album.ids.into_iter().map(to_core_id).collect(),
    }
}

/// Converts a plugin artist into a core artist.
fn to_core_artist(artist: wit::Artist) -> CatalogArtist {
    CatalogArtist {
        id: artist.id,
        name: artist.name,
        images: artist.images.into_iter().map(to_core_image).collect(),
        ids: artist.ids.into_iter().map(to_core_id).collect(),
    }
}

/// Converts a plugin artist credit into a core artist credit.
pub(crate) fn to_core_credit(credit: wit::ArtistCredit) -> ArtistCredit {
    ArtistCredit {
        name: credit.name,
        ids: credit.ids.into_iter().map(to_core_id).collect(),
    }
}

/// Converts a core external identifier into the plugin's identifier pair.
pub(crate) fn to_wit_id(id: &ExternalId) -> wit::IdPair {
    wit::IdPair {
        namespace: id.namespace.clone(),
        value: id.value.clone(),
    }
}

/// Converts a plugin identifier pair into a core external identifier.
pub(crate) fn to_core_id(id: wit::IdPair) -> ExternalId {
    ExternalId {
        namespace: id.namespace,
        value: id.value,
    }
}

/// Converts a plugin image into a core image.
fn to_core_image(image: wit::Image) -> Image {
    Image {
        url: image.url,
        width: image.width,
        height: image.height,
    }
}
