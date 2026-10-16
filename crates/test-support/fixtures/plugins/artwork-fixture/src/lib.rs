//! Artwork fixture: an artwork provider driven by server config.
//!
//! The administrator configures a base URL and the images of the albums and
//! artists the plugin should answer for. A subject matches an entry when its
//! title (or name) is equal ignoring case, or when one of the subject's
//! identifiers is the MusicBrainz id the entry configures; a match answers the
//! entry's images as `base + path`, and anything else answers an empty list.
//!
//! The fixture asks for no host permission beyond the image host, so a test can
//! point the base URL at a local mock server and assert which images the server
//! fetched.

use hearthsound_plugin::artwork_provider::exports::hearthsound::plugin::artwork::{
    ArtworkSubject, Guest,
};
use hearthsound_plugin::artwork_provider::exports::hearthsound::plugin::lifecycle;
use hearthsound_plugin::types::{EntityKind, Image, ProviderError};
use serde::Deserialize;

/// The plugin implementation.
struct ArtworkFixture;

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

/// Reads the plugin's server configuration.
///
/// # Returns
///
/// The configuration.
///
/// # Errors
///
/// Returns [`ProviderError`] when the configuration cannot be read or is not
/// the document the plugin expects.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for ArtworkFixture {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("artwork-fixture ready".to_string())
    }
}

/// Implements the artwork interface.
impl Guest for ArtworkFixture {
    fn find_artwork(subject: ArtworkSubject) -> Result<Vec<Image>, ProviderError> {
        let config = config()?;
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
            hearthsound_plugin::info!("answering {} images for `{}`", entry.images.len(), entry.title);
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

hearthsound_plugin::export_artwork_provider!(ArtworkFixture);
