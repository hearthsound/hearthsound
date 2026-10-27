//! The one identity rule: which stored row a provider's record names.
//!
//! Every record a provider returns enters the database through this module: a
//! search hit, a discography entry, an album's tracklist, an artist of a credit,
//! a discovery suggestion. The rule, in order:
//!
//! 1. the identifier the issuing instance gave the record (`p:<slug>`) resolves
//!    to the row it already names;
//! 2. a canonical identifier resolves: `mb-recording` then `isrc` for a track,
//!    `mb-release` then `upc` for an album, `mb-artist` for an artist;
//! 3. evidence resolves: a track is a row of the resolved album at the same
//!    disc and track number, or with the same folded title and a duration within
//!    [`DURATION_TOLERANCE_MS`]; an album is a row with the same primary artist,
//!    the same folded base title and either the same year or the same track
//!    count, and no canonical identifier of its own; a credit inside a resolved
//!    album or track is the artist row credited at the same position under the
//!    same folded name;
//! 4. otherwise the record is inserted as a new row.
//!
//! Then every identifier the record carries is attached. An identifier another
//! row already holds means the two rows are one item: they are merged
//! ([`hearthsound_library::entities::merge`]) and resolution continues on the
//! survivor. A bare artist - a search hit or a suggestion that names an artist
//! and nothing else - is never matched by its name alone, because two artists
//! can share one.
//!
//! Nothing here calls a provider: it is handed records and writes rows.

use hearthsound_core::providers::{
    ArtistCredit, CatalogAlbum, CatalogAlbumRef, CatalogArtist, CatalogTrack, ExternalId, Image,
    MB_ARTIST, MB_RECORDING, MB_RELEASE_GROUP, UPC, provider_namespace,
};
use hearthsound_core::{EntityKind, Slug};
use hearthsound_library::albums::upsert_album;
use hearthsound_library::artists::upsert_artist;
use hearthsound_library::external_ids;
use hearthsound_library::images::{self, ImageStore};
use hearthsound_library::model::{
    AlbumInput, ArtistCredit as LibraryCredit, ArtistInput, TrackInput,
};
use hearthsound_library::{entities, tracks};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::error::CatalogError;
use crate::normalize::{DURATION_TOLERANCE_MS, normalize, normalize_isrc};

/// One album track's identity evidence, as the resolver reads it.
type AlbumTrackRow = (Vec<u8>, Option<i64>, Option<i64>, String, Option<i64>);

/// Resolves provider records to stored rows, writing what does not exist yet.
#[derive(Debug, Clone)]
pub struct EntityResolver {
    images: ImageStore,
}

impl EntityResolver {
    /// Builds a resolver that stores provider images under the artwork root.
    ///
    /// # Parameters
    ///
    /// - `artwork_root`: the directory images are written to.
    pub fn new(artwork_root: impl Into<std::path::PathBuf>) -> EntityResolver {
        EntityResolver {
            images: ImageStore::new(artwork_root),
        }
    }

    /// Resolves a catalog track and returns its row.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `track`: the record.
    ///
    /// # Returns
    ///
    /// The row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn track(
        &self,
        conn: &mut SqliteConnection,
        instance: Uuid,
        slug: &Slug,
        track: &CatalogTrack,
    ) -> Result<Uuid, CatalogError> {
        // Only the track's own identifiers belong to the track: the album its
        // record names is resolved to its own row, which is where that row's
        // provider id and canonical ids are attached. A catalog that states the
        // ISRC in its own field rather than in the id list is read the same way.
        let mut ids = record_ids(slug, &track.ids, &track.id);
        if let Some(isrc) = normalize_isrc(track.isrc.as_deref())
            && !ids
                .iter()
                .any(|id| id.namespace == hearthsound_core::providers::ISRC)
        {
            ids.push(ExternalId {
                namespace: hearthsound_core::providers::ISRC.to_owned(),
                value: isrc,
            });
        }
        let album = match &track.album {
            Some(record) => Some(self.album_ref(conn, instance, slug, record).await?),
            None => None,
        };

        let mut resolved: Option<Uuid> = external_ids::owner_of(
            conn,
            EntityKind::Track,
            &provider_namespace(slug),
            &track.id,
        )
        .await
        .map_err(CatalogError::Library)?;
        if resolved.is_none()
            && let Some(recording) = ids
                .iter()
                .find(|id| id.namespace == MB_RECORDING)
                .map(|id| id.value.clone())
        {
            resolved = external_ids::owner_of(conn, EntityKind::Track, MB_RECORDING, &recording)
                .await
                .map_err(CatalogError::Library)?;
        }
        if resolved.is_none()
            && let Some(isrc) = ids
                .iter()
                .find(|id| id.namespace == hearthsound_core::providers::ISRC)
                .map(|id| id.value.clone())
        {
            let owner = external_ids::owner_of(
                conn,
                EntityKind::Track,
                hearthsound_core::providers::ISRC,
                &isrc,
            )
            .await
            .map_err(CatalogError::Library)?;
            if let Some(owner) = owner
                && self.isrc_album_agrees(conn, owner, album).await?
            {
                resolved = Some(owner);
            }
        }
        if resolved.is_none() {
            resolved = self.track_by_evidence(conn, track, album).await?;
        }
        let row = match resolved {
            Some(row) => row,
            None => {
                let credits = self.credits(conn, &track.artists, None).await?;
                let credits: Vec<LibraryCredit> = credits
                    .iter()
                    .enumerate()
                    .map(|(position, (input, _))| LibraryCredit {
                        artist: input.clone(),
                        role: if position == 0 {
                            "main".to_owned()
                        } else {
                            "featured".to_owned()
                        },
                    })
                    .collect();
                let input = TrackInput {
                    album_id: album.map(|album| {
                        hearthsound_core::EntityId::from_uuid(
                            hearthsound_core::IdPrefix::Album,
                            album,
                        )
                    }),
                    title: track.title.clone(),
                    sort_title: None,
                    display_artist: display_artist(&track.artists),
                    disc_number: track.disc_number.map(i64::from),
                    track_number: track.track_number.map(i64::from),
                    duration_ms: track.duration_ms.map(i64::from),
                    explicit: track.explicit,
                    ids: Vec::new(),
                    file_tags: None,
                    availability: hearthsound_core::Availability::Remote,
                    unavailable_until: None,
                };
                tracks::insert_track(conn, &input, &credits, &[])
                    .await
                    .map_err(CatalogError::Library)?
                    .uuid()
            }
        };
        let survivor = attach_and_merge(conn, EntityKind::Track, row, &ids).await?;
        if let Some(album) = album {
            set_album_if_empty(conn, survivor, album).await?;
        }
        Ok(survivor)
    }

    /// Resolves a catalog album reference and returns its row.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `album`: the reference.
    ///
    /// # Returns
    ///
    /// The row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn album_ref(
        &self,
        conn: &mut SqliteConnection,
        instance: Uuid,
        slug: &Slug,
        album: &CatalogAlbumRef,
    ) -> Result<Uuid, CatalogError> {
        let ids = album_ids(slug, album);
        let credits = self.credits(conn, &album.artists, None).await?;
        let credits: Vec<LibraryCredit> = credits
            .iter()
            .enumerate()
            .map(|(position, credit)| LibraryCredit {
                artist: credit.0.clone(),
                role: if position == 0 {
                    "main".to_owned()
                } else {
                    "featured".to_owned()
                },
            })
            .collect();
        let input = AlbumInput {
            title: album.title.clone(),
            sort_title: None,
            display_artist: display_artist(&album.artists),
            year: album.year.map(i64::from),
            release_date: None,
            album_type: album.album_type.clone(),
            track_count: album.track_count.map(i64::from),
            ids: Vec::new(),
            file_tags: None,
            edition: match &album.edition {
                Some(label) => hearthsound_library::EditionInput::Labelled(label.clone()),
                None => hearthsound_library::EditionInput::Unstated,
            },
        };
        let row = upsert_album(conn, &input, &credits)
            .await
            .map_err(CatalogError::Library)?
            .uuid();
        let survivor = attach_and_merge(conn, EntityKind::Album, row, &ids).await?;
        if !album.images.is_empty() {
            self.store_provider_images(
                conn,
                EntityKind::Album,
                survivor,
                instance,
                "cover",
                &album.images,
            )
            .await?;
        }
        Ok(survivor)
    }

    /// Resolves a full catalog album, with its tracklist.
    ///
    /// The album row is resolved first, then every track, so a track's evidence
    /// can compare against the rows of the album it belongs to.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `album`: the record.
    ///
    /// # Returns
    ///
    /// The album row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn album(
        &self,
        conn: &mut SqliteConnection,
        instance: Uuid,
        slug: &Slug,
        album: &CatalogAlbum,
    ) -> Result<Uuid, CatalogError> {
        let mut ids = record_ids(slug, &album.ids, &album.id);
        if let Some(upc) = &album.upc {
            ids.push(ExternalId {
                namespace: UPC.to_owned(),
                value: upc.clone(),
            });
        }
        let credits = self.credits(conn, &album.artists, None).await?;
        let credits: Vec<LibraryCredit> = credits
            .iter()
            .enumerate()
            .map(|(position, credit)| LibraryCredit {
                artist: credit.0.clone(),
                role: if position == 0 {
                    "main".to_owned()
                } else {
                    "featured".to_owned()
                },
            })
            .collect();
        let input = AlbumInput {
            title: album.title.clone(),
            sort_title: None,
            display_artist: display_artist(&album.artists),
            year: album
                .release_date
                .as_deref()
                .and_then(|date| date.get(..4))
                .and_then(|year| year.parse().ok()),
            release_date: album.release_date.clone(),
            album_type: album.album_type.clone(),
            track_count: Some(album.tracks.len() as i64),
            ids: Vec::new(),
            file_tags: None,
            edition: match &album.edition {
                Some(label) => hearthsound_library::EditionInput::Labelled(label.clone()),
                None => hearthsound_library::EditionInput::Unstated,
            },
        };
        let row = upsert_album(conn, &input, &credits)
            .await
            .map_err(CatalogError::Library)?
            .uuid();
        let album_row = attach_and_merge(conn, EntityKind::Album, row, &ids).await?;
        if !album.genres.is_empty() {
            write_album_genres(conn, album_row, &album.genres).await?;
        }
        if !album.images.is_empty() {
            self.store_provider_images(
                conn,
                EntityKind::Album,
                album_row,
                instance,
                "cover",
                &album.images,
            )
            .await?;
        }
        for track in &album.tracks {
            let mut record = track.clone();
            if record.album.is_none() {
                record.album = Some(CatalogAlbumRef {
                    id: album.id.clone(),
                    title: album.title.clone(),
                    artists: album.artists.clone(),
                    year: input.year.and_then(|year| u16::try_from(year).ok()),
                    images: Vec::new(),
                    album_type: album.album_type.clone(),
                    edition: album.edition.clone(),
                    track_count: Some(album.tracks.len() as u32),
                    ids: album.ids.clone(),
                });
            }
            let track_row = self.track(conn, instance, slug, &record).await?;
            set_album_if_empty(conn, track_row, album_row).await?;
        }
        Ok(album_row)
    }

    /// Resolves a catalog artist and returns its row.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `artist`: the record.
    ///
    /// # Returns
    ///
    /// The row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn artist(
        &self,
        conn: &mut SqliteConnection,
        instance: Uuid,
        slug: &Slug,
        artist: &CatalogArtist,
    ) -> Result<Uuid, CatalogError> {
        let ids = record_ids(slug, &artist.ids, &artist.id);
        let row = self.artist_row(conn, &artist.name, &ids).await?;
        if !artist.images.is_empty() {
            self.store_provider_images(
                conn,
                EntityKind::Artist,
                row,
                instance,
                "portrait",
                &artist.images,
            )
            .await?;
        }
        if let Some(count) = artist.album_count {
            sqlx::query("UPDATE artists SET album_count = COALESCE(album_count, ?) WHERE id = ?")
                .bind(i64::from(count))
                .bind(row.as_bytes().as_slice())
                .execute(&mut *conn)
                .await?;
        }
        Ok(row)
    }

    /// Resolves one artist credit inside a resolved album or track.
    ///
    /// The credit at the same position of the parent is reused when its folded
    /// name matches, which is what keeps a tracklist from creating a second row
    /// for an artist the album already credits.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `credit`: the credit.
    /// - `parent`: the album or track row the credit belongs to, and its kind.
    ///
    /// # Returns
    ///
    /// The artist row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn credit(
        &self,
        conn: &mut SqliteConnection,
        credit: &ArtistCredit,
        parent: Option<(EntityKind, Uuid)>,
    ) -> Result<Uuid, CatalogError> {
        let ids: Vec<ExternalId> = credit
            .ids
            .iter()
            .map(|id| ExternalId {
                namespace: id.namespace.clone(),
                value: id.value.clone(),
            })
            .collect();
        if let Some((kind, parent)) = parent
            && let Some(row) = existing_credit(conn, kind, parent, &credit.name)
                .await
                .map_err(CatalogError::Library)?
        {
            return attach_and_merge(conn, EntityKind::Artist, row, &ids).await;
        }
        self.artist_row(conn, &credit.name, &ids).await
    }

    /// Resolves a bare artist name with the identifiers a credit carries.
    ///
    /// A bare artist never matches a row by its name alone: a search hit or a
    /// suggestion that names an artist describes its identity, not a row it has
    /// already proven. The identifier the instance issued resolves it, and so
    /// does a canonical one; without either, it is a new row.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `slug`: the instance that issued the record.
    /// - `name`: the artist's name.
    /// - `provider_id`: the instance's own id for the artist, when it sent one.
    ///
    /// # Returns
    ///
    /// The row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn bare_artist(
        &self,
        conn: &mut SqliteConnection,
        slug: &Slug,
        name: &str,
        provider_id: Option<&str>,
    ) -> Result<Uuid, CatalogError> {
        let mut ids = Vec::new();
        if let Some(provider_id) = provider_id {
            ids.push(ExternalId {
                namespace: provider_namespace(slug),
                value: provider_id.to_owned(),
            });
        }
        self.artist_row(conn, name, &ids).await
    }

    /// Resolves an artist from the identifiers a credit carries.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `slug`: the instance that issued the record.
    /// - `name`: the artist's name.
    /// - `ids`: the identifiers, canonical or the instance's own.
    ///
    /// # Returns
    ///
    /// The row's raw primary key.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    pub async fn artist_with_ids(
        &self,
        conn: &mut SqliteConnection,
        slug: &Slug,
        name: &str,
        ids: &[ExternalId],
        provider_id: Option<&str>,
    ) -> Result<Uuid, CatalogError> {
        let mut ids = ids.to_vec();
        if let Some(provider_id) = provider_id {
            ids.push(ExternalId {
                namespace: provider_namespace(slug),
                value: provider_id.to_owned(),
            });
        }
        self.artist_row(conn, name, &ids).await
    }

    /// Resolves the artists of a set of credits.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `instance`: the instance that issued the record.
    /// - `slug`: the instance's slug.
    /// - `credits`: the credits.
    /// - `parent`: the album or track row the credits belong to, when it exists.
    ///
    /// # Returns
    ///
    /// One resolved input per credit, in order.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
    async fn credits(
        &self,
        conn: &mut SqliteConnection,
        credits: &[ArtistCredit],
        parent: Option<(EntityKind, Uuid)>,
    ) -> Result<Vec<(ArtistInput, Uuid)>, CatalogError> {
        let mut resolved = Vec::with_capacity(credits.len());
        for credit in credits {
            let row = self.credit(conn, credit, parent).await?;
            let mut input = ArtistInput::new(&credit.name);
            input.row = Some(hearthsound_core::EntityId::from_uuid(
                hearthsound_core::IdPrefix::Artist,
                row,
            ));
            input.ids = credit
                .ids
                .iter()
                .map(|id| ExternalId {
                    namespace: id.namespace.clone(),
                    value: id.value.clone(),
                })
                .collect();
            resolved.push((input, row));
        }
        Ok(resolved)
    }

    /// Resolves an artist by row or inserts one, and merges what the ids name.
    ///
    /// # Parameters
    ///
    /// - `conn`: the connection or transaction to write on.
    /// - `name`: the artist's name.
    /// - `ids`: the identifiers, the instance's own first.
    async fn artist_row(
        &self,
        conn: &mut SqliteConnection,
        name: &str,
        ids: &[ExternalId],
    ) -> Result<Uuid, CatalogError> {
        for id in ids {
            if id.namespace == MB_ARTIST
                && let Some(owner) =
                    external_ids::owner_of(conn, EntityKind::Artist, MB_ARTIST, &id.value)
                        .await
                        .map_err(CatalogError::Library)?
            {
                let survivor = attach_and_merge(conn, EntityKind::Artist, owner, ids).await?;
                return Ok(survivor);
            }
        }
        for id in ids {
            if let Some(owner) =
                external_ids::owner_of(conn, EntityKind::Artist, &id.namespace, &id.value)
                    .await
                    .map_err(CatalogError::Library)?
            {
                let survivor = attach_and_merge(conn, EntityKind::Artist, owner, ids).await?;
                return Ok(survivor);
            }
        }
        let mut input = ArtistInput::new(name);
        input.ids = ids.to_vec();
        let row = upsert_artist(conn, &input)
            .await
            .map_err(CatalogError::Library)?
            .uuid();
        attach_and_merge(conn, EntityKind::Artist, row, ids).await
    }

    /// Stores the provider images of one entity.
    pub(crate) async fn store_provider_images(
        &self,
        conn: &mut SqliteConnection,
        kind: EntityKind,
        entity: Uuid,
        instance: Uuid,
        role: &str,
        images: &[Image],
    ) -> Result<(), CatalogError> {
        let images: Vec<Image> = largest_first(images);
        images::put_provider_images(conn, kind, entity, role, instance, &images)
            .await
            .map_err(CatalogError::Library)?;
        let _ = &self.images;
        Ok(())
    }

    /// Reports whether the album of an ISRC-resolved row agrees with the record.
    ///
    /// An ISRC belongs to a recording, and one recording can appear on several
    /// releases; a row on another album is therefore not the same item, and the
    /// record is resolved separately.
    async fn isrc_album_agrees(
        &self,
        conn: &mut SqliteConnection,
        row: Uuid,
        album: Option<Uuid>,
    ) -> Result<bool, CatalogError> {
        let Some(album) = album else {
            return Ok(true);
        };
        let current: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                .bind(row.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?;
        match current.flatten() {
            Some(bytes) => Ok(Uuid::from_slice(&bytes)
                .map(|value| value == album)
                .unwrap_or(false)),
            None => Ok(true),
        }
    }

    /// Finds the row a track's evidence names, if any.
    async fn track_by_evidence(
        &self,
        conn: &mut SqliteConnection,
        track: &CatalogTrack,
        album: Option<Uuid>,
    ) -> Result<Option<Uuid>, CatalogError> {
        if let Some(album) = album {
            let rows: Vec<AlbumTrackRow> = sqlx::query_as(
                "SELECT id, disc_number, track_number, title, duration_ms FROM tracks \
                 WHERE album_id = ? ORDER BY id",
            )
            .bind(album.as_bytes().as_slice())
            .fetch_all(&mut *conn)
            .await?;
            let folded = normalize(&track.title);
            for (id, disc, number, title, duration) in rows {
                let same_title_and_duration = normalize(&title) == folded
                    && durations_agree(duration, track.duration_ms.map(i64::from));
                // A position identifies the row only when the tags agree about
                // something besides the number: a tracklist whose numbering
                // shifted would otherwise fold one song into another's row.
                let same_position = track.track_number.is_some()
                    && number == track.track_number.map(i64::from)
                    && disc == track.disc_number.map(i64::from)
                    && (normalize(&title) == folded
                        || durations_agree(duration, track.duration_ms.map(i64::from)));
                if same_position || same_title_and_duration {
                    return Ok(Some(
                        Uuid::from_slice(&id).map_err(|_| CatalogError::CorruptId)?,
                    ));
                }
            }
            return Ok(None);
        }
        // Without an album, the primary artist and the title plus a duration
        // inside the tolerance are the only evidence there is.
        let Some(primary) = track.artists.first() else {
            return Ok(None);
        };
        let folded = normalize(&track.title);
        let row: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT t.id FROM tracks t \
               JOIN track_artists ta ON ta.track_id = t.id AND ta.position = 0 \
               JOIN artists a ON a.id = ta.artist_id \
              WHERE a.name = ? COLLATE NOCASE AND t.album_id IS NULL \
              ORDER BY t.id",
        )
        .bind(&primary.name)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let row = Uuid::from_slice(&row).map_err(|_| CatalogError::CorruptId)?;
        let details: Option<(String, Option<i64>)> =
            sqlx::query_as("SELECT title, duration_ms FROM tracks WHERE id = ?")
                .bind(row.as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?;
        match details {
            Some((title, duration))
                if normalize(&title) == folded
                    && durations_agree(duration, track.duration_ms.map(i64::from)) =>
            {
                Ok(Some(row))
            }
            _ => Ok(None),
        }
    }
}

/// Attaches every identifier and merges the rows they already name.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `kind`: the entity kind.
/// - `row`: the row the record resolved to.
/// - `ids`: every identifier the record carries.
///
/// # Returns
///
/// The surviving row.
///
/// # Errors
///
/// Returns [`CatalogError`] when a write fails or an id cannot be decoded.
async fn attach_and_merge(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    row: Uuid,
    ids: &[ExternalId],
) -> Result<Uuid, CatalogError> {
    let mut survivor = row;
    let conflicts = external_ids::attach(conn, kind, survivor, ids)
        .await
        .map_err(CatalogError::Library)?;
    for conflict in conflicts {
        // A release group groups editions; it never makes two releases one row.
        if kind == EntityKind::Album && conflict.namespace == MB_RELEASE_GROUP {
            continue;
        }
        survivor = entities::merge(conn, kind, survivor, conflict.owner)
            .await
            .map_err(CatalogError::Library)?;
    }
    Ok(survivor)
}

/// Builds the identifier list of a record: the instance's own id, then its ids.
///
/// # Parameters
///
/// - `slug`: the issuing instance.
/// - `ids`: the identifiers the record carries.
/// - `provider_id`: the instance's own id for the record.
fn record_ids(slug: &Slug, ids: &[ExternalId], provider_id: &str) -> Vec<ExternalId> {
    let mut out = Vec::with_capacity(ids.len() + 1);
    out.push(ExternalId {
        namespace: provider_namespace(slug),
        value: provider_id.to_owned(),
    });
    for id in ids {
        if !out
            .iter()
            .any(|known| known.namespace == id.namespace && known.value == id.value)
        {
            out.push(id.clone());
        }
    }
    out
}

/// Builds the identifier list of an album reference.
///
/// # Parameters
///
/// - `slug`: the issuing instance.
/// - `album`: the reference.
fn album_ids(slug: &Slug, album: &CatalogAlbumRef) -> Vec<ExternalId> {
    let mut ids = record_ids(slug, &album.ids, &album.id);
    for id in &album.ids {
        if !ids
            .iter()
            .any(|known| known.namespace == id.namespace && known.value == id.value)
        {
            ids.push(id.clone());
        }
    }
    ids
}

/// Returns the display artist of a credit list.
///
/// # Parameters
///
/// - `credits`: the credits.
fn display_artist(credits: &[ArtistCredit]) -> Option<String> {
    let names: Vec<&str> = credits
        .iter()
        .map(|credit| credit.name.as_str())
        .filter(|name| !name.is_empty())
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Returns the artist a parent's credit at `name`'s folded name holds.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the parent's kind.
/// - `parent`: the parent row.
/// - `name`: the credit's name.
async fn existing_credit(
    conn: &mut SqliteConnection,
    kind: EntityKind,
    parent: Uuid,
    name: &str,
) -> Result<Option<Uuid>, hearthsound_library::LibraryError> {
    let folded = normalize(name);
    let rows: Vec<(Vec<u8>, String)> = match kind {
        EntityKind::Album => {
            sqlx::query_as(
                "SELECT aa.artist_id, a.name FROM album_artists aa \
                   JOIN artists a ON a.id = aa.artist_id \
                  WHERE aa.album_id = ? ORDER BY aa.position",
            )
            .bind(parent.as_bytes().as_slice())
            .fetch_all(&mut *conn)
            .await?
        }
        _ => {
            sqlx::query_as(
                "SELECT ta.artist_id, a.name FROM track_artists ta \
                   JOIN artists a ON a.id = ta.artist_id \
                  WHERE ta.track_id = ? ORDER BY ta.position",
            )
            .bind(parent.as_bytes().as_slice())
            .fetch_all(&mut *conn)
            .await?
        }
    };
    for (id, credited) in rows {
        if normalize(&credited) == folded {
            return Uuid::from_slice(&id)
                .map(Some)
                .map_err(hearthsound_library::LibraryError::CorruptId);
        }
    }
    Ok(None)
}

/// Sets a track's album when it has none.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `track`: the track row.
/// - `album`: the album row.
async fn set_album_if_empty(
    conn: &mut SqliteConnection,
    track: Uuid,
    album: Uuid,
) -> Result<(), CatalogError> {
    sqlx::query("UPDATE tracks SET album_id = COALESCE(album_id, ?) WHERE id = ?")
        .bind(album.as_bytes().as_slice())
        .bind(track.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Replaces an album's genres with the provider's list.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album`: the album row.
/// - `genres`: the genre names.
///
/// # Errors
///
/// Returns [`CatalogError`] when a write fails.
pub(crate) async fn write_genres(
    conn: &mut SqliteConnection,
    album: Uuid,
    genres: &[String],
) -> Result<(), CatalogError> {
    write_album_genres(conn, album, genres).await
}

/// Replaces an album's genres with the provider's list.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to write on.
/// - `album`: the album row.
/// - `genres`: the genre names.
async fn write_album_genres(
    conn: &mut SqliteConnection,
    album: Uuid,
    genres: &[String],
) -> Result<(), CatalogError> {
    sqlx::query("DELETE FROM album_genres WHERE album_id = ?")
        .bind(album.as_bytes().as_slice())
        .execute(&mut *conn)
        .await?;
    for genre in genres {
        let name = normalize(genre);
        if name.is_empty() {
            continue;
        }
        let id = hearthsound_core::new_uuid_v7();
        sqlx::query("INSERT INTO genres (id, name) VALUES (?, ?) ON CONFLICT DO NOTHING")
            .bind(id.as_bytes().as_slice())
            .bind(&name)
            .execute(&mut *conn)
            .await?;
        let genre_id: Option<Vec<u8>> = sqlx::query_scalar("SELECT id FROM genres WHERE name = ?")
            .bind(&name)
            .fetch_optional(&mut *conn)
            .await?;
        let Some(genre_id) = genre_id else {
            continue;
        };
        sqlx::query(
            "INSERT INTO album_genres (album_id, genre_id) VALUES (?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(album.as_bytes().as_slice())
        .bind(genre_id.as_slice())
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Orders images largest first, so the first rendition is the best one.
///
/// # Parameters
///
/// - `images`: the renditions a provider named.
fn largest_first(images: &[Image]) -> Vec<Image> {
    let mut ordered = images.to_vec();
    ordered.sort_by_key(|image| {
        std::cmp::Reverse(
            image
                .width
                .zip(image.height)
                .map(|(width, height)| u64::from(width) * u64::from(height))
                .unwrap_or(0),
        )
    });
    ordered
}

/// Reports whether two durations are close enough to be one recording.
///
/// # Parameters
///
/// - `left`: one duration.
/// - `right`: the other.
fn durations_agree(left: Option<i64>, right: Option<i64>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => (left - right).abs() <= DURATION_TOLERANCE_MS,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_inside_the_tolerance_agree() {
        assert!(durations_agree(Some(200_000), Some(202_000)));
        assert!(durations_agree(Some(200_000), Some(197_000)));
        assert!(!durations_agree(Some(200_000), Some(204_000)));
        assert!(!durations_agree(None, Some(200_000)));
    }

    #[test]
    fn the_record_ids_lead_with_the_instances_own_id() {
        let slug = Slug::new("deezer").unwrap();
        let ids = record_ids(
            &slug,
            &[ExternalId {
                namespace: UPC.to_owned(),
                value: "123".to_owned(),
            }],
            "42",
        );
        assert_eq!(ids[0].namespace, "p:deezer");
        assert_eq!(ids[0].value, "42");
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn base_titles_and_names_fold_the_same_way() {
        assert_eq!(
            crate::normalize::base_title("Abbey Road (Remastered)"),
            "abbey road"
        );
        assert_eq!(normalize("Beyonc\u{e9}"), "beyonce");
        assert_eq!(
            crate::normalize::normalize_isrc(Some(" isrc ")).as_deref(),
            Some("ISRC")
        );
    }
}
