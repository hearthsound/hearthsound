//! Album catalog: a test fixture whose album holds five tracks.
//!
//! The plugin exists for the tests that need a listing longer than the prefetch
//! look-ahead: `get_album` answers one album of five tracks, and every track of
//! it is a track this instance resolves on its own. The provider ids are stable,
//! so a test can build an `x_<slug>_t_<id>` id without asking the plugin first.
//!
//! A search answers the first track by default. The optional `searchResults`
//! table makes a query answer another track of the album: an entry matches when
//! the query text contains its `text` ignoring case, so a test can resolve two
//! different suggestions through one instance. `canonicalIds` turns off the
//! MusicBrainz ids the fixture reports beside its own, which is the shape of a
//! service such as Deezer that identifies nothing canonically. The plugin asks
//! for no host permissions and reports no images.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{
    Album, AlbumRef, Artist, ArtistCredit, EntityKind, IdPair, ProviderError, Track,
};
use serde::Deserialize;

/// The plugin implementation.
struct AlbumCatalog;

/// Server-scoped configuration of the plugin.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The track each configured query answers with.
    #[serde(default)]
    search_results: Vec<ConfiguredSearchResult>,
    /// Whether a searched track's album reference carries no artists, year or
    /// images, the way a service that names only the album does.
    #[serde(default)]
    bare_album_refs: bool,
    /// Whether the fixture reports MusicBrainz ids beside its own.
    ///
    /// A service such as Deezer reports only its own ids, and neither the
    /// canonical recording nor the release nor the artist is identified until
    /// the metadata layer resolves it. A test that starts from that state turns
    /// this off; the default is the fixture's own canonical ids.
    #[serde(default = "default_canonical_ids")]
    canonical_ids: bool,
}

/// Returns whether the fixture reports MusicBrainz ids unless configured not to.
fn default_canonical_ids() -> bool {
    true
}

/// One query text and the track it answers with.
#[derive(Deserialize)]
struct ConfiguredSearchResult {
    /// The text the query must contain, compared ignoring case.
    text: String,
    /// The one-based track number the query answers with.
    track: u8,
}

/// Reads the plugin's server configuration, falling back to the defaults.
///
/// # Returns
///
/// The configuration.
fn config() -> Config {
    hearthsound_plugin::config::server_config().unwrap_or_default()
}

/// Returns the track a search query answers with.
///
/// # Parameters
///
/// - `text`: the query text the provider received.
///
/// # Returns
///
/// The configured track number of the first entry the text contains, or one.
fn search_track(text: &str) -> u8 {
    let text = text.to_lowercase();
    config()
        .search_results
        .iter()
        .find(|entry| text.contains(&entry.text.to_lowercase()))
        .map(|entry| entry.track.clamp(1, TRACKS))
        .unwrap_or(1)
}

/// Builds the track a search answers with.
///
/// The configured `bareAlbumRefs` option strips the album's artists, year and
/// images from the track, which is what a service that names only its albums
/// returns.
///
/// # Parameters
///
/// - `text`: the query text the provider received.
///
/// # Returns
///
/// The track to answer with.
fn searched_track(text: &str) -> Track {
    let mut track = track(search_track(text));
    if config().bare_album_refs
        && let Some(album) = &mut track.album
    {
        album.artists.clear();
        album.year = None;
        album.images.clear();
    }
    track
}

/// The provider id of the fixture's album.
const ALBUM: &str = "album-catalog-album-1";

/// The provider id of the fixture's artist.
const ARTIST: &str = "album-catalog-artist-1";

/// The prefix of the fixture's track ids.
const TRACK_PREFIX: &str = "album-catalog-track-";

/// How many tracks the fixture's album holds.
const TRACKS: u8 = 5;

/// The name of the fixture's artist.
const ARTIST_NAME: &str = "Fixture Artist";

/// The title of the fixture's album.
const ALBUM_TITLE: &str = "Fixture Album";

/// Builds the identifier pairs naming the fixture's artist.
///
/// The MusicBrainz id is the artist's identity; the `album-catalog` pair is this
/// plugin's own id for the same artist, which is what a credit carries so the
/// server resolves the credited artist against this instance.
fn artist_ids() -> Vec<IdPair> {
    let mut ids = vec![IdPair {
        namespace: "album-catalog".to_string(),
        value: ARTIST.to_string(),
    }];
    if config().canonical_ids {
        ids.push(IdPair {
            namespace: "mb-artist".to_string(),
            value: "album-catalog-mb-artist-1".to_string(),
        });
    }
    ids
}

/// Builds the album reference every entity of this plugin points at.
fn album_ref() -> AlbumRef {
    AlbumRef {
        id: ALBUM.to_string(),
        title: ALBUM_TITLE.to_string(),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        year: Some(2026),
        images: vec![],
    }
}

/// Builds the artist every entity of this plugin points at.
fn artist() -> Artist {
    Artist {
        id: ARTIST.to_string(),
        name: ARTIST_NAME.to_string(),
        images: vec![],
        ids: artist_ids(),
        album_count: None,
    }
}

/// Builds one track of the fixture's album.
///
/// # Parameters
///
/// - `number`: the track number within the album, one-based.
fn track(number: u8) -> Track {
    Track {
        id: format!("{TRACK_PREFIX}{number}"),
        title: format!("Fixture Song {number}"),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        album: Some(album_ref()),
        duration_ms: Some(180_000),
        disc_number: Some(1),
        track_number: Some(number.into()),
        isrc: None,
        explicit: Some(false),
        ids: {
            let mut ids = vec![IdPair {
                namespace: "album-catalog".to_string(),
                value: format!("{TRACK_PREFIX}{number}"),
            }];
            if config().canonical_ids {
                ids.push(IdPair {
                    namespace: "mb-recording".to_string(),
                    value: format!("album-catalog-recording-{number}"),
                });
            }
            ids
        },
    }
}

/// Builds the fixture's album with its five tracks.
fn album() -> Album {
    Album {
        id: ALBUM.to_string(),
        title: ALBUM_TITLE.to_string(),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        release_date: Some("2026-02-01".to_string()),
        album_type: Some("album".to_string()),
        upc: Some("0099999999999".to_string()),
        genres: vec![],
        images: vec![],
        tracks: (1..=TRACKS).map(track).collect(),
        ids: if config().canonical_ids {
            vec![IdPair {
                namespace: "mb-release".to_string(),
                value: "album-catalog-release-1".to_string(),
            }]
        } else {
            Vec::new()
        },
    }
}

/// Returns whether the query asked for `kind`.
///
/// An empty `kinds` list means every kind was asked for.
fn wants(kinds: &[EntityKind], wanted: EntityKind) -> bool {
    kinds.is_empty() || kinds.iter().any(|kind| matches!(kind, k if *k == wanted))
}

/// Returns the track number an id names, when it names a track of the fixture.
///
/// # Parameters
///
/// - `id`: the provider-local track id.
fn track_number(id: &str) -> Option<u8> {
    let number = id.strip_prefix(TRACK_PREFIX)?.parse().ok()?;
    (1..=TRACKS).contains(&number).then_some(number)
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for AlbumCatalog {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        match serde_json::from_str::<Config>(&server_config) {
            Ok(_) => Ok(()),
            Err(error) => Err(vec![error.to_string()]),
        }
    }

    fn health() -> Result<String, ProviderError> {
        Ok("album-catalog ready".to_string())
    }
}

/// Implements the catalog interface.
impl catalog::Guest for AlbumCatalog {
    fn search(query: SearchQuery) -> Result<SearchResults, ProviderError> {
        hearthsound_plugin::info!("search: {}", query.text);

        if query.limit == 0 || query.offset > 0 {
            return Ok(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
            });
        }

        Ok(SearchResults {
            tracks: if wants(&query.kinds, EntityKind::Track) {
                vec![searched_track(&query.text)]
            } else {
                vec![]
            },
            albums: if wants(&query.kinds, EntityKind::Album) {
                vec![album_ref()]
            } else {
                vec![]
            },
            artists: if wants(&query.kinds, EntityKind::Artist) {
                vec![artist()]
            } else {
                vec![]
            },
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        match track_number(&id) {
            Some(number) => Ok(track(number)),
            None => Err(ProviderError::NotFound),
        }
    }

    fn get_album(id: String) -> Result<Album, ProviderError> {
        if id != ALBUM {
            return Err(ProviderError::NotFound);
        }
        Ok(album())
    }

    fn get_artist(id: String) -> Result<Artist, ProviderError> {
        if id != ARTIST {
            return Err(ProviderError::NotFound);
        }
        Ok(artist())
    }

    fn artist_albums(id: String, limit: u32, offset: u32) -> Result<Vec<AlbumRef>, ProviderError> {
        if id != ARTIST {
            return Err(ProviderError::NotFound);
        }
        Ok(vec![album_ref()]
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    fn artist_top_tracks(id: String, limit: u32) -> Result<Vec<Track>, ProviderError> {
        if id != ARTIST {
            return Err(ProviderError::NotFound);
        }
        Ok(vec![track(1)].into_iter().take(limit as usize).collect())
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![])
    }
}

hearthsound_plugin::export_catalog_provider!(AlbumCatalog);
