//! HTTP catalog: a fixture plugin whose search and track lookup go out over
//! `host-http`.
//!
//! The plugin appends the search text as a `q` query parameter to the endpoint
//! in its server configuration, performs a `GET`, and maps the JSON array the
//! endpoint returns into catalog tracks. `get-track` asks the same endpoint for
//! one identifier. It exists to prove that a guest can reach a host the
//! administrator approved, and that every other host is denied.
//!
//! Its manifest declares `catalog.imageCachePolicy: no-store`, so it is also
//! the fixture that proves a catalog image is proxied without being stored.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{
    AlbumRef, ArtistCredit, EntityKind, IdPair, Image, ProviderError, Track,
};
use serde::Deserialize;

/// The plugin implementation.
struct HttpCatalog;

/// The album title every remote track is attributed to.
const ALBUM: &str = "Remote Album";

/// Server-scoped configuration of the plugin.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The absolute URL the catalog requests are sent to.
    url: String,
    /// The cover image URL every track carries, when configured.
    #[serde(default)]
    cover_url: Option<String>,
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

/// Asks the configured endpoint for one query.
///
/// # Parameters
///
/// - `text`: the query text.
///
/// # Returns
///
/// The endpoint's answer: a list of tracks, or an object carrying the entities
/// of each kind the search matched.
///
/// # Errors
///
/// Returns the failure of the outbound request, or of decoding its body.
fn fetch(text: &str) -> Result<RemoteAnswer, ProviderError> {
    let config = config()?;
    let url = format!("{}?q={}", config.url, encode(text));
    hearthsound_plugin::info!("searching {url}");

    let request = hearthsound_plugin::http::get(&url);
    hearthsound_plugin::http::send_json(&request)
}

/// One artist as the endpoint reports it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteArtist {
    /// The endpoint's artist identifier.
    id: String,
    /// The artist's display name.
    name: String,
    /// The number of albums the endpoint states, when it states one.
    #[serde(default)]
    album_count: Option<u32>,
    /// The artist's pictures.
    #[serde(default)]
    images: Vec<RemoteImage>,
}

/// One image as the endpoint reports it.
#[derive(Deserialize)]
struct RemoteImage {
    /// The absolute image URL.
    url: String,
    /// The described width in pixels.
    #[serde(default)]
    width: Option<u32>,
    /// The described height in pixels.
    height: Option<u32>,
}

/// One search response the endpoint may return.
///
/// A bare array is the original shape, one track per element. An object carries
/// a list per entity kind, so a test can answer a search with artists, which is
/// what the name-resolution and search-promotion tests need.
#[derive(Deserialize)]
#[serde(untagged)]
enum RemoteAnswer {
    /// A list of tracks.
    Tracks(Vec<RemoteTrack>),
    /// A map of the entity kinds the search matched.
    Kinds(RemoteKinds),
}

/// The entities one object-shaped search answer carries.
#[derive(Deserialize, Default)]
struct RemoteKinds {
    /// The matching tracks.
    #[serde(default)]
    tracks: Vec<RemoteTrack>,
    /// The matching artists.
    #[serde(default)]
    artists: Vec<RemoteArtist>,
}

/// The tracks of a search answer, whichever shape it used.
///
/// # Parameters
///
/// - `answer`: the endpoint's answer.
///
/// # Returns
///
/// The tracks the answer carries.
fn tracks_of(answer: RemoteAnswer) -> Vec<RemoteTrack> {
    match answer {
        RemoteAnswer::Tracks(tracks) => tracks,
        RemoteAnswer::Kinds(kinds) => kinds.tracks,
    }
}

/// Maps one remote artist into a catalog artist.
///
/// # Parameters
///
/// - `remote`: the artist the endpoint reported.
///
/// # Returns
///
/// The artist, with the album count and pictures the endpoint described.
fn to_artist(remote: RemoteArtist) -> hearthsound_plugin::types::Artist {
    hearthsound_plugin::types::Artist {
        id: remote.id.clone(),
        name: remote.name,
        images: remote
            .images
            .into_iter()
            .map(|image| Image {
                url: image.url,
                width: image.width,
                height: image.height,
            })
            .collect(),
        album_count: remote.album_count,
        ids: vec![IdPair {
            namespace: "http-catalog".to_string(),
            value: remote.id,
        }],
    }
}

/// Maps one remote track into a catalog track.
///
/// # Parameters
///
/// - `remote`: the track the endpoint returned.
/// - `cover_url`: the configured cover image URL, when there is one.
fn to_track(remote: RemoteTrack, cover_url: Option<&str>) -> Track {
    let artists: Vec<ArtistCredit> = remote
        .artists
        .into_iter()
        .map(|name| ArtistCredit { name, ids: vec![] })
        .collect();
    Track {
        id: remote.id.clone(),
        title: remote.title,
        artists: artists.clone(),
        album: Some(AlbumRef {
            id: format!("{ALBUM}-1"),
            title: ALBUM.to_string(),
            artists,
            year: None,
            // The fixture reports no dimensions, which is legitimate: a
            // provider that does not describe its images leaves the caller to
            // pick between urls it cannot compare.
            images: cover_url
                .map(|url| {
                    vec![Image {
                        url: url.to_string(),
                        width: None,
                        height: None,
                    }]
                })
                .unwrap_or_default(),
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
        let cover_url = config.cover_url.clone();

        let (tracks, artists) = match fetch(&query.text)? {
            RemoteAnswer::Tracks(tracks) => (tracks, Vec::new()),
            RemoteAnswer::Kinds(kinds) => (kinds.tracks, kinds.artists),
        };
        let tracks: Vec<Track> = tracks
            .into_iter()
            .map(|track| to_track(track, cover_url.as_deref()))
            .collect();

        Ok(SearchResults {
            tracks: if wants(&query.kinds, EntityKind::Track) {
                tracks
            } else {
                vec![]
            },
            albums: vec![],
            artists: if wants(&query.kinds, EntityKind::Artist) {
                artists.into_iter().map(to_artist).collect()
            } else {
                vec![]
            },
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        let config = config()?;
        let cover_url = config.cover_url.clone();

        tracks_of(fetch(&id)?)
            .into_iter()
            .find(|track| track.id == id)
            .map(|track| to_track(track, cover_url.as_deref()))
            .ok_or(ProviderError::NotFound)
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
    ) -> Result<Vec<AlbumRef>, ProviderError> {
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
