//! Hello catalog: a minimal Hearthsound plugin.
//!
//! The plugin answers every search with a single greeting track, plus a
//! matching album and artist. It exists to be a copy-paste starting point for
//! new plugins; for that reason it keeps its whole behavior in one file.
//!
//! The only configuration is `greeting`, which prefixes the track title.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{
    Album, AlbumRef, Artist, ArtistCredit, EntityKind, IdPair, ProviderError, Track,
};

/// The plugin implementation.
struct HelloCatalog;

/// Server-scoped configuration of the plugin.
#[derive(serde::Deserialize)]
struct Config {
    /// Prefix for the title of the returned track.
    #[serde(default = "default_greeting")]
    greeting: String,
}

/// Returns the default greeting used when the server config omits one.
fn default_greeting() -> String {
    "Hello".to_string()
}

/// Builds the album reference every entity in this plugin points at.
fn hello_album_ref() -> AlbumRef {
    AlbumRef {
        id: "hello-album-1".to_string(),
        title: "Hello Album".to_string(),
        artists: vec![ArtistCredit {
            name: "Hello Artist".to_string(),
            ids: vec![],
        }],
        year: Some(2026),
        images: vec![],
    }
}

/// Builds the artist every entity in this plugin points at.
fn hello_artist() -> Artist {
    Artist {
        id: "hello-artist-1".to_string(),
        name: "Hello Artist".to_string(),
        images: vec![],
        ids: vec![],
    }
}

/// Builds the single track this plugin returns.
///
/// # Parameters
///
/// - `greeting`: prefix for the track title.
/// - `text`: the search text the track is answering.
fn hello_track(greeting: &str, text: &str) -> Track {
    Track {
        id: "hello-track-1".to_string(),
        title: format!("{greeting}, {text}"),
        artists: vec![ArtistCredit {
            name: "Hello Artist".to_string(),
            ids: vec![],
        }],
        album: Some(hello_album_ref()),
        duration_ms: Some(180_000),
        disc_number: Some(1),
        track_number: Some(1),
        isrc: None,
        explicit: Some(false),
        ids: vec![IdPair {
            namespace: "hello".to_string(),
            value: "hello-track-1".to_string(),
        }],
    }
}

/// Returns whether the query asked for `kind`.
///
/// An empty `kinds` list means every kind was asked for.
fn wants(kinds: &[EntityKind], wanted: EntityKind) -> bool {
    kinds.is_empty() || kinds.iter().any(|kind| matches!(kind, k if *k == wanted))
}

/// Validates the plugin's server-scoped configuration.
fn validate_config(server_config: &str) -> Result<(), Vec<String>> {
    match serde_json::from_str::<Config>(server_config) {
        Ok(_) => Ok(()),
        Err(e) => Err(vec![e.to_string()]),
    }
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for HelloCatalog {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        validate_config(&server_config)
    }

    fn health() -> Result<String, ProviderError> {
        Ok("hello-catalog ready".to_string())
    }
}

/// Implements the catalog interface.
impl catalog::Guest for HelloCatalog {
    fn search(query: SearchQuery) -> Result<SearchResults, ProviderError> {
        hearthsound_plugin::info!("search: {}", query.text);

        if query.limit == 0 || query.offset > 0 {
            return Ok(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
            });
        }

        let config: Config = hearthsound_plugin::config::server_config()?;
        let greeting = config.greeting;

        Ok(SearchResults {
            tracks: if wants(&query.kinds, EntityKind::Track) {
                vec![hello_track(&greeting, &query.text)]
            } else {
                vec![]
            },
            albums: if wants(&query.kinds, EntityKind::Album) {
                vec![hello_album_ref()]
            } else {
                vec![]
            },
            artists: if wants(&query.kinds, EntityKind::Artist) {
                vec![hello_artist()]
            } else {
                vec![]
            },
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        if id == "hello-track-1" {
            let config: Config = hearthsound_plugin::config::server_config()?;
            Ok(hello_track(&config.greeting, "world"))
        } else {
            Err(ProviderError::NotFound)
        }
    }

    fn get_album(id: String) -> Result<Album, ProviderError> {
        if id == "hello-album-1" {
            let config: Config = hearthsound_plugin::config::server_config()?;
            Ok(Album {
                id: "hello-album-1".to_string(),
                title: "Hello Album".to_string(),
                artists: vec![ArtistCredit {
                    name: "Hello Artist".to_string(),
                    ids: vec![],
                }],
                release_date: Some("2026-01-01".to_string()),
                album_type: Some("album".to_string()),
                upc: None,
                genres: vec![],
                images: vec![],
                tracks: vec![hello_track(&config.greeting, "world")],
                ids: vec![],
            })
        } else {
            Err(ProviderError::NotFound)
        }
    }

    fn get_artist(id: String) -> Result<Artist, ProviderError> {
        if id == "hello-artist-1" {
            Ok(hello_artist())
        } else {
            Err(ProviderError::NotFound)
        }
    }

    fn artist_albums(id: String, limit: u32, offset: u32) -> Result<Vec<AlbumRef>, ProviderError> {
        if id != "hello-artist-1" {
            return Err(ProviderError::NotFound);
        }
        Ok(vec![hello_album_ref()]
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    fn artist_top_tracks(id: String, limit: u32) -> Result<Vec<Track>, ProviderError> {
        if id != "hello-artist-1" {
            return Err(ProviderError::NotFound);
        }
        let config: Config = hearthsound_plugin::config::server_config()?;
        Ok(vec![hello_track(&config.greeting, "world")]
            .into_iter()
            .take(limit as usize)
            .collect())
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![])
    }
}

hearthsound_plugin::export_catalog_provider!(HelloCatalog);
