//! Materialization: turning a catalog entity into stored rows.
//!
//! A client can name an entity that no row describes (`x_<slug>_t_<id>`) and
//! then act on it: save it, like it, add it to a playlist, ask for it to be
//! acquired. Every one of those actions needs a row first, and this module is
//! what creates it.
//!
//! The rules the design fixes (04-data-model.md, section 2.3) are:
//!
//! 1. the `p:<slug>` alias decides. When it exists the entity is already
//!    materialized and the stored row is returned untouched, so the original
//!    `x_` id keeps resolving forever;
//! 2. otherwise the full entity is fetched from the issuing instance, its rows
//!    are written with origin `catalog` and availability `remote`, and the
//!    provider id plus every canonical identifier the entity carries (ISRC,
//!    UPC, MusicBrainz ids) is recorded as an alias in the same transaction, so
//!    two providers that know the same recording agree on one row;
//! 3. a recording that is already stored under a different alias - a scanned
//!    file whose ISRC matches, or a row another instance materialized - is
//!    reused rather than duplicated.
//!
//! The library crate owns `artists`, `albums` and `tracks`, so the rows are
//! written through its transaction-level functions and never with this crate's
//! own SQL. Only `external_ids` is written here.
//!
//! # Race safety
//!
//! The database has exactly one writer connection, so two materializations
//! serialize: each one takes the connection for the whole transaction. The
//! alias is therefore checked twice - once before the provider call, to answer
//! an already materialized id without spending provider quota, and once inside
//! the write transaction, where no other writer can commit between the check
//! and the insert. The alias insert itself is `ON CONFLICT DO NOTHING` and the
//! winner is read back, so a conflict resolves to the row that exists rather
//! than to an error.

use hearthsound_core::providers::{
    ArtistCredit as ProviderCredit, CatalogAlbum, CatalogArtist, CatalogTrack, ExternalId,
};
use hearthsound_core::{Availability, CatalogId, EntityId, EntityKind, Slug};
use hearthsound_db::Database;
use hearthsound_library::{AlbumInput, ArtistCredit, ArtistInput, Origin, TrackInput};
use hearthsound_library::{albums, artists, tracks};
use sqlx::SqliteConnection;

use crate::aliases::{self, PROVIDER_PREFIX};
use crate::dedupe::normalize_isrc;
use crate::error::CatalogError;
use crate::service::{CatalogService, require_kind};

/// The namespace of an International Standard Recording Code.
pub(crate) const ISRC: &str = "isrc";

/// The namespace of a Universal Product Code.
pub(crate) const UPC: &str = "upc";

/// The namespace of a MusicBrainz recording id.
pub(crate) const MB_RECORDING: &str = "mb-recording";

/// The namespace of a MusicBrainz release id.
pub(crate) const MB_RELEASE: &str = "mb-release";

/// The namespace of a MusicBrainz release-group id.
pub(crate) const MB_RELEASE_GROUP: &str = "mb-release-group";

/// The namespace of a MusicBrainz artist id.
pub(crate) const MB_ARTIST: &str = "mb-artist";

/// The namespaces a materialized entity records from the identifiers its
/// provider returned.
///
/// Everything else a provider attaches (`hello`, `http-catalog`) is that
/// provider's own bookkeeping and means nothing to the rest of the server.
const CANONICAL_NAMESPACES: [&str; 6] = [
    ISRC,
    UPC,
    MB_RECORDING,
    MB_RELEASE,
    MB_RELEASE_GROUP,
    MB_ARTIST,
];

/// A provider id that names a stored entity.
///
/// It is the reverse of the alias table's normal direction: the entity is
/// stored, and the caller wants to know which instance issued the id that
/// resolves to it, so it can reach that instance again.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProviderAlias {
    /// The instance that issued the provider id.
    pub slug: Slug,
    /// The provider's own id for the entity.
    pub provider_id: String,
}

/// An album a catalog entity resolved to, with the tracks stored alongside it.
///
/// [`CatalogService::materialize_album`] reports the album and its track rows in
/// the provider's track order, which is the order a caller inserts them into a
/// playlist or a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedAlbum {
    /// The stored album row.
    pub album: EntityId,
    /// The stored track rows, in the album's track order.
    pub tracks: Vec<EntityId>,
}

impl CatalogService {
    /// Materializes one catalog track.
    ///
    /// An entity whose `p:<slug>` alias already exists is returned as it is and
    /// no provider call is made; otherwise the track is fetched from the
    /// issuing instance and stored.
    ///
    /// A recording the server already stores is reused instead of duplicated:
    /// the track is looked up by its ISRC, first through the `isrc` alias and
    /// then through the stored `tracks.isrc` column, which is what a scanned
    /// file has.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of a track.
    ///
    /// # Returns
    ///
    /// The stored track row.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name a track and
    /// when its slug does not resolve to a catalog instance, the mapped provider
    /// failure when the lookup fails, and [`CatalogError::Database`] or
    /// [`CatalogError::Library`] when a row cannot be written.
    pub async fn materialize_track(&self, id: &CatalogId) -> Result<EntityId, CatalogError> {
        require_kind(id, EntityKind::Track)?;
        if let Some(stored) = self.stored_row(id).await? {
            return Ok(stored);
        }
        let track = self.track(id).await?;
        let slug = id.slug().clone();

        let mut tx = self.database().writer().begin().await?;
        let stored = store_track(&mut tx, &slug, &track.track, None).await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// Materializes one catalog album and its tracks.
    ///
    /// The album is fetched with its tracklist and every track is materialized
    /// in order inside one transaction, so an album is never left half stored:
    /// either the album and all of its tracks exist, or none of them does. An
    /// album whose `p:<slug>` alias already exists is reported from the stored
    /// rows without a provider call.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an album.
    ///
    /// # Returns
    ///
    /// The stored album row and its track rows, in track order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an album and
    /// when its slug does not resolve to a catalog instance, the mapped provider
    /// failure when the lookup fails, and [`CatalogError::Database`] or
    /// [`CatalogError::Library`] when a row cannot be written.
    pub async fn materialize_album(
        &self,
        id: &CatalogId,
    ) -> Result<MaterializedAlbum, CatalogError> {
        require_kind(id, EntityKind::Album)?;
        if let Some(album) = self.stored_row(id).await? {
            let tracks = stored_album_tracks(self.database(), album).await?;
            return Ok(MaterializedAlbum { album, tracks });
        }
        let album = self.album(id).await?;
        let slug = id.slug().clone();

        let mut tx = self.database().writer().begin().await?;
        let (stored, tracks) = store_album(&mut tx, &slug, &album.album).await?;
        tx.commit().await?;
        Ok(MaterializedAlbum {
            album: stored,
            tracks,
        })
    }

    /// Materializes one catalog artist.
    ///
    /// An artist is deduped by MusicBrainz artist id and otherwise by name, so
    /// an artist a scan already stored is enriched rather than duplicated.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an artist.
    ///
    /// # Returns
    ///
    /// The stored artist row.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::NotFound`] when the id does not name an artist
    /// and when its slug does not resolve to a catalog instance, the mapped
    /// provider failure when the lookup fails, and [`CatalogError::Database`] or
    /// [`CatalogError::Library`] when a row cannot be written.
    pub async fn materialize_artist(&self, id: &CatalogId) -> Result<EntityId, CatalogError> {
        require_kind(id, EntityKind::Artist)?;
        if let Some(stored) = self.stored_row(id).await? {
            return Ok(stored);
        }
        let artist = self.artist(id).await?;
        let slug = id.slug().clone();

        let mut tx = self.database().writer().begin().await?;
        let stored = store_artist(&mut tx, &slug, &artist.artist).await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// Lists the provider ids that resolve to a stored entity.
    ///
    /// This is how a caller reaches the instance that issued an entity: a
    /// materialized track keeps the alias of every provider that knows it, and
    /// each alias names the instance to ask for more about it.
    ///
    /// # Parameters
    ///
    /// - `entity`: the stored row.
    ///
    /// # Returns
    ///
    /// One entry per `p:<slug>` alias, ordered by instance slug and then by
    /// provider id. A row that no catalog entity named - a scanned track, for
    /// example - has no aliases and reports an empty vector.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the query fails.
    pub async fn provider_aliases(
        &self,
        entity: EntityId,
    ) -> Result<Vec<ProviderAlias>, CatalogError> {
        let Some(kind) = entity.kind().entity_kind() else {
            return Ok(Vec::new());
        };
        let mut found = Vec::new();
        for (namespace, value) in aliases::aliases_of_entity(self.database(), kind, entity).await? {
            let Some(name) = namespace.strip_prefix(PROVIDER_PREFIX) else {
                continue;
            };
            match Slug::new(name) {
                Ok(slug) => found.push(ProviderAlias {
                    slug,
                    provider_id: value,
                }),
                Err(error) => tracing::warn!(
                    namespace,
                    error = %error,
                    "a stored provider alias does not name a valid instance slug",
                ),
            }
        }
        found.sort();
        Ok(found)
    }

    /// Reads the stored row an alias names.
    ///
    /// The alias namespaces are the ones of `external_ids`: `isrc`, `upc`, the
    /// MusicBrainz namespaces, and `p:<slug>` for a provider id.
    ///
    /// # Parameters
    ///
    /// - `namespace`: the alias namespace.
    /// - `kind`: the entity kind the alias names.
    /// - `value`: the alias value.
    ///
    /// # Returns
    ///
    /// The stored row, or `None` when no alias has that value.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the query fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    pub async fn entity_by_alias(
        &self,
        namespace: &str,
        kind: EntityKind,
        value: &str,
    ) -> Result<Option<EntityId>, CatalogError> {
        aliases::entity_by_alias(self.database(), namespace, kind, value).await
    }

    /// Reads the stored row a catalog id already resolves to.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id.
    ///
    /// # Returns
    ///
    /// The stored row when the entity is materialized, otherwise `None`.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError::Database`] when the query fails and
    /// [`CatalogError::CorruptAlias`] when a stored id does not decode.
    async fn stored_row(&self, id: &CatalogId) -> Result<Option<EntityId>, CatalogError> {
        aliases::entity_by_alias(
            self.database(),
            &aliases::provider_namespace(id.slug()),
            id.kind(),
            id.provider_id(),
        )
        .await
    }
}

/// Writes one alias, keeping the row an existing one already names.
///
/// The insert is `ON CONFLICT DO NOTHING`, so the primary key of
/// `external_ids` decides which of two racing writers keeps the alias.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `namespace`: the alias namespace.
/// - `kind`: the entity kind the alias names.
/// - `value`: the alias value.
/// - `entity`: the row the alias should name.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the statement fails.
pub(crate) async fn insert_alias(
    conn: &mut SqliteConnection,
    namespace: &str,
    kind: EntityKind,
    value: &str,
    entity: EntityId,
) -> Result<(), CatalogError> {
    let entity_id = entity.uuid();
    sqlx::query!(
        "INSERT INTO external_ids (namespace, value, entity_kind, entity_id) VALUES (?, ?, ?, ?) \
         ON CONFLICT (namespace, entity_kind, value) DO NOTHING",
        namespace,
        value,
        aliases::kind_literal(kind),
        entity_id.as_bytes().as_slice(),
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// Writes one alias and returns the row it names afterwards.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `namespace`: the alias namespace.
/// - `kind`: the entity kind the alias names.
/// - `value`: the alias value.
/// - `entity`: the row to record when no alias exists yet.
///
/// # Returns
///
/// `entity`, or the row an already stored alias names.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a statement fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
async fn claim_alias(
    conn: &mut SqliteConnection,
    namespace: &str,
    kind: EntityKind,
    value: &str,
    entity: EntityId,
) -> Result<EntityId, CatalogError> {
    insert_alias(conn, namespace, kind, value, entity).await?;
    Ok(aliases::entity_by_alias_conn(conn, namespace, kind, value)
        .await?
        .unwrap_or(entity))
}

/// Stores one catalog album and its tracks in an open transaction.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the album.
/// - `album`: the album the provider returned.
///
/// # Returns
///
/// The stored album row and its track rows, in track order.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] or [`CatalogError::Library`] when a row
/// cannot be written.
async fn store_album(
    conn: &mut SqliteConnection,
    slug: &Slug,
    album: &CatalogAlbum,
) -> Result<(EntityId, Vec<EntityId>), CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    if let Some(stored) =
        aliases::entity_by_alias_conn(conn, &namespace, EntityKind::Album, &album.id).await?
    {
        let tracks = tracks::list_tracks_for_album(conn, stored)
            .await?
            .into_iter()
            .map(|track| track.id)
            .collect();
        return Ok((stored, tracks));
    }

    let credits = credits(&album.artists);
    let mut input = AlbumInput::new(album.title.as_str(), Origin::Catalog);
    input.display_artist = display_artist(&album.artists);
    input.year = release_year(album.release_date.as_deref());
    input.release_date = album.release_date.clone();
    input.album_type = album.album_type.clone();
    input.mb_release_id = canonical_id(&album.ids, MB_RELEASE).map(str::to_owned);
    input.mb_release_group_id = canonical_id(&album.ids, MB_RELEASE_GROUP).map(str::to_owned);
    input.upc = album.upc.clone();

    let stored = albums::upsert_album(conn, &input, &credits).await?;
    let stored = claim_alias(conn, &namespace, EntityKind::Album, &album.id, stored).await?;
    record_aliases(
        conn,
        EntityKind::Album,
        stored,
        &album.ids,
        None,
        album.upc.as_deref(),
    )
    .await?;
    record_credit_aliases(conn, &album.artists).await?;

    let mut tracks_stored = Vec::with_capacity(album.tracks.len());
    for track in &album.tracks {
        tracks_stored.push(store_track(conn, slug, track, Some(stored)).await?);
    }
    Ok((stored, tracks_stored))
}

/// Stores one catalog track in an open transaction.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the track.
/// - `track`: the track the provider returned.
/// - `album_id`: the stored album the track belongs to, when the caller knows
///   one.
///
/// # Returns
///
/// The stored track row.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] or [`CatalogError::Library`] when a row
/// cannot be written.
async fn store_track(
    conn: &mut SqliteConnection,
    slug: &Slug,
    track: &CatalogTrack,
    album_id: Option<EntityId>,
) -> Result<EntityId, CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    if let Some(stored) =
        aliases::entity_by_alias_conn(conn, &namespace, EntityKind::Track, &track.id).await?
    {
        return Ok(stored);
    }

    let isrc = normalize_isrc(track.isrc.as_deref());
    if let Some(stored) = stored_recording(conn, isrc.as_deref()).await? {
        claim_alias(conn, &namespace, EntityKind::Track, &track.id, stored).await?;
        record_aliases(
            conn,
            EntityKind::Track,
            stored,
            &track.ids,
            isrc.as_deref(),
            None,
        )
        .await?;
        record_credit_aliases(conn, &track.artists).await?;
        return Ok(stored);
    }

    let credits = credits(&track.artists);
    let mut input = TrackInput::new(track.title.as_str(), Origin::Catalog, Availability::Remote);
    input.album_id = album_id;
    input.display_artist = display_artist(&track.artists);
    input.disc_number = track.disc_number.map(i64::from);
    input.track_number = track.track_number.map(i64::from);
    input.duration_ms = track.duration_ms.map(i64::from);
    input.isrc = isrc.clone();
    input.mb_recording_id = canonical_id(&track.ids, MB_RECORDING).map(str::to_owned);
    input.explicit = track.explicit;

    let created = tracks::insert_track(conn, &input, &credits, &[]).await?;
    let stored = claim_alias(conn, &namespace, EntityKind::Track, &track.id, created).await?;
    if stored != created {
        // Another writer materialized the same provider id first. That cannot
        // happen while one connection serializes every write, but if it ever
        // does, the row this call created is released rather than left as a
        // second row for one recording.
        tracing::warn!(
            instance = slug.as_str(),
            provider_id = track.id.as_str(),
            "a materialized track lost its provider alias; releasing the duplicate row"
        );
        tracks::delete_track(conn, created).await?;
        return Ok(stored);
    }
    record_aliases(
        conn,
        EntityKind::Track,
        stored,
        &track.ids,
        isrc.as_deref(),
        None,
    )
    .await?;
    record_credit_aliases(conn, &track.artists).await?;
    Ok(stored)
}

/// Stores one catalog artist in an open transaction.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the artist.
/// - `artist`: the artist the provider returned.
///
/// # Returns
///
/// The stored artist row.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] or [`CatalogError::Library`] when a row
/// cannot be written.
async fn store_artist(
    conn: &mut SqliteConnection,
    slug: &Slug,
    artist: &CatalogArtist,
) -> Result<EntityId, CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    if let Some(stored) =
        aliases::entity_by_alias_conn(conn, &namespace, EntityKind::Artist, &artist.id).await?
    {
        return Ok(stored);
    }

    let mut input = ArtistInput::new(artist.name.as_str()).origin(Origin::Catalog);
    input.mb_artist_id = canonical_id(&artist.ids, MB_ARTIST).map(str::to_owned);

    let created = artists::upsert_artist(conn, &input).await?;
    let stored = claim_alias(conn, &namespace, EntityKind::Artist, &artist.id, created).await?;
    record_aliases(conn, EntityKind::Artist, stored, &artist.ids, None, None).await?;
    Ok(stored)
}

/// Finds a stored recording by the ISRC it is known under.
///
/// The alias table is asked first, because that is where a materialized
/// recording records its ISRC; a scanned file carries its ISRC in the `tracks`
/// column instead, so that lookup follows.
///
/// # Parameters
///
/// - `conn`: the transaction to read on.
/// - `isrc`: the normalized ISRC, when the catalog track has one.
///
/// # Returns
///
/// The stored track, or `None` when no row is known under that ISRC.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a query fails and
/// [`CatalogError::CorruptAlias`] when a stored id does not decode.
async fn stored_recording(
    conn: &mut SqliteConnection,
    isrc: Option<&str>,
) -> Result<Option<EntityId>, CatalogError> {
    let Some(isrc) = isrc else {
        return Ok(None);
    };
    if let Some(stored) = aliases::entity_by_alias_conn(conn, ISRC, EntityKind::Track, isrc).await?
    {
        return Ok(Some(stored));
    }
    Ok(tracks::find_track_by_isrc(conn, isrc)
        .await?
        .map(|track| track.id))
}

/// Records the canonical identifiers of an entity as aliases.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `kind`: the entity kind the aliases name.
/// - `entity`: the stored row.
/// - `ids`: the identifiers the provider attached.
/// - `isrc`: the entity's own ISRC field, when it has one.
/// - `upc`: the entity's own UPC field, when it has one.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a statement fails.
async fn record_aliases(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    entity: EntityId,
    ids: &[ExternalId],
    isrc: Option<&str>,
    upc: Option<&str>,
) -> Result<(), CatalogError> {
    for id in ids {
        if !id.value.is_empty() && CANONICAL_NAMESPACES.contains(&id.namespace.as_str()) {
            insert_alias(conn, &id.namespace, kind, &id.value, entity).await?;
        }
    }
    if let Some(isrc) = isrc {
        insert_alias(conn, ISRC, kind, isrc, entity).await?;
    }
    if let Some(upc) = upc {
        insert_alias(conn, UPC, kind, upc, entity).await?;
    }
    Ok(())
}

/// Records the MusicBrainz artist ids of a credit list as aliases.
///
/// The credited artists are upserted by the same transaction that writes the
/// track or the album, so the row each credit names is read back by its
/// MusicBrainz id.
///
/// # Parameters
///
/// - `conn`: the transaction to read and write on.
/// - `credits`: the credits the provider returned.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a statement fails.
async fn record_credit_aliases(
    conn: &mut SqliteConnection,
    credits: &[ProviderCredit],
) -> Result<(), CatalogError> {
    for credit in credits {
        let Some(mbid) = canonical_id(&credit.ids, MB_ARTIST) else {
            continue;
        };
        if let Some(artist) = artists::find_artist_by_mb_id(conn, mbid).await? {
            insert_alias(conn, MB_ARTIST, EntityKind::Artist, mbid, artist.id).await?;
        }
    }
    Ok(())
}

/// Reads the tracks of a stored album.
///
/// # Parameters
///
/// - `database`: the database to read.
/// - `album`: the stored album row.
///
/// # Returns
///
/// The album's track rows, in the order the library lists them.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the query fails.
async fn stored_album_tracks(
    database: &Database,
    album: EntityId,
) -> Result<Vec<EntityId>, CatalogError> {
    let mut conn = database.readers().acquire().await?;
    Ok(tracks::list_tracks_for_album(&mut conn, album)
        .await?
        .into_iter()
        .map(|track| track.id)
        .collect())
}

/// Maps a provider's credits to the library's input for them.
///
/// The first credit is the main artist and every later credit is featured,
/// which is the same convention the scanner uses for tagged files.
///
/// # Parameters
///
/// - `credits`: the credits the provider returned.
///
/// # Returns
///
/// The credits, ready for the library writers.
fn credits(credits: &[ProviderCredit]) -> Vec<ArtistCredit> {
    credits
        .iter()
        .enumerate()
        .map(|(position, credit)| ArtistCredit {
            artist: artist_input(credit),
            role: if position == 0 { "main" } else { "featured" }.to_owned(),
        })
        .collect()
}

/// Builds the library input of one credited artist.
///
/// # Parameters
///
/// - `credit`: the credit the provider returned.
///
/// # Returns
///
/// The artist input, with the origin of a materialized row and the MusicBrainz
/// artist id when the credit carries one.
fn artist_input(credit: &ProviderCredit) -> ArtistInput {
    let mut input = ArtistInput::new(credit.name.as_str()).origin(Origin::Catalog);
    input.mb_artist_id = canonical_id(&credit.ids, MB_ARTIST).map(str::to_owned);
    input
}

/// Joins a credit list into the display artist a stored row shows.
///
/// # Parameters
///
/// - `credits`: the credits the provider returned.
///
/// # Returns
///
/// The credit names joined by `, `, or `None` when there is no credit at all.
fn display_artist(credits: &[ProviderCredit]) -> Option<String> {
    if credits.is_empty() {
        return None;
    }
    let names: Vec<&str> = credits.iter().map(|credit| credit.name.as_str()).collect();
    Some(names.join(", "))
}

/// Returns the value of a canonical identifier, when the list carries one.
///
/// # Parameters
///
/// - `ids`: the identifiers a provider attached.
/// - `namespace`: the canonical namespace to look for.
///
/// # Returns
///
/// The first non-empty value in that namespace, or `None`.
fn canonical_id<'a>(ids: &'a [ExternalId], namespace: &str) -> Option<&'a str> {
    ids.iter()
        .find(|id| id.namespace == namespace && !id.value.is_empty())
        .map(|id| id.value.as_str())
}

/// Reads the release year out of an ISO 8601 date.
///
/// # Parameters
///
/// - `date`: the release date a provider returned.
///
/// # Returns
///
/// The year, or `None` when the date does not start with four digits.
fn release_year(date: Option<&str>) -> Option<i64> {
    let year = date?.get(..4)?;
    if year.bytes().all(|byte| byte.is_ascii_digit()) {
        year.parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an identifier list for the reader tests.
    fn ids(pairs: &[(&str, &str)]) -> Vec<ExternalId> {
        pairs
            .iter()
            .map(|(namespace, value)| ExternalId {
                namespace: (*namespace).to_owned(),
                value: (*value).to_owned(),
            })
            .collect()
    }

    #[test]
    fn a_canonical_identifier_is_found_by_its_namespace() {
        let ids = ids(&[("hello", "1"), ("mb-recording", "rec-1")]);

        assert_eq!(canonical_id(&ids, MB_RECORDING), Some("rec-1"));
        assert_eq!(canonical_id(&ids, MB_ARTIST), None);
    }

    #[test]
    fn an_empty_identifier_value_is_not_a_match() {
        let ids = ids(&[("isrc", "")]);

        assert_eq!(canonical_id(&ids, ISRC), None);
    }

    #[test]
    fn the_first_credit_is_main_and_later_ones_are_featured() {
        let credits = credits(&[
            ProviderCredit {
                name: "Energy 52".to_owned(),
                ids: ids(&[(MB_ARTIST, "artist-1")]),
            },
            ProviderCredit {
                name: "Guest".to_owned(),
                ids: Vec::new(),
            },
        ]);

        assert_eq!(credits[0].role, "main");
        assert_eq!(credits[1].role, "featured");
        assert_eq!(credits[0].artist.mb_artist_id.as_deref(), Some("artist-1"));
        assert_eq!(credits[0].artist.origin, Origin::Catalog);
    }

    #[test]
    fn a_credit_list_becomes_one_display_artist() {
        let names = [
            ProviderCredit {
                name: "Energy 52".to_owned(),
                ids: Vec::new(),
            },
            ProviderCredit {
                name: "Guest".to_owned(),
                ids: Vec::new(),
            },
        ];

        assert_eq!(display_artist(&names), Some("Energy 52, Guest".to_owned()));
        assert_eq!(display_artist(&[]), None);
    }

    #[test]
    fn a_release_year_comes_from_a_date_or_a_date_time() {
        assert_eq!(release_year(Some("2026-01-31")), Some(2026));
        assert_eq!(release_year(Some("2026-01-31T10:00:00Z")), Some(2026));
        assert_eq!(release_year(Some("unknown")), None);
        assert_eq!(release_year(None), None);
    }
}
