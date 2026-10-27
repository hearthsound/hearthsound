//! The composed views of one entity, shared by both APIs.
//!
//! A view is what a page shows: the entity's own row, its pictures' presence, its
//! relationships (top tracks, discography, similar artists, an album's tracklist)
//! and whether the stored data is complete. Both the native API and the Subsonic
//! adapter render these views, so a page means the same thing whichever protocol
//! asked for it.
//!
//! The composition follows the storage rules: a discography lists one
//! representative per release group (the edition the library holds, else the
//! earliest), an album's tracklist is every row that belongs to it in disc and
//! track order, and a viewer that is a third-party Subsonic client sees only what
//! its own scopes hold.
//!
//! A held entity's page is never fetched while it is being viewed: it is queued
//! for the indexer and answered from what is stored, with `complete: false`, and
//! the page re-reads when `entity.updated` arrives. Only a remote entity - one
//! the library neither stores nor lists - is walked inline under
//! `index.view_timeout_ms`, and `complete: false` says the deadline passed. An
//! entity whose data is older than `index.refresh_days` is queued for a refresh
//! and answered from what is stored.

use std::sync::Arc;

use hearthsound_config::keys;
use hearthsound_core::providers::{ExternalId, MB_ARTIST, MB_RELEASE_GROUP};
use hearthsound_core::{Availability, EntityId, EntityKind, IdPrefix};
use hearthsound_db::Database;
use hearthsound_library::index_queue::Depth;
use hearthsound_library::query::{self, Scope};
use hearthsound_library::{Album, Artist, LibraryError, Track};
use uuid::Uuid;

use crate::error::CatalogError;
use crate::index::{Completeness, Indexer};

/// One listener a view is composed for.
#[derive(Debug, Clone, Copy)]
pub struct Viewer {
    /// The listener.
    pub user: Uuid,
    /// Whether the caller is a third-party Subsonic client, which sees only what
    /// its own scopes hold.
    pub vendor: bool,
}

impl Viewer {
    /// Returns the scope a third-party caller is limited to.
    fn scope(&self) -> Scope {
        Scope::Personal { user_id: self.user }
    }
}

/// One page about an artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistView {
    /// The artist's row.
    pub artist: Artist,
    /// The external links the stored answers name, deduplicated by URL.
    pub links: Vec<Link>,
    /// Whether the server holds a portrait.
    pub has_image: bool,
    /// The artist's most popular tracks, at most five.
    pub top_tracks: Vec<TrackRef>,
    /// The discography, one album per release group, newest first.
    pub albums: Vec<AlbumRef>,
    /// Similar artists, at most twelve.
    pub similar: Vec<ArtistRef>,
    /// Whether the stored data covers the page.
    pub complete: bool,
}

/// One page about an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumView {
    /// The album's row.
    pub album: Album,
    /// The album's credited artists, in order.
    pub artists: Vec<ArtistRef>,
    /// The album's genres.
    pub genres: Vec<String>,
    /// The album's tracks, in disc and track order.
    pub tracks: Vec<TrackRef>,
    /// Other albums by the primary artist, at most twelve.
    pub more_by_artist: Vec<AlbumRef>,
    /// Whether the server holds a cover.
    pub has_image: bool,
    /// Whether the stored data covers the page.
    pub complete: bool,
}

/// One stored track in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    /// The track's row.
    pub track: Track,
    /// The track's credited artists, in order.
    pub artists: Vec<ArtistRef>,
    /// The title of the album it belongs to.
    pub album_title: Option<String>,
    /// Whether the server holds a cover for it or its album.
    pub has_image: bool,
}

/// One stored album in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumRef {
    /// The album's row.
    pub album: Album,
    /// The album's primary artist.
    pub artist: Option<ArtistRef>,
    /// Whether the server holds a cover.
    pub has_image: bool,
}

/// One artist in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistRef {
    /// The artist's row.
    pub artist: Artist,
    /// Whether the server holds a portrait.
    pub has_image: bool,
}

/// One external link of an artist.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Link {
    /// The kind of page, for example `wikipedia` or `homepage`.
    pub kind: String,
    /// The page's URL.
    pub url: String,
}

/// Composes entity views from the stored rows.
#[derive(Clone)]
pub struct EntityViews {
    database: Arc<Database>,
    indexer: Arc<Indexer>,
    settings: hearthsound_config::settings::Settings,
}

impl EntityViews {
    /// Builds the views over the stored rows.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `indexer`: the indexer an incomplete page is walked by.
    /// - `settings`: the settings store, read for the view policy.
    pub fn new(
        database: Arc<Database>,
        indexer: Arc<Indexer>,
        settings: hearthsound_config::settings::Settings,
    ) -> EntityViews {
        EntityViews {
            database,
            indexer,
            settings,
        }
    }

    /// Composes an artist's page.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist's id.
    /// - `viewer`: the listener the page is for.
    ///
    /// # Returns
    ///
    /// The view, or `None` when no row (or the row an id was merged into) exists.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub async fn artist(
        &self,
        id: EntityId,
        viewer: Viewer,
    ) -> Result<Option<ArtistView>, CatalogError> {
        let Some(row) = entities_canonical(&self.database, EntityKind::Artist, id.uuid()).await?
        else {
            return Ok(None);
        };
        let (artist, complete) = self.artist_row(row).await?;
        self.touch(EntityKind::Artist, row, "viewed_at", artist.viewed_at)
            .await;
        let mut connection = self.database.readers().acquire().await?;
        let links = links_of(&artist);
        let has_image = has_image(&mut connection, EntityKind::Artist, row).await?;
        let top_tracks = top_tracks(&mut connection, row, 5, viewer).await?;
        let albums = discography(&mut connection, row, 500, viewer).await?;
        let similar = similar(&mut connection, row, 12, viewer).await?;
        Ok(Some(ArtistView {
            artist,
            links,
            has_image,
            top_tracks,
            albums,
            similar,
            complete,
        }))
    }

    /// Composes an album's page.
    ///
    /// # Parameters
    ///
    /// - `id`: the album's id.
    /// - `viewer`: the listener the page is for.
    ///
    /// # Returns
    ///
    /// The view, or `None` when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub async fn album(
        &self,
        id: EntityId,
        viewer: Viewer,
    ) -> Result<Option<AlbumView>, CatalogError> {
        let Some(row) = entities_canonical(&self.database, EntityKind::Album, id.uuid()).await?
        else {
            return Ok(None);
        };
        let (album, complete) = self.album_row(row).await?;
        self.touch(EntityKind::Album, row, "viewed_at", album.viewed_at)
            .await;
        let mut connection = self.database.readers().acquire().await?;
        let has_image = has_image(&mut connection, EntityKind::Album, row).await?;
        let artists = album_artists(&mut connection, row).await?;
        let genres = album_genres(&mut connection, row).await?;
        let tracks = album_tracks(&mut connection, row, viewer).await?;
        let more_by_artist = match artists.first() {
            Some(artist) => {
                more_by_artist(&mut connection, artist.artist.id.uuid(), row, 12, viewer).await?
            }
            None => Vec::new(),
        };
        Ok(Some(AlbumView {
            album,
            artists,
            genres,
            tracks,
            more_by_artist,
            has_image,
            complete,
        }))
    }

    /// Composes one track's summary.
    ///
    /// # Parameters
    ///
    /// - `id`: the track's id.
    /// - `viewer`: the listener the page is for.
    ///
    /// # Returns
    ///
    /// The summary, or `None` when no row exists.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub async fn track(
        &self,
        id: EntityId,
        viewer: Viewer,
    ) -> Result<Option<TrackRef>, CatalogError> {
        let Some(row) = entities_canonical(&self.database, EntityKind::Track, id.uuid()).await?
        else {
            return Ok(None);
        };
        let track = self.track_row(row).await?;
        let mut connection = self.database.readers().acquire().await?;
        if viewer.vendor
            && !query::track_in_scope(
                &mut connection,
                viewer.scope(),
                EntityId::from_uuid(IdPrefix::Track, row),
            )
            .await
            .map_err(CatalogError::Library)?
        {
            return Ok(None);
        }
        Ok(Some(track_ref(&mut connection, track).await?))
    }

    /// Returns how many top tracks an artist has stored.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist's id.
    /// - `limit`: the most to return.
    /// - `viewer`: the listener the list is for.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub async fn top_tracks(
        &self,
        id: EntityId,
        limit: u32,
        viewer: Viewer,
    ) -> Result<Vec<TrackRef>, CatalogError> {
        let Some(row) = entities_canonical(&self.database, EntityKind::Artist, id.uuid()).await?
        else {
            return Ok(Vec::new());
        };
        let mut connection = self.database.readers().acquire().await?;
        top_tracks(&mut connection, row, limit, viewer).await
    }

    /// Returns only the artist's stored top tracks, in position order.
    ///
    /// Unlike [`EntityViews::top_tracks`], this never appends the artist's own
    /// credited tracks: it is the popularity list the catalog stored, which is
    /// what a queue fallback wants.
    ///
    /// # Parameters
    ///
    /// - `id`: the artist's id.
    /// - `limit`: the most to return.
    /// - `viewer`: the listener the list is for.
    ///
    /// # Returns
    ///
    /// The top tracks, or an empty list when the artist has none stored.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read fails.
    pub async fn artist_top_tracks(
        &self,
        id: EntityId,
        limit: u32,
        viewer: Viewer,
    ) -> Result<Vec<TrackRef>, CatalogError> {
        let Some(row) = entities_canonical(&self.database, EntityKind::Artist, id.uuid()).await?
        else {
            return Ok(Vec::new());
        };
        let mut connection = self.database.readers().acquire().await?;
        stored_top_tracks(&mut connection, row, limit, viewer).await
    }

    /// Reads an artist row, completing its page when the data is missing.
    ///
    /// # Parameters
    ///
    /// - `row`: the artist's raw primary key.
    ///
    /// # Returns
    ///
    /// The row as stored after the attempt, and whether the page's data is
    /// complete.
    async fn artist_row(&self, row: Uuid) -> Result<(Artist, bool), CatalogError> {
        let mut connection = self.database.readers().acquire().await?;
        let artist = hearthsound_library::artists::get_artist(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Artist, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)?;
        drop(connection);
        if artist.page_fetched_at.is_some() {
            // A page stored before a discovery instance was in service holds no
            // similar artists, and the shelf that shows them stays empty until
            // the staleness sweep. The page is queued again - the indexer is the
            // one that asks a provider - and answered as incomplete, so the
            // page reads itself again when the walk lands.
            if self.similar_unanswered(row).await? {
                self.queue_page(EntityKind::Artist, row, Depth::Page)
                    .await?;
                return Ok((artist, false));
            }
            return Ok((artist, true));
        }
        let complete = self
            .complete_page(EntityKind::Artist, row, Depth::Page)
            .await?;
        let mut connection = self.database.readers().acquire().await?;
        let refreshed = hearthsound_library::artists::get_artist(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Artist, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)?;
        Ok((refreshed, complete))
    }

    /// Reports whether an artist's stored page lacks the similar-artist answer a
    /// discovery instance could give it.
    ///
    /// The artist's page stores what a discovery instance named, including the
    /// record that it named nobody. A page walked while no discovery instance
    /// was in service holds neither, so the shelf that lists similar artists
    /// stays empty until the staleness sweep - which is days, not minutes. The
    /// next read queues the page again instead.
    ///
    /// # Parameters
    ///
    /// - `row`: the artist's raw primary key.
    ///
    /// # Returns
    ///
    /// `true` when a discovery instance is in service and no instance has
    /// answered the artist's similar-artist question.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the read fails.
    async fn similar_unanswered(&self, row: Uuid) -> Result<bool, CatalogError> {
        let mut connection = self.database.readers().acquire().await?;
        let serving: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM layer_assignments la \
             JOIN provider_instances pi ON pi.id = la.instance_id \
             WHERE la.layer = ? AND pi.enabled = 1",
        )
        .bind(hearthsound_core::Layer::Discovery.as_str())
        .fetch_one(&mut *connection)
        .await?;
        if serving == 0 {
            return Ok(false);
        }
        let answered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entity_sources WHERE entity_kind = 'artist' \
             AND entity_id = ? AND operation = 'similar-artists'",
        )
        .bind(row.as_bytes().as_slice())
        .fetch_one(&mut *connection)
        .await?;
        Ok(answered == 0)
    }

    /// Queues one entity's page for the indexer at the front of the queue.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `row`: the entity's raw primary key.
    /// - `depth`: how far to walk.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the write fails.
    async fn queue_page(
        &self,
        kind: EntityKind,
        row: Uuid,
        depth: Depth,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        hearthsound_library::index_queue::enqueue(&mut connection, kind, row, depth, 0)
            .await
            .map_err(CatalogError::Library)
    }

    /// Completes one entity's page the way the storage rule says.
    ///
    /// A held entity is queued and never walked while it is being viewed: the
    /// page answers with what is stored and re-reads when the indexer lands. The
    /// indexer is one worker, so a read that waited for it would take as long as
    /// the queue in front of the entity. A row the indexer gave up on counts as
    /// complete - there is nothing more the server can make of it until the
    /// sweep retries - so the page stops waiting for a placeholder.
    ///
    /// A remote entity is walked inline under the view deadline.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `row`: the entity's raw primary key.
    /// - `depth`: how far a walk goes.
    ///
    /// # Returns
    ///
    /// Whether the page's data is complete.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a read or a write fails.
    async fn complete_page(
        &self,
        kind: EntityKind,
        row: Uuid,
        depth: Depth,
    ) -> Result<bool, CatalogError> {
        let mut connection = self.database.readers().acquire().await?;
        let held = crate::held::is_held(&mut connection, kind, row).await?;
        drop(connection);
        if held {
            let mut connection = self.database.writer().acquire().await?;
            hearthsound_library::index_queue::enqueue(&mut connection, kind, row, depth, 0)
                .await
                .map_err(CatalogError::Library)?;
            let state = hearthsound_library::index_queue::state_of(&mut connection, kind, row)
                .await
                .map_err(CatalogError::Library)?;
            drop(connection);
            return Ok(state.as_deref() == Some("failed"));
        }
        let timeout = crate::index::view_policy(self.settings.current().as_ref()).0;
        let outcome = self.indexer.ensure(kind, row, depth, timeout).await?;
        Ok(!matches!(outcome, Completeness::Partial))
    }

    /// Reads an album row, completing its page when the data is missing.
    ///
    /// # Parameters
    ///
    /// - `row`: the album's raw primary key.
    ///
    /// # Returns
    ///
    /// The row as stored after the attempt, and whether the page's data is
    /// complete.
    async fn album_row(&self, row: Uuid) -> Result<(Album, bool), CatalogError> {
        let mut connection = self.database.readers().acquire().await?;
        let album = hearthsound_library::albums::get_album(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Album, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)?;
        drop(connection);
        if album.page_fetched_at.is_some() {
            return Ok((album, true));
        }
        let complete = self
            .complete_page(EntityKind::Album, row, Depth::Page)
            .await?;
        let mut connection = self.database.readers().acquire().await?;
        let refreshed = hearthsound_library::albums::get_album(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Album, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)?;
        Ok((refreshed, complete))
    }

    /// Reads a track row, completing its head when the data is missing.
    ///
    /// # Parameters
    ///
    /// - `row`: the track's raw primary key.
    async fn track_row(&self, row: Uuid) -> Result<Track, CatalogError> {
        let mut connection = self.database.readers().acquire().await?;
        let track = hearthsound_library::tracks::get_track(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Track, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)?;
        drop(connection);
        if track.fetched_at.is_some() {
            return Ok(track);
        }
        let _ = self
            .complete_page(EntityKind::Track, row, Depth::Head)
            .await?;
        let mut connection = self.database.readers().acquire().await?;
        hearthsound_library::tracks::get_track(
            &mut connection,
            EntityId::from_uuid(IdPrefix::Track, row),
        )
        .await
        .map_err(CatalogError::Library)?
        .ok_or(CatalogError::NotFound)
    }

    /// Records that an entity was viewed, when it was not viewed recently.
    ///
    /// The write is spawned: a page never waits on the writer, which the indexer
    /// is using, for a timestamp no client sees. A failure is logged and
    /// dropped, because the next view tries again.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the entity's raw primary key.
    /// - `column`: the timestamp column to write.
    /// - `stored`: the stored timestamp, when the row had one.
    async fn touch(&self, kind: EntityKind, id: Uuid, column: &'static str, stored: Option<i64>) {
        let now = unix_millis();
        if stored.is_some_and(|seen| now - seen < 3_600_000) {
            return;
        }
        let statement = match (kind, column) {
            (EntityKind::Artist, "viewed_at") => "UPDATE artists SET viewed_at = ? WHERE id = ?",
            (EntityKind::Album, "viewed_at") => "UPDATE albums SET viewed_at = ? WHERE id = ?",
            _ => return,
        };
        let database = Arc::clone(&self.database);
        tokio::spawn(async move {
            let Ok(mut connection) = database.writer().acquire().await else {
                return;
            };
            if let Err(error) = sqlx::query(statement)
                .bind(now)
                .bind(id.as_bytes().as_slice())
                .execute(&mut *connection)
                .await
            {
                tracing::debug!(%error, "could not record that an entity was viewed");
            }
        });
    }
}

/// Resolves an id through the redirects a merge leaves.
async fn entities_canonical(
    database: &Database,
    kind: EntityKind,
    id: Uuid,
) -> Result<Option<Uuid>, CatalogError> {
    let mut connection = database.readers().acquire().await?;
    hearthsound_library::entities::canonical(&mut connection, kind, id)
        .await
        .map_err(CatalogError::Library)
}

/// Returns the links an artist's row stores, with the MusicBrainz page added.
fn links_of(artist: &Artist) -> Vec<Link> {
    let mut links: Vec<Link> = serde_json::from_str(&artist.links).unwrap_or_default();
    links.retain(|link| !link.url.is_empty());
    links
}

/// Reports whether an entity has a stored image.
async fn has_image(
    conn: &mut sqlx::SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<bool, CatalogError> {
    let kind_literal = match kind {
        EntityKind::Track => "track",
        EntityKind::Album => "album",
        EntityKind::Artist => "artist",
        EntityKind::Playlist => "playlist",
    };
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 AS \"found: i64\" FROM entity_images \
         WHERE entity_kind = ? AND entity_id = ? LIMIT 1",
    )
    .bind(kind_literal)
    .bind(entity.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(found.is_some())
}

/// Builds a track reference from a stored track.
async fn track_ref(
    conn: &mut sqlx::SqliteConnection,
    track: Track,
) -> Result<TrackRef, CatalogError> {
    let album_id = track.album_id;
    let artists = track_artists(conn, track.id.uuid()).await?;
    let album_title: Option<String> = match album_id {
        Some(album) => {
            sqlx::query_scalar("SELECT title FROM albums WHERE id = ?")
                .bind(album.uuid().as_bytes().as_slice())
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    let has_image = match album_id {
        Some(album) => has_image(conn, EntityKind::Album, album.uuid()).await?,
        None => has_image(conn, EntityKind::Track, track.id.uuid()).await?,
    };
    Ok(TrackRef {
        track,
        artists,
        album_title,
        has_image,
    })
}

/// Lists a track's credited artists.
async fn track_artists(
    conn: &mut sqlx::SqliteConnection,
    track: Uuid,
) -> Result<Vec<ArtistRef>, CatalogError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT artist_id FROM track_artists WHERE track_id = ? ORDER BY position",
    )
    .bind(track.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    artist_refs(conn, rows).await
}

/// Lists an album's credited artists.
async fn album_artists(
    conn: &mut sqlx::SqliteConnection,
    album: Uuid,
) -> Result<Vec<ArtistRef>, CatalogError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT artist_id FROM album_artists WHERE album_id = ? ORDER BY position",
    )
    .bind(album.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    artist_refs(conn, rows).await
}

/// Turns artist row ids into references.
async fn artist_refs(
    conn: &mut sqlx::SqliteConnection,
    rows: Vec<Vec<u8>>,
) -> Result<Vec<ArtistRef>, CatalogError> {
    let mut refs = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != 16 {
            continue;
        }
        let id = Uuid::from_slice(&row).map_err(|_| CatalogError::CorruptId)?;
        let Some(artist) = hearthsound_library::artists::get_artist(
            conn,
            EntityId::from_uuid(IdPrefix::Artist, id),
        )
        .await
        .map_err(CatalogError::Library)?
        else {
            continue;
        };
        let has_image = has_image(conn, EntityKind::Artist, id).await?;
        refs.push(ArtistRef { artist, has_image });
    }
    Ok(refs)
}

/// Lists an album's genres.
async fn album_genres(
    conn: &mut sqlx::SqliteConnection,
    album: Uuid,
) -> Result<Vec<String>, CatalogError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT g.name FROM album_genres ag JOIN genres g ON g.id = ag.genre_id \
         WHERE ag.album_id = ? ORDER BY g.name",
    )
    .bind(album.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows)
}

/// Lists an album's tracks in playing order.
async fn album_tracks(
    conn: &mut sqlx::SqliteConnection,
    album: Uuid,
    viewer: Viewer,
) -> Result<Vec<TrackRef>, CatalogError> {
    let tracks = hearthsound_library::tracks::list_tracks_for_album(
        conn,
        EntityId::from_uuid(IdPrefix::Album, album),
    )
    .await
    .map_err(CatalogError::Library)?;
    let mut refs = Vec::with_capacity(tracks.len());
    for track in tracks {
        if viewer.vendor
            && !query::track_in_scope(conn, viewer.scope(), track.id)
                .await
                .map_err(CatalogError::Library)?
        {
            continue;
        }
        refs.push(track_ref(conn, track).await?);
    }
    Ok(refs)
}

/// Lists an artist's top tracks, then the artist's own library tracks.
async fn top_tracks(
    conn: &mut sqlx::SqliteConnection,
    artist: Uuid,
    limit: u32,
    viewer: Viewer,
) -> Result<Vec<TrackRef>, CatalogError> {
    let mut refs = stored_top_tracks(conn, artist, limit, viewer).await?;
    if refs.len() < limit as usize {
        let own: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT DISTINCT t.id FROM tracks t \
               JOIN track_artists ta ON ta.track_id = t.id \
              WHERE ta.artist_id = ? ORDER BY t.id LIMIT ?",
        )
        .bind(artist.as_bytes().as_slice())
        .bind(i64::from(limit) - refs.len() as i64)
        .fetch_all(&mut *conn)
        .await?;
        for row in own {
            if row.len() != 16 {
                continue;
            }
            let id = Uuid::from_slice(&row).map_err(|_| CatalogError::CorruptId)?;
            if refs.iter().any(|known| known.track.id.uuid() == id) {
                continue;
            }
            let Some(track) = hearthsound_library::tracks::get_track(
                conn,
                EntityId::from_uuid(IdPrefix::Track, id),
            )
            .await
            .map_err(CatalogError::Library)?
            else {
                continue;
            };
            if viewer.vendor
                && !query::track_in_scope(conn, viewer.scope(), track.id)
                    .await
                    .map_err(CatalogError::Library)?
            {
                continue;
            }
            refs.push(track_ref(conn, track).await?);
        }
    }
    Ok(refs)
}

/// Lists an artist's discography, one album per release group.
async fn discography(
    conn: &mut sqlx::SqliteConnection,
    artist: Uuid,
    limit: u32,
    viewer: Viewer,
) -> Result<Vec<AlbumRef>, CatalogError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT album_id FROM album_artists WHERE artist_id = ? ORDER BY position",
    )
    .bind(artist.as_bytes().as_slice())
    .fetch_all(&mut *conn)
    .await?;
    let mut albums: Vec<Album> = Vec::new();
    for row in rows {
        if row.len() != 16 {
            continue;
        }
        let id = Uuid::from_slice(&row).map_err(|_| CatalogError::CorruptId)?;
        let Some(album) =
            hearthsound_library::albums::get_album(conn, EntityId::from_uuid(IdPrefix::Album, id))
                .await
                .map_err(CatalogError::Library)?
        else {
            continue;
        };
        if viewer.vendor
            && !query::album_in_scope(conn, viewer.scope(), album.id)
                .await
                .map_err(CatalogError::Library)?
        {
            continue;
        }
        albums.push(album);
    }
    // A discography holds the artist's albums, EPs and singles: a live
    // recording or a compilation a top track pointed at is a release of one of
    // those, not a work of its own. An album the library holds stays whatever
    // its type says - the server can prove it belongs to the artist, and hiding
    // a library album from its artist's page is worse than listing a compilation.
    let mut kept = Vec::with_capacity(albums.len());
    for album in albums {
        if crate::index::is_discography_release(album.album_type.as_deref())
            || crate::held::album(conn, album.id.uuid()).await?
        {
            kept.push(album);
        }
    }
    let albums = kept;
    // One representative per release group: the edition the library holds when
    // it holds one, else the earliest release.
    let mut groups: std::collections::BTreeMap<String, (Album, bool)> =
        std::collections::BTreeMap::new();
    for album in albums {
        let key = group_key(conn, album.id.uuid(), &album.title).await?;
        let owned = crate::held::album(conn, album.id.uuid()).await?;
        match groups.get(&key) {
            Some((existing, existing_owned)) => {
                if (*existing_owned && !owned) || (!*existing_owned && owned) {
                    if owned {
                        groups.insert(key, (album, owned));
                    }
                } else if earlier(&album, existing) {
                    groups.insert(key, (album, owned));
                }
            }
            None => {
                groups.insert(key, (album, owned));
            }
        }
    }
    let mut chosen: Vec<Album> = groups.into_values().map(|(album, _)| album).collect();
    chosen.sort_by(|left, right| {
        right
            .year
            .cmp(&left.year)
            .then_with(|| left.title.cmp(&right.title))
    });
    let mut refs = Vec::new();
    for album in chosen.into_iter().take(limit as usize) {
        refs.push(album_ref(conn, album).await?);
    }
    Ok(refs)
}

/// Builds an album reference, with its primary artist.
async fn album_ref(
    conn: &mut sqlx::SqliteConnection,
    album: Album,
) -> Result<AlbumRef, CatalogError> {
    let artist = album_artists(conn, album.id.uuid())
        .await?
        .into_iter()
        .next();
    let has_image = has_image(conn, EntityKind::Album, album.id.uuid()).await?;
    Ok(AlbumRef {
        album,
        artist,
        has_image,
    })
}

/// Returns the grouping key of an album: its release group, else its base title.
async fn group_key(
    conn: &mut sqlx::SqliteConnection,
    album: Uuid,
    title: &str,
) -> Result<String, CatalogError> {
    let group: Option<String> = sqlx::query_scalar(
        "SELECT value FROM external_ids \
         WHERE entity_kind = 'album' AND namespace = ? AND entity_id = ? LIMIT 1",
    )
    .bind(MB_RELEASE_GROUP)
    .bind(album.as_bytes().as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match group {
        Some(group) => format!("group:{group}"),
        None => format!("title:{}", crate::normalize::base_title(title)),
    })
}

/// Reports whether one album is earlier than another.
fn earlier(candidate: &Album, existing: &Album) -> bool {
    match (&candidate.release_date, &existing.release_date) {
        (Some(left), Some(right)) => left < right,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Lists other albums by an artist, excluding one album's release group.
async fn more_by_artist(
    conn: &mut sqlx::SqliteConnection,
    artist: Uuid,
    excluded: Uuid,
    limit: u32,
    viewer: Viewer,
) -> Result<Vec<AlbumRef>, CatalogError> {
    let excluded_group = group_key(conn, excluded, "").await?;
    let albums = discography(conn, artist, 64, viewer).await?;
    let mut kept = Vec::new();
    for entry in albums {
        if entry.album.id.uuid() == excluded {
            continue;
        }
        let key = group_key(conn, entry.album.id.uuid(), &entry.album.title).await?;
        if key == excluded_group {
            continue;
        }
        kept.push(entry);
        if kept.len() >= limit as usize {
            break;
        }
    }
    Ok(kept)
}

/// Lists an artist's similar artists.
async fn similar(
    conn: &mut sqlx::SqliteConnection,
    artist: Uuid,
    limit: u32,
    viewer: Viewer,
) -> Result<Vec<ArtistRef>, CatalogError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT similar_id FROM artist_similar WHERE artist_id = ? ORDER BY position LIMIT ?",
    )
    .bind(artist.as_bytes().as_slice())
    .bind(i64::from(limit))
    .fetch_all(&mut *conn)
    .await?;
    if viewer.vendor {
        return Ok(Vec::new());
    }
    artist_refs(conn, rows).await
}

/// Returns the current time in unix milliseconds.
fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Reads the identifiers of an entity, which a page shows as links.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `kind`: the entity kind.
/// - `entity`: the row's raw primary key.
///
/// # Errors
///
/// Returns [`LibraryError`] when the read fails.
pub async fn identifiers(
    conn: &mut sqlx::SqliteConnection,
    kind: EntityKind,
    entity: Uuid,
) -> Result<Vec<ExternalId>, LibraryError> {
    hearthsound_library::external_ids::ids_of(conn, kind, entity).await
}

/// Returns whether an artist has a MusicBrainz id, which a page links to.
///
/// # Parameters
///
/// - `ids`: the artist's identifiers.
pub fn musicbrainz_artist(ids: &[ExternalId]) -> Option<&str> {
    ids.iter()
        .find(|id| id.namespace == MB_ARTIST)
        .map(|id| id.value.as_str())
}

/// Returns the best availability among a list of tracks.
///
/// # Parameters
///
/// - `values`: the tracks' availability.
pub fn best_availability(values: impl IntoIterator<Item = Availability>) -> Availability {
    let mut best: Option<Availability> = None;
    for value in values {
        best = Some(match best {
            Some(current) if rank(current) >= rank(value) => current,
            _ => value,
        });
    }
    best.unwrap_or(Availability::Unavailable)
}

/// Ranks an availability from best to worst.
fn rank(value: Availability) -> u8 {
    match value {
        Availability::Local => 5,
        Availability::Cached => 4,
        Availability::Acquiring => 3,
        Availability::Remote => 2,
        Availability::Unavailable => 1,
    }
}

/// Reads the refresh window the views use.
///
/// # Parameters
///
/// - `settings`: the current snapshot.
pub fn refresh_days(settings: &hearthsound_config::SettingsStore) -> i64 {
    settings.get(keys::INDEX_REFRESH_DAYS).unwrap_or(30)
}

/// Reads only the artist's stored top tracks, in position order.
///
/// # Parameters
///
/// - `conn`: the connection or transaction to read on.
/// - `artist`: the artist row.
/// - `limit`: the most to return.
/// - `viewer`: the listener the list is for.
///
/// # Errors
///
/// Returns [`CatalogError`] when a read fails.
async fn stored_top_tracks(
    conn: &mut sqlx::SqliteConnection,
    artist: Uuid,
    limit: u32,
    viewer: Viewer,
) -> Result<Vec<TrackRef>, CatalogError> {
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT track_id FROM artist_top_tracks WHERE artist_id = ? ORDER BY position LIMIT ?",
    )
    .bind(artist.as_bytes().as_slice())
    .bind(i64::from(limit))
    .fetch_all(&mut *conn)
    .await?;
    let mut refs = Vec::new();
    for row in rows {
        if row.len() != 16 {
            continue;
        }
        let id = Uuid::from_slice(&row).map_err(|_| CatalogError::CorruptId)?;
        let Some(track) =
            hearthsound_library::tracks::get_track(conn, EntityId::from_uuid(IdPrefix::Track, id))
                .await
                .map_err(CatalogError::Library)?
        else {
            continue;
        };
        if viewer.vendor
            && !query::track_in_scope(conn, viewer.scope(), track.id)
                .await
                .map_err(CatalogError::Library)?
        {
            continue;
        }
        refs.push(track_ref(conn, track).await?);
    }
    Ok(refs)
}
