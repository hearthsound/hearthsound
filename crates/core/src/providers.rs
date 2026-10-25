//! The provider abstraction: Rust mirrors of the plugin interface records and
//! the traits every provider implementation satisfies.
//!
//! Every record here mirrors a WebAssembly interface record field for field, so
//! a plugin's return value converts to a core type mechanically. The traits are
//! annotated `#[async_trait]` so they can be used through `dyn` dispatch, which
//! is how the registry hands callers provider handles without exposing
//! WebAssembly anywhere outside the plugin host.
//!
//! The mirror covers the `catalog`, `metadata`, `artwork`, `acquisition`,
//! `verification`, `scrobble`, `discovery` and `account-link` interfaces of
//! `hearthsound:plugin@0.4.0`, and of the previous `0.3.0` while the host still
//! loads it: a field only the newer package carries stays `None` for a plugin
//! built against the older one. Acquisition is the only layer whose calls need
//! host state beyond the provider's own configuration: its downloads land in a
//! per-job staging directory, so every call carries an [`AcquisitionContext`].

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{EntityKind, Granularity, ProviderError};

/// An external identifier attached to an entity.
///
/// A provider records the identifiers its upstream service uses so later calls
/// can address the same entity.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExternalId {
    /// The namespace the identifier belongs to, for example `musicbrainz`.
    pub namespace: String,
    /// The identifier's value within its namespace.
    pub value: String,
}

/// An artist credit on a track or album.
///
/// The credit carries the display name plus any external identifiers known for
/// that artist. A credit a catalog provider issues also carries the provider's
/// own id for the artist, under a namespace that is not canonical, which is
/// what [`credit_provider_id`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCredit {
    /// The display name of the credited artist.
    pub name: String,
    /// External identifiers for the credited artist.
    pub ids: Vec<ExternalId>,
}

/// The namespace of an International Standard Recording Code.
pub const ISRC: &str = "isrc";

/// The namespace of a Universal Product Code.
pub const UPC: &str = "upc";

/// The namespace of a MusicBrainz recording id.
pub const MB_RECORDING: &str = "mb-recording";

/// The namespace of a MusicBrainz release id.
pub const MB_RELEASE: &str = "mb-release";

/// The namespace of a MusicBrainz release-group id.
pub const MB_RELEASE_GROUP: &str = "mb-release-group";

/// The namespace of a MusicBrainz artist id.
pub const MB_ARTIST: &str = "mb-artist";

/// The identifier namespaces whose meaning every layer of the server shares.
///
/// An identifier in one of these namespaces names the same thing whoever issued
/// it, so it is recorded as an alias that resolves across providers. Every other
/// namespace belongs to the provider that issued it, and its value is meaningful
/// only to the instance that provider is configured as.
pub const CANONICAL_NAMESPACES: [&str; 6] = [
    ISRC,
    UPC,
    MB_RECORDING,
    MB_RELEASE,
    MB_RELEASE_GROUP,
    MB_ARTIST,
];

/// Returns the provider id a credit carries for its artist.
///
/// A catalog provider puts its own id for a credited artist in the credit's
/// identifier list, under a namespace that is not one of the
/// [`CANONICAL_NAMESPACES`]; the first such identifier is what the issuing
/// instance resolves the artist by. A credit that carries only canonical
/// identifiers describes the artist's identity but not an entity the provider
/// can be asked for, so this returns `None` for it.
///
/// # Parameters
///
/// - `credit`: the artist credit.
///
/// # Returns
///
/// The first non-canonical identifier with a value, or `None` when the credit
/// carries none.
///
/// # Example
///
/// ```
/// use hearthsound_core::providers::{ArtistCredit, ExternalId, credit_provider_id};
///
/// let credit = ArtistCredit {
///     name: "Daft Punk".to_owned(),
///     ids: vec![
///         ExternalId {
///             namespace: "mb-artist".to_owned(),
///             value: "056e4f3e-d505-4dad-8ec1-d04f521cbb56".to_owned(),
///         },
///         ExternalId {
///             namespace: "deezer".to_owned(),
///             value: "27".to_owned(),
///         },
///     ],
/// };
///
/// let id = credit_provider_id(&credit).expect("the credit carries a provider id");
/// assert_eq!((id.namespace.as_str(), id.value.as_str()), ("deezer", "27"));
/// ```
pub fn credit_provider_id(credit: &ArtistCredit) -> Option<&ExternalId> {
    credit
        .ids
        .iter()
        .find(|id| !id.value.is_empty() && !CANONICAL_NAMESPACES.contains(&id.namespace.as_str()))
}

/// An image associated with an entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The absolute URL of the image.
    pub url: String,
    /// The image width in pixels, when known.
    pub width: Option<u32>,
    /// The image height in pixels, when known.
    pub height: Option<u32>,
}

/// The compact form of an album used inside other records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAlbumRef {
    /// The provider-local album identifier.
    pub id: String,
    /// The album title.
    pub title: String,
    /// The album's artists.
    pub artists: Vec<ArtistCredit>,
    /// The release year, when known.
    pub year: Option<u16>,
    /// Cover images for the album.
    pub images: Vec<Image>,
}

/// A track returned by a catalog provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogTrack {
    /// The provider-local track identifier.
    pub id: String,
    /// The track title.
    pub title: String,
    /// The track's artists.
    pub artists: Vec<ArtistCredit>,
    /// The album the track belongs to, when known.
    pub album: Option<CatalogAlbumRef>,
    /// The track duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The disc number within a multi-disc release, when known.
    pub disc_number: Option<u16>,
    /// The track number within its disc, when known.
    pub track_number: Option<u16>,
    /// The International Standard Recording Code, when known.
    pub isrc: Option<String>,
    /// Whether the track carries explicit lyrics, when the provider says so.
    pub explicit: Option<bool>,
    /// External identifiers for the track.
    pub ids: Vec<ExternalId>,
}

/// A full album returned by a catalog provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogAlbum {
    /// The provider-local album identifier.
    pub id: String,
    /// The album title.
    pub title: String,
    /// The album's artists.
    pub artists: Vec<ArtistCredit>,
    /// The release date as an ISO 8601 date or date-time, when known.
    pub release_date: Option<String>,
    /// The release type, for example `album` or `single`, when known.
    pub album_type: Option<String>,
    /// The Universal Product Code, when known.
    pub upc: Option<String>,
    /// The album's genres.
    pub genres: Vec<String>,
    /// Cover images for the album.
    pub images: Vec<Image>,
    /// The album's tracks, in disc and track order when the provider supplies it.
    pub tracks: Vec<CatalogTrack>,
    /// External identifiers for the album.
    pub ids: Vec<ExternalId>,
}

/// An artist returned by a catalog provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogArtist {
    /// The provider-local artist identifier.
    pub id: String,
    /// The artist's display name.
    pub name: String,
    /// Images of the artist.
    pub images: Vec<Image>,
    /// External identifiers for the artist.
    pub ids: Vec<ExternalId>,
    /// The number of albums the provider reports for the artist, when it
    /// reports one.
    ///
    /// This is the provider's own count, not the number of albums the server
    /// stores for the artist; a provider that reports none leaves it `None`.
    pub album_count: Option<u32>,
}

/// The query passed to [`CatalogProvider::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogQuery {
    /// The free-text search terms.
    pub text: String,
    /// The entity kinds to return; an empty vector means every kind.
    pub kinds: Vec<EntityKind>,
    /// The maximum number of results to return; zero means none.
    pub limit: u32,
    /// The number of results to skip before returning the page.
    pub offset: u32,
}

/// The result of [`CatalogProvider::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogResults {
    /// Matching tracks.
    pub tracks: Vec<CatalogTrack>,
    /// Matching albums.
    pub albums: Vec<CatalogAlbumRef>,
    /// Matching artists.
    pub artists: Vec<CatalogArtist>,
}

/// The lifecycle operations every plugin-backed provider supports.
///
/// This trait is a supertrait of every layer's provider trait so that instance
/// health and configuration validation are callable through a `dyn` handle of
/// the layer trait.
#[async_trait]
pub trait ProviderLifecycle: Send + Sync + 'static {
    /// Validates the server-scoped configuration the host will pass to the
    /// plugin.
    ///
    /// # Parameters
    ///
    /// - `server_config`: the server-scoped plugin configuration as JSON text.
    ///
    /// # Returns
    ///
    /// An empty vector when the configuration is valid, otherwise one human
    /// readable problem description per issue.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the plugin could not be asked at all. A
    /// configuration that is merely invalid is reported through the returned
    /// vector, not as an error.
    async fn validate_config(&self, server_config: &str) -> Result<Vec<String>, ProviderError>;

    /// Reports whether the provider can currently serve requests.
    ///
    /// # Returns
    ///
    /// A short, human readable status string.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the provider is unusable, for example
    /// because its upstream service is unreachable.
    async fn health(&self) -> Result<String, ProviderError>;
}

/// Catalog search and entity lookup.
///
/// All identifiers are provider-local: a caller passes back exactly what a
/// previous call returned.
#[async_trait]
pub trait CatalogProvider: ProviderLifecycle {
    /// Searches the upstream catalog.
    ///
    /// # Parameters
    ///
    /// - `query`: the search terms, entity kinds and page window.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the search could not be performed.
    async fn search(&self, query: &CatalogQuery) -> Result<CatalogResults, ProviderError>;

    /// Looks up a track by its provider-local identifier.
    ///
    /// # Parameters
    ///
    /// - `id`: the provider-local track identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when no such track exists.
    async fn track(&self, id: &str) -> Result<CatalogTrack, ProviderError>;

    /// Looks up an album by its provider-local identifier.
    ///
    /// # Parameters
    ///
    /// - `id`: the provider-local album identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when no such album exists.
    async fn album(&self, id: &str) -> Result<CatalogAlbum, ProviderError>;

    /// Looks up an artist by their provider-local identifier.
    ///
    /// # Parameters
    ///
    /// - `id`: the provider-local artist identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when no such artist exists.
    async fn artist(&self, id: &str) -> Result<CatalogArtist, ProviderError>;

    /// Lists an artist's albums.
    ///
    /// # Parameters
    ///
    /// - `id`: the provider-local artist identifier.
    /// - `limit`: the maximum number of albums to return.
    /// - `offset`: the number of albums to skip.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when no such artist exists.
    async fn artist_albums(
        &self,
        id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<CatalogAlbumRef>, ProviderError>;

    /// Lists an artist's most popular tracks.
    ///
    /// # Parameters
    ///
    /// - `id`: the provider-local artist identifier.
    /// - `limit`: the maximum number of tracks to return.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when no such artist exists.
    async fn artist_top_tracks(
        &self,
        id: &str,
        limit: u32,
    ) -> Result<Vec<CatalogTrack>, ProviderError>;

    /// Finds every track carrying an ISRC.
    ///
    /// # Parameters
    ///
    /// - `isrc`: the International Standard Recording Code.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup could not be performed. An ISRC
    /// with no matches is an empty vector, not an error.
    async fn lookup_isrc(&self, isrc: &str) -> Result<Vec<CatalogTrack>, ProviderError>;
}

/// A hint describing the recording a caller is looking for.
///
/// The hint carries the identity the core already knows, so resolution never
/// blocks a request that has to start now; the metadata provider turns it into
/// the canonical recordings its upstream service knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingHint {
    /// The recording title.
    pub title: String,
    /// The artist names, in credit order.
    pub artists: Vec<String>,
    /// The release title the recording appeared on, when known.
    pub album: Option<String>,
    /// The recording duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The International Standard Recording Code, when known.
    pub isrc: Option<String>,
}

/// A release that contains a matched recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSummary {
    /// The release's canonical identifiers.
    pub ids: Vec<ExternalId>,
    /// The release title.
    pub title: String,
    /// The release date as an ISO 8601 date or date-time, when known.
    pub date: Option<String>,
    /// The number of tracks on the release, when known.
    pub track_count: Option<u32>,
}

/// A recording a metadata provider matched to a [`RecordingHint`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingMatch {
    /// The recording's canonical identifiers.
    pub ids: Vec<ExternalId>,
    /// The recording title.
    pub title: String,
    /// The recording's artists.
    pub artists: Vec<ArtistCredit>,
    /// The recording duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The releases the recording appears on.
    pub releases: Vec<ReleaseSummary>,
    /// The provider's own confidence in the match, from 0 to 1.
    pub score: f32,
}

/// What the server knows about an artist it wants information on.
///
/// The name is what a person reads; the identifiers are what a provider
/// matches on, because a name alone is ambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistInfoRequest {
    /// The artist's name.
    pub name: String,
    /// The identifiers the server already has for the artist.
    pub ids: Vec<ExternalId>,
}

/// One web page about an artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistLink {
    /// The kind of page: `homepage`, `wikipedia`, `wikidata`, `lastfm`,
    /// `discogs`, `allmusic`, or `bandcamp`.
    pub kind: String,
    /// The page's URL.
    pub url: String,
}

/// What a provider knows about an artist.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtistInfo {
    /// The identifiers the provider matched the artist by.
    pub ids: Vec<ExternalId>,
    /// The artist's biography, when the provider has one.
    pub biography: Option<String>,
    /// The pages about the artist.
    pub links: Vec<ArtistLink>,
}

/// Track and release metadata enrichment.
///
/// Identity resolution goes through this layer: the core passes what it knows
/// and receives canonical identifiers, whose namespaces are the
/// [`CANONICAL_NAMESPACES`] recorded in `external_ids`.
#[async_trait]
pub trait MetadataProvider: ProviderLifecycle {
    /// Resolves a recording hint to the recordings the provider knows.
    ///
    /// # Parameters
    ///
    /// - `hint`: the identity the core already knows about the recording.
    ///
    /// # Returns
    ///
    /// The matching recordings, best first. An empty vector means the provider
    /// knows no such recording, which is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup could not be performed.
    async fn resolve_recording(
        &self,
        hint: &RecordingHint,
    ) -> Result<Vec<RecordingMatch>, ProviderError>;

    /// Materializes a release from its canonical identifiers.
    ///
    /// # Parameters
    ///
    /// - `ids`: the release's identifiers, as previously returned in a
    ///   [`ReleaseSummary`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when the provider knows no such
    /// release.
    async fn get_release(&self, ids: &[ExternalId]) -> Result<CatalogAlbum, ProviderError>;

    /// Returns what the provider knows about an artist.
    ///
    /// # Parameters
    ///
    /// - `request`: the artist's name and the identifiers the server has for
    ///   it.
    ///
    /// # Returns
    ///
    /// The information, or `None` when the provider does not know the artist,
    /// which is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup could not be performed.
    async fn artist_info(
        &self,
        request: &ArtistInfoRequest,
    ) -> Result<Option<ArtistInfo>, ProviderError>;
}

/// What the server knows about an artist it wants information on.
///
/// The entity whose picture is wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtworkSubject {
    /// The kind of entity the picture is for.
    pub kind: EntityKind,
    /// The issuing instance's own id for the entity, when the server holds one.
    pub provider_id: Option<String>,
    /// The canonical identifiers the server knows for the entity.
    pub ids: Vec<ExternalId>,
    /// The album title, or the artist name.
    pub title: String,
    /// The album's artist names in credit order; empty for an artist.
    pub artists: Vec<String>,
}

/// What an artwork provider can do.
///
/// A provider answers the renditions of one entity's picture; picking one, and
/// fetching it, is the caller's work, because the caller knows the size it
/// needs and the policy of the instance that issued the image.
#[async_trait]
pub trait ArtworkProvider: ProviderLifecycle {
    /// Returns the renditions of the subject's picture.
    ///
    /// # Parameters
    ///
    /// - `subject`: the entity whose picture is wanted.
    ///
    /// # Returns
    ///
    /// The images, best first. An empty vector means the provider has no
    /// picture of the entity, which is not an error.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup could not be performed.
    async fn find_artwork(&self, subject: &ArtworkSubject) -> Result<Vec<Image>, ProviderError>;
}

/// What an acquisition provider can do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionCapabilities {
    /// The granularities the provider supports.
    ///
    /// A provider that supports only [`Granularity::Track`] is filtered out of
    /// an album request unless the request can be fanned out into one job per
    /// track.
    pub granularities: Vec<Granularity>,
    /// Whether the provider can deliver bytes of a file while it is still
    /// downloading them.
    pub progressive: bool,
}

/// One track an acquisition is looking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WantedTrack {
    /// The track title.
    pub title: String,
    /// The artist names, in credit order.
    pub artists: Vec<String>,
    /// The track number within its disc, when known.
    pub track_number: Option<u16>,
    /// The disc number within a multi-disc release, when known.
    pub disc_number: Option<u16>,
    /// The track duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The International Standard Recording Code, when known.
    pub isrc: Option<String>,
    /// The canonical identifiers resolved for the track.
    pub ids: Vec<ExternalId>,
}

/// The query passed to [`AcquisitionProvider::start_search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionQuery {
    /// Whether the query asks for one track or a whole album.
    pub granularity: Granularity,
    /// The album title, for an album request.
    pub album: Option<String>,
    /// The album artist, for an album request.
    pub album_artist: Option<String>,
    /// The release year, when known.
    pub year: Option<u16>,
    /// The tracks to find; an album request carries every track it expects.
    pub tracks: Vec<WantedTrack>,
    /// The file extensions the quality profile prefers, best first.
    pub preferred_formats: Vec<String>,
}

/// One file inside a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFile {
    /// The file name as the backend reports it.
    pub name: String,
    /// The file size in bytes, when known.
    pub size: Option<u64>,
    /// The container format, for example `flac` or `mp3`, when known.
    pub format: Option<String>,
    /// The bitrate in kbps, when known.
    pub bitrate_kbps: Option<u32>,
    /// The sample rate in Hz, when known.
    pub sample_rate: Option<u32>,
    /// The bit depth, when known.
    pub bit_depth: Option<u8>,
    /// The file duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The index of the track within the candidate this file belongs to.
    pub track_index: Option<u32>,
}

/// What the backend knows about the source offering a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// The backend's own identifier for the source, for example a Soulseek
    /// user.
    pub key: String,
    /// The source's upload speed in bytes per second, when known.
    pub speed_bps: Option<u64>,
    /// The source's queue length, when known.
    pub queue_length: Option<u32>,
    /// Whether the source has a free upload slot, when known.
    pub free_slot: Option<bool>,
}

/// One result an acquisition provider found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The provider's opaque token that addresses this candidate in later
    /// calls.
    pub token: String,
    /// The candidate's title, as the backend reports it.
    pub title: String,
    /// The backend's path for the candidate, for display and for matching.
    pub path: String,
    /// The files the candidate offers.
    pub files: Vec<CandidateFile>,
    /// What the backend knows about the source.
    pub source: SourceInfo,
}

/// The state of a running search.
///
/// The core accumulates candidates across polls and across instances; the
/// plugin neither scores nor ranks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchState {
    /// The search is still running; the candidates found so far.
    Running(Vec<Candidate>),
    /// The search has finished; every candidate it found.
    Complete(Vec<Candidate>),
}

/// A file a download produced, named relative to one of the host's roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputFile {
    /// The host root the path is relative to, for example `staging` or the id
    /// of an import root granted to the instance.
    pub root: String,
    /// The path relative to that root.
    pub path: String,
    /// The index of the track this file carries.
    pub track_index: Option<u32>,
}

/// How far a download has progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The number of bytes written so far.
    pub bytes: u64,
    /// The total number of bytes expected, when known.
    pub total_bytes: Option<u64>,
    /// The current transfer speed in bytes per second, when known.
    pub speed_bps: Option<u64>,
}

/// The state of a running download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadState {
    /// The backend has the download queued, at the given queue position when
    /// it reports one.
    Queued(Option<u32>),
    /// The download is transferring bytes.
    Running(Progress),
    /// The download finished; the files it produced.
    Complete(Vec<OutputFile>),
    /// The download failed; the backend's reason.
    Failed(String),
}

/// The host-side context of one acquisition job.
///
/// A plugin's `host-files` calls address the per-job staging directory under
/// the root name `staging`. That directory is host state, laid out as
/// `<storage>/staging/<acquisition-id>/`, and the plugin never learns where it
/// is: it names paths relative to the root. The core therefore has to tell the
/// adapter which staging directory belongs to the call it is making, which is
/// what this context carries.
///
/// # Examples
///
/// ```
/// use hearthsound_core::providers::AcquisitionContext;
///
/// let context = AcquisitionContext::new("/srv/hearthsound/staging/ac_0194/");
/// assert!(context.staging_root().ends_with("ac_0194"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionContext {
    staging_root: PathBuf,
}

impl AcquisitionContext {
    /// Creates the context of a job whose staging root is `staging_root`.
    ///
    /// # Parameters
    ///
    /// - `staging_root`: the absolute host path
    ///   `<storage>/staging/<acquisition-id>/` the job's downloads land in.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_core::providers::AcquisitionContext;
    ///
    /// let context = AcquisitionContext::new("/srv/hearthsound/staging/ac_1");
    /// assert_eq!(context.staging_root().to_str(), Some("/srv/hearthsound/staging/ac_1"));
    /// ```
    pub fn new(staging_root: impl Into<PathBuf>) -> AcquisitionContext {
        AcquisitionContext {
            staging_root: staging_root.into(),
        }
    }

    /// Returns the job's staging directory.
    ///
    /// # Returns
    ///
    /// The absolute host path `<storage>/staging/<acquisition-id>/`.
    pub fn staging_root(&self) -> &Path {
        &self.staging_root
    }
}

/// Acquiring missing content from an external downloader.
///
/// The provider searches, reports raw candidates, and carries bytes; the core
/// owns scoring, quality selection, verification and import, and never asks a
/// plugin to rank anything. Tokens are the provider's own opaque strings: a
/// token returned by [`AcquisitionProvider::start_search`] is passed back to
/// [`AcquisitionProvider::poll_search`] and
/// [`AcquisitionProvider::cancel_search`], and likewise for downloads.
///
/// Every method takes the job's [`AcquisitionContext`] because the provider's
/// file access is relative to the job's staging root.
#[async_trait]
pub trait AcquisitionProvider: ProviderLifecycle {
    /// Reports what the provider can do.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context. Capabilities do not depend on the staging
    ///   root; the argument is present so every call of this trait has the same
    ///   shape.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the provider could not be asked.
    async fn get_capabilities(
        &self,
        context: &AcquisitionContext,
    ) -> Result<AcquisitionCapabilities, ProviderError>;

    /// Starts a search and returns its token.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `query`: the search text and the tracks to find.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the search could not be started.
    async fn start_search(
        &self,
        context: &AcquisitionContext,
        query: &AcquisitionQuery,
    ) -> Result<String, ProviderError>;

    /// Polls a running search.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `search_token`: the token returned by
    ///   [`AcquisitionProvider::start_search`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when the token is unknown, and other
    /// [`ProviderError`] values when the search could not be polled.
    async fn poll_search(
        &self,
        context: &AcquisitionContext,
        search_token: &str,
    ) -> Result<SearchState, ProviderError>;

    /// Cancels a running search.
    ///
    /// Cancelling an unknown token is not an error: the search may already have
    /// finished.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `search_token`: the token returned by
    ///   [`AcquisitionProvider::start_search`].
    async fn cancel_search(&self, context: &AcquisitionContext, search_token: &str);

    /// Starts downloading the given files of a candidate and returns the
    /// download's token.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `candidate_token`: the token of a candidate a previous poll returned.
    /// - `files`: the indexes of the files to download, into the candidate's
    ///   file list.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the download could not be started.
    async fn start_download(
        &self,
        context: &AcquisitionContext,
        candidate_token: &str,
        files: &[u32],
    ) -> Result<String, ProviderError>;

    /// Polls a running download.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `download_token`: the token returned by
    ///   [`AcquisitionProvider::start_download`].
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::NotFound`] when the token is unknown, and other
    /// [`ProviderError`] values when the download could not be polled. A
    /// download the backend itself reports as failed is
    /// [`DownloadState::Failed`], not an error.
    async fn poll_download(
        &self,
        context: &AcquisitionContext,
        download_token: &str,
    ) -> Result<DownloadState, ProviderError>;

    /// Cancels a running download.
    ///
    /// Cancelling an unknown token is not an error: the download may already
    /// have finished.
    ///
    /// # Parameters
    ///
    /// - `context`: the job context.
    /// - `download_token`: the token returned by
    ///   [`AcquisitionProvider::start_download`].
    async fn cancel_download(&self, context: &AcquisitionContext, download_token: &str);

    /// Releases every tool process the provider started under `staging_root`.
    ///
    /// The orchestrator calls this when an acquisition reaches a terminal
    /// state, so a process a plugin left running cannot outlive the directory
    /// it writes into. A provider that runs no processes does nothing, which is
    /// why the default implementation is empty.
    ///
    /// # Parameters
    ///
    /// - `staging_root`: the acquisition's staging directory.
    async fn release_staging(&self, _staging_root: &Path) {}
}

/// The listener a call is made for.
///
/// Every layer that acts on a person's behalf carries this: scrobbling reports
/// that person's listens, discovery suggests music for them, and account link
/// stores their credentials. It exists so a plugin never has to guess whose
/// account it is acting on, and so the host can hand it the account's decrypted
/// configuration without the plugin knowing where that came from.
///
/// # Examples
///
/// ```
/// use hearthsound_core::providers::UserContext;
///
/// let user = UserContext {
///     user_id: "0194f3b0-2b3c-7d4e-8f00-000000000001".to_owned(),
///     config: None,
/// };
/// assert!(user.config.is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserContext {
    /// The user's UUID, lowercase and hyphenated.
    pub user_id: String,
    /// The user's configuration for the instance as JSON text, when they have
    /// one.
    ///
    /// This is the inner `config` object of their stored envelope, decrypted,
    /// so it holds exactly the fields the plugin declared in its user schema.
    pub config: Option<String>,
}

/// One listen of one track.
///
/// The field names serialize in camel case, which is the shape the scrobble
/// outbox stores and the shape a plugin receives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listen {
    /// The track title.
    pub title: String,
    /// The artist names in credit order; the first is the primary artist.
    pub artists: Vec<String>,
    /// The album title, when known.
    pub album: Option<String>,
    /// The album artist, when known.
    pub album_artist: Option<String>,
    /// The track duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The track number within its disc, when known.
    pub track_number: Option<u16>,
    /// The canonical identifiers the server knows for the recording and its
    /// entities.
    pub ids: Vec<ExternalId>,
    /// When playback started, in unix milliseconds UTC.
    pub listened_at: u64,
    /// The client application that played the track, when known.
    pub client: Option<String>,
}

/// A listener's opinion of a track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackValue {
    /// The listener likes the track.
    Love,
    /// The listener dislikes the track.
    Hate,
    /// The listener withdrew an earlier opinion.
    Clear,
}

/// How the host paces and batches deliveries to a scrobbling service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrobbleLimits {
    /// Most listens one submit call may carry; at least one.
    pub max_batch: u32,
    /// Least time between two calls to the service, in milliseconds.
    pub min_interval_ms: u32,
    /// Whether the service supports feedback.
    pub feedback: bool,
}

/// A track a discovery service suggests.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedTrack {
    /// The track title.
    pub title: String,
    /// The artist names in credit order.
    pub artists: Vec<String>,
    /// The album title, when the service reports one.
    pub album: Option<String>,
    /// The track duration in milliseconds, when the service reports one.
    pub duration_ms: Option<u32>,
    /// The identifiers the service knows for the recording.
    pub ids: Vec<ExternalId>,
    /// The service's confidence, when it reports one.
    pub score: Option<f32>,
}

/// An artist a discovery service suggests.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedArtist {
    /// The artist's name.
    pub name: String,
    /// The identifiers the service knows for the artist.
    pub ids: Vec<ExternalId>,
    /// The service's confidence, when it reports one.
    pub score: Option<f32>,
}

/// The track a similarity or popularity query is seeded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSeed {
    /// The track title.
    pub title: String,
    /// The artist names in credit order.
    pub artists: Vec<String>,
    /// The canonical identifiers the server knows for the track.
    pub ids: Vec<ExternalId>,
}

/// The artist a similarity query is seeded with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistSeed {
    /// The artist's name.
    pub name: String,
    /// The canonical identifiers the server knows for the artist.
    pub ids: Vec<ExternalId>,
}

/// A playlist a discovery service generated for the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    /// The service's stable reference for this playlist slot, so a refresh
    /// replaces it in place.
    pub reference: String,
    /// The playlist name.
    pub name: String,
    /// The playlist description, when the service supplies one.
    pub description: Option<String>,
    /// When the service retires the playlist, in unix milliseconds UTC.
    pub expires_at: Option<u64>,
}

/// A generated playlist with its tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedPlaylist {
    /// The playlist's identity and display fields.
    pub summary: PlaylistSummary,
    /// The tracks the playlist holds.
    pub tracks: Vec<SuggestedTrack>,
}

/// The recording a downloaded file is expected to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedTrack {
    /// The track title.
    pub title: String,
    /// The artist names in credit order.
    pub artists: Vec<String>,
    /// The album title, when known.
    pub album: Option<String>,
    /// The track duration in milliseconds, when known.
    pub duration_ms: Option<u32>,
    /// The canonical identifiers the server knows for the recording.
    pub ids: Vec<ExternalId>,
}

/// A Chromaprint fingerprint the host computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// The compressed fingerprint, URL-safe base64 without padding, exactly as
    /// `fpcalc` prints it.
    pub value: String,
    /// The file's full duration in whole seconds.
    pub duration_secs: u32,
}

/// What a verification service decided about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The file is the expected recording.
    Match,
    /// The file is a different recording.
    Mismatch,
    /// The service cannot tell.
    Unknown,
}

/// A verification service's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct VerificationResult {
    /// What the service decided.
    pub verdict: Verdict,
    /// The confidence of the verdict, from 0 to 1.
    pub score: f32,
    /// A short reason for the acquisition log.
    pub detail: Option<String>,
    /// The identifiers of the recording the fingerprint identified.
    pub identified: Vec<ExternalId>,
}

/// How a listener links their account on a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkMethod {
    /// The listener enters the fields of the plugin's user config schema.
    Config,
    /// The listener approves access on the service's own site.
    Browser,
}

/// A linked account: the name to show and the configuration to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedAccount {
    /// The name of the account on the service.
    pub name: String,
    /// The user configuration to store for later calls, as JSON text.
    pub config: String,
}

/// Reporting plays to a scrobbling service.
#[async_trait]
pub trait ScrobbleProvider: ProviderLifecycle {
    /// Reports the service's pacing and batch limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the plugin could not be asked.
    async fn get_limits(&self) -> Result<ScrobbleLimits, ProviderError>;

    /// Reports that the listener started playing a track.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the play belongs to.
    /// - `track`: the track being played.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::RateLimited`] when the service asked to slow
    /// down, and other [`ProviderError`] values when the report failed.
    async fn now_playing(&self, user: &UserContext, track: &Listen) -> Result<(), ProviderError>;

    /// Reports finished listens.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the listens belong to.
    /// - `listens`: the listens to submit, in the order they happened.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::RateLimited`] when the service asked to slow
    /// down, and other [`ProviderError`] values when the submission failed.
    async fn submit_listens(
        &self,
        user: &UserContext,
        listens: &[Listen],
    ) -> Result<(), ProviderError>;

    /// Reports the listener's opinion of a track.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener the opinion belongs to.
    /// - `track`: the track the opinion is about.
    /// - `value`: the opinion.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the report failed.
    async fn send_feedback(
        &self,
        user: &UserContext,
        track: &Listen,
        value: FeedbackValue,
    ) -> Result<(), ProviderError>;
}

/// Suggestions: similar tracks and artists, popular tracks, recommendations and
/// generated playlists.
///
/// A query whose answer is an empty list means the service has nothing to say;
/// the caller then asks the next instance in the layer, which is why an empty
/// list is never an error.
#[async_trait]
pub trait DiscoveryProvider: ProviderLifecycle {
    /// Suggests tracks similar to a seed track.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener, when they linked this instance; a suggestion
    ///   that does not depend on the listener is asked without one.
    /// - `seed`: the track to find similar tracks for.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed.
    async fn similar_tracks(
        &self,
        user: Option<&UserContext>,
        seed: &TrackSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError>;

    /// Suggests artists similar to a seed artist.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener, when they linked this instance.
    /// - `seed`: the artist to find similar artists for.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed.
    async fn similar_artists(
        &self,
        user: Option<&UserContext>,
        seed: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedArtist>, ProviderError>;

    /// Lists an artist's most popular tracks.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener, when they linked this instance.
    /// - `artist`: the artist whose popular tracks are wanted.
    /// - `limit`: the maximum number of tracks.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed.
    async fn artist_top_tracks(
        &self,
        user: Option<&UserContext>,
        artist: &ArtistSeed,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError>;

    /// Suggests tracks for a listener.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener.
    /// - `limit`: the maximum number of suggestions.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed.
    async fn recommended_tracks(
        &self,
        user: &UserContext,
        limit: u32,
    ) -> Result<Vec<SuggestedTrack>, ProviderError>;

    /// Lists the playlists the service generated for a listener.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed.
    async fn playlists(&self, user: &UserContext) -> Result<Vec<GeneratedPlaylist>, ProviderError>;
}

/// Checking that a downloaded file is the recording the server asked for.
#[async_trait]
pub trait VerificationProvider: ProviderLifecycle {
    /// Verifies a fingerprint against the expected recording.
    ///
    /// # Parameters
    ///
    /// - `expected`: the recording the server asked for.
    /// - `print`: the fingerprint the host computed from the file.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the lookup failed. A file the service
    /// cannot identify is [`Verdict::Unknown`], not an error.
    async fn verify(
        &self,
        expected: &ExpectedTrack,
        print: &Fingerprint,
    ) -> Result<VerificationResult, ProviderError>;
}

/// Linking a listener's account on a service to their Hearthsound account.
#[async_trait]
pub trait AccountLinkProvider: ProviderLifecycle {
    /// Reports how a listener links an account with this plugin.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the plugin could not be asked.
    async fn get_link_method(&self) -> Result<LinkMethod, ProviderError>;

    /// Checks a user configuration and names the account it belongs to.
    ///
    /// This is the `config` method: the listener has entered the fields of the
    /// plugin's user schema, and the service either accepts them or not.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener, carrying the configuration to check.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unauthorized`] when the service rejects the
    /// credentials, and other [`ProviderError`] values when the check failed.
    async fn verify_account(&self, user: &UserContext) -> Result<LinkedAccount, ProviderError>;

    /// Starts a browser link flow and returns the URL to open.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener linking their account.
    /// - `callback_url`: where the service must send the listener back.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError`] when the flow could not be started.
    async fn begin_link(
        &self,
        user: &UserContext,
        callback_url: &str,
    ) -> Result<String, ProviderError>;

    /// Finishes a browser link flow from the callback's query parameters.
    ///
    /// # Parameters
    ///
    /// - `user`: the listener linking their account.
    /// - `query`: every query parameter the callback received, except the
    ///   host's own `state`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::Unauthorized`] when the service refused the
    /// flow, and other [`ProviderError`] values when it could not be finished.
    async fn complete_link(
        &self,
        user: &UserContext,
        query: &[(String, String)],
    ) -> Result<LinkedAccount, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a credit carrying the given identifiers.
    ///
    /// # Parameters
    ///
    /// - `ids`: the namespace and value of each identifier, in credit order.
    fn credit(ids: &[(&str, &str)]) -> ArtistCredit {
        ArtistCredit {
            name: "Energy 52".to_owned(),
            ids: ids
                .iter()
                .map(|(namespace, value)| ExternalId {
                    namespace: (*namespace).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_canonical_namespaces_are_the_ones_identity_resolution_shares() {
        assert_eq!(
            CANONICAL_NAMESPACES,
            [
                "isrc",
                "upc",
                "mb-recording",
                "mb-release",
                "mb-release-group",
                "mb-artist"
            ]
        );
    }

    #[test]
    fn a_credits_provider_id_is_its_first_non_canonical_identifier() {
        let credit = credit(&[
            (MB_ARTIST, "artist-1"),
            ("deezer", "27"),
            ("http-catalog", "27"),
        ]);

        let id = credit_provider_id(&credit).unwrap();

        assert_eq!(id.namespace, "deezer");
        assert_eq!(id.value, "27");
    }

    #[test]
    fn a_credit_of_canonical_identifiers_only_has_no_provider_id() {
        assert!(credit_provider_id(&credit(&[(MB_ARTIST, "artist-1")])).is_none());
        assert!(credit_provider_id(&credit(&[(ISRC, "USQX91300105")])).is_none());
        assert!(credit_provider_id(&credit(&[])).is_none());
    }

    #[test]
    fn an_empty_identifier_is_not_a_provider_id() {
        assert!(credit_provider_id(&credit(&[("deezer", "")])).is_none());

        let credit = credit(&[("deezer", ""), ("hello", "artist-1")]);

        assert_eq!(
            credit_provider_id(&credit).unwrap().value.as_str(),
            "artist-1"
        );
    }
}
