//! The provider abstraction: Rust mirrors of the plugin interface records and
//! the traits every provider implementation satisfies.
//!
//! Every record here mirrors a WebAssembly interface record field for field, so
//! a plugin's return value converts to a core type mechanically. The traits are
//! annotated `#[async_trait]` so they can be used through `dyn` dispatch, which
//! is how the registry hands callers provider handles without exposing
//! WebAssembly anywhere outside the plugin host.
//!
//! The mirror covers the `catalog`, `metadata` and `acquisition` interfaces of
//! `hearthsound:plugin@0.1.0`. Acquisition is the only layer whose calls need
//! host state beyond the provider's own configuration: its downloads land in a
//! per-job staging directory, so every call carries an [`AcquisitionContext`].

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{EntityKind, Granularity, ProviderError};

/// An external identifier attached to an entity.
///
/// A provider records the identifiers its upstream service uses so later calls
/// can address the same entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalId {
    /// The namespace the identifier belongs to, for example `musicbrainz`.
    pub namespace: String,
    /// The identifier's value within its namespace.
    pub value: String,
}

/// An artist credit on a track or album.
///
/// The credit carries the display name plus any external identifiers known for
/// that artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistCredit {
    /// The display name of the credited artist.
    pub name: String,
    /// External identifiers for the credited artist.
    pub ids: Vec<ExternalId>,
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

/// Track and release metadata enrichment.
///
/// Identity resolution goes through this layer: the core passes what it knows
/// and receives canonical identifiers whose namespaces (`isrc`, `mb-recording`,
/// `mb-release`, `mb-release-group`, `mb-artist`) are recorded in
/// `external_ids`.
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
}
