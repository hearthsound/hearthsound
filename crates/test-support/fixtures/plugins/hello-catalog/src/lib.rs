//! Hello catalog: a minimal Hearthsound plugin.
//!
//! The plugin answers every search with a greeting track, plus a matching album
//! and artist. It exists to be a copy-paste starting point for new plugins; for
//! that reason it keeps its whole behavior in one file.
//!
//! Its entities carry canonical identifiers (`mb-recording`, `mb-release`,
//! `mb-artist`), which is what identity resolution and materialization record
//! in `external_ids`, and its artist credits carry the artist's own id under
//! the `hello` namespace, which is what names the instance to resolve a
//! credited artist against. The identifiers are optional in the sense that a
//! configured one is reported and an unconfigured one is not.
//!
//! The configuration is:
//!
//! - `greeting`, which prefixes the track title;
//! - `isrc`, the ISRC every track it reports carries;
//! - `coverUrl`, a URL template with a `{size}` placeholder, out of which the
//!   plugin builds three cover images of 250, 500 and 1000 pixels.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{
    Album, AlbumRef, Artist, ArtistCredit, EntityKind, IdPair, Image, ProviderError, Track,
};

/// The plugin implementation.
struct HelloCatalog;

/// The cover sizes the plugin reports, in pixels.
const COVER_SIZES: [u32; 3] = [250, 500, 1000];

/// The placeholder a configured cover URL carries.
const COVER_SIZE_PLACEHOLDER: &str = "{size}";

/// Server-scoped configuration of the plugin.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// Prefix for the title of the returned track.
    #[serde(default = "default_greeting")]
    greeting: String,
    /// The ISRC every track carries, when configured.
    #[serde(default)]
    isrc: Option<String>,
    /// The cover URL template, when configured.
    #[serde(default)]
    cover_url: Option<String>,
    /// The number of albums the provider states for its artist, when configured.
    #[serde(default)]
    album_count: Option<u32>,
}

/// Returns the default greeting used when the server config omits one.
fn default_greeting() -> String {
    "Hello".to_string()
}

/// Builds the identifier pairs naming the artist of the fixture.
///
/// The MusicBrainz id is the artist's identity; the `hello` pair is this
/// plugin's own id for the same artist, which is what a credit of a track or an
/// album carries so the server resolves the credited artist against this
/// instance.
fn artist_ids() -> Vec<IdPair> {
    vec![
        IdPair {
            namespace: "mb-artist".to_string(),
            value: "hello-artist-1".to_string(),
        },
        IdPair {
            namespace: "hello".to_string(),
            value: "hello-artist-1".to_string(),
        },
    ]
}

/// Builds the cover images the fixture reports.
///
/// # Parameters
///
/// - `cover_url`: the configured URL template, when there is one.
fn cover_images(cover_url: Option<&str>) -> Vec<Image> {
    let Some(url) = cover_url else {
        return vec![];
    };
    COVER_SIZES
        .iter()
        .map(|size| Image {
            url: url.replace(COVER_SIZE_PLACEHOLDER, &size.to_string()),
            width: Some(*size),
            height: Some(*size),
        })
        .collect()
}

/// Builds the album reference every entity in this plugin points at.
///
/// # Parameters
///
/// - `cover_url`: the configured cover URL template, when there is one.
fn hello_album_ref(cover_url: Option<&str>) -> AlbumRef {
    AlbumRef {
        id: "hello-album-1".to_string(),
        title: "Hello Album".to_string(),
        artists: vec![ArtistCredit {
            name: "Hello Artist".to_string(),
            ids: artist_ids(),
        }],
        year: Some(2026),
        images: cover_images(cover_url),
        album_type: None,
        edition: None,
        track_count: None,
        ids: vec![],
    }
}

/// Builds the artist every entity in this plugin points at.
///
/// # Parameters
///
/// - `cover_url`: the configured cover URL template, when there is one.
fn hello_artist(cover_url: Option<&str>, album_count: Option<u32>) -> Artist {
    Artist {
        id: "hello-artist-1".to_string(),
        name: "Hello Artist".to_string(),
        images: cover_images(cover_url),
        ids: artist_ids(),
        album_count,
    }
}

/// Builds one of the tracks this plugin returns.
///
/// # Parameters
///
/// - `greeting`: prefix for the track title.
/// - `text`: the search text the track is answering.
/// - `number`: the track number within the album.
/// - `isrc`: the ISRC the track carries, when the configuration sets one.
/// - `cover_url`: the configured cover URL template, when there is one.
fn hello_track(
    greeting: &str,
    text: &str,
    number: u8,
    isrc: Option<String>,
    cover_url: Option<&str>,
) -> Track {
    Track {
        id: format!("hello-track-{number}"),
        title: format!("{greeting}, {text}"),
        artists: vec![ArtistCredit {
            name: "Hello Artist".to_string(),
            ids: artist_ids(),
        }],
        album: Some(hello_album_ref(cover_url)),
        duration_ms: Some(180_000),
        disc_number: Some(1),
        track_number: Some(number.into()),
        isrc,
        explicit: Some(false),
        ids: vec![
            IdPair {
                namespace: "hello".to_string(),
                value: format!("hello-track-{number}"),
            },
            IdPair {
                namespace: "mb-recording".to_string(),
                value: format!("hello-recording-{number}"),
            },
        ],
    }
}

/// Builds the full album this plugin reports.
///
/// # Parameters
///
/// - `greeting`: prefix for the title of every track.
/// - `isrc`: the ISRC every track carries, when the configuration sets one.
/// - `cover_url`: the configured cover URL template, when there is one.
fn hello_album(greeting: &str, isrc: Option<String>, cover_url: Option<&str>) -> Album {
    Album {
        id: "hello-album-1".to_string(),
        title: "Hello Album".to_string(),
        artists: vec![ArtistCredit {
            name: "Hello Artist".to_string(),
            ids: artist_ids(),
        }],
        release_date: Some("2026-01-01".to_string()),
        album_type: Some("album".to_string()),
        upc: Some("0088888888888".to_string()),
        genres: vec![],
        images: cover_images(cover_url),
        edition: None,
        tracks: vec![
            hello_track(greeting, "world", 1, isrc.clone(), cover_url),
            hello_track(greeting, "again", 2, isrc, cover_url),
        ],
        ids: vec![IdPair {
            namespace: "mb-release".to_string(),
            value: "hello-release-1".to_string(),
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
        let cover_url = config.cover_url.as_deref();

        Ok(SearchResults {
            tracks: if wants(&query.kinds, EntityKind::Track) {
                vec![hello_track(
                    &greeting,
                    &query.text,
                    1,
                    config.isrc.clone(),
                    cover_url,
                )]
            } else {
                vec![]
            },
            albums: if wants(&query.kinds, EntityKind::Album) {
                vec![hello_album_ref(cover_url)]
            } else {
                vec![]
            },
            artists: if wants(&query.kinds, EntityKind::Artist) {
                vec![hello_artist(cover_url, config.album_count)]
            } else {
                vec![]
            },
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        let Some(number) = track_number(&id) else {
            return Err(ProviderError::NotFound);
        };
        let config: Config = hearthsound_plugin::config::server_config()?;
        Ok(hello_track(
            &config.greeting,
            "world",
            number,
            config.isrc,
            config.cover_url.as_deref(),
        ))
    }

    fn get_album(id: String) -> Result<Album, ProviderError> {
        if id != "hello-album-1" {
            return Err(ProviderError::NotFound);
        }
        let config: Config = hearthsound_plugin::config::server_config()?;
        Ok(hello_album(
            &config.greeting,
            config.isrc,
            config.cover_url.as_deref(),
        ))
    }

    fn get_artist(id: String) -> Result<Artist, ProviderError> {
        if id != "hello-artist-1" {
            return Err(ProviderError::NotFound);
        }
        let config: Config = hearthsound_plugin::config::server_config()?;
        Ok(hello_artist(config.cover_url.as_deref(), config.album_count))
    }

    fn artist_albums(id: String, limit: u32, offset: u32) -> Result<Vec<AlbumRef>, ProviderError> {
        if id != "hello-artist-1" {
            return Err(ProviderError::NotFound);
        }
        let config: Config = hearthsound_plugin::config::server_config()?;
        Ok(vec![hello_album_ref(config.cover_url.as_deref())]
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
        Ok(vec![hello_track(
            &config.greeting,
            "world",
            1,
            config.isrc,
            config.cover_url.as_deref(),
        )]
        .into_iter()
        .take(limit as usize)
        .collect())
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![])
    }
    fn lookup_album(_ids: Vec<IdPair>) -> Result<Vec<AlbumRef>, ProviderError> {
        Ok(vec![])
    }
}

/// Returns the track number an id names, when it names a track of the fixture.
///
/// # Parameters
///
/// - `id`: the provider-local track id.
fn track_number(id: &str) -> Option<u8> {
    id.strip_prefix("hello-track-")?.parse().ok()
}

hearthsound_plugin::export_catalog_provider!(HelloCatalog);
