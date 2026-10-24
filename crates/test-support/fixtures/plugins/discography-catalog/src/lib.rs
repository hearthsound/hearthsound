//! Discography catalog: a test fixture whose artist released three albums.
//!
//! The plugin exists for the tests that open an artist page and an album page:
//! `artist_albums` lists three releases, one of them a remaster whose title and
//! track titles carry an edition suffix, and `get_album` answers the whole
//! tracklist of each. The provider ids are stable, so a test can build an
//! `x_<slug>_a_<id>` id without asking the plugin first.
//!
//! A search answers the artist when the query matches their name, and the albums
//! and tracks whose titles contain the query, so a page can be reached from a
//! search result. The plugin asks for no host permissions and reports no images.

use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::catalog::{
    SearchQuery, SearchResults,
};
use hearthsound_plugin::catalog_provider::exports::hearthsound::plugin::{catalog, lifecycle};
use hearthsound_plugin::types::{
    Album, AlbumRef, Artist, ArtistCredit, EntityKind, IdPair, ProviderError, Track,
};

/// The plugin implementation.
struct DiscographyCatalog;

/// The provider id of the fixture's artist.
const ARTIST: &str = "disco-artist-1";

/// The name of the fixture's artist.
const ARTIST_NAME: &str = "Discography Artist";

/// The prefix of the fixture's album ids.
const ALBUM_PREFIX: &str = "disco-album-";

/// The prefix of the fixture's track ids.
const TRACK_PREFIX: &str = "disco-track-";

/// One album the fixture's artist released.
struct FixtureAlbum {
    /// The provider-local album id.
    id: &'static str,
    /// The album title.
    title: &'static str,
    /// The release year.
    year: u16,
    /// The release date.
    release_date: &'static str,
    /// The album's tracks: the title and the duration in milliseconds.
    tracks: &'static [(&'static str, u32)],
}

/// The albums the fixture's artist released, in release order.
const ALBUMS: [FixtureAlbum; 3] = [
    FixtureAlbum {
        id: "disco-album-1",
        title: "First Light",
        year: 2001,
        release_date: "2001-04-02",
        tracks: &[("Dawn", 180_000), ("Noon", 200_000), ("Dusk", 220_000)],
    },
    FixtureAlbum {
        id: "disco-album-2",
        title: "Second Wind (Remastered 2009)",
        year: 2009,
        release_date: "2009-06-15",
        tracks: &[
            ("Gust (Remastered 2009)", 150_000),
            ("Breeze", 160_000),
            ("Gale", 170_000),
        ],
    },
    FixtureAlbum {
        id: "disco-album-3",
        title: "Third Single",
        year: 2012,
        release_date: "2012-01-20",
        tracks: &[("Spark", 120_000)],
    },
];

/// Builds the identifier pairs naming the fixture's artist.
///
/// The `discography-catalog` pair is this plugin's own id for the artist, which
/// is what a credit carries so the server resolves the credited artist against
/// this instance.
fn artist_ids() -> Vec<IdPair> {
    vec![
        IdPair {
            namespace: "discography-catalog".to_string(),
            value: ARTIST.to_string(),
        },
        IdPair {
            namespace: "mb-artist".to_string(),
            value: "disco-mb-artist-1".to_string(),
        },
    ]
}

/// Builds the artist every entity of this plugin points at.
fn artist() -> Artist {
    Artist {
        id: ARTIST.to_string(),
        name: ARTIST_NAME.to_string(),
        images: vec![],
        ids: artist_ids(),
    }
}

/// Builds the reference to one album.
///
/// # Parameters
///
/// - `album`: the album.
fn album_ref(album: &FixtureAlbum) -> AlbumRef {
    AlbumRef {
        id: album.id.to_string(),
        title: album.title.to_string(),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        year: Some(album.year),
        images: vec![],
    }
}

/// Builds one track of an album.
///
/// # Parameters
///
/// - `album`: the album the track belongs to.
/// - `number`: the one-based track number within the album.
/// - `title`: the track title.
/// - `duration_ms`: the duration in milliseconds.
fn track(album: &FixtureAlbum, number: usize, title: &str, duration_ms: u32) -> Track {
    let id = format!("{TRACK_PREFIX}{}-{number}", album_key(album));
    Track {
        id: id.clone(),
        title: title.to_string(),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        album: Some(album_ref(album)),
        duration_ms: Some(duration_ms),
        disc_number: Some(1),
        track_number: Some(u16::try_from(number).unwrap_or(u16::MAX)),
        isrc: None,
        explicit: Some(false),
        ids: vec![IdPair {
            namespace: "discography-catalog".to_string(),
            value: id,
        }],
    }
}

/// Returns the id suffix an album's tracks carry.
///
/// # Parameters
///
/// - `album`: the album.
fn album_key(album: &FixtureAlbum) -> &'static str {
    album.id.trim_start_matches(ALBUM_PREFIX)
}

/// Builds every track of an album, in track order.
///
/// # Parameters
///
/// - `album`: the album.
fn album_tracks(album: &FixtureAlbum) -> Vec<Track> {
    album
        .tracks
        .iter()
        .enumerate()
        .map(|(index, (title, duration_ms))| track(album, index + 1, title, *duration_ms))
        .collect()
}

/// Builds one album with its tracklist.
///
/// # Parameters
///
/// - `album`: the album.
fn album(album: &FixtureAlbum) -> Album {
    Album {
        id: album.id.to_string(),
        title: album.title.to_string(),
        artists: vec![ArtistCredit {
            name: ARTIST_NAME.to_string(),
            ids: artist_ids(),
        }],
        release_date: Some(album.release_date.to_string()),
        album_type: Some(if album.tracks.len() == 1 {
            "single".to_string()
        } else {
            "album".to_string()
        }),
        upc: None,
        genres: vec![],
        images: vec![],
        tracks: album_tracks(album),
        ids: vec![IdPair {
            namespace: "discography-catalog".to_string(),
            value: album.id.to_string(),
        }],
    }
}

/// Returns the album an id names.
///
/// # Parameters
///
/// - `id`: the provider-local album id.
fn album_by_id(id: &str) -> Option<&'static FixtureAlbum> {
    ALBUMS.iter().find(|album| album.id == id)
}

/// Returns the track an id names, and the album it belongs to.
///
/// # Parameters
///
/// - `id`: the provider-local track id.
fn track_by_id(id: &str) -> Option<(&'static FixtureAlbum, usize)> {
    let rest = id.strip_prefix(TRACK_PREFIX)?;
    let (key, number) = rest.split_once('-')?;
    let number: usize = number.parse().ok()?;
    let album = ALBUMS
        .iter()
        .find(|album| album_key(album) == key)?;
    (number >= 1 && number <= album.tracks.len()).then_some((album, number))
}

/// Returns whether the query asked for `kind`.
///
/// An empty `kinds` list means every kind was asked for.
fn wants(kinds: &[EntityKind], wanted: EntityKind) -> bool {
    kinds.is_empty() || kinds.iter().any(|kind| *kind == wanted)
}

/// Reports whether an artist name and a query text contain each other.
///
/// # Parameters
///
/// - `name`: the artist name.
/// - `text`: the lowercased query text.
fn name_matches(name: &str, text: &str) -> bool {
    let name = name.to_lowercase();
    name.contains(text) || text.contains(&name)
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for DiscographyCatalog {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("discography-catalog ready".to_string())
    }
}

/// Implements the catalog interface.
impl catalog::Guest for DiscographyCatalog {
    fn search(query: SearchQuery) -> Result<SearchResults, ProviderError> {
        hearthsound_plugin::info!("search: {}", query.text);

        if query.limit == 0 {
            return Ok(SearchResults {
                tracks: vec![],
                albums: vec![],
                artists: vec![],
            });
        }
        let text = query.text.to_lowercase();
        let mut tracks = Vec::new();
        let mut albums = Vec::new();
        let mut artists = Vec::new();
        if wants(&query.kinds, EntityKind::Artist) && name_matches(ARTIST_NAME, &text) {
            artists.push(artist());
        }
        for album in &ALBUMS {
            if wants(&query.kinds, EntityKind::Album)
                && album.title.to_lowercase().contains(&text)
            {
                albums.push(album_ref(album));
            }
            if !wants(&query.kinds, EntityKind::Track) {
                continue;
            }
            tracks.extend(
                album_tracks(album)
                    .into_iter()
                    .filter(|track| track.title.to_lowercase().contains(&text)),
            );
        }
        let skip = query.offset as usize;
        let take = query.limit as usize;
        Ok(SearchResults {
            tracks: tracks.into_iter().skip(skip).take(take).collect(),
            albums: albums.into_iter().skip(skip).take(take).collect(),
            artists: artists.into_iter().skip(skip).take(take).collect(),
        })
    }

    fn get_track(id: String) -> Result<Track, ProviderError> {
        match track_by_id(&id) {
            Some((album, number)) => {
                let (title, duration_ms) = album.tracks[number - 1];
                Ok(track(album, number, title, duration_ms))
            }
            None => Err(ProviderError::NotFound),
        }
    }

    fn get_album(id: String) -> Result<Album, ProviderError> {
        album_by_id(&id)
            .map(album)
            .ok_or(ProviderError::NotFound)
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
        Ok(ALBUMS
            .iter()
            .map(album_ref)
            .skip(offset as usize)
            .take(limit as usize)
            .collect())
    }

    fn artist_top_tracks(id: String, limit: u32) -> Result<Vec<Track>, ProviderError> {
        if id != ARTIST {
            return Err(ProviderError::NotFound);
        }
        Ok(ALBUMS
            .iter()
            .flat_map(album_tracks)
            .take(limit as usize)
            .collect())
    }

    fn lookup_isrc(_isrc: String) -> Result<Vec<Track>, ProviderError> {
        Ok(vec![])
    }
}

hearthsound_plugin::export_catalog_provider!(DiscographyCatalog);
