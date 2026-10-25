//! Deduplication of catalog results against what the server already stores.
//!
//! A search returns local results first and catalog results after them, and the
//! two lists can describe the same recording: a track the server acquired and a
//! catalog plugin that also knows it. The user must not see it twice, so a
//! catalog track whose recording is already stored is dropped and reported
//! instead, which is what lets the API layer answer with the stored track's id.
//!
//! The rules a dropped catalog track matches are the ones the design fixes
//! (02-architecture.md, flow F1):
//!
//! 1. the same ISRC, compared after trimming and case folding;
//! 2. an existing `p:<slug>` alias in `external_ids`, which means the entity was
//!    already materialized and keeps resolving to its row;
//! 3. the same normalized title and artist with a duration within
//!    [`DURATION_TOLERANCE_MS`] of the local track's.
//!
//! The third rule exists because the same recording is routinely listed with
//! slightly different titles and durations by different providers; three seconds
//! is wider than any rounding a provider applies to a track length and narrower
//! than the gap to a different version of the same song.
//!
//! Albums and artists are reported when they are already materialized but never
//! dropped: a stored album and a catalog album of the same name may legitimately
//! be different releases, whereas two rows for one recording are always a
//! duplicate.
//!
//! # Examples
//!
//! ```
//! use hearthsound_catalog::dedupe::{LocalTrackHit, normalize, normalize_isrc};
//! use hearthsound_core::{EntityId, IdPrefix};
//!
//! let hit = LocalTrackHit {
//!     track_id: EntityId::new(IdPrefix::Track),
//!     isrc: Some(" usrc17607839 ".to_owned()),
//!     title: "Cafe del Mar".to_owned(),
//!     artist: Some("Energy 52".to_owned()),
//!     duration_ms: Some(180_000),
//! };
//! assert_eq!(normalize(&hit.title), "cafe del mar");
//! assert_eq!(normalize_isrc(hit.isrc.as_deref()).as_deref(), Some("USRC17607839"));
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use hearthsound_core::providers::CatalogTrack;
use hearthsound_core::{CatalogId, EntityId, EntityKind, Slug};
use hearthsound_library::{Album, albums, artists, tracks};
use sqlx::SqliteConnection;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::aliases;
use crate::error::CatalogError;
use crate::materialize::{ISRC, MB_RECORDING, canonical_id};
use crate::pages::title_match;
use crate::service::{
    CatalogAlbumRefResult, CatalogArtistResult, CatalogSearch, CatalogService, CatalogTrackResult,
};

/// How far a catalog duration may differ from a local one and still count as the
/// same recording.
///
/// Three seconds, in milliseconds.
pub const DURATION_TOLERANCE_MS: i64 = 3_000;

/// The words that name an edition of a release rather than a title.
///
/// A scanned album and the provider's album of it often differ by the edition
/// in a bracket or after a dash: "Abbey Road" against "Abbey Road (Remastered
/// 2009)". A bracket or a trailing segment that contains one of these words is
/// edition information, and [`base_title`] drops it to compare the titles.
///
/// The list is deliberately short: a word that appears in real titles would
/// fold two different releases together. "Live", "Acoustic" and "Version"
/// therefore stay out, because "Live at Leeds" and "Live at Wembley" are two
/// albums.
pub const EDITION_WORDS: [&str; 8] = [
    "remaster",
    "deluxe",
    "edition",
    "anniversary",
    "mono",
    "stereo",
    "expanded",
    "bonus",
];

/// Folds a title down to the part that names the work rather than its edition.
///
/// The edition suffixes a provider and a scanner disagree about are dropped
/// before the text is folded: a trailing bracket group whose content contains
/// one of [`EDITION_WORDS`], and a trailing ` - ` segment whose remainder does
/// so. The rest of the folded text is returned, normalized by [`normalize`].
///
/// # Parameters
///
/// - `text`: the display title.
///
/// # Returns
///
/// The normalized title without its edition suffixes, which is what
/// [`normalize`] returns when the title carries none.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::dedupe::base_title;
///
/// assert_eq!(base_title("Abbey Road (Remastered 2009)"), "abbey road");
/// assert_eq!(base_title("Come Together - Remastered 2009"), "come together");
/// assert_eq!(base_title("Song (Live)"), "song live");
/// ```
pub fn base_title(text: &str) -> String {
    let mut current = text.to_owned();
    loop {
        let trimmed = current.trim_end().to_owned();
        let mut shortened: Option<String> = None;
        if let Some(last) = trimmed.chars().last() {
            let opener = match last {
                ')' => Some('('),
                ']' => Some('['),
                _ => None,
            };
            if let Some(open) = opener
                && let Some(position) = trimmed.rfind(open)
            {
                let content = &trimmed[position + open.len_utf8()..trimmed.len() - last.len_utf8()];
                if carries_edition_word(content) {
                    shortened = Some(trimmed[..position].to_owned());
                }
            }
        }
        if shortened.is_none()
            && let Some(position) = trimmed.rfind(" - ")
            && carries_edition_word(&trimmed[position + 3..])
        {
            shortened = Some(trimmed[..position].to_owned());
        }
        match shortened {
            Some(candidate) => current = candidate,
            None => return normalize(&trimmed),
        }
    }
}

/// Reports whether a piece of text names an edition.
///
/// # Parameters
///
/// - `text`: the bracket content or the trailing segment of a title.
///
/// # Returns
///
/// `true` when the lowercased text contains one of [`EDITION_WORDS`].
fn carries_edition_word(text: &str) -> bool {
    let folded = text.to_lowercase();
    EDITION_WORDS.iter().any(|word| folded.contains(word))
}

/// One local track, reduced to the fields deduplication compares.
///
/// The caller builds one per local search hit from the track view the library
/// queries return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTrackHit {
    /// The stored track's client id.
    pub track_id: EntityId,
    /// The stored track's ISRC, when it has one.
    pub isrc: Option<String>,
    /// The stored track's title.
    pub title: String,
    /// The stored track's display artist, when it has one.
    pub artist: Option<String>,
    /// The stored track's duration in milliseconds, when known.
    pub duration_ms: Option<i64>,
}

/// A catalog entity that already names a stored row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedEntity {
    /// The catalog id the provider issued.
    pub catalog_id: CatalogId,
    /// The row the entity resolves to.
    pub entity_id: EntityId,
}

/// What deduplication did to one catalog search result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupedCatalog {
    /// The catalog tracks that are not stored yet, in the provider's order.
    pub tracks: Vec<CatalogTrackResult>,
    /// The catalog albums, in the provider's order.
    ///
    /// An album is never dropped; a caller that wants to hide one whose id is in
    /// [`DedupedCatalog::materialized`] can, and a caller that wants the more
    /// complete catalog view keeps it.
    pub albums: Vec<CatalogAlbumRefResult>,
    /// The catalog artists, in the provider's order.
    ///
    /// Never dropped, for the same reason as albums.
    pub artists: Vec<CatalogArtistResult>,
    /// The catalog entities that already resolve to a stored row, tracks
    /// included: every dropped track is listed here.
    pub materialized: Vec<MaterializedEntity>,
}

impl DedupedCatalog {
    /// Returns whether no catalog entity is left.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::DedupedCatalog;
    ///
    /// assert!(DedupedCatalog::default().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.albums.is_empty() && self.artists.is_empty()
    }

    /// Returns the stored row a catalog id resolves to, when it has one.
    ///
    /// # Parameters
    ///
    /// - `catalog_id`: the id to resolve.
    ///
    /// # Returns
    ///
    /// The stored entity the id already names, or `None` when the catalog entity
    /// is not materialized.
    ///
    /// # Examples
    ///
    /// ```
    /// use hearthsound_catalog::DedupedCatalog;
    /// use hearthsound_core::{CatalogId, EntityKind, Slug};
    ///
    /// let catalog_id = CatalogId::new(Slug::new("deezer").unwrap(), EntityKind::Track, "1").unwrap();
    /// let deduped = DedupedCatalog::default();
    /// assert_eq!(deduped.stored_entity(&catalog_id), None);
    /// ```
    pub fn stored_entity(&self, catalog_id: &CatalogId) -> Option<EntityId> {
        self.materialized
            .iter()
            .find(|entity| &entity.catalog_id == catalog_id)
            .map(|entity| entity.entity_id)
    }
}

impl CatalogService {
    /// Drops the catalog tracks the server already stores.
    ///
    /// The alias table is asked once per provider instance and entity kind, and
    /// once for the ISRCs of the catalog tracks, so a search costs a handful of
    /// statements rather than one per result.
    ///
    /// # Parameters
    ///
    /// - `local`: one hit per local track the same search returned.
    /// - `search`: the catalog half of the response.
    ///
    /// # Returns
    ///
    /// The catalog tracks that are not stored yet, the albums and artists to
    /// append, and every catalog entity that already resolves to a stored row.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when an alias lookup fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    pub async fn deduplicate(
        &self,
        local: &[LocalTrackHit],
        search: &CatalogSearch,
    ) -> Result<DedupedCatalog, CatalogError> {
        let provider_aliases = self.provider_alias_index(search).await?;
        let isrc_aliases = aliases::isrc_aliases(self.database(), &catalog_isrcs(search)).await?;

        let mut local_isrcs: HashMap<String, EntityId> = HashMap::new();
        for hit in local {
            if let Some(isrc) = normalize_isrc(hit.isrc.as_deref()) {
                local_isrcs.entry(isrc).or_insert(hit.track_id);
            }
        }

        let mut deduped = DedupedCatalog {
            albums: search.albums.clone(),
            artists: search.artists.clone(),
            ..DedupedCatalog::default()
        };

        for track in &search.tracks {
            let slug = track.id.slug();
            let stored = provider_aliases
                .get(&(slug.as_str().to_owned(), track.id.provider_id().to_owned()))
                .copied()
                .or_else(|| {
                    normalize_isrc(track.track.isrc.as_deref())
                        .and_then(|isrc| local_isrcs.get(&isrc).copied())
                })
                .or_else(|| {
                    normalize_isrc(track.track.isrc.as_deref())
                        .and_then(|isrc| isrc_aliases.get(&isrc).copied())
                })
                .or_else(|| matching_local_track(track, local));
            match stored {
                Some(entity_id) => deduped.materialized.push(MaterializedEntity {
                    catalog_id: track.id.clone(),
                    entity_id,
                }),
                None => deduped.tracks.push(track.clone()),
            }
        }

        for catalog_id in &albums_and_artists(search) {
            let key = (
                catalog_id.slug().as_str().to_owned(),
                catalog_id.provider_id().to_owned(),
            );
            if let Some(entity_id) = provider_aliases.get(&key) {
                let already = deduped
                    .materialized
                    .iter()
                    .any(|entity| &entity.catalog_id == catalog_id);
                if !already {
                    deduped.materialized.push(MaterializedEntity {
                        catalog_id: catalog_id.clone(),
                        entity_id: *entity_id,
                    });
                }
            }
        }

        Ok(deduped)
    }

    /// Finds the stored recording each catalog track names, if any.
    ///
    /// A browse view uses this to show a catalog album or song as the row the
    /// server already holds: an entity whose recording is stored is not offered
    /// as a remote duplicate. The rules are the ones [`CatalogService::deduplicate`]
    /// applies to a search, extended with the stored album title and the
    /// MusicBrainz recording id so a scanned file without an ISRC still matches.
    ///
    /// # Parameters
    ///
    /// - `slug`: the instance that issued the tracks.
    /// - `tracks`: the catalog tracks to look up.
    ///
    /// # Returns
    ///
    /// One entry per track, in order: the stored row that already holds the
    /// recording, or `None` when the server has no row for it.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when a read fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    pub async fn stored_recordings(
        &self,
        slug: &Slug,
        tracks: &[CatalogTrack],
    ) -> Result<Vec<Option<EntityId>>, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        let mut found = Vec::with_capacity(tracks.len());
        for track in tracks {
            found.push(stored_recording(&mut conn, slug, track).await?);
        }
        Ok(found)
    }

    /// Reads the provider aliases of every entity one search returned.
    ///
    /// # Parameters
    ///
    /// - `search`: the catalog search result.
    ///
    /// # Returns
    ///
    /// One entry per provider id that has an alias, keyed by the instance slug
    /// and the provider id.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when a lookup fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    async fn provider_alias_index(
        &self,
        search: &CatalogSearch,
    ) -> Result<HashMap<(String, String), EntityId>, CatalogError> {
        let mut grouped: BTreeMap<(String, EntityKind), Vec<String>> = BTreeMap::new();
        for track in &search.tracks {
            grouped
                .entry((track.id.slug().as_str().to_owned(), EntityKind::Track))
                .or_default()
                .push(track.id.provider_id().to_owned());
        }
        for (catalog_id, kind) in album_and_artist_kinds(search) {
            grouped
                .entry((catalog_id.slug().as_str().to_owned(), kind))
                .or_default()
                .push(catalog_id.provider_id().to_owned());
        }

        let mut index = HashMap::new();
        for ((slug, kind), values) in grouped {
            let Ok(slug) = Slug::new(&slug) else {
                continue;
            };
            let found = aliases::provider_aliases(self.database(), &slug, kind, &values).await?;
            for (value, entity) in found {
                index.insert((slug.as_str().to_owned(), value), entity);
            }
        }
        Ok(index)
    }

    /// Finds the catalog artists and albums that name an entity already stored.
    ///
    /// A search shows the stored artists and albums first and the catalog's
    /// after them, and the two lists may name the same act or release when the
    /// two have never been paired: a scanned "Radiohead" and the catalog's
    /// "Radiohead" are one entry to a user, not two. This is the name-based
    /// pairing a browse page already uses, applied to a search result without
    /// recording anything, so a search never writes.
    ///
    /// A stored row that already carries an alias for the answering instance is
    /// skipped: the alias table has settled which provider entity it is, and a
    /// second catalog hit of the same name is a different entity.
    ///
    /// # Parameters
    ///
    /// - `stored_artists`: the matched stored artists, as their id and name, in
    ///   match order.
    /// - `stored_albums`: the matched stored album ids, in match order.
    /// - `search`: the catalog half of the response.
    ///
    /// # Returns
    ///
    /// The catalog ids to leave out of the response. A catalog id is claimed
    /// once, by the first stored entity whose name it agrees with.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when a read fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    pub async fn name_twins(
        &self,
        stored_artists: &[(EntityId, String)],
        stored_albums: &[EntityId],
        search: &CatalogSearch,
    ) -> Result<HashSet<CatalogId>, CatalogError> {
        let mut twins = HashSet::new();
        if search.artists.is_empty() && search.albums.is_empty() {
            return Ok(twins);
        }
        let aliases = self.provider_alias_index(search).await?;
        let instances: HashSet<Slug> = search
            .artists
            .iter()
            .map(|hit| hit.id.slug().clone())
            .chain(search.albums.iter().map(|hit| hit.id.slug().clone()))
            .collect();

        let mut claimed_artists = vec![false; search.artists.len()];
        for (entity, name) in stored_artists {
            let wanted = normalize(name);
            if wanted.is_empty() || self.aliased_in(*entity, &instances).await? {
                continue;
            }
            for (index, hit) in search.artists.iter().enumerate() {
                if claimed_artists[index] || normalize(&hit.artist.name) != wanted {
                    continue;
                }
                let key = (
                    hit.id.slug().as_str().to_owned(),
                    hit.id.provider_id().to_owned(),
                );
                if aliases.contains_key(&key) {
                    continue;
                }
                claimed_artists[index] = true;
                twins.insert(hit.id.clone());
                break;
            }
        }

        let mut claimed_albums = vec![false; search.albums.len()];
        for entity in stored_albums {
            let Some(stored) = self.stored_album(*entity).await? else {
                continue;
            };
            let Some(artist) = stored.artist else {
                continue;
            };
            if self.aliased_in(*entity, &instances).await? {
                continue;
            }
            for (index, hit) in search.albums.iter().enumerate() {
                if claimed_albums[index] {
                    continue;
                }
                if title_match(&stored.album, &hit.album.title).is_none() {
                    continue;
                }
                let credit = hit
                    .album
                    .artists
                    .first()
                    .map(|credit| normalize(&credit.name));
                if credit.as_deref() != Some(artist.as_str()) {
                    continue;
                }
                let key = (
                    hit.id.slug().as_str().to_owned(),
                    hit.id.provider_id().to_owned(),
                );
                if aliases.contains_key(&key) {
                    continue;
                }
                claimed_albums[index] = true;
                twins.insert(hit.id.clone());
                break;
            }
        }
        Ok(twins)
    }

    /// Reports whether a stored row already carries an alias for one of the
    /// instances a search named.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored row.
    /// - `instances`: the instances that issued the search's catalog hits.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the alias read fails.
    async fn aliased_in(
        &self,
        entity: EntityId,
        instances: &HashSet<Slug>,
    ) -> Result<bool, CatalogError> {
        Ok(self
            .provider_aliases(entity)
            .await?
            .iter()
            .any(|alias| instances.contains(&alias.slug)))
    }

    /// Reads a stored album with the name of its primary artist.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored album.
    ///
    /// # Returns
    ///
    /// The album and its folded primary artist name, or `None` when the album
    /// has no row or no credited artist.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when a read fails.
    async fn stored_album(&self, entity: EntityId) -> Result<Option<StoredAlbum>, CatalogError> {
        let mut conn = self.database().readers().acquire().await?;
        let Some(album) = albums::get_album(&mut conn, entity).await? else {
            return Ok(None);
        };
        let credits = artists::list_album_artists(&mut conn, entity).await?;
        let artist = credits
            .first()
            .map(|credit| normalize(&credit.name))
            .or_else(|| album.display_artist.as_deref().map(normalize))
            .filter(|name| !name.is_empty());
        Ok(Some(StoredAlbum { album, artist }))
    }
}

/// A stored album and the folded name of its primary artist.
struct StoredAlbum {
    /// The album row.
    album: Album,
    /// The primary artist's name, folded; `None` when the album credits nobody.
    artist: Option<String>,
}

/// Finds the stored recording a catalog track names.
///
/// The rules are checked in order of strength: the provider alias of an
/// already materialized entity, the ISRC (through the alias table and the
/// `tracks` column, which is what a scanned file carries), the MusicBrainz
/// recording id (the same two ways), and finally the same normalized title,
/// display artist and album with a duration within
/// [`DURATION_TOLERANCE_MS`]. The last rule is what lets a scanned file without
/// an ISRC or a recording id be reused instead of duplicated.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `slug`: the instance that issued the track.
/// - `track`: the catalog track.
///
/// # Returns
///
/// The stored row, or `None` when no row holds the recording.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a read fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
pub(crate) async fn stored_recording(
    conn: &mut SqliteConnection,
    slug: &Slug,
    track: &CatalogTrack,
) -> Result<Option<EntityId>, CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    if let Some(stored) =
        aliases::entity_by_alias_conn(conn, &namespace, EntityKind::Track, &track.id).await?
    {
        return Ok(Some(stored));
    }
    if let Some(isrc) = normalize_isrc(track.isrc.as_deref()) {
        if let Some(stored) =
            aliases::entity_by_alias_conn(conn, ISRC, EntityKind::Track, &isrc).await?
        {
            return Ok(Some(stored));
        }
        if let Some(stored) = tracks::find_track_by_isrc(conn, &isrc).await? {
            return Ok(Some(stored.id));
        }
    }
    if let Some(mbid) = canonical_id(&track.ids, MB_RECORDING) {
        if let Some(stored) =
            aliases::entity_by_alias_conn(conn, MB_RECORDING, EntityKind::Track, mbid).await?
        {
            return Ok(Some(stored));
        }
        if let Some(stored) = tracks::find_track_by_mb_recording_id(conn, mbid).await? {
            return Ok(Some(stored.id));
        }
    }
    matching_stored_track(conn, track).await
}

/// Finds a stored track with the same title, artist and album as a catalog
/// track, within a few seconds of its duration.
///
/// The candidate set is the rows whose sort title is the catalog title's, which
/// a case-folding and whitespace-collapsing key narrows without a table scan;
/// the normalized comparison then decides.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `track`: the catalog track.
///
/// # Returns
///
/// The stored track, or `None` when no row is the same recording.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a read fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
async fn matching_stored_track(
    conn: &mut SqliteConnection,
    track: &CatalogTrack,
) -> Result<Option<EntityId>, CatalogError> {
    let title = normalize(&track.title);
    if title.is_empty() {
        return Ok(None);
    }
    let joined = track
        .artists
        .iter()
        .map(|credit| credit.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let artist = normalize(&joined);
    let primary = track.artists.first().map(|credit| normalize(&credit.name));
    let album = track
        .album
        .as_ref()
        .map(|album| normalize(&album.title))
        .filter(|album| !album.is_empty());
    let duration = track.duration_ms.map(i64::from);

    let candidates =
        tracks::find_tracks_by_sort_title(conn, &hearthsound_library::sort_key(&track.title))
            .await?;
    for candidate in candidates {
        if normalize(&candidate.track.title) != title {
            continue;
        }
        let stored_artist = normalize(
            candidate
                .track
                .display_artist
                .as_deref()
                .unwrap_or_default(),
        );
        if stored_artist != artist && Some(&stored_artist) != primary.as_ref() {
            continue;
        }
        if let Some(wanted) = &album {
            let stored = candidate
                .album_title
                .as_deref()
                .map(normalize)
                .unwrap_or_default();
            if &stored != wanted {
                continue;
            }
        }
        let close = match (duration, candidate.track.duration_ms) {
            (Some(catalog), Some(stored)) => (catalog - stored).abs() <= DURATION_TOLERANCE_MS,
            _ => true,
        };
        if close {
            return Ok(Some(candidate.track.id));
        }
    }
    Ok(None)
}

/// Returns the normalized ISRCs of every track in a search result.
///
/// # Parameters
///
/// - `search`: the catalog search result.
fn catalog_isrcs(search: &CatalogSearch) -> Vec<String> {
    let mut isrcs: Vec<String> = search
        .tracks
        .iter()
        .filter_map(|track| normalize_isrc(track.track.isrc.as_deref()))
        .collect();
    isrcs.sort();
    isrcs.dedup();
    isrcs
}

/// Returns the album and artist ids of a search result.
///
/// # Parameters
///
/// - `search`: the catalog search result.
fn albums_and_artists(search: &CatalogSearch) -> Vec<CatalogId> {
    let mut entities: Vec<CatalogId> = search.albums.iter().map(|album| album.id.clone()).collect();
    entities.extend(search.artists.iter().map(|artist| artist.id.clone()));
    entities
}

/// Returns the album and artist ids of a search result with their kinds.
///
/// # Parameters
///
/// - `search`: the catalog search result.
fn album_and_artist_kinds(search: &CatalogSearch) -> Vec<(CatalogId, EntityKind)> {
    let mut entities: Vec<(CatalogId, EntityKind)> = search
        .albums
        .iter()
        .map(|album| (album.id.clone(), EntityKind::Album))
        .collect();
    entities.extend(
        search
            .artists
            .iter()
            .map(|artist| (artist.id.clone(), EntityKind::Artist)),
    );
    entities
}

/// Finds the local hit a catalog track duplicates by title, artist and duration.
///
/// # Parameters
///
/// - `track`: the catalog track.
/// - `local`: the local hits of the search.
///
/// # Returns
///
/// The stored track's id, or `None` when no local hit is the same recording.
fn matching_local_track(track: &CatalogTrackResult, local: &[LocalTrackHit]) -> Option<EntityId> {
    let title = normalize(&track.track.title);
    let artist = track
        .track
        .artists
        .first()
        .map(|credit| normalize(&credit.name));
    let duration = track.track.duration_ms.map(i64::from);

    local.iter().find_map(|hit| {
        if normalize(&hit.title) != title {
            return None;
        }
        if hit.artist.as_deref().map(normalize) != artist {
            return None;
        }
        let close = match (duration, hit.duration_ms) {
            (Some(catalog), Some(local)) => (catalog - local).abs() <= DURATION_TOLERANCE_MS,
            _ => true,
        };
        close.then_some(hit.track_id)
    })
}

/// Folds a title or an artist name into the form deduplication compares.
///
/// The pipeline is: lowercase, NFKD compatibility decomposition with combining
/// marks dropped, every character that is not alphanumeric turned into a space,
/// and runs of spaces collapsed. `Beyonce` therefore matches an accented
/// `Beyonce`, and `Rock'n'Roll` matches `Rock n Roll`.
///
/// # Parameters
///
/// - `text`: the display text.
///
/// # Returns
///
/// The folded text, which is empty when the text holds no alphanumeric
/// character.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::dedupe::normalize;
///
/// assert_eq!(normalize("Rock'n'Roll"), "rock n roll");
/// assert_eq!(normalize("  Caf\u{e9}   del Mar "), "cafe del mar");
/// ```
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for folded in text.nfkd().flat_map(char::to_lowercase) {
        if is_combining_mark(folded) {
            continue;
        }
        if folded.is_alphanumeric() {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(folded);
        } else {
            pending_space = true;
        }
    }
    out
}

/// Folds an ISRC into the form deduplication compares.
///
/// # Parameters
///
/// - `isrc`: the ISRC a provider or a row carries, or `None`.
///
/// # Returns
///
/// The trimmed, uppercased ISRC, or `None` when there was none or it was blank.
///
/// # Examples
///
/// ```
/// use hearthsound_catalog::dedupe::normalize_isrc;
///
/// assert_eq!(normalize_isrc(Some(" usrc17607839 ")).as_deref(), Some("USRC17607839"));
/// assert_eq!(normalize_isrc(Some("   ")), None);
/// ```
pub fn normalize_isrc(isrc: Option<&str>) -> Option<String> {
    let isrc = isrc?.trim();
    if isrc.is_empty() {
        return None;
    }
    Some(isrc.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_case_accents_and_punctuation() {
        assert_eq!(normalize("Cafe del Mar"), "cafe del mar");
        assert_eq!(normalize("  Caf\u{e9}   Del   MAR  "), "cafe del mar");
        assert_eq!(normalize("Rock'n'Roll"), "rock n roll");
        assert_eq!(normalize("AC/DC"), "ac dc");
        assert_eq!(normalize("!!!"), "");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn isrc_normalization_trims_case_and_blanks() {
        assert_eq!(
            normalize_isrc(Some(" usrc17607839 ")).as_deref(),
            Some("USRC17607839")
        );
        assert_eq!(normalize_isrc(Some("   ")), None);
        assert_eq!(normalize_isrc(None), None);
    }
}
