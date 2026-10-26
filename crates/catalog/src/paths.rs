//! Readable paths: one stable, human readable URL for every artist and album.
//!
//! A client that opens an artist or an album needs a URL it can show, type and
//! bookmark. Ids cannot serve that purpose: a stored row is named `ar_...` and a
//! catalog entity is named `x_<slug>_r_...`, so the address would change with
//! the plugins the administrator installed. A path is built from names instead,
//! and the id is what the path resolves to.
//!
//! Two forms exist:
//!
//! - an artist path is one segment, `/artist/radiohead`;
//! - an album path is the artist's segment and the album's, `/album/radiohead/
//!   in-rainbows`.
//!
//! [`segment`] folds a name into one URL segment. Two artists that share a name
//! are told apart by an ordinal suffix, `nirvana` and `nirvana--2`, and the same
//! rule applies to albums of one artist. [`CatalogService::resolve_artist_path`]
//! and [`CatalogService::resolve_album_path`] turn a path back into an id, and
//! [`CatalogService::path_of`] turns an id into its canonical path.
//!
//! ## What the ordinal means
//!
//! The bare segment belongs to the artist in the caller's library, or to the
//! catalog's best match when the library holds none. The remaining artists are
//! ordered by catalog rank. The same rule applies to the albums of one artist:
//! the bare segment belongs to the first album of that title in the artist
//! page's listing.

use std::collections::HashMap;

use hearthsound_core::providers::{CatalogQuery, credit_provider_id};
use hearthsound_core::{CatalogId, ClientId, EntityId, EntityKind, IdPrefix, Slug};
use hearthsound_library::query::{self, Scope};
use hearthsound_library::{albums, artists};

use crate::aliases::provider_namespace;
use crate::dedupe::normalize;
use crate::error::CatalogError;
use crate::pages::{PageAlbum, PageDepth};
use crate::service::{CatalogArtistResult, CatalogService};

/// The separator between a path segment and the ordinal that disambiguates it.
///
/// [`segment`] never produces two hyphens in a row, so the separator is
/// unambiguous.
pub const ORDINAL_SEPARATOR: &str = "--";

/// The segment of a name that folds to nothing at all.
///
/// A name made only of punctuation has no alphanumeric character to build a
/// segment from.
const EMPTY_SEGMENT: &str = "untitled";

/// Whether a path resolution may call a catalog provider.
///
/// A first-party path lookup must not spend provider quota: it resolves through
/// the library and the answers the catalog layers already cached, and the page
/// it opens asks for the rest through a `catalog.enrich` job. A native or internal
/// lookup resolves live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathMode {
    /// Pair by name and title, and call providers as needed.
    Live,
    /// Pair through recorded aliases and cached provider answers only.
    Cached,
}

/// Folds a name into one URL path segment.
///
/// The name is normalized the way deduplication folds a title (lowercase,
/// compatibility decomposition, no combining marks, every other character
/// turned into a space), and each space becomes a hyphen. A name that folds to
/// nothing keeps its own characters with the characters that would end a
/// segment (`/`, `?`, `#`, `%`) replaced by hyphens, so the band `!!!` becomes
/// `!!!` and `/` alone becomes [`EMPTY_SEGMENT`].
///
/// The function is idempotent: folding an already folded segment returns it
/// unchanged.
///
/// # Parameters
///
/// - `text`: the display name to fold.
///
/// # Returns
///
/// The segment: lowercase, non-empty, and free of `/`, `?`, `#` and `%`.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::paths::segment;
///
/// assert_eq!(segment("Radiohead"), "radiohead");
/// assert_eq!(segment("AC/DC"), "ac-dc");
/// assert_eq!(segment("Second Wind (Remastered 2009)"), "second-wind-remastered-2009");
/// assert_eq!(segment("!!!"), "!!!");
/// ```
pub fn segment(text: &str) -> String {
    let folded = normalize(text);
    if !folded.is_empty() {
        return folded.replace(' ', "-");
    }
    let mut out = String::with_capacity(text.len());
    let mut pending_dash = false;
    for character in text.trim().chars().flat_map(char::to_lowercase) {
        if character.is_whitespace() || matches!(character, '/' | '?' | '#' | '%') {
            pending_dash = !out.is_empty();
            continue;
        }
        if pending_dash {
            out.push('-');
            pending_dash = false;
        }
        out.push(character);
    }
    if out.is_empty() {
        return EMPTY_SEGMENT.to_owned();
    }
    out
}

/// Splits a path segment into its base and its ordinal.
///
/// A segment that ends in [`ORDINAL_SEPARATOR`] and at least two ASCII digits
/// carries an ordinal: `nirvana--2` is the second artist called Nirvana, which
/// is ordinal one. Every other segment is ordinal zero, including a segment
/// whose suffix is not a valid ordinal (`x--1` is the base `x-1`).
///
/// # Parameters
///
/// - `segment`: the path segment, folded or not.
///
/// # Returns
///
/// The folded base segment and the 0-based ordinal.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::paths::parse_segment;
///
/// assert_eq!(parse_segment("nirvana--2"), ("nirvana".to_owned(), 1));
/// assert_eq!(parse_segment("Radiohead"), ("radiohead".to_owned(), 0));
/// assert_eq!(parse_segment("blink-182"), ("blink-182".to_owned(), 0));
/// ```
pub fn parse_segment(value: &str) -> (String, usize) {
    let trimmed = value.trim();
    if let Some(position) = trimmed.rfind(ORDINAL_SEPARATOR) {
        let (head, suffix) = trimmed.split_at(position);
        let digits = &suffix[ORDINAL_SEPARATOR.len()..];
        if !digits.is_empty()
            && digits.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(number) = digits.parse::<usize>()
            && number >= 2
        {
            return (segment(head), number - 1);
        }
    }
    (segment(trimmed), 0)
}

/// Appends an ordinal to a base segment.
///
/// # Parameters
///
/// - `base`: the folded base segment.
/// - `ordinal`: the 0-based ordinal; zero adds no suffix.
///
/// # Returns
///
/// The segment, with the ordinal spelled as `base--{ordinal + 1}`.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::paths::with_ordinal;
///
/// assert_eq!(with_ordinal("nirvana", 0), "nirvana");
/// assert_eq!(with_ordinal("nirvana", 1), "nirvana--2");
/// ```
pub fn with_ordinal(base: &str, ordinal: usize) -> String {
    if ordinal == 0 {
        return base.to_owned();
    }
    format!("{base}{ORDINAL_SEPARATOR}{}", ordinal + 1)
}

/// Assigns one segment to each of a listing's titles.
///
/// The first title of one folded value gets the bare segment and the later ones
/// get `--2`, `--3`, ... in listing order.
///
/// # Parameters
///
/// - `titles`: the titles in listing order.
///
/// # Returns
///
/// One segment per title, in the same order.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::paths::assign_segments;
///
/// let segments = assign_segments(["Weezer", "Weezer", "Pinkerton"]);
/// assert_eq!(segments, ["weezer", "weezer--2", "pinkerton"]);
/// ```
pub fn assign_segments<'a>(titles: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    titles
        .into_iter()
        .map(|title| {
            let base = segment(title);
            let ordinal = seen.entry(base.clone()).or_insert(0);
            let assigned = with_ordinal(&base, *ordinal);
            *ordinal += 1;
            assigned
        })
        .collect()
}

/// The id one path resolves to, and the canonical path it spells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    /// The entity the path names: a stored row's id, or a catalog id.
    pub id: ClientId,
    /// Whether the path names an artist or an album.
    pub kind: EntityKind,
    /// The canonical path, with the ordinal the resolution established.
    pub path: String,
}

/// One artist a base segment may name.
///
/// A candidate is a stored artist, a catalog artist, or a stored artist together
/// with the catalog artist its name matches.
#[derive(Debug, Clone)]
struct ArtistCandidate {
    /// The stored row, when the caller's scopes hold one.
    stored: Option<EntityId>,
    /// The catalog artist the name resolves to.
    catalog: Option<CatalogArtistResult>,
}

impl ArtistCandidate {
    /// Returns the id to answer with: the stored row wins over the catalog
    /// entity, because a stored row is what the caller can see and play.
    fn client_id(&self) -> Option<ClientId> {
        match (self.stored, &self.catalog) {
            (Some(stored), _) => Some(ClientId::Entity(stored)),
            (None, Some(catalog)) => Some(ClientId::Catalog(catalog.id.clone())),
            (None, None) => None,
        }
    }

    /// Reports whether this candidate is the entity an id names.
    ///
    /// # Parameters
    ///
    /// - `identity`: the id to compare with, with its aliases.
    fn matches(&self, identity: &EntityIdentity) -> bool {
        if let Some(stored) = self.stored
            && identity.matches_entity(stored)
        {
            return true;
        }
        if let Some(catalog) = &self.catalog
            && identity.matches_catalog(&catalog.id)
        {
            return true;
        }
        false
    }
}

/// The ids one client id names, seen from both directions.
///
/// An entity is the same entity when its stored row is the id, when the id's
/// provider alias names its stored row, when its catalog id is the id, or when
/// the stored row's provider alias names its catalog id.
struct EntityIdentity {
    /// The stored row, when the id was an entity id.
    entity: Option<EntityId>,
    /// The catalog id, when the id was a catalog id.
    catalog: Option<CatalogId>,
    /// The catalog ids the stored row's provider aliases name.
    aliases: Vec<CatalogId>,
    /// The stored row the catalog id's provider alias names.
    alias_row: Option<EntityId>,
}

impl EntityIdentity {
    /// Reports whether an id is the entity this identity names.
    fn matches(&self, id: &ClientId) -> bool {
        match id {
            ClientId::Entity(entity) => self.matches_entity(*entity),
            ClientId::Catalog(catalog) => self.matches_catalog(catalog),
        }
    }

    /// Reports whether a stored row is the entity this identity names.
    fn matches_entity(&self, entity: EntityId) -> bool {
        self.entity == Some(entity) || self.alias_row == Some(entity)
    }

    /// Reports whether a catalog id is the entity this identity names.
    fn matches_catalog(&self, catalog: &CatalogId) -> bool {
        self.catalog.as_ref() == Some(catalog) || self.aliases.iter().any(|alias| alias == catalog)
    }
}

/// One album of an artist's listing, with the title its segment is built from.
struct ListingAlbum {
    /// The album's id, stored or catalog.
    id: ClientId,
    /// The album's title.
    title: String,
}

impl CatalogService {
    /// Resolves an artist path segment to the artist it names.
    ///
    /// # Parameters
    ///
    /// - `segment`: the path segment, folded or not.
    /// - `scopes`: the scopes the caller reads, in listing order.
    ///
    /// # Returns
    ///
    /// The artist and its canonical path, or `None` when no artist of the
    /// caller's library or the catalog answers to that segment.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read or an alias read fails. A
    /// catalog search that fails is logged and treated as no catalog candidate.
    pub async fn resolve_artist_path(
        &self,
        segment: &str,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Option<ResolvedPath>, CatalogError> {
        let (base, ordinal) = parse_segment(segment);
        let candidates = self.artist_candidates(&base, scopes, mode).await?;
        let Some(candidate) = candidates.get(ordinal) else {
            return Ok(None);
        };
        let Some(id) = candidate.client_id() else {
            return Ok(None);
        };
        Ok(Some(ResolvedPath {
            id,
            kind: EntityKind::Artist,
            path: with_ordinal(&base, ordinal),
        }))
    }

    /// Resolves an album path to the album it names.
    ///
    /// The album segment is matched against the artist page's listing, in the
    /// order the page shows it, so the ordinal of an album agrees with the one
    /// its card links to. An album the listing does not hold is searched for in
    /// the catalog when the artist is a catalog artist.
    ///
    /// # Parameters
    ///
    /// - `artist_segment`: the artist path segment.
    /// - `album_segment`: the album path segment.
    /// - `scopes`: the scopes the caller reads, in listing order.
    ///
    /// # Returns
    ///
    /// The album and its canonical path, or `None` when nothing answers to the
    /// pair.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read or an alias read fails.
    pub async fn resolve_album_path(
        &self,
        artist_segment: &str,
        album_segment: &str,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Option<ResolvedPath>, CatalogError> {
        let (base, ordinal) = parse_segment(album_segment);
        let (artist_base, _) = parse_segment(artist_segment);
        let Some(artist) = self.resolve_artist_path(artist_segment, scopes, mode).await? else {
            return Ok(None);
        };
        let listing = self.album_listing(&artist.id, scopes, mode).await?;
        let segments = assign_segments(listing.iter().map(|entry| entry.title.as_str()));
        let wanted = with_ordinal(&base, ordinal);
        if let Some(index) = segments.iter().position(|assigned| *assigned == wanted) {
            return Ok(Some(ResolvedPath {
                id: listing[index].id.clone(),
                kind: EntityKind::Album,
                path: format!("{}/{}", artist.path, wanted),
            }));
        }
        let query = CatalogQuery {
            text: format!("{} {}", words(&artist_base), words(&base)),
            kinds: vec![EntityKind::Album],
            limit: 10,
            offset: 0,
        };
        let found = match self.search_for_path(&query, mode).await {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::debug!(
                    %error,
                    artist = %artist_base,
                    album = %base,
                    "the catalog could not be searched for an album path"
                );
                return Ok(None);
            }
        };
        let matching: Vec<_> = found
            .albums
            .into_iter()
            .filter(|hit| segment(&hit.album.title) == base)
            .filter(|hit| {
                hit.album
                    .artists
                    .first()
                    .is_some_and(|credit| segment(&credit.name) == artist_base)
            })
            .collect();
        let Some(hit) = matching.get(ordinal) else {
            return Ok(None);
        };
        Ok(Some(ResolvedPath {
            id: ClientId::Catalog(hit.id.clone()),
            kind: EntityKind::Album,
            path: format!("{}/{}", artist.path, wanted),
        }))
    }

    /// Returns the canonical path of an artist or an album.
    ///
    /// # Parameters
    ///
    /// - `id`: the stored id or the catalog id of the entity.
    /// - `scopes`: the scopes the caller reads, in listing order.
    ///
    /// # Returns
    ///
    /// The entity and its canonical path, or `None` when the id names another
    /// kind of entity or no longer exists. An entity the candidates do not hold
    /// keeps its own id and a path built from its name alone.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read or an alias read fails.
    pub async fn path_of(
        &self,
        id: &ClientId,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Option<ResolvedPath>, CatalogError> {
        match kind_of(id) {
            Some(EntityKind::Artist) => self.artist_path_of(id, scopes, mode).await,
            Some(EntityKind::Album) => self.album_path_of(id, scopes, mode).await,
            _ => Ok(None),
        }
    }

    /// Returns the canonical path of an artist.
    ///
    /// # Parameters
    ///
    /// - `id`: the stored id or the catalog id of the artist.
    /// - `scopes`: the scopes the caller reads, in listing order.
    async fn artist_path_of(
        &self,
        id: &ClientId,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Option<ResolvedPath>, CatalogError> {
        let name = match id {
            ClientId::Entity(entity) => {
                let mut conn = self.database().readers().acquire().await?;
                match artists::get_artist(&mut conn, *entity).await {
                    Ok(Some(artist)) => artist.name,
                    Ok(None) => return Ok(None),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            artist = %entity,
                            "a stored artist could not be read for its path"
                        );
                        return Ok(None);
                    }
                }
            }
            ClientId::Catalog(catalog) => match self.artist_at(catalog, mode).await {
                Ok(found) => found.artist.name,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        artist = %catalog,
                        "a catalog artist could not be read for its path"
                    );
                    return Ok(None);
                }
            },
        };
        let base = segment(&name);
        let candidates = self.artist_candidates(&base, scopes, mode).await?;
        let identity = self.identity_of(id, EntityKind::Artist).await?;
        let Some(index) = candidates
            .iter()
            .position(|candidate| candidate.matches(&identity))
        else {
            return Ok(Some(ResolvedPath {
                id: id.clone(),
                kind: EntityKind::Artist,
                path: base,
            }));
        };
        let Some(resolved) = candidates[index].client_id() else {
            return Ok(None);
        };
        Ok(Some(ResolvedPath {
            id: resolved,
            kind: EntityKind::Artist,
            path: with_ordinal(&base, index),
        }))
    }

    /// Returns the canonical path of an album.
    ///
    /// # Parameters
    ///
    /// - `id`: the stored id or the catalog id of the album.
    /// - `scopes`: the scopes the caller reads, in listing order.
    async fn album_path_of(
        &self,
        id: &ClientId,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Option<ResolvedPath>, CatalogError> {
        let (title, artist, artist_segment) = match id {
            ClientId::Entity(entity) => {
                let mut conn = self.database().readers().acquire().await?;
                let Some(album) = albums::get_album(&mut conn, *entity).await? else {
                    return Ok(None);
                };
                let credits = artists::list_album_artists(&mut conn, *entity).await?;
                let artist = credits.first().map(|credit| ClientId::Entity(credit.id));
                let artist_segment = credits.first().map(|credit| segment(&credit.name));
                (album.title, artist, artist_segment)
            }
            ClientId::Catalog(catalog) => {
                let found = match self.album_at(catalog, mode).await {
                    Ok(found) => found,
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            album = %catalog,
                            "a catalog album could not be read for its path"
                        );
                        return Ok(None);
                    }
                };
                let credit = found.album.artists.first();
                let artist = credit
                    .and_then(credit_provider_id)
                    .filter(|external| external.namespace == catalog.slug().as_str())
                    .and_then(|external| {
                        CatalogId::new(
                            catalog.slug().clone(),
                            EntityKind::Artist,
                            external.value.clone(),
                        )
                        .ok()
                    })
                    .map(ClientId::Catalog);
                let artist_segment = credit.map(|credit| segment(&credit.name));
                (found.album.title, artist, artist_segment)
            }
        };
        let Some(artist_segment) = artist_segment else {
            return Ok(None);
        };
        if let Some(artist) = artist
            && let Some(artist_path) = self.artist_path_of(&artist, scopes, mode).await?
        {
            let listing = self.album_listing(&artist_path.id, scopes, mode).await?;
            let segments = assign_segments(listing.iter().map(|entry| entry.title.as_str()));
            let identity = self.identity_of(id, EntityKind::Album).await?;
            if let Some(index) = listing.iter().position(|entry| identity.matches(&entry.id)) {
                return Ok(Some(ResolvedPath {
                    id: listing[index].id.clone(),
                    kind: EntityKind::Album,
                    path: format!("{}/{}", artist_path.path, segments[index]),
                }));
            }
            return Ok(Some(ResolvedPath {
                id: id.clone(),
                kind: EntityKind::Album,
                path: format!("{}/{}", artist_path.path, segment(&title)),
            }));
        }
        Ok(Some(ResolvedPath {
            id: id.clone(),
            kind: EntityKind::Album,
            path: format!("{}/{}", artist_segment, segment(&title)),
        }))
    }

    /// Lists the albums an artist page shows, with their titles.
    ///
    /// The page's listing order is what assigns the ordinals, so this is the
    /// same read the client does.
    ///
    /// # Parameters
    ///
    /// - `artist`: the artist's id, stored or catalog.
    /// - `scopes`: the scopes the caller reads, in listing order.
    async fn album_listing(
        &self,
        artist: &ClientId,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Vec<ListingAlbum>, CatalogError> {
        let depth = match mode {
            PathMode::Cached => PageDepth::Cached,
            PathMode::Live => PageDepth::Full,
        };
        let Some(page) = self.artist_page(artist, scopes, depth).await? else {
            return Ok(Vec::new());
        };
        let stored_ids: Vec<EntityId> = page
            .albums
            .iter()
            .filter_map(|album| match album {
                PageAlbum::Stored(id) => Some(*id),
                PageAlbum::Catalog(_) => None,
            })
            .collect();
        let mut titles = HashMap::new();
        if !stored_ids.is_empty() {
            let mut conn = self.database().readers().acquire().await?;
            for id in stored_ids {
                match albums::get_album(&mut conn, id).await {
                    Ok(Some(album)) => {
                        titles.insert(id, album.title);
                    }
                    Ok(None) => {}
                    Err(error) => tracing::debug!(
                        %error,
                        album = %id,
                        "a stored album title could not be read for its path"
                    ),
                }
            }
        }
        Ok(page
            .albums
            .into_iter()
            .filter_map(|album| match album {
                PageAlbum::Stored(id) => titles.remove(&id).map(|title| ListingAlbum {
                    id: ClientId::Entity(id),
                    title,
                }),
                PageAlbum::Catalog(reference) => Some(ListingAlbum {
                    id: ClientId::Catalog(reference.id),
                    title: reference.album.title,
                }),
            })
            .collect())
    }

    /// Lists the artists a base segment may name, in path order.
    ///
    /// The stored artists of the caller's scopes come first, in creation order,
    /// each paired with the catalog artist its name matches; the catalog artists
    /// no stored row is paired with follow, in provider rank order.
    ///
    /// # Parameters
    ///
    /// - `base`: the folded base segment.
    /// - `scopes`: the scopes the caller reads, in listing order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a stored read or an alias read fails.
    async fn artist_candidates(
        &self,
        base: &str,
        scopes: &[Scope],
        mode: PathMode,
    ) -> Result<Vec<ArtistCandidate>, CatalogError> {
        let mut candidates: Vec<ArtistCandidate> = Vec::new();
        let mut stored: Vec<_> = {
            let mut conn = self.database().readers().acquire().await?;
            artists::list_artists(&mut conn).await?
        }
        .into_iter()
        .filter(|artist| segment(&artist.name) == base)
        .collect();
        stored.sort_by_key(|artist| (artist.created_at, artist.id.uuid()));
        if !stored.is_empty() {
            let mut conn = self.database().readers().acquire().await?;
            for artist in stored {
                let mut visible = false;
                for scope in scopes {
                    if query::artist_in_scope(&mut conn, *scope, artist.id).await? {
                        visible = true;
                        break;
                    }
                }
                if visible {
                    candidates.push(ArtistCandidate {
                        stored: Some(artist.id),
                        catalog: None,
                    });
                }
            }
        }
        let query = CatalogQuery {
            text: words(base),
            kinds: vec![EntityKind::Artist],
            limit: 10,
            offset: 0,
        };
        let hits: Vec<CatalogArtistResult> = match self.search_for_path(&query, mode).await {
            Ok(Some(found)) => found
                .artists
                .into_iter()
                .filter(|hit| segment(&hit.artist.name) == base)
                .collect(),
            Ok(None) => return Ok(candidates),
            Err(error) => {
                tracing::debug!(
                    %error,
                    base,
                    "the catalog could not be searched for an artist path"
                );
                return Ok(candidates);
            }
        };
        if hits.is_empty() {
            return Ok(candidates);
        }
        let mut claimed: HashMap<EntityId, Vec<Slug>> = HashMap::new();
        for candidate in &candidates {
            if let Some(stored) = candidate.stored {
                let slugs = self
                    .provider_aliases(stored)
                    .await?
                    .into_iter()
                    .map(|alias| alias.slug)
                    .collect();
                claimed.insert(stored, slugs);
            }
        }
        for hit in hits {
            let aliased = self
                .entity_by_alias(
                    &provider_namespace(hit.id.slug()),
                    EntityKind::Artist,
                    hit.id.provider_id(),
                )
                .await?;
            let placed = match aliased {
                Some(row) => candidates.iter().position(|candidate| {
                    candidate.stored == Some(row) && candidate.catalog.is_none()
                }),
                None => candidates
                    .iter()
                    .position(|candidate| match candidate.stored {
                        Some(stored) => {
                            candidate.catalog.is_none()
                                && !claimed
                                    .get(&stored)
                                    .is_some_and(|slugs| slugs.contains(hit.id.slug()))
                        }
                        None => false,
                    }),
            };
            match placed {
                Some(index) => candidates[index].catalog = Some(hit),
                None => candidates.push(ArtistCandidate {
                    stored: None,
                    catalog: Some(hit),
                }),
            }
        }
        Ok(candidates)
    }

    /// Searches the catalog for a path, using the cache at the cached mode.
    ///
    /// # Parameters
    ///
    /// - `query`: the query.
    /// - `mode`: whether a provider may be called.
    ///
    /// # Returns
    ///
    /// The answer, or `None` when the cached mode has none.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the live search fails or a setting cannot be
    /// read.
    async fn search_for_path(
        &self,
        query: &CatalogQuery,
        mode: PathMode,
    ) -> Result<Option<crate::service::CatalogSearch>, CatalogError> {
        match mode {
            PathMode::Cached => self.cached_search(query).await,
            PathMode::Live => self.search(query).await.map(Some),
        }
    }

    /// Reads one catalog artist at the mode the caller asked for.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of the artist.
    /// - `mode`: whether a provider may be called.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the cached mode has no answer for
    /// it, and whatever a live read returns.
    async fn artist_at(
        &self,
        id: &CatalogId,
        mode: PathMode,
    ) -> Result<CatalogArtistResult, CatalogError> {
        match mode {
            PathMode::Cached => self.cached_artist(id).await?.ok_or(CatalogError::NotFound),
            PathMode::Live => self.artist(id).await,
        }
    }

    /// Reads one catalog album at the mode the caller asked for.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of the album.
    /// - `mode`: whether a provider may be called.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the cached mode has no answer for
    /// it, and whatever a live read returns.
    async fn album_at(
        &self,
        id: &CatalogId,
        mode: PathMode,
    ) -> Result<crate::service::CatalogAlbumResult, CatalogError> {
        if mode == PathMode::Live {
            return self.album(id).await;
        }
        if let Some(album) = self.cached_album(id).await? {
            return Ok(album);
        }
        // A path needs the title and the credits, which the reference a search or
        // an artist listing promoted already carries; the tracklist is not part
        // of a path.
        let reference = self.cached_album_ref(id).await?.ok_or(CatalogError::NotFound)?;
        Ok(crate::service::CatalogAlbumResult {
            id: reference.id,
            album: hearthsound_core::providers::CatalogAlbum {
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
        })
    }

    /// Reads the ids an entity id names, from both directions.
    ///
    /// # Parameters
    ///
    /// - `id`: the stored id or the catalog id.
    /// - `kind`: the entity kind the alias names.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when an alias read fails.
    async fn identity_of(
        &self,
        id: &ClientId,
        kind: EntityKind,
    ) -> Result<EntityIdentity, CatalogError> {
        let mut identity = EntityIdentity {
            entity: None,
            catalog: None,
            aliases: Vec::new(),
            alias_row: None,
        };
        match id {
            ClientId::Entity(entity) => {
                identity.entity = Some(*entity);
                for alias in self.provider_aliases(*entity).await? {
                    if let Ok(catalog) = CatalogId::new(alias.slug, kind, alias.provider_id) {
                        identity.aliases.push(catalog);
                    }
                }
            }
            ClientId::Catalog(catalog) => {
                identity.catalog = Some(catalog.clone());
                identity.alias_row = self
                    .entity_by_alias(
                        &provider_namespace(catalog.slug()),
                        kind,
                        catalog.provider_id(),
                    )
                    .await?;
            }
        }
        Ok(identity)
    }
}

/// Returns the kind of entity an id names, when it names a browsable one.
///
/// # Parameters
///
/// - `id`: the stored id or the catalog id.
fn kind_of(id: &ClientId) -> Option<EntityKind> {
    match id {
        ClientId::Entity(entity) => match entity.kind() {
            IdPrefix::Artist => Some(EntityKind::Artist),
            IdPrefix::Album => Some(EntityKind::Album),
            _ => None,
        },
        ClientId::Catalog(catalog) => Some(catalog.kind()),
    }
}

/// Turns a folded base segment back into search words.
///
/// # Parameters
///
/// - `base`: the folded base segment.
///
/// # Returns
///
/// The base with each hyphen replaced by a space.
fn words(base: &str) -> String {
    base.replace('-', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_folds_a_name() {
        assert_eq!(segment("Radiohead"), "radiohead");
        assert_eq!(segment("Sigur R\u{f3}s"), "sigur-ros");
        assert_eq!(segment("AC/DC"), "ac-dc");
        assert_eq!(
            segment("Second Wind (Remastered 2009)"),
            "second-wind-remastered-2009"
        );
        assert_eq!(segment("  Caf\u{e9}   del Mar "), "cafe-del-mar");
        assert_eq!(segment("!!!"), "!!!");
        assert_eq!(segment("   "), EMPTY_SEGMENT);
        assert_eq!(segment(""), EMPTY_SEGMENT);
        assert_eq!(segment("//"), EMPTY_SEGMENT);
    }

    #[test]
    fn a_segment_is_idempotent() {
        for name in [
            "Radiohead",
            "Sigur R\u{f3}s",
            "AC/DC",
            "Second Wind (Remastered 2009)",
            "!!!",
            "   ",
            "blink-182",
        ] {
            let once = segment(name);
            assert_eq!(segment(&once), once, "{name}");
        }
    }

    #[test]
    fn an_ordinal_suffix_is_parsed() {
        assert_eq!(parse_segment("nirvana--2"), ("nirvana".to_owned(), 1));
        assert_eq!(parse_segment("Nirvana--3"), ("nirvana".to_owned(), 2));
        assert_eq!(parse_segment("Radiohead"), ("radiohead".to_owned(), 0));
        assert_eq!(parse_segment("blink-182"), ("blink-182".to_owned(), 0));
        assert_eq!(parse_segment("x--1"), ("x-1".to_owned(), 0));
        assert_eq!(parse_segment("x--"), ("x".to_owned(), 0));
        assert_eq!(parse_segment("x--a"), ("x-a".to_owned(), 0));
    }

    #[test]
    fn an_ordinal_suffix_round_trips() {
        for ordinal in 0..=3 {
            let assigned = with_ordinal("nirvana", ordinal);
            assert_eq!(parse_segment(&assigned), ("nirvana".to_owned(), ordinal));
        }
    }

    #[test]
    fn a_listing_assigns_one_segment_per_title() {
        assert_eq!(
            assign_segments(["Weezer", "Weezer", "Pinkerton"]),
            vec!["weezer", "weezer--2", "pinkerton"]
        );
        assert_eq!(
            assign_segments([
                "Third Single",
                "First Light",
                "Second Wind (Remastered 2009)"
            ]),
            vec!["third-single", "first-light", "second-wind-remastered-2009"]
        );
        assert!(assign_segments(Vec::<&str>::new()).is_empty());
    }

    #[test]
    fn an_id_names_its_kind() {
        let artist = EntityId::new(IdPrefix::Artist);
        assert_eq!(kind_of(&ClientId::Entity(artist)), Some(EntityKind::Artist));
        let track = EntityId::new(IdPrefix::Track);
        assert_eq!(kind_of(&ClientId::Entity(track)), None);
        let slug = Slug::new("deezer").unwrap();
        let album = CatalogId::new(slug, EntityKind::Album, "1").unwrap();
        assert_eq!(kind_of(&ClientId::Catalog(album)), Some(EntityKind::Album));
    }
}
