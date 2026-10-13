//! HTTP catalog: a fixture plugin whose search goes out over `host-http`.
//!
//! The plugin appends the search text as a `q` query parameter to the endpoint
//! in its server configuration, performs a `GET`, and maps the JSON array the
//! endpoint returns into catalog tracks. It exists to prove that a guest can
//! reach a host the administrator approved, and that every other host is denied.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{ArtistCredit, EntityKind, IdPair, ProviderError, Track};
use serde::Deserialize;

/// The plugin implementation.
struct HttpCatalog;

/// Server-scoped configuration of the plugin.
#[derive(Deserialize)]
struct Config {
    /// The absolute URL the search request is sent to.
    url: String,
}

/// One track as the endpoint reports it.
#[derive(Deserialize)]
struct RemoteTrack {
    /// The endpoint's track identifier.
    id: String,
    /// The track title.
    title: String,
    /// The artist names, in credit order.
    #[serde(default)]
    artists: Vec<String>,
    /// The track duration in milliseconds.
    #[serde(default)]
    duration_ms: Option<u32>,
    /// The International Standard Recording Code.
    #[serde(default)]
    isrc: Option<String>,
}

/// Validates the plugin's server-scoped configuration.
fn validate_config(server_config: &str) -> Result<(), Vec<String>> {
    match serde_json::from_str::<Config>(server_config) {
        Ok(_) => Ok(()),
        Err(error) => Err(vec![error.to_string()]),
    }
}

/// Loads the plugin's server-scoped configuration.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Percent-encodes a query value.
///
/// # Parameters
///
/// - `text`: the value to encode.
///
/// # Returns
///
/// The value with everything except unreserved ASCII characters escaped.
fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Maps one remote track into a catalog track.
///
/// # Parameters
///
/// - `remote`: the track the endpoint returned.
/// - `album`: the album title every remote track is attributed to.
fn to_track(remote: RemoteTrack, album: &str) -> Track {
    let artists: Vec<ArtistCredit> = remote
        .artists
        .into_iter()
        .map(|name| ArtistCredit { name, ids: vec![] })
        .collect();
    Track {
        id: remote.id.clone(),
        title: remote.title,
        artists: artists.clone(),
        album: Some(hearthsound_plugin::types::AlbumRef {
            id: format!("{album}-1"),
            title: album.to_string(),
            artists,
            year: None,
            images: vec![],
        }),
        duration_ms: remote.duration_ms,
        disc_number: None,
        track_number: None,
        isrc: remote.isrc,
        explicit: None,
        ids: vec![IdPair {
            namespace: "http-catalog".to_string(),
            value: remote.id,
        }],
    }
}

/// Returns whether the query asked for `kind`.
fn wants(kinds: &[EntityKind], wanted: EntityKind) -> bool {
    kinds.is_empty() || kinds.iter().any(|kind| matches!(kind, k if *k == wanted))
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for HttpCatalog {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        validate_config(&server_config)
    }

    fn health() -> Result<String, ProviderError> {
        Ok("http-catalog ready".to_string())
    }
}

/// Implements the catalog interface.
impl catalog::Guest for HttpCatalog {
    fn search(query: SearchQuery) -> Result<SearchResults, ProviderError> {
        let config = config()?;
        let url = format!("{}?q={}", config.url, encode(&query.text));
        hearthsound_plugin::info!("searching {url}");

        let request = hearthsound_plugin::http::get(&url);
        let remote: Vec<RemoteTrack> = hearthsound_plugin::http::send_json(&request)?;

        let tracks: Vec<Track> = remote
            .into_iter()
            .map(|track| to_track(track, "Remote Album"))
            .collect();

        Ok(SearchResults {
            tracks: if wants(&query.kinds, EntityKind::Track) {
                tracks
            } else {
                vec![]
            },
            albums: vec![],
            artists: vec![],
        })
    }

    fn get_track(_id: String) -> Result<Track, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn get_album(_id: String) -> Result<hearthsound_plugin::types::Album, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn get_artist(_id: String) -> Result<hearthsound_plugin::types::Artist, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn artist_albums(
        _id: String,
        _limit: u32,
        _offset: u32,
    ) -> Result<Vec<hearthsound_plugin::types::AlbumRef>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn artist_top_tracks(_id: String, _limit: u32) -> Result<Vec<Track>, ProviderError> {
        Err(ProviderError::NotFound)
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![])
    }
}

hearthsound_plugin::export_catalog_provider!(HttpCatalog);
