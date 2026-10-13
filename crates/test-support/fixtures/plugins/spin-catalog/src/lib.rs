//! Spin catalog: a test fixture whose search never returns.
//!
//! The plugin exists so the plugin host's deadline and trap handling can be
//! exercised: `search` spins forever, and every other catalog function reports
//! that the entity does not exist.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{Album, AlbumRef, Artist, ProviderError, Track};

/// The plugin implementation.
struct SpinCatalog;

/// Implements the lifecycle interface.
impl lifecycle::Guest for SpinCatalog {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("spin-catalog ready".to_string())
    }
}

/// Implements the catalog interface.
impl catalog::Guest for SpinCatalog {
    fn search(_query: SearchQuery) -> Result<SearchResults, ProviderError> {
        loop {
            core::hint::spin_loop();
        }
    }

    fn get_track(_id: String) -> Result<Track, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn get_album(_id: String) -> Result<Album, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn get_artist(_id: String) -> Result<Artist, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn artist_albums(_id: String, _limit: u32, _offset: u32) -> Result<Vec<AlbumRef>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn artist_top_tracks(_id: String, _limit: u32) -> Result<Vec<Track>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Err(ProviderError::NotFound)
    }
}

hearthsound_plugin::export_catalog_provider!(SpinCatalog);
