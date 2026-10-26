//! Whole artist and album pages: the library and the catalog as one listing.
//!
//! A client that opens an artist sees the same page whether it holds the stored
//! `ar_` id or the catalog `x_` id, and an album page holds its whole tracklist
//! for the same reason. This module builds those two pages: it pairs the stored
//! row with the catalog entity that names the same release, merges the two
//! listings, and keeps a stored row's id for every entity the server already
//! holds.
//!
//! The pairing is what makes the merge possible. A stored row is matched to a
//! catalog entity through the `p:<slug>` alias a materialization recorded, and
//! otherwise through the artist's name or the album's title, with the edition
//! suffixes a provider and a scanner disagree about folded away. A pair the
//! builder establishes is recorded, so the next visit answers from the alias.
//!
//! Two depths decide how much work a page does, because a browse page is read by
//! the first-party client and by any Subsonic app:
//!
//! - [`PageDepth::Full`] pairs by name and title and records what it finds. The
//!   first-party client asks for it by sending `hearthsound=1`, and the native
//!   API always reads at this depth.
//! - [`PageDepth::Library`] pairs only through recorded aliases and never
//!   writes. A stored entity the caller's scopes hold answers from stored rows
//!   without touching the catalog at all, which is what a third-party app
//!   syncing a library needs.
//! - [`PageDepth::Cached`] is what a first-party page read uses: it pairs through
//!   recorded aliases and through the answers the catalog layers already cached,
//!   never calls a provider and never writes. A cold entity answers from the
//!   library alone and a `catalog.enrich` job fills the rest.
//!
//! ## Privacy
//!
//! A stored row is shown by its own id only when one of the caller's scopes
//! contains it; otherwise the catalog entity it pairs with stands in for it, and
//! a stored row the catalog does not know is left out. That is the rule the
//! Subsonic browse endpoints already apply to their scopes, and it is what keeps
//! another user's upload out of a shared artist page.

use std::collections::{HashMap, HashSet};

use hearthsound_core::providers::{CatalogAlbum, CatalogTrack, credit_provider_id};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Layer, Slug};
use hearthsound_library::query::{self, AlbumDetail, Scope};
use hearthsound_library::{Album, Track, albums, artists};

use crate::ProviderAlias;
use crate::aliases::{aliases_of_entities, provider_aliases, provider_namespace};
use crate::dedupe::{DURATION_TOLERANCE_MS, base_title, normalize};
use crate::error::CatalogError;
use crate::materialize::claim_alias;
use crate::service::{
    CatalogAlbumRefResult, CatalogAlbumResult, CatalogArtistResult, CatalogService,
    CatalogTrackResult, catalog_id,
};

/// The maximum number of catalog albums one artist page lists.
pub const MAX_PAGE_ALBUMS: u32 = 500;

/// How much a page may pair and write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageDepth {
    /// Pair through recorded aliases only and never write.
    ///
    /// This is the depth of a request that does not carry `hearthsound=1`: a
    /// stored entity inside a scope answers from stored rows, without a catalog
    /// call.
    Library,
    /// Pair by alias and by cached provider answers only; never call a provider
    /// and never write.
    ///
    /// This is the depth of a first-party page read: the page answers from the
    /// library plus whatever the catalog layers already cached, and the extras a
    /// cold entity lacks are fetched by a `catalog.enrich` job.
    Cached,
    /// Pair by alias, then by name or title, and record what the page found.
    ///
    /// This is the depth of the enrichment job and of the native API.
    Full,
}

/// The head of an artist page.
#[derive(Debug, Clone)]
pub enum ArtistHead {
    /// The stored artist, shown by its own id.
    Stored(EntityId),
    /// The catalog artist, when the caller's scopes hold no stored row for it.
    Catalog(CatalogArtistResult),
}

/// One album of an artist page.
#[derive(Debug, Clone)]
pub enum PageAlbum {
    /// A stored album the caller's scopes hold, by its own id.
    Stored(EntityId),
    /// A catalog album, by the id its instance issued.
    Catalog(CatalogAlbumRefResult),
}

/// The listings of one artist.
#[derive(Debug, Clone)]
pub struct ArtistPage {
    /// The artist the page is about.
    pub head: ArtistHead,
    /// The artist's albums: the catalog discography with stored albums in the
    /// places they belong, then the stored albums the catalog does not list.
    pub albums: Vec<PageAlbum>,
    /// Whether the discography the page could list is the whole one.
    ///
    /// A provider error while listing leaves the page answerable but marks the
    /// listing incomplete, so a client can offer a reload instead of presenting a
    /// partial discography as the artist's work.
    pub discography_complete: bool,
    /// Whether the discography was cut at [`MAX_PAGE_ALBUMS`].
    pub discography_truncated: bool,
}

/// The head of an album page.
#[derive(Debug, Clone)]
pub enum AlbumHead {
    /// The stored album, shown by its own id.
    Stored(EntityId),
    /// The catalog album, when the caller's scopes hold no stored row for it.
    Catalog(Box<CatalogAlbumResult>),
}

/// One track of an album page.
#[derive(Debug, Clone)]
pub enum PageTrack {
    /// A stored track the caller may see, by its own id.
    Stored(EntityId),
    /// A catalog track, by the id its instance issued.
    Catalog(Box<CatalogTrackResult>),
}

/// The listings of one album.
#[derive(Debug, Clone)]
pub struct AlbumPage {
    /// The album the page is about.
    pub head: AlbumHead,
    /// The album's tracks, in playing order.
    pub tracks: Vec<PageTrack>,
}

/// How strongly a stored album answers to a catalog title.
///
/// The ordering is the preference order: a title that agrees once both are
/// folded wins over one that only agrees once its edition suffix is folded away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TitleMatch {
    /// The titles agree once their edition suffixes are folded away too.
    Edition,
    /// The titles agree once both are folded.
    Exact,
}

/// One catalog album a stored album may be the same release as.
struct Candidate<'a> {
    /// The album's position in the provider's listing.
    index: usize,
    /// The album title.
    title: &'a str,
    /// The release year, when the provider stated one.
    year: Option<u16>,
}

impl CatalogService {
    /// Lists the catalog instances in the administrator's order.
    ///
    /// Only these slugs may issue an entity of a page: an alias pointing at an
    /// instance that no longer serves the catalog layer names an entity no
    /// lookup can reach.
    pub(crate) fn catalog_layer(&self) -> Vec<Slug> {
        self.registry()
            .layers(Layer::Catalog)
            .iter()
            .map(|instance| instance.slug().clone())
            .collect()
    }

    /// Reads the provider alias of a stored row that a catalog instance issued.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored row.
    /// - `layer`: the catalog instances, in the administrator's order.
    ///
    /// # Returns
    ///
    /// The alias of the first instance in `layer` that recorded one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the alias table cannot be read.
    async fn catalog_alias(
        &self,
        entity: EntityId,
        layer: &[Slug],
    ) -> Result<Option<ProviderAlias>, CatalogError> {
        if layer.is_empty() {
            return Ok(None);
        }
        let aliases = self.provider_aliases(entity).await?;
        Ok(layer
            .iter()
            .find_map(|slug| aliases.iter().find(|alias| &alias.slug == slug).cloned()))
    }

    /// Records that a stored row is the entity a catalog id names.
    ///
    /// A pairing that cannot be written is logged and dropped: the page it was
    /// found for is already built, and the next visit tries again.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored row.
    /// - `kind`: the kind of the row.
    /// - `catalog`: the catalog id that names the same entity.
    pub(crate) async fn record_alias(
        &self,
        entity: EntityId,
        kind: EntityKind,
        catalog: &CatalogId,
    ) {
        let namespace = provider_namespace(catalog.slug());
        let outcome = async {
            let mut tx = self.database().writer().begin().await?;
            claim_alias(&mut tx, &namespace, kind, catalog.provider_id(), entity).await?;
            tx.commit().await?;
            Ok::<(), CatalogError>(())
        }
        .await;
        if let Err(error) = outcome {
            tracing::debug!(
                %error,
                entity = %entity,
                catalog = %catalog,
                "a page pairing could not record its provider alias"
            );
        }
    }

    /// Records the pairs an artist page established between stored albums and
    /// the provider's album ids, in one short transaction.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that issued the provider ids.
    /// - `pairs`: the stored album and its provider id.
    async fn record_album_aliases(&self, slug: &Slug, pairs: &[(EntityId, String)]) {
        if pairs.is_empty() {
            return;
        }
        let namespace = provider_namespace(slug);
        let outcome = async {
            let mut tx = self.database().writer().begin().await?;
            for (entity, provider_id) in pairs {
                claim_alias(&mut tx, &namespace, EntityKind::Album, provider_id, *entity).await?;
            }
            tx.commit().await?;
            Ok::<(), CatalogError>(())
        }
        .await;
        if let Err(error) = outcome {
            tracing::debug!(
                %error,
                slug = %slug,
                pairs = pairs.len(),
                "an artist page could not record its album pairings"
            );
        }
    }

    /// Returns the first of the caller's scopes that holds an artist.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller reads, in listing order.
    /// - `artist`: the stored artist.
    ///
    /// # Returns
    ///
    /// The first scope that contains the artist, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a query fails.
    async fn scope_of_artist(
        &self,
        scopes: &[Scope],
        artist: EntityId,
    ) -> Result<Option<Scope>, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        for scope in scopes {
            if query::artist_in_scope(&mut conn, *scope, artist).await? {
                return Ok(Some(*scope));
            }
        }
        Ok(None)
    }

    /// Returns the first of the caller's scopes that holds an album.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller reads, in listing order.
    /// - `album`: the stored album.
    ///
    /// # Returns
    ///
    /// The first scope that contains the album, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a query fails.
    async fn scope_of_album(
        &self,
        scopes: &[Scope],
        album: EntityId,
    ) -> Result<Option<Scope>, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        for scope in scopes {
            if query::album_in_scope(&mut conn, *scope, album).await? {
                return Ok(Some(*scope));
            }
        }
        Ok(None)
    }

    /// Reports whether any of the caller's scopes holds a track.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller reads.
    /// - `track`: the stored track.
    ///
    /// # Returns
    ///
    /// `true` when one scope contains the track.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a query fails.
    async fn track_in_any_scope(
        &self,
        scopes: &[Scope],
        track: EntityId,
    ) -> Result<bool, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        for scope in scopes {
            if query::track_in_scope(&mut conn, *scope, track).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Lists the stored albums of an artist that the caller's scopes hold.
    ///
    /// # Parameters
    ///
    /// - `scopes`: the scopes the caller reads, in listing order.
    /// - `artist`: the stored artist.
    ///
    /// # Returns
    ///
    /// The albums, in the order each scope lists them, one entry per album.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a query fails.
    async fn visible_albums_of(
        &self,
        scopes: &[Scope],
        artist: EntityId,
    ) -> Result<Vec<EntityId>, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        let mut seen = HashSet::new();
        let mut found = Vec::new();
        for scope in scopes {
            for album in query::list_albums_for_artist(&mut conn, *scope, artist).await? {
                if seen.insert(album.id) {
                    found.push(album.id);
                }
            }
        }
        Ok(found)
    }

    /// Reads one catalog artist at the depth the caller asked for.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of the artist.
    /// - `depth`: how much the page may spend.
    ///
    /// # Returns
    ///
    /// The artist, or `None` when it is unknown and when the cached depth has no
    /// answer for it.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a live read fails.
    async fn page_artist(
        &self,
        id: &CatalogId,
        depth: PageDepth,
    ) -> Result<Option<CatalogArtistResult>, CatalogError> {
        if depth == PageDepth::Cached {
            return self.cached_artist(id).await;
        }
        match self.artist(id).await {
            Ok(found) => Ok(Some(found)),
            Err(CatalogError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads one catalog album with its tracklist at the depth the caller asked
    /// for.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of the album.
    /// - `depth`: how much the page may spend.
    ///
    /// # Returns
    ///
    /// The album, or `None` when it is unknown and when the cached depth has no
    /// answer for it.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a live read fails.
    async fn page_album(
        &self,
        id: &CatalogId,
        depth: PageDepth,
    ) -> Result<Option<CatalogAlbumResult>, CatalogError> {
        if depth == PageDepth::Cached {
            return self.cached_album(id).await;
        }
        match self.album(id).await {
            Ok(found) => Ok(Some(found)),
            Err(CatalogError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads an artist's album listing at the depth the caller asked for.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of the artist.
    /// - `limit`: the page size.
    /// - `offset`: the offset.
    /// - `depth`: how much the page may spend.
    ///
    /// # Returns
    ///
    /// The listing, or `None` when the cached depth has no answer for that page.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a live read fails.
    async fn page_artist_albums(
        &self,
        id: &CatalogId,
        limit: u32,
        offset: u32,
        depth: PageDepth,
    ) -> Result<Option<Vec<CatalogAlbumRefResult>>, CatalogError> {
        if depth == PageDepth::Cached {
            return self.cached_artist_albums(id, limit, offset).await;
        }
        self.artist_albums(id, limit, offset).await.map(Some)
    }

    /// Resolves the catalog artist a stored artist pairs with.
    ///
    /// The recorded alias is tried first. When `by_name` is set, the artist's
    /// name is searched too, and a match is recorded for the next visit.
    ///
    /// # Parameters
    ///
    /// - `artist`: the stored artist.
    /// - `layer`: the catalog instances, in the administrator's order.
    /// - `depth`: how much the page may pair and write. A `Cached` read uses the
    ///   cached provider answer of a recorded alias and never searches by name;
    ///   a `Library` read uses neither.
    ///
    /// # Returns
    ///
    /// The catalog artist, or `None` when nothing names one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read or an alias read fails; a
    /// pairing by name that fails is logged and reported as `None`.
    pub(crate) async fn catalog_artist_of_stored(
        &self,
        artist: EntityId,
        layer: &[Slug],
        depth: PageDepth,
    ) -> Result<Option<CatalogArtistResult>, CatalogError> {
        if depth == PageDepth::Library {
            return Ok(None);
        }
        if let Some(alias) = self.catalog_alias(artist, layer).await?
            && let Ok(id) = catalog_id(&alias.slug, EntityKind::Artist, &alias.provider_id)
        {
            match self.page_artist(&id, depth).await {
                Ok(Some(found)) => return Ok(Some(found)),
                Ok(None) => return Ok(None),
                Err(CatalogError::NotFound) => {}
                Err(error) => tracing::debug!(
                    %error,
                    artist = %artist,
                    "a recorded artist alias could not be read back"
                ),
            }
        }
        if depth == PageDepth::Cached {
            let name = {
                let mut conn = self.database().readers().acquire().await?;
                artists::get_artist(&mut conn, artist).await?
            };
            let Some(name) = name else {
                return Ok(None);
            };
            return Ok(self.cached_artist_by_name(&name.name).await);
        }
        let name = {
            let mut conn = self.database().readers().acquire().await?;
            artists::get_artist(&mut conn, artist).await?
        };
        let Some(name) = name else {
            return Ok(None);
        };
        let found = match self.artist_by_name(&name.name).await {
            Ok(found) => found,
            Err(error) => {
                // A browse page degrades to the library it holds: the page the
                // caller asked for is answerable without the catalog.
                tracing::warn!(
                    %error,
                    artist = %artist,
                    "an artist could not be resolved by name"
                );
                return Ok(None);
            }
        };
        if let Some(found) = &found
            && depth == PageDepth::Full
        {
            self.record_alias(artist, EntityKind::Artist, &found.id)
                .await;
        }
        Ok(found)
    }

    /// Finds the stored artist a catalog artist is the same act as.
    ///
    /// The name is compared once both are folded, and only an artist that no
    /// catalog instance already claims is adopted, so two acts whose names
    /// merely resemble each other are not merged.
    ///
    /// # Parameters
    ///
    /// - `catalog`: the catalog artist.
    /// - `layer`: the catalog instances, in the administrator's order.
    ///
    /// # Returns
    ///
    /// The stored artist, or `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub(crate) async fn stored_artist_of_catalog(
        &self,
        catalog: &CatalogArtistResult,
        layer: &[Slug],
    ) -> Result<Option<EntityId>, CatalogError> {
        let candidate = {
            let mut conn = self.database().readers().acquire().await?;
            artists::find_artist_by_name(&mut conn, &catalog.artist.name).await?
        };
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        if normalize(&candidate.name) != normalize(&catalog.artist.name) {
            return Ok(None);
        }
        if self.catalog_alias(candidate.id, layer).await?.is_some() {
            return Ok(None);
        }
        self.record_alias(candidate.id, EntityKind::Artist, &catalog.id)
            .await;
        Ok(Some(candidate.id))
    }

    /// Finds the stored album a catalog album is the same release as.
    ///
    /// The local artist is resolved from the album's credits, and that artist's
    /// stored albums are compared with the catalog album.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that issued the catalog album.
    /// - `album`: the catalog album, with its tracklist.
    ///
    /// # Returns
    ///
    /// The stored album, or `None` when no stored album is the same release.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub(crate) async fn stored_album_of_catalog(
        &self,
        slug: &Slug,
        album: &CatalogAlbum,
    ) -> Result<Option<EntityId>, CatalogError> {
        let credit = album
            .artists
            .first()
            .or_else(|| album.tracks.iter().find_map(|track| track.artists.first()));
        let Some(credit) = credit else {
            return Ok(None);
        };
        let mut artist = None;
        if let Some(provider) = credit_provider_id(credit)
            && provider.namespace == slug.as_str()
        {
            artist = self
                .entity_by_alias(
                    &provider_namespace(slug),
                    EntityKind::Artist,
                    &provider.value,
                )
                .await?;
        }
        if artist.is_none() {
            let found = {
                let mut conn = self.database().readers().acquire().await?;
                artists::find_artist_by_name(&mut conn, &credit.name).await?
            };
            artist = found
                .filter(|found| normalize(&found.name) == normalize(&credit.name))
                .map(|found| found.id);
        }
        let Some(artist) = artist else {
            return Ok(None);
        };
        let stored_albums = {
            let mut conn = self.database().readers().acquire().await?;
            albums::list_albums_for_artist(&mut conn, artist).await?
        };
        if stored_albums.is_empty() {
            return Ok(None);
        }
        let ids: Vec<EntityId> = stored_albums.iter().map(|stored| stored.id).collect();
        let aliased = aliases_of_entities(
            self.database(),
            &provider_namespace(slug),
            EntityKind::Album,
            &ids,
        )
        .await?;
        let candidates = [Candidate {
            index: 0,
            title: &album.title,
            year: release_year(album.release_date.as_deref()),
        }];
        let pairs = match_albums(&stored_albums, &aliased, &candidates);
        Ok(pairs.first().map(|(_, stored)| *stored))
    }

    /// Resolves the catalog album a stored album pairs with.
    ///
    /// The album is matched among the discography of the catalog artist its
    /// credits name, so two albums of different artists that share a title are
    /// never paired.
    ///
    /// # Parameters
    ///
    /// - `album`: the stored album.
    /// - `detail`: the stored album with its credits and tracks.
    /// - `layer`: the catalog instances, in the administrator's order.
    /// - `depth`: how much the page may pair and write.
    ///
    /// # Returns
    ///
    /// The catalog album reference, or `None` when nothing names one.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub(crate) async fn catalog_album_of_stored(
        &self,
        album: EntityId,
        detail: &AlbumDetail,
        layer: &[Slug],
        depth: PageDepth,
    ) -> Result<Option<CatalogAlbumRefResult>, CatalogError> {
        if depth == PageDepth::Library {
            return Ok(None);
        }
        if depth == PageDepth::Cached {
            // A cached read pairs through the recorded alias only: a name search
            // and a discography walk are provider calls.
            let Some(alias) = self.catalog_alias(album, layer).await? else {
                return Ok(None);
            };
            let Ok(id) = catalog_id(&alias.slug, EntityKind::Album, &alias.provider_id) else {
                return Ok(None);
            };
            let Some(found) = self.cached_album(&id).await? else {
                let reference = self.cached_album_ref(&id).await?;
                return Ok(reference);
            };
            return Ok(Some(CatalogAlbumRefResult {
                id: found.id,
                album: crate::service::album_ref_of(&found.album),
            }));
        }
        // A recorded alias is one lookup; without one the album is resolved by
        // its artist and title, which is one search instead of the discography
        // walk a page used to pay for.
        if let Some(alias) = self.catalog_alias(album, layer).await?
            && let Ok(id) = catalog_id(&alias.slug, EntityKind::Album, &alias.provider_id)
            && let Ok(found) = self.album(&id).await
        {
            return Ok(Some(CatalogAlbumRefResult {
                id: found.id,
                album: crate::service::album_ref_of(&found.album),
            }));
        }
        let Some(stored) = ({
            let mut conn = self.database().readers().acquire().await?;
            albums::get_album(&mut conn, album).await?
        }) else {
            return Ok(None);
        };
        let credit = detail
            .artists
            .first()
            .map(|artist| artist.name.clone())
            .or_else(|| {
                detail
                    .tracks
                    .iter()
                    .find_map(|track| track.artists.first().map(|artist| artist.name.clone()))
            })
            .or_else(|| stored.display_artist.clone());
        let Some(credit) = credit else {
            return Ok(None);
        };
        let Some(reference) = self
            .album_by_title(
                &credit,
                &stored.title,
                stored
                    .year
                    .map(|year| i32::try_from(year).unwrap_or(i32::MAX)),
            )
            .await?
        else {
            return Ok(None);
        };
        if depth == PageDepth::Full {
            let slug = reference.id.slug().clone();
            self.record_album_aliases(&slug, &[(album, reference.album.id.clone())])
                .await;
        }
        Ok(Some(reference))
    }

    /// Builds one artist page.
    ///
    /// The page lists the artist's catalog discography with the stored albums
    /// in the places they belong, followed by the stored albums the catalog does
    /// not list. Which id the caller named does not change the listing: an `x_`
    /// artist that pairs with a stored row opens the stored artist.
    ///
    /// A catalog that is not installed, does not know the artist, or fails the
    /// lookup leaves a stored artist's page as the library holds it: a browse
    /// page degrades to the content the server has rather than to an error.
    ///
    /// One catalog album resolves to one stored album: a reference that a stored
    /// album already claimed is not paired with another, so a page never re-points
    /// a claim the store holds.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist id, stored or catalog.
    /// - `scopes`: the scopes the caller reads, in listing order.
    /// - `depth`: how much the page may pair and write.
    ///
    /// # Returns
    ///
    /// The page, or `None` when the id names no artist the caller may see.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read fails and when a catalog
    /// artist the caller named cannot be read.
    pub async fn artist_page(
        &self,
        id: &ClientId,
        scopes: &[Scope],
        depth: PageDepth,
    ) -> Result<Option<ArtistPage>, CatalogError> {
        let layer = self.catalog_layer();
        let (stored, catalog, visible) = match id {
            ClientId::Entity(entity) if entity.kind() == IdPrefix::Artist => {
                let exists = {
                    let mut conn = self.database().readers().acquire().await?;
                    artists::get_artist(&mut conn, *entity).await?.is_some()
                };
                if !exists {
                    return Ok(None);
                }
                let visible = self.scope_of_artist(scopes, *entity).await?;
                let mut catalog = None;
                if depth != PageDepth::Library || visible.is_none() {
                    catalog = self
                        .catalog_artist_of_stored(*entity, &layer, depth)
                        .await?;
                }
                (Some(*entity), catalog, visible)
            }
            ClientId::Catalog(catalog_id) if catalog_id.kind() == EntityKind::Artist => {
                let Some(fetched) = self.page_artist(catalog_id, depth).await? else {
                    return Ok(None);
                };
                let namespace = provider_namespace(catalog_id.slug());
                let mut stored = self
                    .entity_by_alias(&namespace, EntityKind::Artist, catalog_id.provider_id())
                    .await?;
                if stored.is_none() && depth == PageDepth::Full {
                    stored = self.stored_artist_of_catalog(&fetched, &layer).await?;
                }
                let visible = match stored {
                    Some(stored) => self.scope_of_artist(scopes, stored).await?,
                    None => None,
                };
                (stored, Some(fetched), visible)
            }
            _ => return Ok(None),
        };

        let stored_head = stored.filter(|_| visible.is_some());
        let head = match (&stored_head, &catalog) {
            (Some(stored), _) => ArtistHead::Stored(*stored),
            (None, Some(catalog)) => ArtistHead::Catalog(catalog.clone()),
            (None, None) => return Ok(None),
        };

        let mut albums: Vec<PageAlbum> = Vec::new();
        let mut emitted: HashSet<EntityId> = HashSet::new();
        let mut discography_complete = true;
        let mut discography_truncated = false;
        if let Some(catalog) = &catalog {
            let references = match self
                .page_artist_albums(&catalog.id, MAX_PAGE_ALBUMS, 0, depth)
                .await
            {
                Ok(Some(found)) => found,
                Ok(None) => Vec::new(),
                Err(error) => {
                    // An operator-visible event: the page answers the library it
                    // holds, but the artist's work is not all there.
                    tracing::warn!(
                        %error,
                        artist = %catalog.id,
                        "an artist discography could not be listed"
                    );
                    discography_complete = false;
                    Vec::new()
                }
            };
            discography_truncated = u32::try_from(references.len()).unwrap_or(u32::MAX) >= MAX_PAGE_ALBUMS;
            if !references.is_empty() {
                let slug = catalog.id.slug().clone();
                let provider_ids: Vec<String> = references
                    .iter()
                    .map(|reference| reference.album.id.clone())
                    .collect();
                let mut by_provider_id =
                    provider_aliases(self.database(), &slug, EntityKind::Album, &provider_ids)
                        .await?;
                if depth != PageDepth::Library
                    && let Some(artist) = stored_head
                {
                    let stored_albums = {
                        let mut conn = self.database().readers().acquire().await?;
                        albums::list_albums_for_artist(&mut conn, artist).await?
                    };
                    if !stored_albums.is_empty() {
                        let ids: Vec<EntityId> =
                            stored_albums.iter().map(|album| album.id).collect();
                        let aliased = aliases_of_entities(
                            self.database(),
                            &provider_namespace(&slug),
                            EntityKind::Album,
                            &ids,
                        )
                        .await?;
                        // A reference another stored album already claimed is not a
                        // candidate: one catalog album resolves to one stored album,
                        // and a page read never re-points an existing claim.
                        let candidates: Vec<Candidate<'_>> = references
                            .iter()
                            .enumerate()
                            .filter(|(_, reference)| {
                                !by_provider_id.contains_key(&reference.album.id)
                            })
                            .map(|(index, reference)| Candidate {
                                index,
                                title: &reference.album.title,
                                year: reference.album.year,
                            })
                            .collect();
                        let pairs = match_albums(&stored_albums, &aliased, &candidates);
                        let mut recorded = Vec::with_capacity(pairs.len());
                        for (index, album) in &pairs {
                            by_provider_id.insert(references[*index].album.id.clone(), *album);
                            recorded.push((*album, references[*index].album.id.clone()));
                        }
                        if depth == PageDepth::Full {
                            self.record_album_aliases(&slug, &recorded).await;
                        }
                    }
                }
                for reference in &references {
                    let stored = by_provider_id.get(&reference.album.id).copied();
                    match stored {
                        Some(album) if self.scope_of_album(scopes, album).await?.is_some() => {
                            if emitted.insert(album) {
                                albums.push(PageAlbum::Stored(album));
                            }
                        }
                        _ => albums.push(PageAlbum::Catalog(reference.clone())),
                    }
                }
            }
        }
        if let Some(artist) = stored_head {
            for album in self.visible_albums_of(scopes, artist).await? {
                if emitted.insert(album) {
                    albums.push(PageAlbum::Stored(album));
                }
            }
        }
        Ok(Some(ArtistPage {
            head,
            albums,
            discography_complete,
            discography_truncated,
        }))
    }

    /// Builds one album page.
    ///
    /// The page lists the whole tracklist of the album, with the stored tracks
    /// in the places they belong, and the stored tracks the catalog does not
    /// list after them. Which id the caller named does not change the listing.
    ///
    /// # Parameters
    ///
    /// - `id`: the album id, stored or catalog.
    /// - `scopes`: the scopes the caller reads, in listing order.
    /// - `depth`: how much the page may pair and write.
    ///
    /// # Returns
    ///
    /// The page, or `None` when the id names no album the caller may see.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read fails and when a catalog
    /// album the caller named cannot be read.
    pub async fn album_page(
        &self,
        id: &ClientId,
        scopes: &[Scope],
        depth: PageDepth,
    ) -> Result<Option<AlbumPage>, CatalogError> {
        let layer = self.catalog_layer();
        let (stored, fetched, visible) = match id {
            ClientId::Entity(entity) if entity.kind() == IdPrefix::Album => {
                let detail = {
                    let mut conn = self.database().readers().acquire().await?;
                    query::album_detail_any(&mut conn, *entity).await?
                };
                let Some(detail) = detail else {
                    return Ok(None);
                };
                let visible = self.scope_of_album(scopes, *entity).await?;
                let mut fetched = None;
                if (depth != PageDepth::Library || visible.is_none())
                    && let Some(reference) = self
                        .catalog_album_of_stored(*entity, &detail, &layer, depth)
                        .await?
                {
                    match self.page_album(&reference.id, depth).await {
                        Ok(album) => fetched = album,
                        Err(error) if visible.is_none() => return Err(error),
                        Err(error) => tracing::debug!(
                            %error,
                            album = %reference.id,
                            "a paired album tracklist could not be read"
                        ),
                    }
                }
                (Some(*entity), fetched, visible)
            }
            ClientId::Catalog(catalog_id) if catalog_id.kind() == EntityKind::Album => {
                let fetched = match self.page_album(catalog_id, depth).await? {
                    Some(found) => found,
                    // The hero of a cold album page is the cached reference: the
                    // title and the credits are known, the tracklist arrives with
                    // the enrichment.
                    None => match self.cached_album_ref(catalog_id).await? {
                        Some(reference) => CatalogAlbumResult {
                            id: reference.id.clone(),
                            album: CatalogAlbum {
                                id: reference.album.id,
                                title: reference.album.title,
                                artists: reference.album.artists,
                                release_date: None,
                                album_type: None,
                                upc: None,
                                genres: Vec::new(),
                                images: reference.album.images,
                                tracks: Vec::new(),
                                ids: Vec::new(),
                            },
                        },
                        None => return Ok(None),
                    },
                };
                let namespace = provider_namespace(catalog_id.slug());
                let mut stored = self
                    .entity_by_alias(&namespace, EntityKind::Album, catalog_id.provider_id())
                    .await?;
                if stored.is_none() && depth == PageDepth::Full {
                    stored = self
                        .stored_album_of_catalog(catalog_id.slug(), &fetched.album)
                        .await?;
                    if let Some(stored) = stored {
                        self.record_alias(stored, EntityKind::Album, &fetched.id)
                            .await;
                    }
                }
                let visible = match stored {
                    Some(stored) => self.scope_of_album(scopes, stored).await?,
                    None => None,
                };
                (stored, Some(fetched), visible)
            }
            _ => return Ok(None),
        };

        let stored_head = stored.filter(|_| visible.is_some());
        let head = match (&stored_head, &fetched) {
            (Some(stored), _) => AlbumHead::Stored(*stored),
            (None, Some(album)) => AlbumHead::Catalog(Box::new(album.clone())),
            (None, None) => return Ok(None),
        };
        let stored_detail = match stored_head {
            Some(stored) => {
                let mut conn = self.database().readers().acquire().await?;
                query::album_detail_any(&mut conn, stored).await?
            }
            None => None,
        };

        let mut tracks = Vec::new();
        let mut emitted: HashSet<EntityId> = HashSet::new();
        if let Some(fetched) = &fetched {
            let slug = fetched.id.slug().clone();
            let known: Vec<&Track> = stored_detail
                .as_ref()
                .map(|detail| detail.tracks.iter().map(|row| &row.track).collect())
                .unwrap_or_default();
            let recordings = self.stored_recordings(&slug, &fetched.album.tracks).await?;
            let matches = match_tracks(&known, &fetched.album.tracks, &recordings);
            for (track, matched) in fetched.album.tracks.iter().zip(matches) {
                let visible_track = match matched {
                    Some(stored) => {
                        let in_head = stored_detail.as_ref().is_some_and(|detail| {
                            detail.tracks.iter().any(|row| row.track.id == stored)
                        });
                        in_head || self.track_in_any_scope(scopes, stored).await?
                    }
                    None => false,
                };
                if let (Some(stored), true) = (matched, visible_track) {
                    if emitted.insert(stored) {
                        tracks.push(PageTrack::Stored(stored));
                    }
                    continue;
                }
                match catalog_id(&slug, EntityKind::Track, &track.id) {
                    Ok(id) => tracks.push(PageTrack::Catalog(Box::new(CatalogTrackResult {
                        id,
                        track: track.clone(),
                    }))),
                    Err(error) => tracing::debug!(
                        %error,
                        track = %track.id,
                        "a catalog track could not be named"
                    ),
                }
            }
        }
        if let Some(detail) = &stored_detail {
            for known in &detail.tracks {
                if emitted.insert(known.track.id) {
                    tracks.push(PageTrack::Stored(known.track.id));
                }
            }
        }
        Ok(Some(AlbumPage { head, tracks }))
    }

    /// Resolves a whole album page to stored track ids, materializing the
    /// catalog tracks it lists.
    ///
    /// This is what a playlist insert needs: the page decides which tracks the
    /// album has, and every catalog track is stored before its id is returned,
    /// so the inserted rows are the rows the server serves.
    ///
    /// # Parameters
    ///
    /// - `page`: the album page.
    ///
    /// # Returns
    ///
    /// The stored track ids, in the page's order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a catalog track cannot be materialized.
    pub async fn album_page_tracks(&self, page: &AlbumPage) -> Result<Vec<EntityId>, CatalogError> {
        let mut ids = Vec::with_capacity(page.tracks.len());
        for track in &page.tracks {
            match track {
                PageTrack::Stored(id) => ids.push(*id),
                PageTrack::Catalog(track) => {
                    ids.push(self.materialize_track(&track.id).await?);
                }
            }
        }
        Ok(ids)
    }
}

/// Pairs stored albums with the catalog albums they are the same release as.
///
/// A stored album that already carries a provider alias is skipped: the pair it
/// belongs to is known. The remaining stored albums are compared with the
/// candidate titles in stored order, an exact folded title first and an
/// edition-folded one second, and a candidate whose year agrees wins over one
/// whose year does not. A candidate whose year is two or more years away is not
/// the same release and is never paired.
///
/// # Parameters
///
/// - `stored`: the stored albums, in listing order.
/// - `aliased`: the stored albums that already carry an alias for the instance.
/// - `candidates`: the catalog albums to pair with, in provider order.
///
/// A catalog album one stored album already claimed is never paired again: the
/// claim the store holds is the one the page shows.
///
/// # Returns
///
/// One entry per pair: the candidate index and the stored album.
fn match_albums(
    stored: &[Album],
    aliased: &HashMap<EntityId, String>,
    candidates: &[Candidate<'_>],
) -> Vec<(usize, EntityId)> {
    // The candidate's index is its position in the provider's listing, which is
    // what the caller resolves the pair against; a filtered candidate list can
    // therefore carry indices the list itself does not cover.
    let mut taken: HashSet<usize> = HashSet::new();
    let mut pairs = Vec::new();
    for album in stored {
        if aliased.contains_key(&album.id) {
            continue;
        }
        let chosen = candidates
            .iter()
            .filter(|candidate| !taken.contains(&candidate.index))
            .filter(|candidate| years_fold(album.year, candidate.year))
            .filter_map(|candidate| {
                title_match(album, candidate.title).map(|strength| (strength, candidate))
            })
            .max_by_key(|(strength, candidate)| {
                (
                    *strength,
                    year_agrees(album.year, candidate.year),
                    std::cmp::Reverse(candidate.index),
                )
            });
        if let Some((_, candidate)) = chosen {
            taken.insert(candidate.index);
            pairs.push((candidate.index, album.id));
        }
    }
    pairs
}

/// Returns how strongly a stored album answers to a catalog title.
///
/// # Parameters
///
/// - `album`: the stored album.
/// - `title`: the catalog title.
///
/// # Returns
///
/// The match strength, or `None` when the titles do not agree.
pub(crate) fn title_match(album: &Album, title: &str) -> Option<TitleMatch> {
    let exact = normalize(&album.title);
    if !exact.is_empty() && exact == normalize(title) {
        return Some(TitleMatch::Exact);
    }
    let base = base_title(&album.title);
    if !base.is_empty() && base == base_title(title) {
        return Some(TitleMatch::Edition);
    }
    None
}

/// Reports whether two release years may name one album.
///
/// A remaster, a deluxe edition and a local tag disagree by the pressing, not by
/// the album, so a gap of one year still folds. Two years or more is a different
/// release, and a year nobody stated never separates two albums.
///
/// # Parameters
///
/// - `stored`: the stored album's year.
/// - `catalog`: the catalog album's year.
///
/// # Returns
///
/// `true` when the two may be one album.
pub(crate) fn years_fold(stored: Option<i64>, catalog: Option<u16>) -> bool {
    match (stored, catalog) {
        (Some(stored), Some(catalog)) => (stored - i64::from(catalog)).abs() <= 1,
        _ => true,
    }
}

/// Reports whether a stored year and a catalog year name the same release year.
///
/// # Parameters
///
/// - `stored`: the stored year.
/// - `catalog`: the catalog year.
///
/// # Returns
///
/// `true` when both are present and equal.
fn year_agrees(stored: Option<i64>, catalog: Option<u16>) -> bool {
    match (stored, catalog) {
        (Some(stored), Some(catalog)) => stored == i64::from(catalog),
        _ => false,
    }
}

/// Reads the release year out of an ISO 8601 date or date-time.
///
/// # Parameters
///
/// - `date`: the release date.
///
/// # Returns
///
/// The year, or `None` when the value does not start with four digits.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::pages::release_year;
///
/// assert_eq!(release_year(Some("2009-09-09")), Some(2009));
/// assert_eq!(release_year(Some("unknown")), None);
/// assert_eq!(release_year(None), None);
/// ```
pub fn release_year(date: Option<&str>) -> Option<u16> {
    let year = date?.get(..4)?;
    year.parse().ok()
}

/// Pairs the catalog tracks of one album with the stored tracks that are the
/// same recordings.
///
/// The recorded matches come first, then the same edition-folded title within
/// [`DURATION_TOLERANCE_MS`], then the same disc and track number within the
/// same tolerance. Every stored track is used at most once, so a provider that
/// lists one recording twice does not consume the row twice.
///
/// # Parameters
///
/// - `stored`: the stored tracks of the album, in playing order.
/// - `catalog`: the catalog tracks, in the provider's order.
/// - `recordings`: the stored row each catalog track already resolves to.
///
/// # Returns
///
/// One entry per catalog track, in order: the stored track it is, or `None`.
fn match_tracks(
    stored: &[&Track],
    catalog: &[CatalogTrack],
    recordings: &[Option<EntityId>],
) -> Vec<Option<EntityId>> {
    let mut used: HashSet<EntityId> = HashSet::new();
    let mut matches = Vec::with_capacity(catalog.len());
    for (index, track) in catalog.iter().enumerate() {
        if let Some(known) = recordings.get(index).copied().flatten()
            && used.insert(known)
        {
            matches.push(Some(known));
            continue;
        }
        let base = base_title(&track.title);
        let by_title = stored.iter().find(|candidate| {
            !used.contains(&candidate.id)
                && !base.is_empty()
                && base_title(&candidate.title) == base
                && duration_agrees(candidate.duration_ms, track.duration_ms)
        });
        let by_position = || {
            let disc = track.disc_number.map(i64::from);
            let number = track.track_number.map(i64::from);
            stored.iter().find(|candidate| {
                !used.contains(&candidate.id)
                    && disc.is_some()
                    && number.is_some()
                    && candidate.disc_number == disc
                    && candidate.track_number == number
                    && duration_agrees(candidate.duration_ms, track.duration_ms)
            })
        };
        let found = by_title.or_else(by_position);
        match found {
            Some(found) => {
                used.insert(found.id);
                matches.push(Some(found.id));
            }
            None => matches.push(None),
        }
    }
    matches
}

/// Reports whether a stored duration and a catalog duration name the same
/// recording length.
///
/// # Parameters
///
/// - `stored`: the stored duration in milliseconds.
/// - `catalog`: the catalog duration in milliseconds.
///
/// # Returns
///
/// `true` when both are present and within [`DURATION_TOLERANCE_MS`], and when
/// either is missing: a duration nobody stated is not a reason to refuse the
/// match.
fn duration_agrees(stored: Option<i64>, catalog: Option<u32>) -> bool {
    match (stored, catalog) {
        (Some(stored), Some(catalog)) => {
            (stored - i64::from(catalog)).abs() <= DURATION_TOLERANCE_MS
        }
        _ => true,
    }
}
