//! Scrobble fixture: a scrobble, discovery and account-link provider driven by
//! server config.
//!
//! The administrator points `endpoint` at a mock service; the plugin posts each
//! now-playing report, listen batch and feedback to a path under it, so a test
//! can assert what crossed the wire. The discovery answers come from tables in
//! the same configuration, which keeps every suggestion deterministic.
//!
//! The account-link method is `config` by default: the listener enters a token,
//! the plugin posts nothing to verify it, and the token `good` names the account
//! `fixture-user`. A `browser` link method answers the mock service's
//! authorization URL and accepts the callback token `approved`.
//!
//! `failDiscovery` makes the three suggestion queries answer with an
//! unavailable error, which is what lets a test prove that the discovery layer
//! skips an instance that cannot answer.

use hearthsound_plugin::http;
use hearthsound_plugin::scrobble_discovery_link_provider::exports::hearthsound::plugin::{
    account_link, discovery, lifecycle, scrobble,
};
use hearthsound_plugin::types::{IdPair, ProviderError, UserContext};
use serde::{Deserialize, Serialize};

/// The plugin implementation.
struct ScrobbleFixture;

/// Server-scoped configuration of the plugin.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    /// The base URL of the mock service.
    #[serde(default)]
    endpoint: String,
    /// How a listener links their account.
    #[serde(default)]
    link_method: LinkMethod,
    /// The batch size the service accepts.
    #[serde(default = "default_max_batch")]
    max_batch: u32,
    /// The least interval between two calls to the service.
    #[serde(default)]
    min_interval_ms: u32,
    /// Whether the service supports feedback.
    #[serde(default = "default_feedback")]
    feedback: bool,
    /// The tracks `similar-tracks` suggests.
    #[serde(default)]
    similar_tracks: Vec<ConfiguredTrack>,
    /// The artists `similar-artists` suggests.
    #[serde(default)]
    similar_artists: Vec<ConfiguredArtist>,
    /// The tracks `artist-top-tracks` suggests.
    #[serde(default)]
    top_tracks: Vec<ConfiguredTrack>,
    /// The tracks `recommended-tracks` suggests.
    #[serde(default)]
    recommended: Vec<ConfiguredTrack>,
    /// The playlists `playlists` reports.
    #[serde(default)]
    playlists: Vec<ConfiguredPlaylist>,
    /// Whether the discovery methods fail instead of answering.
    ///
    /// A test uses it to prove that the layer skips an instance that cannot
    /// answer and asks the next one.
    #[serde(default)]
    fail_discovery: bool,
}

/// How a listener links their account.
#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum LinkMethod {
    /// The listener enters the fields of the user config schema.
    #[default]
    Config,
    /// The listener approves access on the mock service's site.
    Browser,
}

/// The default `maxBatch`.
fn default_max_batch() -> u32 {
    50
}

/// The default `feedback`.
fn default_feedback() -> bool {
    true
}

/// One configured suggested track.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredTrack {
    /// The track title.
    title: String,
    /// The artist names.
    #[serde(default)]
    artists: Vec<String>,
}

/// One configured suggested artist.
#[derive(Deserialize)]
struct ConfiguredArtist {
    /// The artist name.
    name: String,
    /// The identifiers the service knows the artist by, as `namespace` and
    /// `value` pairs. A real service names its own or a shared id space here,
    /// which is what lets the server resolve the artist to the row a catalog
    /// already holds.
    #[serde(default)]
    ids: Vec<ConfiguredId>,
}

/// One configured identifier of a suggested artist.
#[derive(Deserialize)]
struct ConfiguredId {
    /// The identifier space, for example `mb-artist`.
    namespace: String,
    /// The identifier itself.
    value: String,
}

/// One configured generated playlist.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfiguredPlaylist {
    /// The service's stable reference for the playlist.
    reference: String,
    /// The playlist name.
    name: String,
    /// The playlist description.
    #[serde(default)]
    description: Option<String>,
    /// When the service retires the playlist, in unix milliseconds UTC.
    #[serde(default)]
    expires_at: Option<u64>,
    /// The playlist's tracks.
    #[serde(default)]
    tracks: Vec<ConfiguredTrack>,
}

/// The listener's token, as their user configuration carries it.
#[derive(Deserialize)]
struct UserConfig {
    /// The token the service accepts.
    token: String,
}

/// One listen, as the mock service receives it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListenBody {
    /// The track title.
    title: String,
    /// The artist names in credit order.
    artists: Vec<String>,
    /// The album title.
    album: Option<String>,
    /// The album artist.
    album_artist: Option<String>,
    /// The duration in milliseconds.
    duration_ms: Option<u32>,
    /// The track number.
    track_number: Option<u16>,
    /// The identifiers the server knows.
    ids: Vec<IdBody>,
    /// When playback started, in unix milliseconds UTC.
    listened_at: u64,
    /// The client application.
    client: Option<String>,
}

/// One identifier pair, as the mock service receives it.
#[derive(Serialize)]
struct IdBody {
    /// The namespace.
    namespace: String,
    /// The value.
    value: String,
}

/// The body of a feedback report.
#[derive(Serialize)]
struct FeedbackBody {
    /// The track the opinion is about.
    track: ListenBody,
    /// The opinion: `love`, `hate` or `clear`.
    value: String,
}

/// The body of a listen batch.
#[derive(Serialize)]
struct ListensBody {
    /// The finished listens.
    listens: Vec<ListenBody>,
}

/// Reads the plugin's server configuration.
///
/// # Errors
///
/// Returns [`ProviderError`] when the configuration cannot be read or is not the
/// document the plugin expects.
fn config() -> Result<Config, ProviderError> {
    hearthsound_plugin::config::server_config()
}

/// Converts one WIT listen into the body the mock service receives.
///
/// # Parameters
///
/// - `listen`: the listen the host submitted.
fn listen_body(listen: scrobble::Listen) -> ListenBody {
    ListenBody {
        title: listen.title,
        artists: listen.artists,
        album: listen.album,
        album_artist: listen.album_artist,
        duration_ms: listen.duration_ms,
        track_number: listen.track_number,
        ids: listen.ids.into_iter().map(id_body).collect(),
        listened_at: listen.listened_at,
        client: listen.client,
    }
}

/// Converts one identifier pair into the body the mock service receives.
///
/// # Parameters
///
/// - `id`: the identifier the host supplied.
fn id_body(id: IdPair) -> IdBody {
    IdBody {
        namespace: id.namespace,
        value: id.value,
    }
}

/// Posts one JSON body and maps a failure onto the provider's vocabulary.
///
/// A `429` answer becomes [`ProviderError::RateLimited`] with the fixture's
/// fixed retry hint; any other non-success status becomes
/// [`ProviderError::Unavailable`] carrying the status code.
///
/// # Parameters
///
/// - `url`: the absolute URL to post to.
/// - `body`: the value to serialize.
///
/// # Errors
///
/// Returns [`ProviderError`] when the body cannot be serialized, the request is
/// denied, or the service answers with a failure.
fn post<T: Serialize>(url: &str, body: &T) -> Result<(), ProviderError> {
    let request = http::post_json(url, body)?;
    let response = http::send(&request)?;
    if http::is_success(&response) {
        return Ok(());
    }
    if response.status == 429 {
        return Err(ProviderError::RateLimited(1000));
    }
    Err(ProviderError::Unavailable(response.status.to_string()))
}

/// Percent-encodes a callback URL so it can travel as a query value.
///
/// Every byte outside the unreserved set of RFC 3986 is escaped, which is what
/// the mock service expects.
///
/// # Parameters
///
/// - `value`: the text to escape.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Reads the listener's configuration.
///
/// # Parameters
///
/// - `user`: the listener the call is made for.
///
/// # Errors
///
/// Returns [`ProviderError::Unauthorized`] when the listener carries no readable
/// token.
fn user_config(user: &UserContext) -> Result<UserConfig, ProviderError> {
    let config = user
        .config
        .as_deref()
        .ok_or_else(|| ProviderError::Unauthorized("bad token".to_string()))?;
    serde_json::from_str(config).map_err(|_| ProviderError::Unauthorized("bad token".to_string()))
}

/// Implements the lifecycle interface.
impl lifecycle::Guest for ScrobbleFixture {
    fn validate_config(_server_config: String) -> Result<(), Vec<String>> {
        Ok(())
    }

    fn health() -> Result<String, ProviderError> {
        Ok("scrobble-fixture ready".to_string())
    }
}

/// Implements the scrobble interface.
impl scrobble::Guest for ScrobbleFixture {
    fn get_limits() -> scrobble::ScrobbleLimits {
        match config() {
            Ok(config) => scrobble::ScrobbleLimits {
                max_batch: config.max_batch,
                min_interval_ms: config.min_interval_ms,
                feedback: config.feedback,
            },
            Err(_) => scrobble::ScrobbleLimits {
                max_batch: 1,
                min_interval_ms: 60_000,
                feedback: false,
            },
        }
    }

    fn now_playing(_user: UserContext, track: scrobble::Listen) -> Result<(), ProviderError> {
        let config = config()?;
        post(
            &format!("{}/now-playing", config.endpoint),
            &listen_body(track),
        )
    }

    fn submit_listens(
        _user: UserContext,
        listens: Vec<scrobble::Listen>,
    ) -> Result<(), ProviderError> {
        let config = config()?;
        let body = ListensBody {
            listens: listens.into_iter().map(listen_body).collect(),
        };
        post(&format!("{}/listens", config.endpoint), &body)
    }

    fn send_feedback(
        _user: UserContext,
        track: scrobble::Listen,
        value: scrobble::FeedbackValue,
    ) -> Result<(), ProviderError> {
        let config = config()?;
        let value = match value {
            scrobble::FeedbackValue::Love => "love",
            scrobble::FeedbackValue::Hate => "hate",
            scrobble::FeedbackValue::Clear => "clear",
        };
        let body = FeedbackBody {
            track: listen_body(track),
            value: value.to_string(),
        };
        post(&format!("{}/feedback", config.endpoint), &body)
    }
}

/// Implements the discovery interface.
impl discovery::Guest for ScrobbleFixture {
    fn similar_tracks(
        _user: Option<UserContext>,
        _seed: discovery::TrackSeed,
        limit: u32,
    ) -> Result<Vec<discovery::SuggestedTrack>, ProviderError> {
        let config = config()?;
        if config.fail_discovery {
            return Err(ProviderError::Unavailable("the fixture is configured to fail".to_owned()));
        }
        Ok(suggested_tracks(config.similar_tracks, limit))
    }

    fn similar_artists(
        _user: Option<UserContext>,
        _seed: discovery::ArtistSeed,
        limit: u32,
    ) -> Result<Vec<discovery::SuggestedArtist>, ProviderError> {
        let config = config()?;
        if config.fail_discovery {
            return Err(ProviderError::Unavailable("the fixture is configured to fail".to_owned()));
        }
        Ok(config
            .similar_artists
            .into_iter()
            .take(limit as usize)
            .map(|artist| discovery::SuggestedArtist {
                name: artist.name,
                ids: artist
                    .ids
                    .into_iter()
                    .map(|id| IdPair {
                        namespace: id.namespace,
                        value: id.value,
                    })
                    .collect(),
                score: None,
            })
            .collect())
    }

    fn artist_top_tracks(
        _user: Option<UserContext>,
        _artist: discovery::ArtistSeed,
        limit: u32,
    ) -> Result<Vec<discovery::SuggestedTrack>, ProviderError> {
        let config = config()?;
        if config.fail_discovery {
            return Err(ProviderError::Unavailable("the fixture is configured to fail".to_owned()));
        }
        Ok(suggested_tracks(config.top_tracks, limit))
    }

    fn recommended_tracks(
        _user: UserContext,
        limit: u32,
    ) -> Result<Vec<discovery::SuggestedTrack>, ProviderError> {
        Ok(suggested_tracks(config()?.recommended, limit))
    }

    fn playlists(
        _user: UserContext,
    ) -> Result<Vec<discovery::GeneratedPlaylist>, ProviderError> {
        Ok(config()?
            .playlists
            .into_iter()
            .map(|playlist| discovery::GeneratedPlaylist {
                summary: discovery::PlaylistSummary {
                    reference: playlist.reference,
                    name: playlist.name,
                    description: playlist.description,
                    expires_at: playlist.expires_at,
                },
                tracks: suggested_tracks(playlist.tracks, u32::MAX),
            })
            .collect())
    }
}

/// Converts configured tracks into suggestions, at most `limit` of them.
///
/// # Parameters
///
/// - `tracks`: the configured tracks.
/// - `limit`: the maximum number of suggestions.
fn suggested_tracks(
    tracks: Vec<ConfiguredTrack>,
    limit: u32,
) -> Vec<discovery::SuggestedTrack> {
    tracks
        .into_iter()
        .take(limit as usize)
        .map(|track| discovery::SuggestedTrack {
            title: track.title,
            artists: track.artists,
            album: None,
            duration_ms: None,
            ids: Vec::new(),
            score: None,
        })
        .collect()
}

/// Implements the account-link interface.
impl account_link::Guest for ScrobbleFixture {
    fn get_link_method() -> account_link::LinkMethod {
        match config() {
            Ok(config) if config.link_method == LinkMethod::Browser => {
                account_link::LinkMethod::Browser
            }
            _ => account_link::LinkMethod::Config,
        }
    }

    fn verify_account(user: UserContext) -> Result<account_link::Account, ProviderError> {
        let config = user_config(&user)?;
        if config.token != "good" {
            return Err(ProviderError::Unauthorized("bad token".to_string()));
        }
        Ok(account_link::Account {
            name: "fixture-user".to_string(),
            config: user.config.unwrap_or_default(),
        })
    }

    fn begin_link(user: UserContext, callback_url: String) -> Result<String, ProviderError> {
        let _ = user;
        let config = config()?;
        Ok(format!(
            "{}/authorize?cb={}",
            config.endpoint,
            percent_encode(&callback_url)
        ))
    }

    fn complete_link(
        _user: UserContext,
        query: Vec<(String, String)>,
    ) -> Result<account_link::Account, ProviderError> {
        let approved = query
            .iter()
            .any(|(name, value)| name == "token" && value == "approved");
        if !approved {
            return Err(ProviderError::Unauthorized(
                "the link was not approved".to_string(),
            ));
        }
        Ok(account_link::Account {
            name: "browser-user".to_string(),
            config: r#"{"session":"s1"}"#.to_string(),
        })
    }
}

hearthsound_plugin::export_scrobble_discovery_link_provider!(ScrobbleFixture);
