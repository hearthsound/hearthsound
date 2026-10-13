//! The provider abstraction: Rust mirrors of the plugin interface records and
//! the traits every provider implementation satisfies.
//!
//! Every record here mirrors a WebAssembly interface record field for field, so
//! a plugin's return value converts to a core type mechanically. The traits are
//! annotated `#[async_trait]` so they can be used through `dyn` dispatch, which
//! is how the registry hands callers provider handles without exposing
//! WebAssembly anywhere outside the plugin host.

use async_trait::async_trait;

use crate::{EntityKind, ProviderError};

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
