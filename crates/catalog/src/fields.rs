//! Recomputing an entity's fields from its file tags and its provider answers.
//!
//! Every scalar field of a row is a choice between sources: the tags the scanner
//! read, and what each provider instance said. The choice is made here and
//! nowhere else, so an administrator who reorders a layer, adds an instance or
//! flips `metadata.precedence` changes what the server shows without another
//! provider call and without a re-scan.
//!
//! The rule is one setting wide. `metadata.precedence = tags` (the default) puts
//! the file's own tags first and lets providers fill the fields the tags leave
//! empty; `plugins` puts the providers first and the tags last. Within one side,
//! the order is the administrator's layer order: the catalog layer for the
//! fields a catalog supplies, the metadata layer for a release or an artist's
//! information. The first source with a value wins, field by field.
//!
//! Credits follow the same rule: a row written from tags keeps its credits
//! unless the precedence is `plugins`, which is what makes a provider's credit
//! list replace a scanner's.

use std::collections::HashMap;

use hearthsound_core::EntityKind;
use hearthsound_core::providers::{ExternalId, MB_RELEASE, MB_RELEASE_GROUP};
use hearthsound_db::Database;
use serde::Deserialize;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::CatalogError;
use crate::normalize::base_title;
use crate::sources::{self, Operation, Source};
use crate::wire::{
    WireAlbum, WireAlbumRef, WireAlbumRefs, WireArtistInfo, WireArtistLink, WireProviderIds,
    WireTracks,
};

/// The administrator's layer order, as the field rule reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayerOrder {
    /// The catalog layer's instances, primary first.
    pub catalog: Vec<Uuid>,
    /// The metadata layer's instances, primary first.
    pub metadata: Vec<Uuid>,
    /// The artwork layer's instances, primary first.
    pub artwork: Vec<Uuid>,
    /// The discovery layer's instances, primary first.
    pub discovery: Vec<Uuid>,
}

/// Which side wins a field both sides state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precedence {
    /// The file's own tags win; providers fill the gaps.
    Tags,
    /// The providers win; the tags fill the gaps.
    Plugins,
}

impl Precedence {
    /// Parses the `metadata.precedence` setting.
    ///
    /// # Parameters
    ///
    /// - `value`: the stored setting.
    ///
    /// # Returns
    ///
    /// The precedence, defaulting to [`Precedence::Tags`] for anything the
    /// setting does not name.
    pub fn parse(value: &str) -> Precedence {
        match value {
            "plugins" => Precedence::Plugins,
            _ => Precedence::Tags,
        }
    }
}

/// The album columns the field rule reads back.
type AlbumFieldRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// The track columns the field rule reads back.
type TrackFieldRow = (
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// One stored artist's file tags.
#[derive(Debug, Clone, Deserialize, Default)]
struct ArtistTags {
    #[serde(default)]
    name: Option<String>,
}

/// One stored album's file tags.
#[derive(Debug, Clone, Deserialize, Default)]
struct AlbumTags {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "displayArtist", default)]
    display_artist: Option<String>,
    #[serde(rename = "releaseDate", default)]
    release_date: Option<String>,
    #[serde(default)]
    year: Option<i64>,
    #[serde(default)]
    edition: Option<String>,
    #[serde(rename = "albumType", default)]
    album_type: Option<String>,
}

/// One stored track's file tags.
#[derive(Debug, Clone, Deserialize, Default)]
struct TrackTags {
    #[serde(default)]
    title: Option<String>,
    #[serde(rename = "displayArtist", default)]
    display_artist: Option<String>,
    #[serde(rename = "discNumber", default)]
    disc_number: Option<i64>,
    #[serde(rename = "trackNumber", default)]
    track_number: Option<i64>,
    #[serde(rename = "durationMs", default)]
    duration_ms: Option<i64>,
}

/// Recomputes one entity's fields, credits and list relations.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
/// - `precedence`: which side wins a field both sides state.
/// - `order`: the layer order the providers are tried in.
///
/// # Errors
///
/// Returns [`CatalogError`] when a read or write fails, and
/// [`CatalogError::Json`] when a stored payload cannot be decoded.
pub async fn recompute(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
    precedence: Precedence,
    order: &LayerOrder,
) -> Result<(), CatalogError> {
    let sources = sources::all(conn, kind, entity).await?;
    match kind {
        EntityKind::Artist => recompute_artist(conn, entity, precedence, order, &sources).await,
        EntityKind::Album => recompute_album(conn, entity, precedence, order, &sources).await,
        EntityKind::Track => recompute_track(conn, entity, precedence, order, &sources).await,
        EntityKind::Playlist => Ok(()),
    }
}

/// Recomputes an artist's fields and its list relations.
async fn recompute_artist(
    conn: &mut SqliteConnection,
    entity: Uuid,
    precedence: Precedence,
    order: &LayerOrder,
    sources: &[Source],
) -> Result<(), CatalogError> {
    let row: Option<(String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name, file_tags, biography FROM artists WHERE id = ?")
            .bind(entity.as_bytes().as_slice())
            .fetch_optional(&mut *conn)
            .await?;
    let Some((stored_name, file_tags, stored_biography)) = row else {
        return Ok(());
    };
    let tags: ArtistTags = file_tags
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let catalog_artists =
        payloads::<crate::wire::WireArtist>(sources, Operation::Catalog, &order.catalog);
    let infos: Vec<(usize, WireArtistInfo)> =
        payloads(sources, Operation::ArtistInfo, &order.metadata);

    let name = pick(
        precedence,
        tags.name.clone(),
        catalog_artists
            .iter()
            .map(|(_, artist)| artist.name.clone())
            .next()
            .filter(|name| !name.is_empty()),
    )
    .unwrap_or(stored_name);
    let biography = pick(
        precedence,
        None,
        infos
            .iter()
            .find_map(|(_, info)| info.biography.clone())
            .filter(|text| !text.trim().is_empty()),
    )
    .or(stored_biography);
    let links = merge_links(&infos);
    let album_count = catalog_artists
        .iter()
        .find_map(|(_, artist)| artist.album_count)
        .map(i64::from);

    sqlx::query(
        "UPDATE artists SET name = ?, sort_name = ?, biography = ?, links = ?, \
                album_count = COALESCE(album_count, ?) WHERE id = ?",
    )
    .bind(&name)
    .bind(hearthsound_library::sort_key(&name))
    .bind(biography)
    .bind(serde_json::to_string(&links)?)
    .bind(album_count)
    .bind(entity.as_bytes().as_slice())
    .execute(&mut *conn)
    .await?;

    // The discography: the first catalog instance that listed albums wins, and
    // the albums it listed are credited to this artist.
    let discography: Vec<WireAlbumRef> =
        payloads(sources, Operation::CatalogAlbums, &order.catalog)
            .into_iter()
            .find_map(|(_, refs): (usize, WireAlbumRefs)| {
                let refs = refs.into_inner();
                (!refs.is_empty()).then_some(refs)
            })
            .unwrap_or_default();
    if !discography.is_empty() {
        for reference in &discography {
            let album: Option<Vec<u8>> = sqlx::query_scalar(
                "SELECT entity_id FROM external_ids \
                 WHERE entity_kind = 'album' AND namespace LIKE 'p:%' AND value = ? LIMIT 1",
            )
            .bind(&reference.id)
            .fetch_optional(&mut *conn)
            .await?;
            if let Some(album) = album
                && album.len() == 16
                && let Ok(album) = Uuid::from_slice(&album)
            {
                sqlx::query(
                    "INSERT OR IGNORE INTO album_artists (album_id, artist_id, position, role) \
                     SELECT ?, ?, COALESCE(MAX(position) + 1, 0), 'main' FROM album_artists \
                     WHERE album_id = ?",
                )
                .bind(album.as_bytes().as_slice())
                .bind(entity.as_bytes().as_slice())
                .bind(album.as_bytes().as_slice())
                .execute(&mut *conn)
                .await?;
            }
        }
    }

    let top: Option<WireTracks> =
        payloads::<WireTracks>(sources, Operation::CatalogTopTracks, &order.catalog)
            .into_iter()
            .find_map(|(_, tracks)| (!tracks.is_empty()).then_some(tracks));
    if let Some(tracks) = top {
        write_artist_top_tracks(conn, entity, &tracks).await?;
    }

    let similar: Option<WireProviderIds> =
        payloads::<WireProviderIds>(sources, Operation::SimilarArtists, &order.discovery)
            .into_iter()
            .find_map(|(_, artists)| (!artists.is_empty()).then_some(artists));
    if let Some(artists) = similar {
        write_artist_similar(conn, entity, &artists).await?;
    }
    Ok(())
}

/// Replaces an artist's top tracks with the ids the catalog listed.
async fn write_artist_top_tracks(
    conn: &mut SqliteConnection,
    artist: Uuid,
    tracks: &WireTracks,
) -> Result<(), CatalogError> {
    sqlx::query("DELETE FROM artist_top_tracks WHERE artist_id = ?")
        .bind(artist.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    for (position, track) in tracks.iter().enumerate() {
        let Some(row) = crate::wire::row_by_uuid(conn, EntityKind::Track, &track.id).await? else {
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO artist_top_tracks (artist_id, position, track_id) \
             VALUES (?, ?, ?)",
        )
        .bind(artist.as_bytes().as_slice())
        .bind(position as i64)
        .bind(row.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Replaces an artist's similar artists with the ids discovery listed.
async fn write_artist_similar(
    conn: &mut SqliteConnection,
    artist: Uuid,
    artists: &WireProviderIds,
) -> Result<(), CatalogError> {
    sqlx::query("DELETE FROM artist_similar WHERE artist_id = ?")
        .bind(artist.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    for (position, similar) in artists.iter().enumerate() {
        let Some(row) = crate::wire::row_by_uuid(conn, EntityKind::Artist, similar).await? else {
            continue;
        };
        sqlx::query(
            "INSERT OR IGNORE INTO artist_similar (artist_id, position, similar_id) VALUES (?, ?, ?)",
        )
        .bind(artist.as_bytes().as_slice())
        .bind(position as i64)
        .bind(row.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Recomputes an album's fields.
async fn recompute_album(
    conn: &mut SqliteConnection,
    entity: Uuid,
    precedence: Precedence,
    order: &LayerOrder,
    sources: &[Source],
) -> Result<(), CatalogError> {
    let row: Option<AlbumFieldRow> = sqlx::query_as(
        "SELECT title, display_artist, release_date, album_type, year, file_tags \
               FROM albums WHERE id = ?",
    )
    .bind(entity.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((stored_title, stored_artist, stored_date, stored_type, stored_year, file_tags)) = row
    else {
        return Ok(());
    };
    let tags: AlbumTags = file_tags
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let catalog: Vec<WireAlbum> = payloads(sources, Operation::Catalog, &order.catalog)
        .into_iter()
        .map(|(_, album)| album)
        .collect();
    let releases: Vec<WireAlbum> = payloads(sources, Operation::Release, &order.metadata)
        .into_iter()
        .map(|(_, album)| album)
        .collect();
    let providers: Vec<&WireAlbum> = catalog.iter().chain(releases.iter()).collect();

    let title = pick(
        precedence,
        tags.title.clone(),
        providers
            .iter()
            .map(|album| album.title.clone())
            .next()
            .filter(|title| !title.is_empty()),
    )
    .unwrap_or(stored_title);
    let display_artist = pick(
        precedence,
        tags.display_artist.clone(),
        providers
            .iter()
            .find_map(|album| display_of(&album.artists))
            .filter(|name| !name.is_empty()),
    )
    .or(stored_artist);
    let release_date = pick(
        precedence,
        tags.release_date.clone(),
        providers
            .iter()
            .find_map(|album| album.release_date.clone()),
    )
    .or(stored_date);
    let album_type = pick(
        precedence,
        tags.album_type.clone(),
        providers.iter().find_map(|album| album.album_type.clone()),
    )
    .or(stored_type);
    let edition = pick(
        precedence,
        tags.edition.clone(),
        providers.iter().find_map(|album| album.edition.clone()),
    );
    let year = pick(
        precedence,
        tags.year,
        release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .and_then(|year| year.parse().ok()),
    )
    .or(stored_year);
    let track_count = providers
        .iter()
        .map(|album| album.tracks.len() as i64)
        .find(|count| *count > 0);

    sqlx::query(
        "UPDATE albums SET title = ?, sort_title = ?, display_artist = ?, release_date = ?, \
                album_type = ?, edition = COALESCE(?, edition), year = COALESCE(?, year), \
                track_count = COALESCE(track_count, ?) WHERE id = ?",
    )
    .bind(&title)
    .bind(hearthsound_library::sort_key(&title))
    .bind(display_artist)
    .bind(release_date)
    .bind(album_type)
    .bind(edition)
    .bind(year)
    .bind(track_count)
    .bind(entity.as_bytes().as_slice())
    .execute(&mut *conn)
    .await?;

    let genres: Vec<String> = providers
        .iter()
        .find_map(|album| (!album.genres.is_empty()).then(|| album.genres.clone()))
        .unwrap_or_default();
    if !genres.is_empty() {
        crate::resolve::write_genres(conn, entity, &genres).await?;
    }
    Ok(())
}

/// Recomputes a track's fields.
async fn recompute_track(
    conn: &mut SqliteConnection,
    entity: Uuid,
    precedence: Precedence,
    order: &LayerOrder,
    sources: &[Source],
) -> Result<(), CatalogError> {
    let row: Option<TrackFieldRow> = sqlx::query_as(
        "SELECT title, display_artist, disc_number, track_number, duration_ms, file_tags \
           FROM tracks WHERE id = ?",
    )
    .bind(entity.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((stored_title, stored_artist, stored_disc, stored_number, stored_duration, file_tags)) =
        row
    else {
        return Ok(());
    };
    let tags: TrackTags = file_tags
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default();
    let catalog: Vec<WireAlbum> = payloads(sources, Operation::Catalog, &order.catalog)
        .into_iter()
        .map(|(_, album)| album)
        .collect();
    let tracks: Vec<crate::wire::WireTrack> = catalog
        .iter()
        .flat_map(|album| album.tracks.iter().cloned())
        .collect();
    let releases: Vec<WireAlbum> = payloads(sources, Operation::Release, &order.metadata)
        .into_iter()
        .map(|(_, album)| album)
        .collect();
    let all: Vec<&crate::wire::WireTrack> = tracks
        .iter()
        .chain(releases.iter().flat_map(|album| album.tracks.iter()))
        .collect();

    let title = pick(
        precedence,
        tags.title.clone(),
        all.iter()
            .map(|track| track.title.clone())
            .next()
            .filter(|title| !title.is_empty()),
    )
    .unwrap_or(stored_title);
    let display_artist = pick(
        precedence,
        tags.display_artist.clone(),
        all.iter()
            .find_map(|track| display_of(&track.artists))
            .filter(|name| !name.is_empty()),
    )
    .or(stored_artist);
    let disc_number = pick(
        precedence,
        tags.disc_number,
        all.iter()
            .find_map(|track| track.disc_number.map(i64::from)),
    )
    .or(stored_disc);
    let track_number = pick(
        precedence,
        tags.track_number,
        all.iter()
            .find_map(|track| track.track_number.map(i64::from)),
    )
    .or(stored_number);
    let duration_ms = pick(
        precedence,
        tags.duration_ms,
        all.iter()
            .find_map(|track| track.duration_ms.map(i64::from)),
    )
    .or(stored_duration);

    sqlx::query(
        "UPDATE tracks SET title = ?, sort_title = ?, display_artist = ?, disc_number = ?, \
                track_number = ?, duration_ms = ? WHERE id = ?",
    )
    .bind(&title)
    .bind(hearthsound_library::sort_key(&title))
    .bind(display_artist)
    .bind(disc_number)
    .bind(track_number)
    .bind(duration_ms)
    .bind(entity.as_bytes().as_slice())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Reads and decodes the payloads of one operation, in layer order.
///
/// # Parameters
///
/// - `sources`: every stored answer for the entity.
/// - `operation`: the operation to keep.
/// - `order`: the layer the operation's instances are ordered by.
fn payloads<T: for<'de> Deserialize<'de>>(
    sources: &[Source],
    operation: Operation,
    order: &[Uuid],
) -> Vec<(usize, T)> {
    let mut found: HashMap<Uuid, &Source> = HashMap::new();
    for source in sources {
        if source.operation == operation && source.has_payload() {
            found.insert(source.instance, source);
        }
    }
    let mut out = Vec::new();
    for (position, instance) in order.iter().enumerate() {
        let Some(source) = found.get(instance) else {
            continue;
        };
        let Some(payload) = source.payload.as_deref() else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<T>(payload) {
            out.push((position, value));
        }
    }
    out
}

/// Picks the value of one field.
///
/// # Parameters
///
/// - `precedence`: which side wins.
/// - `tags`: the value the file states, when it states one.
/// - `provider`: the value the providers state, when one states one.
fn pick<T>(precedence: Precedence, tags: Option<T>, provider: Option<T>) -> Option<T> {
    match precedence {
        Precedence::Tags => tags.or(provider),
        Precedence::Plugins => provider.or(tags),
    }
}

/// Returns the display artist of a credit list.
fn display_of(credits: &[crate::wire::WireArtistCredit]) -> Option<String> {
    let names: Vec<&str> = credits
        .iter()
        .map(|credit| credit.name.as_str())
        .filter(|name| !name.is_empty())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Merges the links of every metadata answer, deduplicated by URL.
fn merge_links(infos: &[(usize, WireArtistInfo)]) -> Vec<WireArtistLink> {
    let mut links: Vec<WireArtistLink> = Vec::new();
    for (_, info) in infos {
        for link in &info.links {
            if !links.iter().any(|known| known.url == link.url) {
                links.push(link.clone());
            }
        }
    }
    links
}

/// Recomputes every entity that holds provider answers.
///
/// The job a settings change and a layer change run: precedence and order moved,
/// so the stored fields are recomputed from what is already stored, with no
/// provider call at all.
///
/// # Parameters
///
/// - `db`: the database.
/// - `precedence`: which side wins.
/// - `order`: the layer order.
///
/// # Returns
///
/// How many entities were recomputed.
///
/// # Errors
///
/// Returns [`CatalogError`] when a read or write fails.
pub async fn recompute_all(
    db: &Database,
    precedence: Precedence,
    order: &LayerOrder,
) -> Result<usize, CatalogError> {
    let mut done = 0;
    for kind in [EntityKind::Artist, EntityKind::Album, EntityKind::Track] {
        let ids: Vec<Vec<u8>> = match kind {
            EntityKind::Artist => {
                sqlx::query_scalar(
                    "SELECT id FROM artists WHERE EXISTS (SELECT 1 FROM entity_sources s \
                     WHERE s.entity_kind = 'artist' AND s.entity_id = artists.id)",
                )
                .fetch_all(db.readers())
                .await?
            }
            EntityKind::Album => {
                sqlx::query_scalar(
                    "SELECT id FROM albums WHERE EXISTS (SELECT 1 FROM entity_sources s \
                     WHERE s.entity_kind = 'album' AND s.entity_id = albums.id)",
                )
                .fetch_all(db.readers())
                .await?
            }
            _ => {
                sqlx::query_scalar(
                    "SELECT id FROM tracks WHERE EXISTS (SELECT 1 FROM entity_sources s \
                     WHERE s.entity_kind = 'track' AND s.entity_id = tracks.id)",
                )
                .fetch_all(db.readers())
                .await?
            }
        };
        for id in ids {
            if id.len() != 16 {
                continue;
            }
            let Ok(entity) = Uuid::from_slice(&id) else {
                continue;
            };
            let mut conn = db.writer().acquire().await?;
            recompute(&mut conn, kind, entity, precedence, order).await?;
            done += 1;
        }
    }
    Ok(done)
}

/// Returns the identifiers that name an entity, which the pages show.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Returns
///
/// The identifiers, ordered by namespace.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the read fails.
pub async fn identifiers(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<Vec<ExternalId>, CatalogError> {
    hearthsound_library::external_ids::ids_of(conn, kind, entity)
        .await
        .map_err(CatalogError::Library)
}

/// Returns whether an album holds one of the canonical identifiers.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album`: the album row.
///
/// # Returns
///
/// `true` when the album has an `mb-release` or `mb-release-group` row.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the read fails.
pub async fn album_is_identified(
    conn: &mut SqliteConnection,
    album: Uuid,
) -> Result<bool, CatalogError> {
    for namespace in [MB_RELEASE, MB_RELEASE_GROUP] {
        let found: Option<i64> = sqlx::query_scalar(
            "SELECT 1 AS \"found: i64\" FROM external_ids \
             WHERE entity_kind = 'album' AND namespace = ? AND entity_id = ? LIMIT 1",
        )
        .bind(namespace)
        .bind(album.as_bytes().as_slice())
        .fetch_optional(&mut *conn)
        .await?;
        if found.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Returns an album's grouping key: its release group, or its base title.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `album`: the album row.
/// - `title`: the album's title.
///
/// # Returns
///
/// The release-group id when the album has one, and the folded base title
/// otherwise.
///
/// # Errors
///
/// Returns [`CatalogError::Library`] when the read fails.
pub async fn album_group(
    conn: &mut SqliteConnection,
    album: Uuid,
    title: &str,
) -> Result<String, CatalogError> {
    let group: Option<String> = sqlx::query_scalar(
        "SELECT value FROM external_ids \
         WHERE entity_kind = 'album' AND namespace = 'mb-release-group' AND entity_id = ? LIMIT 1",
    )
    .bind(album.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match group {
        Some(group) => format!("group:{group}"),
        None => format!("title:{}", base_title(title)),
    })
}
