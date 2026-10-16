//! Metadata fixture: a deterministic metadata provider driven by server config.
//!
//! The administrator configures a table of recordings and the releases they
//! appear on. A resolution matches a recording when the hint's ISRC equals the
//! configured one, or when the hint's title equals the configured title
//! ignoring case; the configured score is reported as-is, so a test can build
//! any confidence it wants to exercise. `get-release` materializes the album the
//! matched recording appears on.

use hearthsound_plugin::metadata_provider::exports::hearthsound::plugin::metadata::{
    ArtistInfo, ArtistInfoRequest, ArtistLink, RecordingHint, RecordingMatch, ReleaseSummary,
};
use hearthsound_plugin::metadata_provider::exports::hearthsound::plugin::{lifecycle, metadata};
use hearthsound_plugin::types::{Album, ArtistCredit, IdPair, ProviderError, Track};
use serde::Deserialize;

/// The namespace of a MusicBrainz recording identifier.
const RECORDING_NAMESPACE: &str = "mb-recording";

/// The namespace of a MusicBrainz release identifier.
const RELEASE_NAMESPACE: &str = "mb-release";

/// The namespace of a MusicBrainz artist identifier.
const ARTIST_NAMESPACE: &str = "mb-artist";

/// The plugin implementation.
struct MetadataFixture;

/// Server-scoped configuration of the plugin.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The recordings the plugin knows.
    #[serde(default)]
    recordings: Vec<Recording>,
    /// The artists the plugin knows information about.
    #[serde(default)]
    artists: Vec<ConfiguredArtist>,
}

/// One artist and what the plugin knows about them.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredArtist {
    /// The artist's name.
    name: String,
    /// The artist's MusicBrainz id.
    #[serde(default)]
    mbid: Option<String>,
    /// The artist's biography.
    #[serde(default)]
    biography: Option<String>,
    /// The pages about the artist.
    #[serde(default)]
    links: Vec<ConfiguredLink>,
}

/// One page about a configured artist.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredLink {
    /// The kind of page.
    kind: String,
    /// The page's URL.
    url: String,
}

/// One recording and the releases it appears on.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recording {
    /// The recording title.
    title: String,
    /// The artist names, in credit order.
    #[serde(default)]
    artists: Vec<String>,
    /// The International Standard Recording Code, when the fixture matches it.
    #[serde(default)]
    isrc: Option<String>,
    /// The recording's MusicBrainz id.
    mbid: String,
    /// The MusicBrainz artist id of the recording's first credited artist.
    #[serde(default)]
    artist_mbid: Option<String>,
    /// The confidence the fixture reports for a match.
    #[serde(default = "default_score")]
    score: f32,
    /// The releases the recording appears on.
    #[serde(default)]
    releases: Vec<Release>,
}

/// One release a recording appears on.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Release {
    /// The release's MusicBrainz id.
    mbid: String,
    /// The release title.
    title: String,
    /// The release date.
    #[serde(default)]
    date: Option<String>,
    /// The number of tracks on the release.
    #[serde(default)]
    track_count: Option<u32>,
}

/// The confidence used when a recording configures none.
fn default_score() -> f32 {
    0.95
}

/// Loads the plugin's server-scoped configuration.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Returns whether a recording answers the hint.
///
/// # Parameters
///
/// - `recording`: the configured recording.
/// - `hint`: the identity the caller already knows.
fn matches(recording: &Recording, hint: &RecordingHint) -> bool {
    match (&hint.isrc, &recording.isrc) {
        (Some(isrc), Some(configured)) if isrc.eq_ignore_ascii_case(configured) => return true,
        _ => {}
    }
    !recording.title.is_empty() && hint.title.trim().eq_ignore_ascii_case(&recording.title)
}

/// Builds the identifier pair of a recording.
fn recording_ids(recording: &Recording) -> Vec<IdPair> {
    vec![IdPair {
        namespace: RECORDING_NAMESPACE.to_string(),
        value: recording.mbid.clone(),
    }]
}

/// Builds the summary of one release.
fn release_summary(release: &Release) -> ReleaseSummary {
    ReleaseSummary {
        ids: vec![IdPair {
            namespace: RELEASE_NAMESPACE.to_string(),
            value: release.mbid.clone(),
        }],
        title: release.title.clone(),
        date: release.date.clone(),
        track_count: release.track_count,
    }
}

/// Builds the artist credits of a recording.
///
/// The first credited artist carries the configured MusicBrainz artist id, so a
/// test can exercise the `mb-artist` alias a resolution records.
///
/// # Parameters
///
/// - `recording`: the configured recording.
fn credits(recording: &Recording) -> Vec<ArtistCredit> {
    recording
        .artists
        .iter()
        .enumerate()
        .map(|(position, name)| ArtistCredit {
            name: name.clone(),
            ids: match (&recording.artist_mbid, position) {
                (Some(mbid), 0) => vec![IdPair {
                    namespace: ARTIST_NAMESPACE.to_string(),
                    value: mbid.clone(),
                }],
                _ => vec![],
            },
        })
        .collect()
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for MetadataFixture {
    fn validate_config(server_config: String) -> Result<(), Vec<String>> {
        match serde_json::from_str::<Config>(&server_config) {
            Ok(_) => Ok(()),
            Err(error) => Err(vec![error.to_string()]),
        }
    }

    fn health() -> Result<String, ProviderError> {
        Ok("metadata-fixture ready".to_string())
    }
}

/// Implements the metadata interface.
impl metadata::Guest for MetadataFixture {
    fn resolve_recording(hint: RecordingHint) -> Result<Vec<RecordingMatch>, ProviderError> {
        let config = config()?;
        hearthsound_plugin::info!(
            "resolving recording `{}`{}",
            hint.title,
            hint.isrc.as_deref().unwrap_or("")
        );

        let matched: Vec<RecordingMatch> = config
            .recordings
            .into_iter()
            .filter(|recording| matches(recording, &hint))
            .map(|recording| RecordingMatch {
                ids: recording_ids(&recording),
                title: recording.title.clone(),
                artists: credits(&recording),
                duration_ms: hint.duration_ms,
                releases: recording.releases.iter().map(release_summary).collect(),
                score: recording.score,
            })
            .collect();

        Ok(matched)
    }

    fn get_artist_info(request: ArtistInfoRequest) -> Result<Option<ArtistInfo>, ProviderError> {
        let config = config()?;
        let requested_mbid = request
            .ids
            .iter()
            .find(|id| id.namespace == ARTIST_NAMESPACE)
            .map(|id| id.value.as_str());
        for artist in config.artists {
            let known = artist.name.eq_ignore_ascii_case(&request.name)
                || match (&artist.mbid, requested_mbid) {
                    (Some(configured), Some(requested)) => configured == requested,
                    _ => false,
                };
            if !known {
                continue;
            }
            return Ok(Some(ArtistInfo {
                ids: artist
                    .mbid
                    .map(|mbid| IdPair {
                        namespace: ARTIST_NAMESPACE.to_string(),
                        value: mbid,
                    })
                    .into_iter()
                    .collect(),
                biography: artist.biography,
                links: artist
                    .links
                    .into_iter()
                    .map(|link| ArtistLink {
                        kind: link.kind,
                        url: link.url,
                    })
                    .collect(),
            }));
        }
        Ok(None)
    }

    fn get_release(ids: Vec<IdPair>) -> Result<Album, ProviderError> {
        let config = config()?;
        for recording in config.recordings {
            for release in &recording.releases {
                let known = ids.iter().any(|id| {
                    id.value == release.mbid
                        && (id.namespace == RELEASE_NAMESPACE || id.namespace.is_empty())
                });
                if !known {
                    continue;
                }
                let artists = credits(&recording);
                return Ok(Album {
                    id: release.mbid.clone(),
                    title: release.title.clone(),
                    artists: artists.clone(),
                    release_date: release.date.clone(),
                    album_type: Some("album".to_string()),
                    upc: None,
                    genres: vec![],
                    images: vec![],
                    tracks: vec![Track {
                        id: recording.mbid.clone(),
                        title: recording.title.clone(),
                        artists: artists.clone(),
                        album: None,
                        duration_ms: None,
                        disc_number: Some(1),
                        track_number: Some(1),
                        isrc: recording.isrc.clone(),
                        explicit: Some(false),
                        ids: recording_ids(&recording),
                    }],
                    ids: vec![IdPair {
                        namespace: RELEASE_NAMESPACE.to_string(),
                        value: release.mbid.clone(),
                    }],
                });
            }
        }
        Err(ProviderError::NotFound)
    }
}

hearthsound_plugin::export_metadata_provider!(MetadataFixture);
