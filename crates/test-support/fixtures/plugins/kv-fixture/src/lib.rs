//! Key-value fixture: a catalog plugin that reports a `host-kv` read.
//!
//! The plugin exists to prove how the server answers a guest's key-value read.
//! Its `search` reads the key `probe` through the SDK's raw wrapper and turns the
//! outcome into one track, so a test can tell the three outcomes apart:
//!
//! - a stored value answers with that value as the track title;
//! - an absent or expired key answers with the title `no value`;
//! - a storage failure answers with the host's reason as an internal error.
//!
//! It has no configuration; `validate-config` accepts everything and `health`
//! answers with its own name.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{Album, AlbumRef, Artist, EntityKind, ProviderError, Track};

/// The plugin implementation.
struct KvFixture;

/// The key the plugin reads.
const PROBE: &str = "probe";

/// The id of the track the plugin reports.
const TRACK_ID: &str = "kv-probe";

/// The title the plugin reports when the key holds no value.
const NO_VALUE: &str = "no value";

/// Returns whether a search asks for entities of `wanted` kind.
///
/// An empty kind list means every kind, which is what the host sends when the
/// caller named none.
///
/// # Parameters
///
/// - `kinds`: the kinds the search asked for.
/// - `wanted`: the kind being tested.
fn wants(kinds: &[EntityKind], wanted: EntityKind) -> bool {
    kinds.is_empty() || kinds.iter().any(|kind| *kind == wanted)
}

/// Builds the one track the plugin reports.
///
/// # Parameters
///
/// - `title`: the track title.
fn track(title: String) -> Track {
    Track {
        id: TRACK_ID.to_string(),
        title,
        artists: vec![],
        album: None,
        duration_ms: None,
        disc_number: None,
        track_number: None,
        isrc: None,
        explicit: None,
        ids: vec![],
    }
}

impl lifecycle::Guest for KvFixture {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("kv fixture".to_string())
    }
}

impl catalog::Guest for KvFixture {
    fn search(query: SearchQuery) -> Result<SearchResults, ProviderError> {
        if query.limit == 0 || query.offset > 0 {
            return Ok(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
            });
        }

        let tracks = if wants(&query.kinds, EntityKind::Track) {
            let title = match hearthsound_plugin::host::kv::get(PROBE) {
                Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
                Ok(None) => NO_VALUE.to_string(),
                Err(reason) => return Err(ProviderError::Internal(reason)),
            };
            vec![track(title)]
        } else {
            vec![]
        };

        Ok(SearchResults {
            tracks,
            albums: vec![],
            artists: vec![],
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        if id != TRACK_ID {
            return Err(ProviderError::NotFound);
        }
        Ok(track(NO_VALUE.to_string()))
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

hearthsound_plugin::export_catalog_provider!(KvFixture);
