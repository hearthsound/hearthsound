//! The indexer: it walks the queue and stores what the providers say.
//!
//! One worker processes one entity at a time. For an entity it calls the enabled
//! instances in the administrator's order - a catalog's record, a metadata
//! provider's release, an artwork provider's images, a discovery provider's
//! similar artists - stores each answer in `entity_sources`, resolves what the
//! answer named ([`crate::resolve`]), and recomputes the entity's fields from
//! everything stored ([`crate::fields`]).
//!
//! Depth decides how far it walks: `head` fetches the entity's own fields,
//! `page` a page's worth of data (an album's tracklist, an artist's discography
//! and top tracks), and `deep` the recursion that queues everything the page
//! points at - a library track's album, an album's credits, a primary artist's
//! discography. A featured artist is only ever indexed to `head`.
//!
//! A viewer that asks for a page whose data the server does not have runs
//! [`Indexer::ensure`], which walks the entity inline under the request's
//! deadline and reports whether it finished. Work that outlives the deadline
//! keeps going and publishes `entity.updated` when it lands.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hearthsound_config::keys;
use hearthsound_core::Layer;
use hearthsound_core::ProviderError;
use hearthsound_core::events::EntityUpdated;
use hearthsound_core::events::{Audience, EventBus, IndexProgress, ServerEvent};
use hearthsound_core::providers::{
    CatalogAlbumRef, CatalogProvider, ExternalId, MB_RECORDING, MB_RELEASE, MB_RELEASE_GROUP, UPC,
    provider_namespace,
};
use hearthsound_core::{EntityKind, Slug};
use hearthsound_db::Database;
use hearthsound_jobs::Shutdown;
use hearthsound_library::index_queue::{self, Depth, QueueItem};
use hearthsound_library::{entities, external_ids};
use hearthsound_plugin_host::ProviderRegistry;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::CatalogError;
use crate::fields::{self, LayerOrder, Precedence};
use crate::images::ImageService;
use crate::resolve::EntityResolver;
use crate::sources::{self, Operation, Status};
use crate::wire::{WireAlbumRefs, WireProviderIds};

/// The job kind that recomputes stored fields after a settings or layer change.
pub const RECOMPUTE_JOB_KIND: &str = "index.recompute";

/// Whether an inline walk finished inside its deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// Everything the depth asks for is stored.
    Complete,
    /// The deadline passed; the rest is still being fetched.
    Partial,
}

/// The entity indexer.
pub struct Indexer {
    database: Arc<Database>,
    registry: Arc<ProviderRegistry>,
    settings: hearthsound_config::settings::Settings,
    events: Arc<EventBus>,
    resolver: EntityResolver,
    in_flight: Arc<Mutex<HashMap<(EntityKind, Uuid), ()>>>,
    done: AtomicU64,
    total: AtomicU64,
}

impl Indexer {
    /// Builds the indexer.
    ///
    /// # Parameters
    ///
    /// - `database`: the open database.
    /// - `registry`: the provider registry.
    /// - `settings`: the settings store, read on every pass so a change applies
    ///   without a restart.
    /// - `events`: the bus `entity.updated` and `index.progress` are published on.
    /// - `images`: the image service, which the resolver's store is shared with.
    /// - `artwork_root`: the directory images are stored under.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        database: Arc<Database>,
        registry: Arc<ProviderRegistry>,
        settings: hearthsound_config::settings::Settings,
        events: Arc<EventBus>,
        _images: Arc<ImageService>,
        artwork_root: impl Into<std::path::PathBuf>,
    ) -> Indexer {
        Indexer {
            database,
            registry,
            settings,
            events,
            resolver: EntityResolver::new(artwork_root),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
        }
    }

    /// Returns the resolver every provider record enters through.
    pub fn resolver(&self) -> &EntityResolver {
        &self.resolver
    }

    /// Returns this process's progress counters: items processed, and items
    /// claimed since the process started.
    ///
    /// The queue's own counts are [`Indexer::queue_counts`]; these two are what
    /// the progress card's "done" and "total" show, and they reset with the
    /// process because they describe its work, not the queue's.
    pub fn progress(&self) -> (u64, u64) {
        (
            self.done.load(Ordering::Relaxed),
            self.total.load(Ordering::Relaxed),
        )
    }

    /// Returns how many items are queued and how many are marked failed.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the queue cannot be read.
    pub async fn queue_counts(&self) -> Result<(i64, i64), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        index_queue::counts(&mut connection)
            .await
            .map_err(CatalogError::Library)
    }

    /// Runs the queue until `shutdown` is signalled.
    ///
    /// # Parameters
    ///
    /// - `shutdown`: the process-wide stop signal.
    pub async fn run(&self, shutdown: Shutdown) {
        loop {
            if shutdown.is_triggered() {
                return;
            }
            match self.claim().await {
                Ok(Some(item)) => {
                    self.total.fetch_add(1, Ordering::Relaxed);
                    let outcome = self.process(&item).await;
                    let (kind, entity) = (item.kind, item.entity);
                    self.finish(item, outcome).await;
                    let _ = (kind, entity);
                    self.publish_progress().await;
                }
                Ok(None) => {
                    if self.drains() {
                        self.publish_progress().await;
                    }
                    tokio::select! {
                        () = shutdown.wait() => return,
                        () = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "could not claim an index queue item");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Walks one entity inline, up to the deadline.
    ///
    /// # Parameters
    ///
    /// - `kind`: the entity kind.
    /// - `id`: the row's raw primary key.
    /// - `depth`: how far to walk.
    /// - `deadline`: the longest this call may take.
    ///
    /// # Returns
    ///
    /// [`Completeness::Complete`] when the walk finished, and
    /// [`Completeness::Partial`] when the deadline passed; the rest keeps running
    /// and publishes `entity.updated` when it lands.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when the entity cannot be read.
    pub async fn ensure(
        &self,
        kind: EntityKind,
        id: Uuid,
        depth: Depth,
        deadline: Duration,
    ) -> Result<Completeness, CatalogError> {
        let item = QueueItem {
            kind,
            entity: id,
            depth,
            priority: 0,
            attempts: 0,
        };
        let this = self.clone_handle();
        let owned = item.clone();
        match tokio::time::timeout(deadline, async move {
            this.in_flight.lock().await.insert((kind, id), ());
            let outcome = this.process(&item).await;
            this.in_flight.lock().await.remove(&(kind, id));
            outcome
        })
        .await
        {
            Ok(Ok(())) => {
                publish_updated(&self.events, kind, id);
                Ok(Completeness::Complete)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let this = self.clone_handle();
                tokio::spawn(async move {
                    let _ = this.process(&owned).await;
                    this.publish_updated(kind, id);
                    this.in_flight.lock().await.remove(&(kind, id));
                });
                Ok(Completeness::Partial)
            }
        }
    }

    /// Returns a cheap handle sharing this indexer's state.
    fn clone_handle(&self) -> IndexerHandle {
        IndexerHandle {
            database: Arc::clone(&self.database),
            registry: Arc::clone(&self.registry),
            settings: self.settings.clone(),
            events: Arc::clone(&self.events),
            resolver: self.resolver.clone(),
            in_flight: Arc::clone(&self.in_flight),
        }
    }

    /// Claims one due row.
    async fn claim(&self) -> Result<Option<QueueItem>, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        index_queue::claim(&mut connection)
            .await
            .map_err(CatalogError::Library)
    }

    /// Reports whether the queue is empty, which is when progress is published.
    fn drains(&self) -> bool {
        self.total.load(Ordering::Relaxed) > 0
    }

    /// Records the outcome of one claimed row.
    async fn finish(&self, item: QueueItem, outcome: Result<(), CatalogError>) {
        let Ok(mut connection) = self.database.writer().acquire().await else {
            return;
        };
        match outcome {
            Ok(()) => {
                let _ = index_queue::complete(&mut connection, &item).await;
                self.done.fetch_add(1, Ordering::Relaxed);
                self.publish_updated(item.kind, item.entity);
            }
            Err(error) => {
                tracing::debug!(
                    ?item.kind,
                    entity = %item.entity,
                    %error,
                    "an index pass failed"
                );
                let _ = index_queue::fail(&mut connection, &item, &error.to_string(), None).await;
            }
        }
    }

    /// Publishes the queue's progress to administrators.
    async fn publish_progress(&self) {
        let Ok(mut connection) = self.database.writer().acquire().await else {
            return;
        };
        let Ok((pending, failed)) = index_queue::counts(&mut connection).await else {
            return;
        };
        self.events.publish_for(
            Audience::Admins,
            ServerEvent::IndexProgress(IndexProgress {
                pending: pending.max(0) as u64,
                failed: failed.max(0) as u64,
                done: self.done.load(Ordering::Relaxed),
                total: self.total.load(Ordering::Relaxed),
            }),
        );
    }

    /// Publishes `entity.updated` for a stored change.
    fn publish_updated(&self, kind: EntityKind, id: Uuid) {
        publish_updated(&self.events, kind, id);
    }

    /// Walks one entity through its depth steps.
    async fn process(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let handle = self.clone_handle();
        handle.process(item).await
    }
}

/// The parts one walk needs, cloned so a spawned walk outlives its caller.
#[derive(Clone)]
struct IndexerHandle {
    database: Arc<Database>,
    registry: Arc<ProviderRegistry>,
    settings: hearthsound_config::settings::Settings,
    events: Arc<EventBus>,
    resolver: EntityResolver,
    in_flight: Arc<Mutex<HashMap<(EntityKind, Uuid), ()>>>,
}

impl IndexerHandle {
    /// Publishes `entity.updated` for a stored change.
    fn publish_updated(&self, kind: EntityKind, id: Uuid) {
        publish_updated(&self.events, kind, id);
    }

    /// Walks one entity through its depth steps.
    async fn process(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let order = self.order();
        match (item.kind, item.depth) {
            (EntityKind::Track, Depth::Head) | (EntityKind::Track, Depth::Page) => {
                self.track_head(item, &order).await
            }
            (EntityKind::Track, Depth::Deep) => self.track_deep(item, &order).await,
            (EntityKind::Album, Depth::Head) | (EntityKind::Album, Depth::Page) => {
                self.album_page(item, &order).await
            }
            (EntityKind::Album, Depth::Deep) => self.album_deep(item, &order).await,
            (EntityKind::Artist, Depth::Head) => self.artist_head(item, &order).await,
            (EntityKind::Artist, Depth::Page) => self.artist_page(item, &order).await,
            (EntityKind::Artist, Depth::Deep) => self.artist_deep(item, &order).await,
            (EntityKind::Playlist, _) => Ok(()),
        }
    }

    /// Returns the layer order the field rule uses.
    fn order(&self) -> LayerOrder {
        let ids = |layer: Layer| -> Vec<Uuid> {
            self.registry
                .layers(layer)
                .into_iter()
                .map(|instance| instance.id())
                .collect()
        };
        LayerOrder {
            catalog: ids(Layer::Catalog),
            metadata: ids(Layer::Metadata),
            artwork: ids(Layer::Artwork),
            discovery: ids(Layer::Discovery),
        }
    }

    /// Recomputes an entity's fields after a pass.
    async fn recompute(&self, kind: EntityKind, id: Uuid) -> Result<(), CatalogError> {
        let precedence = Precedence::parse(
            &self
                .settings
                .current()
                .get(keys::METADATA_PRECEDENCE)
                .unwrap_or_else(|_| "tags".to_owned()),
        );
        let order = self.order();
        let mut connection = self.database.writer().acquire().await?;
        fields::recompute(&mut connection, kind, id, precedence, &order).await?;
        Ok(())
    }

    /// Indexes a track's own fields.
    async fn track_head(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Track, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match provider.track(&provider_id).await {
                Ok(track) => {
                    self.store_catalog_track(item, instance.id(), instance.slug(), &track)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        // A library track with no identifier gets one from the metadata layer,
        // which is what lets a second provider agree on the same recording.
        if !ids.iter().any(|id| id.namespace == MB_RECORDING) {
            self.resolve_library_track(item).await?;
        }
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE tracks SET fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Track, item.entity).await
    }

    /// Indexes a track and queues its album.
    async fn track_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.track_head(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let album: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT album_id FROM tracks WHERE id = ?")
                .bind(item.entity.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?;
        sqlx::query("UPDATE tracks SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        if let Some(Some(bytes)) = album
            && bytes.len() == 16
            && let Ok(album) = Uuid::from_slice(&bytes)
        {
            index_queue::enqueue(
                &mut connection,
                EntityKind::Album,
                album,
                Depth::Deep,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        Ok(())
    }

    /// Indexes an album's page: its record, tracklist, release and images.
    async fn album_page(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Album, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match provider.album(&provider_id).await {
                Ok(album) => {
                    self.store_catalog_album(item, instance.id(), instance.slug(), &album)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        // An album with canonical ids but no catalog record is looked up by the
        // ids themselves, which is what lets MusicBrainz name a release the
        // catalog layer does not carry.
        if !ids
            .iter()
            .any(|id| id.namespace.starts_with("p:") && id.value != ids[0].value)
            && ids.iter().any(|id| {
                id.namespace == MB_RELEASE
                    || id.namespace == MB_RELEASE_GROUP
                    || id.namespace == UPC
            })
        {
            self.lookup_album(item, &ids).await?;
        }
        if ids
            .iter()
            .any(|id| id.namespace == MB_RELEASE || id.namespace == MB_RELEASE_GROUP)
        {
            self.album_release(item, &ids).await?;
        }
        self.album_images(item).await?;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE albums SET fetched_at = ?, page_fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Album, item.entity).await
    }

    /// Indexes an album and queues the artists its credits name.
    async fn album_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.album_page(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let credits: Vec<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT artist_id, role FROM album_artists WHERE album_id = ? ORDER BY position",
        )
        .bind(item.entity.as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await?;
        for (artist, role) in credits {
            if artist.len() != 16 {
                continue;
            }
            let Ok(artist) = Uuid::from_slice(&artist) else {
                continue;
            };
            // A primary artist is deep-indexed once; afterwards an album only
            // refreshes its own fields, which is what keeps the recursion from
            // circling between artists and their albums.
            let deep = if role == "main" {
                let indexed_at: Option<i64> =
                    sqlx::query_scalar("SELECT deep_indexed_at FROM artists WHERE id = ?")
                        .bind(artist.as_bytes().as_slice())
                        .fetch_optional(&mut *connection)
                        .await?
                        .flatten();
                indexed_at.is_none()
            } else {
                false
            };
            let depth = if deep { Depth::Deep } else { Depth::Head };
            index_queue::enqueue(
                &mut connection,
                EntityKind::Artist,
                artist,
                depth,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        sqlx::query("UPDATE albums SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        Ok(())
    }

    /// Indexes an artist's own fields.
    async fn artist_head(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            match provider.artist(&provider_id).await {
                Ok(artist) => {
                    self.store_catalog_artist(item, instance.id(), instance.slug(), &artist)
                        .await?;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Catalog,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        let name = self.artist_name(item.entity).await?;
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let request = hearthsound_core::providers::ArtistInfoRequest {
                name: name.clone(),
                ids: ids.clone(),
            };
            match provider.artist_info(&request).await {
                Ok(Some(info)) => {
                    let payload = serde_json::to_string(&crate::wire::WireArtistInfo::from(&info))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::ArtistInfo,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                }
                Ok(None) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::ArtistInfo,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a catalog instance could not answer while indexing"
                    );
                }
            }
        }
        self.adopt_catalog_ids(item, &name).await?;
        self.artist_images(item).await?;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE artists SET fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        let _ = order;
        self.recompute(EntityKind::Artist, item.entity).await
    }

    /// Gives an artist the identifiers of a catalog instance that knows it by
    /// a shared one.
    ///
    /// A discovery service names an artist by a shared identifier such as its
    /// MusicBrainz id, which no catalog instance owns; the catalog that can list
    /// the artist's albums answers by its own id instead. The artist is searched
    /// there by name, and a hit is adopted only when it names an identifier the
    /// artist already holds, so two artists that merely share a name are never
    /// taken for one.
    ///
    /// # Parameters
    ///
    /// - `item`: the artist's queue item.
    /// - `name`: the artist's name.
    ///
    /// # Errors
    ///
    /// Returns [`CatalogError`] when a write fails.
    async fn adopt_catalog_ids(&self, item: &QueueItem, name: &str) -> Result<(), CatalogError> {
        if name.is_empty() {
            return Ok(());
        }
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        if ids.is_empty() {
            return Ok(());
        }
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            if provider_id(&ids, instance.slug()).is_some() {
                continue;
            }
            let query = hearthsound_core::providers::CatalogQuery {
                text: name.to_owned(),
                kinds: vec![EntityKind::Artist],
                limit: 5,
                offset: 0,
            };
            let Ok(results) = provider.search(&query).await else {
                continue;
            };
            for hit in &results.artists {
                if !hit.ids.iter().any(|named| {
                    ids.iter()
                        .any(|held| held.namespace == named.namespace && held.value == named.value)
                }) {
                    continue;
                }
                let mut connection = self.database.writer().acquire().await?;
                self.resolver
                    .artist(&mut connection, instance.id(), instance.slug(), hit)
                    .await?;
                drop(connection);
                break;
            }
        }
        Ok(())
    }

    /// Indexes an artist's page: discography, top tracks and similar artists.
    async fn artist_page(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.artist_head(item, order).await?;
        let ids = self.ids_of(EntityKind::Artist, item.entity).await?;
        let name = self.artist_name(item.entity).await?;

        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            let references = self
                .discography(item, instance.id(), instance.slug(), provider, &provider_id)
                .await?;
            if !references.is_empty() {
                break;
            }
        }
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let Some(provider_id) = provider_id(&ids, instance.slug()) else {
                continue;
            };
            let tracks = match provider.artist_top_tracks(&provider_id, 100).await {
                Ok(tracks) if !tracks.is_empty() => tracks,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no top tracks");
                    continue;
                }
            };
            let mut resolved = Vec::with_capacity(tracks.len());
            let mut connection = self.database.writer().acquire().await?;
            for track in &tracks {
                let row = self
                    .resolver
                    .track(&mut connection, instance.id(), instance.slug(), track)
                    .await?;
                resolved.push(row);
            }
            drop(connection);
            let ids: Vec<String> = resolved.iter().map(Uuid::to_string).collect();
            let payload = serde_json::to_string(&WireProviderIds::new(ids))?;
            self.store_payload(
                item,
                instance.id(),
                Operation::CatalogTopTracks,
                Status::Found,
                Some(&payload),
            )
            .await?;
            break;
        }
        self.similar_artists(item, &name, &ids).await?;
        let mut connection = self.database.writer().acquire().await?;
        sqlx::query("UPDATE artists SET page_fetched_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        drop(connection);
        self.recompute(EntityKind::Artist, item.entity).await
    }

    /// Indexes an artist and queues its discography.
    async fn artist_deep(&self, item: &QueueItem, order: &LayerOrder) -> Result<(), CatalogError> {
        self.artist_page(item, order).await?;
        let mut connection = self.database.writer().acquire().await?;
        let albums: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT album_id FROM album_artists WHERE artist_id = ? ORDER BY position",
        )
        .bind(item.entity.as_bytes().as_slice())
        .fetch_all(&mut *connection)
        .await?;
        for album in albums {
            if album.len() != 16 {
                continue;
            }
            let Ok(album) = Uuid::from_slice(&album) else {
                continue;
            };
            index_queue::enqueue(
                &mut connection,
                EntityKind::Album,
                album,
                Depth::Page,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        sqlx::query("UPDATE artists SET deep_indexed_at = ? WHERE id = ?")
            .bind(unix_millis())
            .bind(item.entity.as_bytes().as_slice())
            .execute(&mut *connection)
            .await?;
        Ok(())
    }

    /// Lists an artist's discography from one catalog instance.
    async fn discography(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        provider: &Arc<dyn CatalogProvider>,
        provider_id: &str,
    ) -> Result<Vec<CatalogAlbumRef>, CatalogError> {
        let mut references = Vec::new();
        let mut offset = 0u32;
        while references.len() < 500 {
            let page = match provider.artist_albums(provider_id, 100, offset).await {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = slug.as_str(),
                        "a catalog instance could not list a discography"
                    );
                    break;
                }
            };
            if page.is_empty() {
                break;
            }
            offset += page.len() as u32;
            references.extend(page);
        }
        let mut connection = self.database.writer().acquire().await?;
        for reference in &references {
            let album = self
                .resolver
                .album_ref(&mut connection, instance, slug, reference)
                .await?;
            // A discography entry needs its page (tracklist and cover), not its
            // own recursion: the artist that owns it is already being indexed.
            index_queue::enqueue(
                &mut connection,
                EntityKind::Album,
                album,
                Depth::Page,
                item.priority,
            )
            .await
            .map_err(CatalogError::Library)?;
        }
        drop(connection);
        let payload = serde_json::to_string(&WireAlbumRefs::new(
            references
                .iter()
                .map(crate::wire::WireAlbumRef::from)
                .collect(),
        ))?;
        self.store_payload(
            item,
            instance,
            Operation::CatalogAlbums,
            Status::Found,
            Some(&payload),
        )
        .await?;
        Ok(references)
    }

    /// Asks the discovery layer for similar artists and stores their rows.
    async fn similar_artists(
        &self,
        item: &QueueItem,
        name: &str,
        ids: &[ExternalId],
    ) -> Result<(), CatalogError> {
        let seed = hearthsound_core::providers::ArtistSeed {
            name: name.to_owned(),
            ids: ids.to_vec(),
        };
        for instance in &self.registry.layers(Layer::Discovery) {
            let Some(provider) = instance.discovery() else {
                continue;
            };
            let suggestions = match provider.similar_artists(None, &seed, 20).await {
                Ok(suggestions) if !suggestions.is_empty() => suggestions,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no similar artists");
                    continue;
                }
            };
            let mut resolved = Vec::with_capacity(suggestions.len());
            let mut connection = self.database.writer().acquire().await?;
            for suggestion in &suggestions {
                let row = self
                    .resolver
                    .artist_with_ids(
                        &mut connection,
                        instance.slug(),
                        &suggestion.name,
                        &suggestion.ids,
                        None,
                    )
                    .await?;
                resolved.push(row);
            }
            drop(connection);
            let ids: Vec<String> = resolved.iter().map(Uuid::to_string).collect();
            let payload = serde_json::to_string(&WireProviderIds::new(ids))?;
            self.store_payload(
                item,
                instance.id(),
                Operation::SimilarArtists,
                Status::Found,
                Some(&payload),
            )
            .await?;
            return Ok(());
        }
        // An instance that names nobody is recorded too, so the page is not
        // walked again on every read.
        if let Some(instance) = self.registry.layers(Layer::Discovery).first() {
            self.store_payload(
                item,
                instance.id(),
                Operation::SimilarArtists,
                Status::Missing,
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// Asks the metadata layer to name a library track's recording.
    async fn resolve_library_track(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let row: Option<(String, Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT title, display_artist, duration_ms FROM tracks WHERE id = ?")
                .bind(item.entity.as_bytes().as_slice())
                .fetch_optional(&mut *connection)
                .await?;
        drop(connection);
        let Some((title, display_artist, duration_ms)) = row else {
            return Ok(());
        };
        let hint = hearthsound_core::providers::RecordingHint {
            title: title.clone(),
            artists: display_artist.iter().cloned().collect(),
            album: None,
            duration_ms: duration_ms.and_then(|value| u32::try_from(value).ok()),
            isrc: None,
        };
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            let matches = match provider.resolve_recording(&hint).await {
                Ok(matches) => matches,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no recording match");
                    continue;
                }
            };
            let Some(best) = matches.first() else {
                continue;
            };
            let mut connection = self.database.writer().acquire().await?;
            let conflicts =
                external_ids::attach(&mut connection, EntityKind::Track, item.entity, &best.ids)
                    .await
                    .map_err(CatalogError::Library)?;
            for conflict in conflicts {
                entities::merge(
                    &mut connection,
                    EntityKind::Track,
                    item.entity,
                    conflict.owner,
                )
                .await
                .map_err(CatalogError::Library)?;
            }
            break;
        }
        Ok(())
    }

    /// Looks an album up by its canonical ids on the catalog layer.
    async fn lookup_album(&self, item: &QueueItem, ids: &[ExternalId]) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Catalog) {
            let Some(provider) = instance.catalog() else {
                continue;
            };
            let references = match provider.lookup_album(ids).await {
                Ok(references) if !references.is_empty() => references,
                Ok(_) => continue,
                Err(error) => {
                    tracing::warn!(%error, instance = instance.slug().as_str(), "no album lookup");
                    continue;
                }
            };
            let Some(first) = references.first() else {
                continue;
            };
            let mut connection = self.database.writer().acquire().await?;
            let album = self
                .resolver
                .album_ref(&mut connection, instance.id(), instance.slug(), first)
                .await?;
            if album != item.entity {
                entities::merge(&mut connection, EntityKind::Album, item.entity, album)
                    .await
                    .map_err(CatalogError::Library)?;
            }
            let stored = external_ids::ids_of(&mut connection, EntityKind::Album, item.entity)
                .await
                .map_err(CatalogError::Library)?;
            drop(connection);
            // The provider call happens on no connection: a slow service must
            // never hold the writer another request needs.
            if let Some(provider_id) = provider_id(&stored, instance.slug())
                && let Ok(album) = provider.album(&provider_id).await
            {
                self.store_catalog_album(item, instance.id(), instance.slug(), &album)
                    .await?;
            }
            break;
        }
        Ok(())
    }

    /// Asks the metadata layer to materialize an album's release.
    ///
    /// The first instance that answers stores its release payload, which is what
    /// lets a metadata provider's title, date, type, edition and genres win the
    /// fields it fills.
    async fn album_release(
        &self,
        item: &QueueItem,
        ids: &[ExternalId],
    ) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Metadata) {
            let Some(provider) = instance.metadata() else {
                continue;
            };
            match provider.get_release(ids).await {
                Ok(album) => {
                    let payload = serde_json::to_string(&crate::wire::WireAlbum::from(&album))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::Release,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                    let mut connection = self.database.writer().acquire().await?;
                    self.resolver
                        .album(&mut connection, instance.id(), instance.slug(), &album)
                        .await?;
                    break;
                }
                Err(ProviderError::NotFound) => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Release,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "a metadata instance could not materialize the release"
                    );
                }
            }
        }
        Ok(())
    }

    /// Asks the artwork layer for an album's images.
    async fn album_images(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let subject = hearthsound_core::providers::ArtworkSubject {
            kind: EntityKind::Album,
            provider_id: None,
            title: self.title_of(item.kind, item.entity).await?,
            ids: self.ids_of(EntityKind::Album, item.entity).await?,
            artists: Vec::new(),
        };
        self.artwork(item, subject).await
    }

    /// Asks the artwork layer for an artist's images.
    async fn artist_images(&self, item: &QueueItem) -> Result<(), CatalogError> {
        let subject = hearthsound_core::providers::ArtworkSubject {
            kind: EntityKind::Artist,
            provider_id: None,
            title: self.title_of(item.kind, item.entity).await?,
            ids: self.ids_of(EntityKind::Artist, item.entity).await?,
            artists: Vec::new(),
        };
        self.artwork(item, subject).await
    }

    /// Stores what each artwork instance names for a subject.
    async fn artwork(
        &self,
        item: &QueueItem,
        subject: hearthsound_core::providers::ArtworkSubject,
    ) -> Result<(), CatalogError> {
        for instance in &self.registry.layers(Layer::Artwork) {
            let Some(provider) = instance.artwork() else {
                continue;
            };
            match provider.find_artwork(&subject).await {
                Ok(images) if images.is_empty() => {
                    self.store_answer(
                        item,
                        instance.id(),
                        Operation::Artwork,
                        Status::Missing,
                        None,
                    )
                    .await?;
                }
                Ok(images) => {
                    let payload = serde_json::to_string(&crate::wire::WireImages::new(
                        images.iter().map(crate::wire::WireImage::from).collect(),
                    ))?;
                    self.store_payload(
                        item,
                        instance.id(),
                        Operation::Artwork,
                        Status::Found,
                        Some(&payload),
                    )
                    .await?;
                    let role = if subject.kind == EntityKind::Artist {
                        "portrait"
                    } else {
                        "cover"
                    };
                    let mut connection = self.database.writer().acquire().await?;
                    let images = images.clone();
                    let item_kind = item.kind;
                    let item_entity = item.entity;
                    let instance_id = instance.id();
                    self.resolver
                        .store_provider_images(
                            &mut connection,
                            item_kind,
                            item_entity,
                            instance_id,
                            role,
                            &images,
                        )
                        .await?;
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        instance = instance.slug().as_str(),
                        "an artwork instance could not answer"
                    );
                }
            }
        }
        Ok(())
    }

    /// Stores a catalog track's record and every row it names.
    async fn store_catalog_track(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        track: &hearthsound_core::providers::CatalogTrack,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        if let Some(album) = &track.album {
            let album_row = self
                .resolver
                .album_ref(&mut connection, instance, slug, album)
                .await?;
            let mut owned = track.clone();
            owned.album = None;
            let row = self
                .resolver
                .track(&mut connection, instance, slug, &owned)
                .await?;
            sqlx::query("UPDATE tracks SET album_id = COALESCE(album_id, ?) WHERE id = ?")
                .bind(album_row.as_bytes().as_slice())
                .bind(row.as_bytes().as_slice())
                .execute(&mut *connection)
                .await?;
        } else {
            self.resolver
                .track(&mut connection, instance, slug, track)
                .await?;
        }
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireTrack::from(track))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a catalog album's record and every row it names.
    async fn store_catalog_album(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        album: &hearthsound_core::providers::CatalogAlbum,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        self.resolver
            .album(&mut connection, instance, slug, album)
            .await?;
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireAlbum::from(album))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a catalog artist's record.
    async fn store_catalog_artist(
        &self,
        item: &QueueItem,
        instance: Uuid,
        slug: &Slug,
        artist: &hearthsound_core::providers::CatalogArtist,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        self.resolver
            .artist(&mut connection, instance, slug, artist)
            .await?;
        drop(connection);
        let payload = serde_json::to_string(&crate::wire::WireArtist::from(artist))?;
        self.store_payload(
            item,
            instance,
            Operation::Catalog,
            Status::Found,
            Some(&payload),
        )
        .await
    }

    /// Stores a provider's answer that carries no payload.
    async fn store_answer(
        &self,
        item: &QueueItem,
        instance: Uuid,
        operation: Operation,
        status: Status,
        payload: Option<&str>,
    ) -> Result<(), CatalogError> {
        self.store_payload(item, instance, operation, status, payload)
            .await
    }

    /// Stores one answer on its own connection.
    async fn store_payload(
        &self,
        item: &QueueItem,
        instance: Uuid,
        operation: Operation,
        status: Status,
        payload: Option<&str>,
    ) -> Result<(), CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        sources::put(
            &mut connection,
            item.kind,
            item.entity,
            instance,
            operation,
            status,
            payload,
        )
        .await
    }

    /// Reads an entity's stored identifiers.
    async fn ids_of(
        &self,
        kind: EntityKind,
        entity: Uuid,
    ) -> Result<Vec<ExternalId>, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        external_ids::ids_of(&mut connection, kind, entity)
            .await
            .map_err(CatalogError::Library)
    }

    /// Reads an entity's title: an album's title or an artist's name.
    async fn title_of(&self, kind: EntityKind, entity: Uuid) -> Result<String, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let title: Option<String> = match kind {
            EntityKind::Album => {
                sqlx::query_scalar("SELECT title FROM albums WHERE id = ?")
                    .bind(entity.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            EntityKind::Artist => {
                sqlx::query_scalar("SELECT name FROM artists WHERE id = ?")
                    .bind(entity.as_bytes().as_slice())
                    .fetch_optional(&mut *connection)
                    .await?
            }
            _ => None,
        };
        Ok(title.unwrap_or_default())
    }

    /// Reads an artist's name.
    async fn artist_name(&self, entity: Uuid) -> Result<String, CatalogError> {
        let mut connection = self.database.writer().acquire().await?;
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM artists WHERE id = ?")
            .bind(entity.as_bytes().as_slice())
            .fetch_optional(&mut *connection)
            .await?;
        Ok(name.unwrap_or_default())
    }
}

/// Returns the provider id an instance issued for an entity.
///
/// # Parameters
///
/// - `ids`: the entity's stored identifiers.
/// - `slug`: the instance.
fn provider_id(ids: &[ExternalId], slug: &Slug) -> Option<String> {
    let namespace = provider_namespace(slug);
    ids.iter()
        .find(|id| id.namespace == namespace)
        .map(|id| id.value.clone())
}

/// Returns the current time in unix milliseconds.
pub(crate) fn unix_millis() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// Reads the view deadline, the refresh window and the precedence setting.
///
/// # Parameters
///
/// - `settings`: the current snapshot.
///
/// # Returns
///
/// The deadline for an inline walk, the age at which stored data is stale, and
/// the precedence.
pub fn view_policy(
    settings: &hearthsound_config::SettingsStore,
) -> (Duration, Duration, Precedence) {
    let timeout = settings
        .get(keys::INDEX_VIEW_TIMEOUT_MS)
        .unwrap_or(5000)
        .clamp(500, 30_000) as u64;
    let refresh = settings
        .get(keys::INDEX_REFRESH_DAYS)
        .unwrap_or(30)
        .clamp(1, 3650) as u64;
    let precedence = Precedence::parse(
        &settings
            .get(keys::METADATA_PRECEDENCE)
            .unwrap_or_else(|_| "tags".to_owned()),
    );
    (
        Duration::from_millis(timeout),
        Duration::from_secs(refresh * 86_400),
        precedence,
    )
}

/// Publishes `entity.updated` for a stored change.
///
/// # Parameters
///
/// - `events`: the bus.
/// - `kind`: the entity kind.
/// - `id`: the row's raw primary key.
pub(crate) fn publish_updated(events: &EventBus, kind: EntityKind, id: Uuid) {
    let prefix = match kind {
        EntityKind::Track => hearthsound_core::IdPrefix::Track,
        EntityKind::Album => hearthsound_core::IdPrefix::Album,
        EntityKind::Artist => hearthsound_core::IdPrefix::Artist,
        EntityKind::Playlist => hearthsound_core::IdPrefix::Playlist,
    };
    events.publish_for(
        Audience::Everyone,
        ServerEvent::EntityUpdated(EntityUpdated {
            id: hearthsound_core::EntityId::from_uuid(prefix, id).to_string(),
            kind,
        }),
    );
}
