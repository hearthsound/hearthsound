//! Artwork fixture whose manifest forbids caching the images it answers.
//!
//! The artwork half is the artwork fixture's: the administrator configures a
//! base URL and the images of the albums and artists the plugin answers for, and
//! a subject matches an entry by title or by the MusicBrainz id it configures.
//! The catalog half answers nothing at all; it exists because the manifest's
//! `catalog.imageCachePolicy` is only legal for a plugin that provides the
//! catalog layer, and it is what makes this plugin's images uncacheable.
//!
//! A test points the base URL at a local mock server and asserts that the server
//! passes the `no-store` policy on to the client instead of storing the image.

use hearthsound_plugin::catalog_artwork_provider::exports::hearthsound::plugin::artwork::{
    ArtworkSubject, Guest as ArtworkGuest,
};
use hearthsound_plugin::catalog_artwork_provider::exports::hearthsound::plugin::catalog::{
    Guest as CatalogGuest, SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_artwork_provider::exports::hearthsound::plugin::lifecycle;
use hearthsound_plugin::types::{Album, AlbumRef, Artist, EntityKind, Image, ProviderError, Track};
use serde::Deserialize;

/// The plugin implementation.
struct ArtworkNoStore;

/// Server-scoped configuration of the plugin.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The base URL every image is served from.
    #[serde(default)]
    image_url: String,
    /// The albums the plugin answers for.
    #[serde(default)]
    albums: Vec<ConfiguredEntry>,
    /// The artists the plugin answers for.
    #[serde(default)]
    artists: Vec<ConfiguredEntry>,
}

/// One album or artist the plugin knows images for.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredEntry {
    /// The album title or artist name.
    title: String,
    /// The MusicBrainz id the entry also answers to.
    #[serde(default)]
    mbid: Option<String>,
    /// The entry's images.
    #[serde(default)]
    images: Vec<ConfiguredImage>,
}

/// One image of a configured entry.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredImage {
    /// The path the image is served at, relative to the base URL.
    path: String,
    /// The image's width in pixels, when the administrator knows it.
    #[serde(default)]
    width: Option<u32>,
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for ArtworkNoStore {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("artwork-nostore-fixture ready".to_string())
    }
}

/// Implements the catalog interface: the plugin provides the layer but adds
/// nothing to a search.
impl CatalogGuest for ArtworkNoStore {
    fn search(_query: SearchQuery) -> Result<SearchResults, ProviderError> {
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: Vec::new(),
        })
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

    fn artist_albums(
        _id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<Vec<AlbumRef>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn artist_top_tracks(_id: String, _limit: u32) -> Result<Vec<Track>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(Vec::new())
    }
}

/// Implements the artwork interface.
impl ArtworkGuest for ArtworkNoStore {
    fn find_artwork(subject: ArtworkSubject) -> Result<Vec<Image>, ProviderError> {
        let config: Config = hearthsound_plugin::config::server_config()?;
        let entries = match subject.kind {
            EntityKind::Album => &config.albums,
            EntityKind::Artist => &config.artists,
            // Any other kind is answered with an empty list.
            _ => return Ok(Vec::new()),
        };
        let mbid = subject
            .ids
            .iter()
            .find(|id| id.namespace == "mb-artist" || id.namespace == "mb-release")
            .map(|id| id.value.as_str());
        for entry in entries {
            let known = entry.title.eq_ignore_ascii_case(&subject.title)
                || match (&entry.mbid, mbid) {
                    (Some(configured), Some(requested)) => configured == requested,
                    _ => false,
                };
            if !known {
                continue;
            }
            hearthsound_plugin::info!(
                "answering {} images for `{}`",
                entry.images.len(),
                entry.title
            );
            return Ok(entry
                .images
                .iter()
                .map(|image| Image {
                    url: format!("{}{}", config.image_url, image.path),
                    width: image.width,
                    height: None,
                })
                .collect());
        }
        Ok(Vec::new())
    }
}

hearthsound_plugin::export_catalog_artwork_provider!(ArtworkNoStore);
