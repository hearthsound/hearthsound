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
//!    `x_` id keeps resolving forever. An album is the exception: its stored
//!    row is kept, but its tracklist is materialized again from the provider,
//!    because an album whose tracks arrived one at a time can still be missing
//!    some of them;
//! 2. otherwise the full entity is fetched from the issuing instance, its rows
//!    are written with origin `catalog` and availability `remote`, and the
//!    provider id plus every canonical identifier the entity carries (ISRC,
//!    UPC, MusicBrainz ids) is recorded as an alias in the same transaction, so
//!    two providers that know the same recording agree on one row. The provider
//!    id each credited artist carries is recorded the same way, so the artist a
//!    credit names resolves to its stored row;
//! 3. a recording that is already stored under a different alias - a scanned
//!    file whose ISRC matches, or a row another instance materialized - is
//!    reused rather than duplicated;
//! 4. a track that is materialized on its own - which is what streaming a
//!    search result does - is linked to the album it belongs to in the same
//!    transaction. The track carries an album reference, not a stored album, so
//!    the reference's provider id either resolves to an album another
//!    materialization already stored or creates the row from the reference. An
//!    album created that way holds what a reference carries (title, credits,
//!    year); a later materialization of the album itself finds it under its
//!    alias and reuses it. Without this the track row has no album and an
//!    import writes no album artist, album or date tag and names the file after
//!    the bare title.
//!
//! The library crate owns `artists`, `albums` and `tracks`, so the rows are
//! written through its transaction-level functions and never with this crate's
//! own SQL. Only `external_ids` is written here.
//!
//! # Race safety
//!
//! The database has exactly one writer connection, so two materializations
//! serialize: each one takes the connection for the whole transaction. The
//! alias is therefore checked twice for tracks and artists - once before the
//! provider call, to answer an already materialized id without spending
//! provider quota, and once inside the write transaction, where no other writer
//! can commit between the check and the insert. An album always asks the
//! provider for its tracklist, because only that answer says which tracks are
//! missing; its tracks are then resolved inside the write transaction by the
//! same alias rule. The alias insert itself is `ON CONFLICT DO NOTHING` and the
//! winner is read back, so a conflict resolves to the row that exists rather
//! than to an error.

use hearthsound_core::providers::{
    ArtistCredit as ProviderCredit, CANONICAL_NAMESPACES, CatalogAlbum, CatalogAlbumRef,
    CatalogArtist, CatalogTrack, ExternalId, credit_provider_id,
};
use hearthsound_core::{Availability, CatalogId, EntityId, EntityKind, Slug};
use hearthsound_library::{AlbumInput, ArtistCredit, ArtistInput, Origin, TrackInput};
use hearthsound_library::{albums, artists, tracks};
use sqlx::SqliteConnection;

use crate::aliases::{self, PROVIDER_PREFIX};
use crate::dedupe::normalize_isrc;
use crate::error::CatalogError;
use crate::service::{CatalogService, require_kind};

/// The canonical identifier namespaces, re-exported for the rest of the crate.
pub(crate) use hearthsound_core::providers::{
    ISRC, MB_ARTIST, MB_RECORDING, MB_RELEASE, MB_RELEASE_GROUP, UPC,
};

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

/// Materializes one catalog id of any of the three materializable kinds.
///
/// A client that names a catalog entity only knows the kind its id encodes, so
/// the caller that turns such an id into a stored row - a playlist edit, a
/// save, a like - dispatches on it here instead of repeating the match. An
/// album is materialized with its whole tracklist, which keeps the provider's
/// track order.
///
/// # Parameters
///
/// - `catalog`: the catalog layer to materialize through.
/// - `id`: the catalog id to store.
///
/// # Returns
///
/// The stored entity's row: the track, the album, or the artist.
///
/// # Errors
///
/// Returns [`CatalogError::NotFound`] when the id names neither a track, an
/// album nor an artist, and the errors of [`CatalogService::materialize_track`],
/// [`CatalogService::materialize_album`] and
/// [`CatalogService::materialize_artist`] otherwise.
///
/// # Examples
///
/// ```no_run
/// # async fn example(
/// #     catalog: &hearthsound_catalog::CatalogService,
/// #     id: &hearthsound_core::CatalogId,
/// # ) -> Result<(), hearthsound_catalog::CatalogError> {
/// let stored = hearthsound_catalog::materialize_entity(catalog, id).await?;
/// println!("{}", stored);
/// # Ok(())
/// # }
/// ```
pub async fn materialize_entity(
    catalog: &CatalogService,
    id: &CatalogId,
) -> Result<EntityId, CatalogError> {
    match id.kind() {
        EntityKind::Track => catalog.materialize_track(id).await,
        EntityKind::Album => catalog
            .materialize_album(id)
            .await
            .map(|materialized| materialized.album),
        EntityKind::Artist => catalog.materialize_artist(id).await,
        _ => Err(CatalogError::NotFound),
    }
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
    /// The track is linked to its album in the same transaction, so a track
    /// streamed from a search result is stored with an album artist, an album
    /// title and a year - what the import naming template and the written tags
    /// read. The track's album reference names the album's provider id; the
    /// album row is reused when that id already has a `p:<slug>` alias and is
    /// otherwise created from the reference (see [`store_album_ref`]). No album
    /// is created when the recording is reused, because the row that already
    /// exists keeps the album it has.
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
        let album = track.track.album.as_ref().map(TrackAlbum::Reference);
        let stored = store_track(&mut tx, &slug, &track.track, album).await?;
        tx.commit().await?;
        Ok(stored)
    }

    /// Materializes one catalog album and its tracks.
    ///
    /// The album is fetched with its tracklist and every track is materialized
    /// in order inside one transaction, so an album is never left half stored:
    /// either the album and all of its tracks exist, or none of them does.
    ///
    /// An album whose `p:<slug>` alias already exists is completed the same
    /// way: the provider's tracklist is fetched and every track that is not
    /// stored yet is created against the stored album, so an album whose tracks
    /// arrived one at a time - through a search result, a suggestion or a
    /// single acquired track - gains the rest of them. A stored track keeps the
    /// row its alias names, so materializing a fully stored album writes no
    /// duplicate rows and reports the same ids.
    ///
    /// The provider is asked even when the album is fully stored, because only
    /// its tracklist says whether anything is missing. A provider failure is
    /// therefore reported for a stored album too, with the same error a
    /// never-seen album reports; the stored tracks are left untouched.
    ///
    /// # Parameters
    ///
    /// - `id`: the catalog id of an album.
    ///
    /// # Returns
    ///
    /// The stored album row and its track rows, in the provider's track order.
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
/// An album the transaction already stores - its `p:<slug>` alias exists - is
/// completed rather than replaced: the stored row is kept and every provider
/// track that is not stored yet is created against it, so the answer holds the
/// provider's whole tracklist in its order. A track that is already stored
/// keeps the row its alias names; the reuse and dedupe rules of [`store_track`]
/// decide that, so a fully stored album is reported without duplicate rows.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the album.
/// - `album`: the album the provider returned.
///
/// # Returns
///
/// The stored album row and its track rows, in the provider's track order.
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
        let mut tracks_stored = Vec::with_capacity(album.tracks.len());
        for track in &album.tracks {
            tracks_stored
                .push(store_track(conn, slug, track, Some(TrackAlbum::Stored(stored))).await?);
        }
        return Ok((stored, tracks_stored));
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
    record_credit_aliases(conn, slug, &album.artists).await?;

    let mut tracks_stored = Vec::with_capacity(album.tracks.len());
    for track in &album.tracks {
        tracks_stored.push(store_track(conn, slug, track, Some(TrackAlbum::Stored(stored))).await?);
    }
    Ok((stored, tracks_stored))
}

/// The album a materialized track is linked to.
///
/// A track fetched from a provider carries an album reference, not a stored
/// album; the reference is resolved into a row only when the track row is
/// created, so a recording that is reused does not leave an album behind that
/// nothing points at. A track materialized as part of its album already knows
/// the stored album, so it is linked to it directly.
#[derive(Debug, Clone, Copy)]
enum TrackAlbum<'a> {
    /// The album row is already stored.
    Stored(EntityId),
    /// The album is known only by the reference the provider returned.
    Reference(&'a CatalogAlbumRef),
}

/// Stores the album a materialized track belongs to.
///
/// A track fetched on its own carries an album reference: the album's provider
/// id, its title, its credits and a year, without the tracklist, the release
/// date, the UPC or the genres. When another materialization already stored the
/// album, the row its `p:<slug>` alias names is reused; otherwise the row is
/// created from the reference with origin `catalog`, the credits are stored as
/// the album's artists (with their provider-id aliases, the same way a track's
/// credits are), and the reference's provider id is recorded as the album's
/// `p:<slug>` alias.
///
/// The row is deliberately built from the reference and not from a second
/// provider call: the title, the credits and the year are everything the import
/// naming template and the written tags read, so resolving the album of a
/// streamed track spends no catalog quota of its own. A reference carries
/// neither the release date, nor the UPC, nor the genres, nor the tracklist, so
/// a row created here holds none of them. The alias recorded either way keeps
/// the album's provider id resolving, and [`CatalogService::materialize_album`]
/// reports this row and the tracks linked to it instead of asking the instance
/// again and creating a second one.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the track.
/// - `album`: the album reference the track carries.
///
/// # Returns
///
/// The stored album row.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] or [`CatalogError::Library`] when a row
/// cannot be written.
async fn store_album_ref(
    conn: &mut SqliteConnection,
    slug: &Slug,
    album: &CatalogAlbumRef,
) -> Result<EntityId, CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    if let Some(stored) =
        aliases::entity_by_alias_conn(conn, &namespace, EntityKind::Album, &album.id).await?
    {
        return Ok(stored);
    }

    let credits = credits(&album.artists);
    let mut input = AlbumInput::new(album.title.as_str(), Origin::Catalog);
    input.display_artist = display_artist(&album.artists);
    input.year = album.year.map(i64::from);

    let stored = albums::upsert_album(conn, &input, &credits).await?;
    let stored = claim_alias(conn, &namespace, EntityKind::Album, &album.id, stored).await?;
    record_credit_aliases(conn, slug, &album.artists).await?;
    Ok(stored)
}

/// Stores one catalog track in an open transaction.
///
/// # Parameters
///
/// - `conn`: the transaction to write on.
/// - `slug`: the instance that issued the track.
/// - `track`: the track the provider returned.
/// - `album`: the album the track belongs to, when the caller knows one.
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
    album: Option<TrackAlbum<'_>>,
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
        record_credit_aliases(conn, slug, &track.artists).await?;
        return Ok(stored);
    }

    // The album is resolved here, after the reuse checks: a recording that is
    // already stored keeps the album it has, so a track fetched on its own does
    // not leave behind an album row that nothing points at.
    let album_id = match album {
        Some(TrackAlbum::Stored(album)) => Some(album),
        Some(TrackAlbum::Reference(reference)) => {
            Some(store_album_ref(conn, slug, reference).await?)
        }
        None => None,
    };

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
    record_credit_aliases(conn, slug, &track.artists).await?;
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

/// Records the artist identifiers of a credit list as aliases.
///
/// The credited artists are upserted by the same transaction that writes the
/// track or the album, so the row each credit names is read back by the
/// MusicBrainz artist id when the credit carries one and otherwise by the name
/// the upsert deduplicated on. Two aliases are recorded per credit: `mb-artist`
/// for the canonical id, so another provider that knows the same artist resolves
/// to one row, and `p:<slug>` for the provider id, so the artist the credit
/// names resolves to the stored row the same way the track or the album does.
///
/// # Parameters
///
/// - `conn`: the transaction to read and write on.
/// - `slug`: the instance that issued the credits.
/// - `credits`: the credits the provider returned.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when a statement fails.
async fn record_credit_aliases(
    conn: &mut SqliteConnection,
    slug: &Slug,
    credits: &[ProviderCredit],
) -> Result<(), CatalogError> {
    let namespace = aliases::provider_namespace(slug);
    for credit in credits {
        let mbid = canonical_id(&credit.ids, MB_ARTIST);
        let provider_id = credit_provider_id(credit);
        if mbid.is_none() && provider_id.is_none() {
            continue;
        }
        let Some(artist) = stored_credited_artist(conn, credit, mbid).await? else {
            continue;
        };
        if let Some(mbid) = mbid {
            insert_alias(conn, MB_ARTIST, EntityKind::Artist, mbid, artist).await?;
        }
        if let Some(provider_id) = provider_id {
            insert_alias(
                conn,
                &namespace,
                EntityKind::Artist,
                &provider_id.value,
                artist,
            )
            .await?;
        }
    }
    Ok(())
}

/// Reads the stored artist one credit names.
///
/// The artist was upserted by the same transaction, and [`artists::upsert_artist`]
/// deduplicates by MusicBrainz artist id when there is one and otherwise by
/// case-insensitive name, so the same rule finds the row here.
///
/// # Parameters
///
/// - `conn`: the transaction to read on.
/// - `credit`: the credit the provider returned.
/// - `mbid`: the credit's MusicBrainz artist id, when it carries one.
///
/// # Returns
///
/// The stored artist, or `None` when the transaction stored none under the
/// credit's identity.
///
/// # Errors
///
/// Returns [`CatalogError::Database`] when the query fails.
async fn stored_credited_artist(
    conn: &mut SqliteConnection,
    credit: &ProviderCredit,
    mbid: Option<&str>,
) -> Result<Option<EntityId>, CatalogError> {
    let found = match mbid {
        Some(mbid) => artists::find_artist_by_mb_id(conn, mbid).await?,
        None => artists::find_artist_by_name(conn, &credit.name).await?,
    };
    Ok(found.map(|artist| artist.id))
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
pub(crate) fn canonical_id<'a>(ids: &'a [ExternalId], namespace: &str) -> Option<&'a str> {
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
pub(crate) fn release_year(date: Option<&str>) -> Option<i64> {
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
